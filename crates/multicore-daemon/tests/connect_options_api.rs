use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use axum::{
    body::Body,
    http::{Request, StatusCode, header},
};
use multicore_daemon::{
    Backend, BackendError, CatalogDto, ConnectOptions, ConnectionState, EventDto,
    ImportSubscriptionRequest, StatusDto, router,
};
use tower::ServiceExt;

const TOKEN: &str = "connect-options-token";

#[derive(Clone, Default)]
struct RecordingBackend {
    connects: Arc<Mutex<Vec<ConnectOptions>>>,
}

fn status() -> StatusDto {
    StatusDto {
        state: ConnectionState::Connected,
        active_profile_id: None,
        profile: None,
        current_node: None,
        message: None,
        degraded: false,
        subscription: None,
        cores_ready: false,
        service_mode: false,
        imported_profile_id: None,
    }
}

#[async_trait]
impl Backend for RecordingBackend {
    async fn status(&self) -> Result<StatusDto, BackendError> {
        Ok(status())
    }

    async fn catalog(&self) -> Result<CatalogDto, BackendError> {
        Ok(CatalogDto {
            revision: 0,
            groups: Vec::new(),
        })
    }

    async fn import_subscription(
        &self,
        _request: ImportSubscriptionRequest,
    ) -> Result<StatusDto, BackendError> {
        Err(BackendError::new())
    }

    async fn connect(&self) -> Result<StatusDto, BackendError> {
        self.connect_with(ConnectOptions::default()).await
    }

    async fn connect_with(&self, options: ConnectOptions) -> Result<StatusDto, BackendError> {
        self.connects.lock().unwrap().push(options);
        Ok(status())
    }

    async fn disconnect(&self) -> Result<StatusDto, BackendError> {
        Ok(status())
    }

    async fn events_after(&self, _after: u64) -> Result<Vec<EventDto>, BackendError> {
        Ok(Vec::new())
    }
}

async fn post_connect(backend: &RecordingBackend, body: &'static str) -> StatusCode {
    router(backend.clone(), TOKEN)
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/v1/connect")
                .header(header::AUTHORIZATION, format!("Bearer {TOKEN}"))
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(body))
                .unwrap(),
        )
        .await
        .unwrap()
        .status()
}

#[derive(Clone)]
struct FailingBackend;

#[async_trait]
impl Backend for FailingBackend {
    async fn status(&self) -> Result<StatusDto, BackendError> {
        Ok(status())
    }

    async fn catalog(&self) -> Result<CatalogDto, BackendError> {
        Ok(CatalogDto {
            revision: 0,
            groups: Vec::new(),
        })
    }

    async fn import_subscription(
        &self,
        _request: ImportSubscriptionRequest,
    ) -> Result<StatusDto, BackendError> {
        Err(BackendError::new())
    }

    async fn connect(&self) -> Result<StatusDto, BackendError> {
        Err(BackendError::from_start_failure(
            multicore_core::Engine::Mihomo,
            multicore_core::StartFailureCause::TunFailed,
        ))
    }

    async fn disconnect(&self) -> Result<StatusDto, BackendError> {
        Ok(status())
    }

    async fn events_after(&self, _after: u64) -> Result<Vec<EventDto>, BackendError> {
        Ok(Vec::new())
    }
}

#[tokio::test]
async fn a_connect_whose_cores_did_not_start_says_why() {
    let response = router(FailingBackend, TOKEN)
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/v1/connect")
                .header(header::AUTHORIZATION, format!("Bearer {TOKEN}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    // Not a bare 500 "backend_error": the cause has its own code.
    assert_eq!(response.status(), StatusCode::BAD_GATEWAY);
    let body = http_body_util::BodyExt::collect(response.into_body())
        .await
        .unwrap()
        .to_bytes();
    let body: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(body["code"], "tun_failed");
    assert!(
        body["message"]
            .as_str()
            .unwrap()
            .starts_with("Не удалось включить TUN-адаптер."),
        "{body}"
    );
}

#[tokio::test]
async fn an_empty_connect_body_keeps_tun_mode() {
    let backend = RecordingBackend::default();
    assert_eq!(post_connect(&backend, "").await, StatusCode::OK);
    assert_eq!(
        *backend.connects.lock().unwrap(),
        [ConnectOptions {
            tun: true,
            ..Default::default()
        }]
    );
}

#[tokio::test]
async fn a_connect_body_selects_the_mode() {
    let backend = RecordingBackend::default();
    assert_eq!(
        post_connect(&backend, r#"{"tun":false}"#).await,
        StatusCode::OK
    );
    assert_eq!(
        post_connect(&backend, r#"{"tun":true}"#).await,
        StatusCode::OK
    );
    assert_eq!(post_connect(&backend, "{}").await, StatusCode::OK);
    assert_eq!(
        *backend.connects.lock().unwrap(),
        [
            ConnectOptions {
                tun: false,
                ..Default::default()
            },
            ConnectOptions {
                tun: true,
                ..Default::default()
            },
            ConnectOptions {
                tun: true,
                ..Default::default()
            },
        ]
    );
}

#[tokio::test]
async fn a_malformed_connect_body_is_rejected_without_connecting() {
    let backend = RecordingBackend::default();
    for body in [
        "unexpected",
        r#"{"tun":"no"}"#,
        r#"{"tun":false,"extra":1}"#,
    ] {
        assert_eq!(
            post_connect(&backend, body).await,
            StatusCode::BAD_REQUEST,
            "{body}"
        );
    }
    assert!(backend.connects.lock().unwrap().is_empty());
}
