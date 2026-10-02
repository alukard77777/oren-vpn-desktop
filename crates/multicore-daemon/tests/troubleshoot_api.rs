use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use axum::{
    body::Body,
    http::{Request, StatusCode, header},
};
use http_body_util::BodyExt;
use multicore_core::troubleshoot::{
    CheckStep, Finding, FixId, FixReport, ListenPort, StepReport, Tone,
};
use multicore_daemon::troubleshoot::Troubleshooting;
use multicore_daemon::{
    Backend, BackendError, CatalogDto, ConnectionState, EventDto, ImportSubscriptionRequest,
    StatusDto, try_router_with_troubleshooting,
};
use tower::ServiceExt;

const TOKEN: &str = "troubleshoot-token";

#[derive(Clone)]
struct FakeBackend {
    state: ConnectionState,
    recorded: Arc<Mutex<Vec<String>>>,
}

#[async_trait]
impl Backend for FakeBackend {
    async fn status(&self) -> Result<StatusDto, BackendError> {
        Ok(StatusDto {
            state: self.state,
            active_profile_id: None,
            profile: None,
            current_node: None,
            message: None,
            degraded: false,
            subscription: None,
            cores_ready: false,
            service_mode: false,
            imported_profile_id: None,
        })
    }

    async fn catalog(&self) -> Result<CatalogDto, BackendError> {
        Err(BackendError::new())
    }

    async fn import_subscription(
        &self,
        _request: ImportSubscriptionRequest,
    ) -> Result<StatusDto, BackendError> {
        Err(BackendError::new())
    }

    async fn connect(&self) -> Result<StatusDto, BackendError> {
        Err(BackendError::new())
    }

    async fn disconnect(&self) -> Result<StatusDto, BackendError> {
        Err(BackendError::new())
    }

    async fn events_after(&self, _after: u64) -> Result<Vec<EventDto>, BackendError> {
        Ok(Vec::new())
    }

    async fn listen_ports(&self) -> Vec<ListenPort> {
        vec![ListenPort {
            port: 10808,
            udp: false,
            mihomo: false,
        }]
    }

    async fn record_troubleshoot(&self, message: &str) {
        self.recorded.lock().unwrap().push(message.to_owned());
    }
}

#[derive(Default)]
struct FakeTroubleshooting {
    checks: Mutex<Vec<(CheckStep, Vec<ListenPort>)>>,
    fixes: Mutex<Vec<Vec<FixId>>>,
}

#[async_trait]
impl Troubleshooting for FakeTroubleshooting {
    async fn check(&self, step: CheckStep, ports: Vec<ListenPort>) -> StepReport {
        self.checks.lock().unwrap().push((step, ports));
        StepReport {
            findings: vec![Finding::new(Tone::Warning, "Порт 10808 занят", "x")],
            fixes: Vec::new(),
        }
    }

    async fn fix(&self, fixes: Vec<FixId>) -> FixReport {
        self.fixes.lock().unwrap().push(fixes);
        FixReport {
            findings: vec![Finding::new(Tone::Success, "Готово", "")],
            reboot_recommended: true,
        }
    }
}

async fn post(
    state: ConnectionState,
    troubleshooting: &Arc<FakeTroubleshooting>,
    path: &str,
    body: &'static str,
) -> (StatusCode, String, Vec<String>) {
    let recorded = Arc::new(Mutex::new(Vec::new()));
    let backend: Arc<dyn Backend> = Arc::new(FakeBackend {
        state,
        recorded: recorded.clone(),
    });
    let app = try_router_with_troubleshooting(
        backend,
        TOKEN,
        None,
        None,
        Some(troubleshooting.clone() as Arc<dyn Troubleshooting>),
    )
    .unwrap();
    let response = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri(path)
                .header(header::AUTHORIZATION, format!("Bearer {TOKEN}"))
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(body))
                .unwrap(),
        )
        .await
        .unwrap();
    let status = response.status();
    let bytes = response.into_body().collect().await.unwrap().to_bytes();
    let recorded = recorded.lock().unwrap().clone();
    (
        status,
        String::from_utf8_lossy(&bytes).into_owned(),
        recorded,
    )
}

#[tokio::test]
async fn a_check_gets_the_subscription_ports_and_returns_its_cards() {
    let troubleshooting = Arc::new(FakeTroubleshooting::default());
    let (status, body, _) = post(
        ConnectionState::Error,
        &troubleshooting,
        "/v1/troubleshoot/check",
        r#"{"step":"ports"}"#,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let report: StepReport = serde_json::from_str(&body).unwrap();
    assert_eq!(report.findings[0].title, "Порт 10808 занят");
    let checks = troubleshooting.checks.lock().unwrap();
    assert_eq!(checks[0].0, CheckStep::Ports);
    assert_eq!(checks[0].1[0].port, 10808);
}

#[tokio::test]
async fn nothing_runs_while_connected_or_connecting() {
    for state in [ConnectionState::Connected, ConnectionState::Connecting] {
        let troubleshooting = Arc::new(FakeTroubleshooting::default());
        for (path, body) in [
            ("/v1/troubleshoot/check", r#"{"step":"proxy"}"#),
            ("/v1/troubleshoot/fix", r#"{"fixes":["winsock-reset"]}"#),
        ] {
            let (status, body, _) = post(state, &troubleshooting, path, body).await;
            assert_eq!(status, StatusCode::CONFLICT, "{state:?} {path}");
            assert!(body.contains("disconnect_first"));
        }
        assert!(troubleshooting.checks.lock().unwrap().is_empty());
        assert!(troubleshooting.fixes.lock().unwrap().is_empty());
    }
}

#[tokio::test]
async fn fixes_are_only_known_ids_and_each_run_is_logged() {
    let troubleshooting = Arc::new(FakeTroubleshooting::default());
    for body in [
        r#"{"fixes":["remove-dpi-service:Spooler"]}"#,
        r#"{"fixes":["cmd.exe /c whoami"]}"#,
        r#"{"fixes":[]}"#,
        r#"{"fixes":["winsock-reset"],"path":"C:\\x"}"#,
        r#"{"step":"unknown"}"#,
    ] {
        let (status, _, recorded) = post(
            ConnectionState::Ready,
            &troubleshooting,
            "/v1/troubleshoot/fix",
            body,
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
        assert!(recorded.is_empty());
    }
    assert!(troubleshooting.fixes.lock().unwrap().is_empty());
    let (status, body, recorded) = post(
        ConnectionState::Ready,
        &troubleshooting,
        "/v1/troubleshoot/fix",
        r#"{"fixes":["remove-dpi-service:zapret","delete-drover"]}"#,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let report: FixReport = serde_json::from_str(&body).unwrap();
    assert!(report.reboot_recommended);
    assert_eq!(troubleshooting.fixes.lock().unwrap().len(), 1);
    assert_eq!(recorded.len(), 1);
    assert!(
        recorded[0].contains("remove-dpi-service:zapret"),
        "{recorded:?}"
    );
}

#[tokio::test]
async fn without_troubleshooting_the_routes_are_absent() {
    let backend: Arc<dyn Backend> = Arc::new(FakeBackend {
        state: ConnectionState::Ready,
        recorded: Arc::default(),
    });
    let app = multicore_daemon::try_router_shared(backend, TOKEN, None, None).unwrap();
    let response = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/v1/troubleshoot/check")
                .header(header::AUTHORIZATION, format!("Bearer {TOKEN}"))
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(r#"{"step":"proxy"}"#))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::NOT_FOUND);
}
