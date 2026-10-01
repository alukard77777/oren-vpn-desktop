use async_trait::async_trait;
use axum::{
    body::Body,
    http::{Method, Request, StatusCode, header},
};
use http_body_util::BodyExt;
use multicore_core::MAX_PROFILES;
use multicore_daemon::{
    ApiErrorBody, Backend, BackendError, CatalogDto, ConnectionState, EventDto,
    ImportSubscriptionRequest, MemoryBackend, ProfileMutationError, StatusDto, SubscriptionListDto,
    router_with_event_epoch,
};
use serde_json::{Value, json};
use tower::ServiceExt;

const TOKEN: &str = "profile-api-token";

struct MutationErrorBackend(ProfileMutationError);

#[async_trait]
impl Backend for MutationErrorBackend {
    async fn status(&self) -> Result<StatusDto, BackendError> {
        Ok(StatusDto {
            state: ConnectionState::Empty,
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
        Ok(CatalogDto {
            revision: 0,
            groups: Vec::new(),
        })
    }

    async fn import_subscription(
        &self,
        _request: ImportSubscriptionRequest,
    ) -> Result<StatusDto, BackendError> {
        Err(BackendError::from_profile_mutation(self.0))
    }

    async fn refresh_subscription_profile(
        &self,
        _id: multicore_core::ProfileId,
    ) -> Result<SubscriptionListDto, BackendError> {
        Err(BackendError::from_profile_mutation(self.0))
    }

    async fn replace_subscription_source(
        &self,
        _id: multicore_core::ProfileId,
        _request: ImportSubscriptionRequest,
    ) -> Result<SubscriptionListDto, BackendError> {
        Err(BackendError::from_profile_mutation(self.0))
    }

    async fn connect(&self) -> Result<StatusDto, BackendError> {
        self.status().await
    }

    async fn disconnect(&self) -> Result<StatusDto, BackendError> {
        self.status().await
    }

    async fn events_after(&self, _after: u64) -> Result<Vec<EventDto>, BackendError> {
        Ok(Vec::new())
    }
}

fn app() -> axum::Router {
    router_with_event_epoch(MemoryBackend::default(), TOKEN, "profiles-test")
}

fn request(method: Method, uri: &str, body: Body) -> Request<Body> {
    Request::builder()
        .method(method)
        .uri(uri)
        .header(header::AUTHORIZATION, format!("Bearer {TOKEN}"))
        .header(header::CONTENT_TYPE, "application/json")
        .body(body)
        .unwrap()
}

async fn json_body(response: axum::response::Response) -> Value {
    let bytes = response.into_body().collect().await.unwrap().to_bytes();
    serde_json::from_slice(&bytes).unwrap()
}

async fn import(app: &axum::Router, url: &str) -> StatusDto {
    let response = app
        .clone()
        .oneshot(request(
            Method::POST,
            "/v1/subscriptions/import",
            Body::from(json!({ "url": url }).to_string()),
        ))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    serde_json::from_value(json_body(response).await).unwrap()
}

async fn list(app: &axum::Router) -> SubscriptionListDto {
    let response = app
        .clone()
        .oneshot(request(Method::GET, "/v1/subscriptions", Body::empty()))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    serde_json::from_value(json_body(response).await).unwrap()
}

#[tokio::test]
async fn every_profile_route_requires_authentication() {
    let id = "00000000-0000-4000-8000-000000000001";
    let routes = [
        (Method::GET, "/v1/subscriptions".to_owned()),
        (Method::PUT, format!("/v1/subscriptions/{id}/activate")),
        (Method::POST, format!("/v1/subscriptions/{id}/refresh")),
        (Method::PUT, format!("/v1/subscriptions/{id}/source")),
        (Method::DELETE, format!("/v1/subscriptions/{id}")),
    ];
    for (method, uri) in routes {
        let response = app()
            .oneshot(
                Request::builder()
                    .method(method)
                    .uri(&uri)
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::UNAUTHORIZED, "{uri}");
        let error: ApiErrorBody = serde_json::from_value(json_body(response).await).unwrap();
        assert_eq!(error.code, "unauthorized");
    }
}

#[tokio::test]
async fn list_is_ordered_exact_and_never_exposes_import_sources() {
    let app = app();
    let first_source = "https://user:password@first.invalid/raw?token=first-secret";
    let second_source = "https://second.invalid/raw?token=second-secret";
    let first = import(&app, first_source).await.active_profile_id.unwrap();
    let second = import(&app, second_source).await.active_profile_id.unwrap();

    let response = app
        .clone()
        .oneshot(request(Method::GET, "/v1/subscriptions", Body::empty()))
        .await
        .unwrap();
    let value = json_body(response).await;
    let text = value.to_string();
    for forbidden in [
        "https://",
        "password",
        "token",
        "first-secret",
        "second-secret",
        "raw",
    ] {
        assert!(!text.contains(forbidden), "leaked {forbidden}");
    }
    let listed: SubscriptionListDto = serde_json::from_value(value.clone()).unwrap();
    assert_eq!(listed.active_profile_id.as_deref(), Some(second.as_str()));
    assert_eq!(listed.profiles.len(), 2);
    assert_eq!(listed.profiles[0].id, first);
    assert_eq!(listed.profiles[1].id, second);
    assert!(!listed.profiles[0].active);
    assert!(listed.profiles[1].active);
    assert!(listed.profiles.iter().all(|profile| profile.available));
    assert_eq!(value["profiles"][0].as_object().unwrap().len(), 8);
}

#[tokio::test]
async fn inactive_refresh_keeps_active_and_activation_and_delete_update_status() {
    let app = app();
    let first = import(&app, "https://first.invalid/sub").await;
    let first_id = first.active_profile_id.unwrap();
    let second_id = import(&app, "https://second.invalid/sub")
        .await
        .active_profile_id
        .unwrap();

    let refreshed = app
        .clone()
        .oneshot(request(
            Method::POST,
            &format!("/v1/subscriptions/{first_id}/refresh"),
            Body::empty(),
        ))
        .await
        .unwrap();
    assert_eq!(refreshed.status(), StatusCode::OK);
    let refreshed: SubscriptionListDto =
        serde_json::from_value(json_body(refreshed).await).unwrap();
    assert_eq!(
        refreshed.active_profile_id.as_deref(),
        Some(second_id.as_str())
    );

    let activated = app
        .clone()
        .oneshot(request(
            Method::PUT,
            &format!("/v1/subscriptions/{first_id}/activate"),
            Body::empty(),
        ))
        .await
        .unwrap();
    let activated: StatusDto = serde_json::from_value(json_body(activated).await).unwrap();
    assert_eq!(
        activated.active_profile_id.as_deref(),
        Some(first_id.as_str())
    );

    let deleted = app
        .clone()
        .oneshot(request(
            Method::DELETE,
            &format!("/v1/subscriptions/{first_id}"),
            Body::empty(),
        ))
        .await
        .unwrap();
    let deleted: StatusDto = serde_json::from_value(json_body(deleted).await).unwrap();
    assert_eq!(
        deleted.active_profile_id.as_deref(),
        Some(second_id.as_str())
    );
    let listed = list(&app).await;
    assert_eq!(listed.profiles.len(), 1);
    assert_eq!(listed.profiles[0].id, second_id);
    assert!(listed.profiles[0].active);
}

#[tokio::test]
async fn source_replacement_targets_one_profile_and_never_echoes_the_url() {
    let app = app();
    let first_id = import(&app, "https://first.invalid/sub")
        .await
        .active_profile_id
        .unwrap();
    let second_id = import(&app, "https://second.invalid/sub")
        .await
        .active_profile_id
        .unwrap();
    let secret = "replacement-secret-canary";

    let response = app
        .clone()
        .oneshot(request(
            Method::PUT,
            &format!("/v1/subscriptions/{first_id}/source"),
            Body::from(
                json!({ "url": format!("https://replacement.invalid/sub?token={secret}") })
                    .to_string(),
            ),
        ))
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::OK);
    let value = json_body(response).await;
    let listed: SubscriptionListDto = serde_json::from_value(value.clone()).unwrap();
    assert_eq!(
        listed.active_profile_id.as_deref(),
        Some(second_id.as_str())
    );
    assert_eq!(listed.profiles.len(), 2);
    assert_eq!(listed.profiles[0].id.as_str(), first_id.as_str());
    assert!(!value.to_string().contains(secret));
    assert!(!value.to_string().contains("https://"));
}

#[tokio::test]
async fn source_replacement_rejects_invalid_non_http_urls_before_backend_work() {
    let app = app();
    let id = import(&app, "https://first.invalid/sub")
        .await
        .active_profile_id
        .unwrap();

    for invalid in ["", "not a URL", "ftp://provider.invalid/sub", "https://"] {
        let response = app
            .clone()
            .oneshot(request(
                Method::PUT,
                &format!("/v1/subscriptions/{id}/source"),
                Body::from(json!({ "url": invalid }).to_string()),
            ))
            .await
            .unwrap();
        assert_eq!(
            response.status(),
            StatusCode::BAD_REQUEST,
            "accepted {invalid}"
        );
        let body = json_body(response).await;
        assert_eq!(body["code"], "invalid_request");
        if !invalid.is_empty() {
            assert!(!body.to_string().contains(invalid), "reflected {invalid}");
        }
    }

    assert_eq!(list(&app).await.profiles.len(), 1);
}

#[tokio::test]
async fn committed_source_replacement_failure_is_explicitly_ambiguous() {
    let id = "00000000-0000-4000-8000-000000000001";
    let app = router_with_event_epoch(
        MutationErrorBackend(ProfileMutationError::Committed),
        TOKEN,
        "profiles-test",
    );
    let response = app
        .oneshot(request(
            Method::PUT,
            &format!("/v1/subscriptions/{id}/source"),
            Body::from(json!({ "url": "https://replacement.invalid/sub" }).to_string()),
        ))
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::INTERNAL_SERVER_ERROR);
    assert_eq!(
        json_body(response).await["code"],
        "profile_mutation_ambiguous"
    );
}

#[tokio::test]
async fn invalid_and_unknown_ids_are_redacted_not_found_even_while_connected() {
    let app = app();
    import(&app, "https://first.invalid/sub").await;
    let connected = app
        .clone()
        .oneshot(request(Method::POST, "/v1/connect", Body::empty()))
        .await
        .unwrap();
    assert_eq!(connected.status(), StatusCode::OK);

    for id in ["not-an-id", "00000000-0000-4000-8000-ffffffffffff"] {
        let response = app
            .clone()
            .oneshot(request(
                Method::DELETE,
                &format!("/v1/subscriptions/{id}"),
                Body::empty(),
            ))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::NOT_FOUND);
        let body = json_body(response).await;
        assert_eq!(body["code"], "profile_not_found");
        assert!(!body.to_string().contains(id));
    }
}

#[tokio::test]
async fn connected_refresh_and_delete_succeed_while_activation_stays_busy() {
    let app = app();
    let id = import(&app, "https://first.invalid/sub")
        .await
        .active_profile_id
        .unwrap();
    app.clone()
        .oneshot(request(Method::POST, "/v1/connect", Body::empty()))
        .await
        .unwrap();

    let activation = app
        .clone()
        .oneshot(request(
            Method::PUT,
            &format!("/v1/subscriptions/{id}/activate"),
            Body::empty(),
        ))
        .await
        .unwrap();
    assert_eq!(activation.status(), StatusCode::CONFLICT);
    assert_eq!(json_body(activation).await["code"], "profile_busy");

    let refresh = app
        .clone()
        .oneshot(request(
            Method::POST,
            &format!("/v1/subscriptions/{id}/refresh"),
            Body::empty(),
        ))
        .await
        .unwrap();
    assert_eq!(refresh.status(), StatusCode::OK);
    let status = app
        .clone()
        .oneshot(request(Method::GET, "/v1/status", Body::empty()))
        .await
        .unwrap();
    let status: StatusDto = serde_json::from_value(json_body(status).await).unwrap();
    assert_eq!(status.state, ConnectionState::Ready);

    app.clone()
        .oneshot(request(Method::POST, "/v1/connect", Body::empty()))
        .await
        .unwrap();
    let deletion = app
        .clone()
        .oneshot(request(
            Method::DELETE,
            &format!("/v1/subscriptions/{id}"),
            Body::empty(),
        ))
        .await
        .unwrap();
    assert_eq!(deletion.status(), StatusCode::OK);
    let deleted: StatusDto = serde_json::from_value(json_body(deletion).await).unwrap();
    assert_eq!(deleted.state, ConnectionState::Empty);
}

#[tokio::test]
async fn seventeenth_distinct_import_returns_profile_limit() {
    let app = app();
    for index in 0..MAX_PROFILES {
        import(&app, &format!("https://provider-{index}.invalid/sub")).await;
    }
    let response = app
        .oneshot(request(
            Method::POST,
            "/v1/subscriptions/import",
            Body::from(r#"{"url":"https://one-too-many.invalid/sub"}"#),
        ))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::CONFLICT);
    assert_eq!(json_body(response).await["code"], "profile_limit");
}

#[tokio::test]
async fn bodyless_profile_routes_reject_bodies_and_normalize_method_mismatches() {
    let app = app();
    let id = import(&app, "https://first.invalid/sub")
        .await
        .active_profile_id
        .unwrap();
    let routes = [
        (Method::GET, "/v1/subscriptions".to_owned()),
        (Method::PUT, format!("/v1/subscriptions/{id}/activate")),
        (Method::POST, format!("/v1/subscriptions/{id}/refresh")),
        (Method::DELETE, format!("/v1/subscriptions/{id}")),
    ];
    for (method, uri) in routes {
        let response = app
            .clone()
            .oneshot(request(method, &uri, Body::from("unexpected")))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::BAD_REQUEST, "{uri}");
        assert_eq!(json_body(response).await["code"], "invalid_request");
    }

    let mismatch = app
        .oneshot(request(
            Method::PATCH,
            &format!("/v1/subscriptions/{id}/activate"),
            Body::empty(),
        ))
        .await
        .unwrap();
    assert_eq!(mismatch.status(), StatusCode::METHOD_NOT_ALLOWED);
    let body = json_body(mismatch).await;
    assert_eq!(body["code"], "method_not_allowed");
    assert!(!body.to_string().contains(&id));
}

#[tokio::test]
async fn backend_storage_and_degraded_conflicts_use_exact_redacted_errors() {
    let id = "00000000-0000-4000-8000-000000000001";
    for (mutation, status, code) in [
        (
            ProfileMutationError::Storage,
            StatusCode::INTERNAL_SERVER_ERROR,
            "backend_error",
        ),
        (
            ProfileMutationError::Connected,
            StatusCode::CONFLICT,
            "profile_busy",
        ),
    ] {
        let response =
            router_with_event_epoch(MutationErrorBackend(mutation), TOKEN, "mutation-error-test")
                .oneshot(request(
                    Method::POST,
                    &format!("/v1/subscriptions/{id}/refresh"),
                    Body::empty(),
                ))
                .await
                .unwrap();
        assert_eq!(response.status(), status);
        let body = json_body(response).await;
        assert_eq!(body["code"], code);
        let serialized = body.to_string();
        assert!(!serialized.contains(id));
        assert!(!serialized.contains("Storage"));
        assert!(!serialized.contains("Connected"));
        assert!(!serialized.contains("https://"));
    }
}

#[tokio::test]
async fn importing_while_connected_answers_with_the_added_profile_and_keeps_the_connection() {
    let app = app();
    let first = import(&app, "https://first.invalid/sub")
        .await
        .active_profile_id
        .unwrap();
    app.clone()
        .oneshot(request(Method::POST, "/v1/connect", Body::empty()))
        .await
        .unwrap();

    let added = import(&app, "https://second.invalid/sub").await;

    assert_eq!(added.state, ConnectionState::Connected);
    assert_eq!(added.active_profile_id.as_deref(), Some(first.as_str()));
    let added_id = added.imported_profile_id.expect("the added profile");
    assert_ne!(added_id, first);
    assert_eq!(list(&app).await.profiles.len(), 2);
}
