//! Provider headers sent as `#key: value` comment lines at the very top of a subscription
//! body (Mihomo YAML, Xray JSON or the combined MULTICORE bundle), for providers that host
//! a static file or run a panel that cannot set HTTP response headers.

use std::{
    collections::VecDeque,
    future::Future,
    pin::Pin,
    sync::{Arc, Mutex},
};

use multicore_core::{
    AtomicSnapshot, FetchError, HttpClient, HttpResponse, Snapshot, SubscriptionFetcher, UA_NATIVE,
};
use multicore_i18n::Lang;

const MIHOMO: &str = "proxies:\n  - name: Node\n    type: socks5\n    server: 127.0.0.1\n    port: 30001\nproxy-groups:\n  - name: Proxy\n    type: select\n    proxies: [Node]\nrules: [MATCH,Proxy]\n";
/// Routes nothing into Xray, so it stands alone without an Xray answer.
const STANDALONE: &str = "proxies:\n  - name: Node\n    type: ss\n    server: 203.0.113.1\n    port: 8388\n    cipher: aes-128-gcm\n    password: pw\nproxy-groups:\n  - name: Proxy\n    type: select\n    proxies: [Node]\nrules: [MATCH,Proxy]\n";
const XRAY: &str = "{\n  \"provider-owned\": true\n}";
const URL: &str = "https://static.example/private-token";

#[derive(Clone)]
struct FakeHttp {
    responses: Arc<Mutex<VecDeque<Result<HttpResponse, FetchError>>>>,
    agents: Arc<Mutex<Vec<&'static str>>>,
}

impl FakeHttp {
    fn new(responses: Vec<Result<HttpResponse, FetchError>>) -> Self {
        Self {
            responses: Arc::new(Mutex::new(responses.into())),
            agents: Arc::default(),
        }
    }
}

impl HttpClient for FakeHttp {
    fn get<'a>(
        &'a self,
        _url: &'a str,
        user_agent: &'static str,
    ) -> Pin<Box<dyn Future<Output = Result<HttpResponse, FetchError>> + Send + 'a>> {
        Box::pin(async move {
            self.agents.lock().unwrap().push(user_agent);
            self.responses
                .lock()
                .unwrap()
                .pop_front()
                .unwrap_or(Err(FetchError::Network))
        })
    }
}

fn response(
    status: u16,
    body: impl Into<Vec<u8>>,
    headers: impl IntoIterator<Item = (&'static str, &'static str)>,
) -> Result<HttpResponse, FetchError> {
    Ok(HttpResponse::new(status, body.into(), headers))
}

fn no_headers() -> [(&'static str, &'static str); 0] {
    []
}

async fn refresh(responses: Vec<Result<HttpResponse, FetchError>>) -> Arc<Snapshot> {
    try_refresh(responses).await.unwrap()
}

async fn try_refresh(
    responses: Vec<Result<HttpResponse, FetchError>>,
) -> Result<Arc<Snapshot>, FetchError> {
    let store = AtomicSnapshot::default();
    let (outcome, _) = SubscriptionFetcher::new(FakeHttp::new(responses))
        .refresh_deferring_logo(URL, &store)
        .await;
    outcome.map(|_| store.current().unwrap())
}

/// A static Mihomo YAML (the panel refuses the combined request and the Xray one).
async fn yaml_only(
    body: impl Into<Vec<u8>>,
    headers: impl IntoIterator<Item = (&'static str, &'static str)>,
) -> Arc<Snapshot> {
    refresh(vec![
        response(404, Vec::new(), []),
        response(200, body, headers),
        response(404, Vec::new(), []),
    ])
    .await
}

fn title(snapshot: &Snapshot) -> &str {
    &snapshot.subscription_info().unwrap().display_name
}

fn bundle_with(mihomo: &str) -> String {
    format!("[{},{}]", serde_json::to_string(mihomo).unwrap(), XRAY)
}

#[tokio::test]
async fn every_supported_header_is_read_from_leading_yaml_comments() {
    let body = format!(
        "#profile-title: base64:8J+agCBGam9yZA==\n\
         #profile-update-interval: 12\n\
         #subscription-userinfo: upload=1; download=123; total=1073741824; expire=1798761600\n\
         #support-url: https://t.me/support\n\
         #profile-web-page-url: https://example.com/home\n\
         #announce: base64:0J/RgNC40LLQtdGC\n\
         #announce-url: https://example.com/news\n\
         #sub-info-button-text: Open\n\
         #sub-info-color: green\n\
         #dumb-mode: on\n\
         #app-language: en\n\
         #flclashx-servicelogo: https://cdn.example/logo.png\n\
         {STANDALONE}"
    );
    let snapshot = yaml_only(body, no_headers()).await;
    let info = snapshot.subscription_info().unwrap();
    assert_eq!(info.display_name, "🚀 Fjord");
    assert_eq!(info.refresh_interval_secs, Some(12 * 60 * 60));
    assert_eq!(info.uploaded_bytes, Some(1));
    assert_eq!(info.downloaded_bytes, Some(123));
    assert_eq!(info.total_bytes, Some(1_073_741_824));
    assert_eq!(info.expires_at_unix, Some(1_798_761_600));
    assert_eq!(info.announcement_text.as_deref(), Some("Привет"));
    assert_eq!(info.announcement_action_label.as_deref(), Some("Open"));
    assert_eq!(
        info.announcement_tone.map(|tone| tone.as_str()),
        Some("success")
    );
    assert!(info.dumb_mode);
    assert_eq!(info.app_language, Some(Lang::En));
    assert_eq!(
        snapshot.subscription_support_url(),
        Some("https://t.me/support")
    );
    assert_eq!(
        snapshot.subscription_home_url(),
        Some("https://example.com/home")
    );
    assert_eq!(
        snapshot.subscription_announcement_url(),
        Some("https://example.com/news")
    );
    assert_eq!(
        snapshot.subscription_logo_url(),
        Some("https://cdn.example/logo.png")
    );
    // The comment block never reaches the core configuration; the YAML is otherwise intact.
    assert_eq!(snapshot.mihomo.raw_yaml(), STANDALONE);
}

#[tokio::test]
async fn keys_are_case_insensitive_with_optional_space_and_vendor_prefixes() {
    let body = format!(
        "# Profile-Title: Plain title\n\
         #   FLCLASHX-DUMB-MODE :  on  \n\
         # vendor-App-Language: ru\n\
         # vendor-subscription-userinfo: total=5\n\
         #banner-text: Banner\n\
         #banner-button-url: https://example.com/banner\n\
         {STANDALONE}"
    );
    let snapshot = yaml_only(body, no_headers()).await;
    let info = snapshot.subscription_info().unwrap();
    assert_eq!(info.display_name, "Plain title");
    assert!(info.dumb_mode);
    assert_eq!(info.app_language, Some(Lang::Ru));
    assert_eq!(info.total_bytes, Some(5));
    assert_eq!(info.announcement_text.as_deref(), Some("Banner"));
    assert_eq!(
        snapshot.subscription_announcement_url(),
        Some("https://example.com/banner")
    );

    let vendor_title = yaml_only(
        format!("#flclashx-servicename: Vendor name\n{STANDALONE}"),
        no_headers(),
    )
    .await;
    assert_eq!(title(&vendor_title), "Vendor name");
}

#[tokio::test]
async fn a_real_header_beats_a_comment_for_the_same_key_and_comments_fill_the_rest() {
    let body = format!(
        "#profile-title: Comment title\n\
         #app-language: ru\n\
         #support-url: https://comment.example/help\n\
         {STANDALONE}"
    );
    let snapshot = yaml_only(
        body,
        [("profile-title", "Header title"), ("app-language", "en")],
    )
    .await;
    let info = snapshot.subscription_info().unwrap();
    assert_eq!(info.display_name, "Header title");
    assert_eq!(info.app_language, Some(Lang::En));
    assert_eq!(
        snapshot.subscription_support_url(),
        Some("https://comment.example/help")
    );

    // A header that was sent still owns its key even when its value is unusable.
    let invalid_header = yaml_only(
        format!("#profile-title: Comment title\n{STANDALONE}"),
        [("profile-title", "base64:%%%")],
    )
    .await;
    assert_eq!(title(&invalid_header), "static.example");
}

#[tokio::test]
async fn a_comment_title_beats_a_file_name_from_content_disposition() {
    let snapshot = yaml_only(
        format!("#profile-title: Comment title\n{STANDALONE}"),
        [("content-disposition", "attachment; filename=config.yaml")],
    )
    .await;
    assert_eq!(title(&snapshot), "Comment title");
}

#[tokio::test]
async fn transport_only_headers_are_never_taken_from_the_body() {
    let snapshot = yaml_only(
        format!(
            "#x-hwid-max-devices-reached: true\n\
             #content-disposition: attachment; filename=Evil\n\
             {STANDALONE}"
        ),
        no_headers(),
    )
    .await;
    assert_eq!(title(&snapshot), "static.example");
}

#[tokio::test]
async fn comment_values_go_through_the_same_sanitisers_as_headers() {
    let snapshot = yaml_only(
        format!(
            "#profile-title: user:secret@provider.example\n\
             #support-url: javascript:alert(1)\n\
             #profile-web-page-url: https://user:secret@provider.example\n\
             #flclashx-servicelogo: http://cdn.example/logo.png\n\
             #app-language: klingon\n\
             {STANDALONE}"
        ),
        no_headers(),
    )
    .await;
    assert_eq!(title(&snapshot), "static.example");
    assert_eq!(snapshot.subscription_support_url(), None);
    assert_eq!(snapshot.subscription_home_url(), None);
    assert_eq!(snapshot.subscription_logo_url(), None);
    assert_eq!(snapshot.subscription_info().unwrap().app_language, None);
}

#[tokio::test]
async fn comments_after_the_first_configuration_line_are_ignored() {
    let body = format!("mixed-port: 7890\n#profile-title: Too late\n{STANDALONE}");
    let snapshot = yaml_only(body.clone(), no_headers()).await;
    assert_eq!(title(&snapshot), "static.example");
    // Without a leading comment block the YAML is kept byte for byte.
    assert_eq!(snapshot.mihomo.raw_yaml(), body);
}

#[tokio::test]
async fn blank_lines_free_text_comments_crlf_and_a_bom_are_tolerated() {
    let crlf_yaml = STANDALONE.replace('\n', "\r\n");
    let body = format!(
        "\u{feff}\r\n# Generated by a panel at 12:00\r\n\r\n#profile-title: Crlf title\r\n#app-language: en\r\n{crlf_yaml}"
    );
    let snapshot = yaml_only(body, no_headers()).await;
    let info = snapshot.subscription_info().unwrap();
    assert_eq!(info.display_name, "Crlf title");
    assert_eq!(info.app_language, Some(Lang::En));
    assert_eq!(snapshot.mihomo.raw_yaml(), crlf_yaml);
}

#[tokio::test]
async fn only_the_first_64_lines_and_16_kib_are_scanned() {
    let filler = "# filler\n".repeat(63);
    let within = yaml_only(
        format!("{filler}#profile-title: Line 64\n{STANDALONE}"),
        no_headers(),
    )
    .await;
    assert_eq!(title(&within), "Line 64");
    let beyond = yaml_only(
        format!("{filler}# one more\n#profile-title: Line 65\n{STANDALONE}"),
        no_headers(),
    )
    .await;
    assert_eq!(title(&beyond), "static.example");
    // The YAML itself still parses: later comment lines are ordinary YAML comments.
    assert!(beyond.mihomo.raw_yaml().ends_with(STANDALONE));

    let long_comment = format!("# {}\n", "x".repeat(16 * 1024));
    let past_bytes = yaml_only(
        format!("{long_comment}#profile-title: After a long line\n{STANDALONE}"),
        no_headers(),
    )
    .await;
    assert_eq!(title(&past_bytes), "static.example");

    // An over-long value is dropped like an over-long header; later lines still count.
    let long_value = format!("#announce: {}\n", "a".repeat(5 * 1024));
    let skipped = yaml_only(
        format!("{long_value}#profile-title: Still read\n{STANDALONE}"),
        no_headers(),
    )
    .await;
    let info = skipped.subscription_info().unwrap();
    assert_eq!(info.display_name, "Still read");
    assert_eq!(info.announcement_text, None);
}

#[tokio::test]
async fn binary_junk_neither_panics_nor_becomes_metadata() {
    let mut body = b"#profile-title: \xff\xfe\x00junk\n#\x80\x81: x\n".to_vec();
    body.extend_from_slice(b"#app-language: en\n");
    body.extend_from_slice(STANDALONE.as_bytes());
    let snapshot = yaml_only(body, no_headers()).await;
    let info = snapshot.subscription_info().unwrap();
    assert_eq!(info.display_name, "static.example");
    assert_eq!(info.app_language, Some(Lang::En));

    let junk = try_refresh(vec![
        response(404, Vec::new(), []),
        response(200, b"#\xff\xff\xff\n\x00\x01\x02".to_vec(), []),
        response(404, Vec::new(), []),
    ])
    .await;
    assert!(junk.is_err());
}

#[tokio::test]
async fn leading_comments_are_stripped_from_xray_json_and_plain_json_is_unchanged() {
    let snapshot = refresh(vec![
        response(404, Vec::new(), []),
        response(200, MIHOMO, []),
        response(
            200,
            format!("\u{feff}#profile-title: From Xray\r\n# note\n\n{XRAY}"),
            [],
        ),
    ])
    .await;
    assert_eq!(title(&snapshot), "From Xray");
    assert_eq!(snapshot.xray.raw_bytes(), format!("\n{XRAY}").as_bytes());

    let plain = refresh(vec![
        response(404, Vec::new(), []),
        response(200, MIHOMO, []),
        response(200, format!("\u{feff}\n{XRAY}"), []),
    ])
    .await;
    assert_eq!(plain.xray.raw_bytes(), format!("\n{XRAY}").as_bytes());

    // A `#` anywhere past the leading block is still invalid JSON, exactly as before.
    let broken = try_refresh(vec![
        response(404, Vec::new(), []),
        response(200, MIHOMO, []),
        response(200, "{\n#profile-title: x\n}", []),
    ])
    .await;
    assert!(broken.is_err());
}

#[tokio::test]
async fn fallback_priority_is_headers_then_yaml_comments_then_xray_comments() {
    let snapshot = refresh(vec![
        response(404, Vec::new(), []),
        response(
            200,
            format!(
                "#app-language: ru\n\
                 #profile-title: YAML title\n\
                 #support-url: https://yaml.example/help\n\
                 {MIHOMO}"
            ),
            [],
        ),
        response(
            200,
            format!(
                "#app-language: en\n\
                 #profile-title: Xray title\n\
                 #dumb-mode: on\n\
                 #profile-update-interval: 6\n\
                 {XRAY}"
            ),
            [("support-url", "https://header.example/help")],
        ),
    ])
    .await;
    let info = snapshot.subscription_info().unwrap();
    assert_eq!(info.app_language, Some(Lang::Ru));
    assert_eq!(info.display_name, "YAML title");
    // A header on the Xray response beats the YAML comment for the same key.
    assert_eq!(
        snapshot.subscription_support_url(),
        Some("https://header.example/help")
    );
    // Keys only the Xray body sent still fill in.
    assert!(info.dumb_mode);
    assert_eq!(info.refresh_interval_secs, Some(6 * 60 * 60));
}

#[tokio::test]
async fn bundle_priority_is_headers_then_bundle_comments_then_embedded_yaml_comments() {
    let embedded = format!(
        "#app-language: en\n\
         #profile-title: Embedded title\n\
         #dumb-mode: on\n\
         {MIHOMO}"
    );
    let http = FakeHttp::new(vec![response(
        200,
        format!(
            "#profile-title: Bundle title\n\
             #app-language: ru\n\
             #support-url: https://bundle.example/help\n\
             {}",
            bundle_with(&embedded)
        ),
        [("support-url", "https://header.example/help")],
    )]);
    let agents = http.agents.clone();
    let store = AtomicSnapshot::default();
    SubscriptionFetcher::new(http)
        .refresh_deferring_logo(URL, &store)
        .await
        .0
        .unwrap();
    let snapshot = store.current().unwrap();
    assert_eq!(*agents.lock().unwrap(), [UA_NATIVE]);
    let info = snapshot.subscription_info().unwrap();
    assert_eq!(info.display_name, "Bundle title");
    assert_eq!(info.app_language, Some(Lang::Ru));
    assert!(info.dumb_mode);
    assert_eq!(
        snapshot.subscription_support_url(),
        Some("https://header.example/help")
    );
    assert_eq!(snapshot.mihomo.raw_yaml(), MIHOMO);
    assert_eq!(snapshot.xray.raw_bytes(), XRAY.as_bytes());
}

#[tokio::test]
async fn a_bundle_without_comments_is_parsed_exactly_as_before() {
    let snapshot = refresh(vec![response(
        200,
        bundle_with(MIHOMO),
        [("profile-title", "Massive")],
    )])
    .await;
    assert_eq!(title(&snapshot), "Massive");
    assert_eq!(snapshot.mihomo.raw_yaml(), MIHOMO);
    assert_eq!(snapshot.xray.raw_bytes(), XRAY.as_bytes());
}

#[tokio::test]
async fn the_same_static_yaml_served_to_every_request_still_works() {
    let body = format!("#profile-title: Static\n#app-language: en\n{STANDALONE}");
    let snapshot = refresh(vec![
        response(200, body.clone(), []),
        response(200, body.clone(), []),
        response(200, body, []),
    ])
    .await;
    let info = snapshot.subscription_info().unwrap();
    assert_eq!(info.display_name, "Static");
    assert_eq!(info.app_language, Some(Lang::En));
    assert_eq!(snapshot.mihomo.raw_yaml(), STANDALONE);
}
