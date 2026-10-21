//! Hard-coded product strings that a shared-crate refactor will turn into
//! configuration: user agents, file names, realms, default names, and what
//! actually goes over the wire for an outgoing webhook.

use std::sync::{Arc, Mutex};

use axum::Router;
use axum::body::Bytes;
use axum::extract::State;
use axum::http::{HeaderMap, Method as HttpMethod, Uri};
use insta::assert_snapshot;
use serde_json::json;
use vigilant_vole::db::models::{EventKind, NotificationEvent};
use vigilant_vole::db::settings::Settings;
use vigilant_vole::i18n::{self, Lang};

use crate::harness::{App, SITE_NAME};
use crate::norm;

#[tokio::test]
async fn product_strings_and_file_names() {
    let mut out = String::new();
    let mut line = |k: &str, v: String| out.push_str(&format!("{k}: {}\n", norm::text(&v)));
    line(
        "server user agent",
        vigilant_vole::server::USER_AGENT.into(),
    );
    line(
        "notification product name",
        vigilant_vole::notification::message::PRODUCT.into(),
    );
    line("default site name", Settings::default().site_name);
    line(
        "agent docker image",
        vigilant_vole::api::agents::DOCKER_IMAGE.into(),
    );
    line(
        "MCP protocol versions",
        format!("{:?}", vigilant_vole::mcp::PROTOCOL_VERSIONS),
    );
    let totp = vigilant_vole::auth::totp::build("K5YMSTXGCAMWB5SHRO24JENVW2TLI4VP", "admin", "")
        .unwrap_or_else(|_| panic!("totp"));
    line(
        "TOTP URL (no site name)",
        vigilant_vole::auth::totp::otpauth_url(&totp).unwrap_or_else(|_| panic!("url")),
    );
    let totp =
        vigilant_vole::auth::totp::build("K5YMSTXGCAMWB5SHRO24JENVW2TLI4VP", "admin", SITE_NAME)
            .unwrap_or_else(|_| panic!("totp"));
    line(
        "TOTP URL (site name)",
        vigilant_vole::auth::totp::otpauth_url(&totp).unwrap_or_else(|_| panic!("url")),
    );

    // Files a fresh instance creates in its data directory.
    let mut app = App::seeded("product").await;
    let config = app.dir.config();
    line(
        "database file",
        config
            .db_path()
            .file_name()
            .unwrap()
            .to_string_lossy()
            .into_owned(),
    );
    let mut files: Vec<String> = std::fs::read_dir(app.dir.path())
        .unwrap()
        .filter_map(Result::ok)
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .filter(|n| !n.ends_with("-wal") && !n.ends_with("-shm"))
        .collect();
    files.sort();
    line("data directory after startup", format!("{files:?}"));

    // Download names (dates and times of day normalised).
    let stamp = regex::Regex::new(r"\d{8}(-\d{4})?").unwrap();
    let res = app.get("/settings/backup/database").await;
    line(
        "database download",
        format!(
            "{} {} | {}",
            res.status.as_u16(),
            res.header("content-type"),
            stamp.replace(&res.header("content-disposition"), "<stamp>")
        ),
    );
    let res = app
        .api(HttpMethod::GET, "/api/v1/config?download=1", None)
        .await;
    line(
        "config download",
        format!(
            "{} {} | {}",
            res.status.as_u16(),
            res.header("content-type"),
            stamp.replace(&res.header("content-disposition"), "<stamp>")
        ),
    );

    // Authentication challenges (metrics are off by default).
    app.state
        .with_conn(|c| {
            vigilant_vole::db::settings::set_bool(
                c,
                vigilant_vole::db::settings::METRICS_ENABLED,
                true,
            )
        })
        .await
        .unwrap_or_else(|_| panic!("enable metrics"));
    app.clear_cookies();
    let res = app.get("/metrics").await;
    line(
        "metrics challenge",
        format!(
            "{} www-authenticate={:?}",
            res.status.as_u16(),
            res.header("www-authenticate")
        ),
    );
    let req = axum::http::Request::post("/mcp")
        .header("content-type", "application/json")
        .body(axum::body::Body::from("{}"))
        .unwrap();
    let res = app.send(req).await;
    line(
        "MCP challenge",
        format!(
            "{} www-authenticate={:?} body={}",
            res.status.as_u16(),
            res.header("www-authenticate"),
            res.text()
        ),
    );
    assert_snapshot!("product_strings", out);
}

#[derive(Clone, Default)]
struct Captured(Arc<Mutex<Vec<String>>>);

async fn record(
    State(cap): State<Captured>,
    method: HttpMethod,
    uri: Uri,
    headers: HeaderMap,
    body: Bytes,
) -> &'static str {
    let mut out = format!("{method} {uri}\n");
    let mut names: Vec<&str> = headers.keys().map(|k| k.as_str()).collect();
    names.sort_unstable();
    for name in names {
        for v in headers.get_all(name) {
            let v = v.to_str().unwrap_or("<binary>");
            let v = if name == "host" {
                "127.0.0.1:<port>"
            } else if name == "content-length" {
                "<n>"
            } else {
                v
            };
            out.push_str(&format!("{name}: {v}\n"));
        }
    }
    out.push_str(&format!("\n{}\n", String::from_utf8_lossy(&body)));
    cap.0.lock().unwrap().push(out);
    "ok"
}

/// A webhook notification as the receiving server sees it (user agent,
/// content type, body), sent through the app's shared HTTP client.
#[tokio::test]
async fn webhook_on_the_wire() {
    let app = App::fresh("product-wire").await;
    let cap = Captured::default();
    let router = Router::new().fallback(record).with_state(cap.clone());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let server = tokio::spawn(async move { axum::serve(listener, router).await });

    let event = NotificationEvent {
        kind: EventKind::Down,
        monitor_id: Some(7),
        monitor_name: "Payments API".into(),
        monitor_type: "http".into(),
        target: "https://user:secret@pay.example/health?token=abc".into(),
        message: Some("HTTP 503".into()),
        downtime: None,
        expires_in_days: None,
        url: Some("https://vv.example/monitors/7".into()),
        occurred_at: time::OffsetDateTime::from_unix_timestamp(1_792_418_592).unwrap(),
    };
    let config = json!({"url": format!("http://{addr}/hook?k=1")});
    i18n::scope(Lang::EN, async {
        vigilant_vole::notification::send_config(&app.state.http, "webhook", &config, &event)
            .await
            .expect("webhook delivered");
    })
    .await;
    server.abort();
    let got = cap.0.lock().unwrap().clone();
    assert_eq!(got.len(), 1);
    assert_snapshot!("product_webhook_wire", norm::text(&got[0]));
}
