//! Handler tests through the real routers (tower `oneshot`), with a temporary
//! SQLite file and the user injected as a request extension (bypassing
//! sessions, which belong to the auth module).

use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering};

use axum::Router;
use axum::body::Body;
use axum::extract::Request;
use axum::http::{StatusCode, header};
use axum::middleware::{self, Next};
use axum::response::Response;
use axum_extra::extract::cookie::Key;
use tokio::sync::broadcast;
use tower::ServiceExt as _;

use crate::api::AppState;
use crate::auth::{CurrentUser, Role};
use crate::config::Config;
use crate::db;
use crate::db::models::MonitorType;
use crate::monitor::Scheduler;

static NEXT_DB: AtomicU32 = AtomicU32::new(0);

struct TestApp {
    state: AppState,
    path: PathBuf,
}

impl Drop for TestApp {
    fn drop(&mut self) {
        for suffix in ["", "-wal", "-shm"] {
            let mut p = self.path.clone().into_os_string();
            p.push(suffix);
            let _ = std::fs::remove_file(p);
        }
    }
}

async fn app() -> TestApp {
    let _ = rustls::crypto::aws_lc_rs::default_provider().install_default();
    let path = std::env::temp_dir().join(format!(
        "vv-monitors-test-{}-{}.db",
        std::process::id(),
        NEXT_DB.fetch_add(1, Ordering::Relaxed)
    ));
    let pool = db::build_pool(&path);
    db::run_migrations(&pool, &std::env::temp_dir()).await;
    let (tx, _) = broadcast::channel(16);
    let http = reqwest::Client::new();
    let state = AppState {
        scheduler: Scheduler::new(pool.clone(), tx.clone(), http.clone()),
        db: pool,
        session_key: Key::generate(),
        update_tx: tx,
        http,
        config: Arc::new(Config {
            data_dir: std::env::temp_dir(),
            bind_addr: "127.0.0.1:0".into(),
        }),
    };
    TestApp { state, path }
}

fn router(state: &AppState, role: Role) -> Router {
    let user = CurrentUser {
        id: 1,
        username: "ada".into(),
        display_name: "Ada".into(),
        role,
    };
    Router::new()
        .merge(super::router())
        .merge(crate::api::dashboard::router())
        .merge(crate::api::tags::router())
        .layer(middleware::from_fn(move |mut req: Request, next: Next| {
            let user = user.clone();
            async move {
                req.extensions_mut().insert(user);
                next.run(req).await
            }
        }))
        .with_state(state.clone())
}

async fn send(app: &Router, req: Request<Body>) -> Response {
    app.clone().oneshot(req).await.unwrap()
}

async fn body_text(res: Response) -> String {
    let bytes = axum::body::to_bytes(res.into_body(), 8 * 1024 * 1024).await.unwrap();
    String::from_utf8_lossy(&bytes).into_owned()
}

fn post_form(uri: &str, body: &str) -> Request<Body> {
    Request::post(uri)
        .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
        .body(Body::from(body.to_string()))
        .unwrap()
}

fn htmx_post(uri: &str, body: &str) -> Request<Body> {
    Request::post(uri)
        .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
        .header("hx-request", "true")
        .body(Body::from(body.to_string()))
        .unwrap()
}

fn get(uri: &str) -> Request<Body> {
    Request::get(uri).body(Body::empty()).unwrap()
}

fn location(res: &Response) -> String {
    res.headers()
        .get(header::LOCATION)
        .and_then(|v| v.to_str().ok())
        .unwrap_or_default()
        .to_string()
}

fn created_id(res: &Response) -> i64 {
    location(res).trim_start_matches("/monitors/").parse().unwrap()
}

const HTTP_FORM: &str = "monitor_type=http&name=Payments+API&target_url=https%3A%2F%2F127.0.0.1%3A9%2Fhealth\
&interval_seconds=60&timeout_seconds=10&retries=2&http_method=GET&accepted_status_codes=200-299\
&max_redirects=10&http_body_encoding=json&expiry_days=14&resend_interval=0&escalation_minutes=0\
&flap_detection=true&new_tag_name=prod&new_tag_color=green&new_tag_value=eu";

#[tokio::test]
async fn create_then_view_detail_and_dashboard() {
    let t = app().await;
    let app = router(&t.state, Role::Editor);

    let res = send(&app, get("/monitors/new")).await;
    assert_eq!(res.status(), StatusCode::OK);
    assert!(body_text(res).await.contains("name=\"monitor_type\""));

    let res = send(&app, post_form("/monitors/new", HTTP_FORM)).await;
    assert_eq!(res.status(), StatusCode::SEE_OTHER);
    let id = created_id(&res);

    let m = db::interact(&t.state.db, move |c| db::monitors::get_monitor(c, id))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(m.name, "Payments API");
    assert_eq!(m.target, "https://127.0.0.1:9/health");
    assert_eq!(m.retries, 2);
    assert_eq!(m.accepted_status_codes, "200-299");
    let tags = db::interact(&t.state.db, move |c| db::tags::tags_for_monitor(c, id))
        .await
        .unwrap();
    assert_eq!(
        (tags[0].name.as_str(), tags[0].value.as_str(), tags[0].color.as_str()),
        ("prod", "eu", "green")
    );

    let res = send(&app, get(&format!("/monitors/{id}"))).await;
    assert_eq!(res.status(), StatusCode::OK);
    let html = body_text(res).await;
    assert!(html.contains("Payments API"));
    assert!(html.contains("Response time"));
    assert!(html.contains("Last 90 days"));

    for range in ["1h", "24h", "7d", "30d"] {
        let res = send(&app, get(&format!("/monitors/{id}/chart?range={range}"))).await;
        assert_eq!(res.status(), StatusCode::OK);
    }
    let res = send(&app, get(&format!("/monitors/{id}/heartbeats?before=999999"))).await;
    assert_eq!(res.status(), StatusCode::OK);

    let res = send(&app, get("/")).await;
    assert_eq!(res.status(), StatusCode::OK);
    let html = body_text(res).await;
    assert!(html.contains("Payments API"));
    assert!(html.contains("sse-connect=\"/events\""));

    // htmx filter request → only the results fragment, filter carried into the SSE URL.
    let res = send(
        &app,
        Request::get("/?q=nothing-matches")
            .header("hx-request", "true")
            .header("hx-target", "dash-results")
            .body(Body::empty())
            .unwrap(),
    )
    .await;
    let html = body_text(res).await;
    assert!(!html.contains("<html"));
    assert!(html.contains("No monitors match"));
    assert!(html.contains("/events?q=nothing-matches"));

    let res = send(&app, get(&format!("/monitors/{id}/edit"))).await;
    assert_eq!(res.status(), StatusCode::OK);
    let res = send(&app, get(&format!("/monitors/{id}/clone"))).await;
    assert!(body_text(res).await.contains("Payments API (copy)"));

    let res = send(&app, get("/monitors/form-fields?type=dns")).await;
    assert!(body_text(res).await.contains("Record type"));
}

#[tokio::test]
async fn invalid_form_rerenders_with_errors_and_input() {
    let t = app().await;
    let app = router(&t.state, Role::Admin);
    let res = send(
        &app,
        post_form(
            "/monitors/new",
            "monitor_type=tcp&name=&target_hostport=db.internal&interval_seconds=5",
        ),
    )
    .await;
    assert_eq!(res.status(), StatusCode::UNPROCESSABLE_ENTITY);
    let html = body_text(res).await;
    assert!(html.contains("aria-invalid=\"true\""));
    assert!(html.contains("value=\"db.internal\""), "input is preserved");
    assert!(html.contains("Enter a host and port"));
    let count = db::interact(&t.state.db, |c| db::monitors::list_monitors(c))
        .await
        .unwrap()
        .len();
    assert_eq!(count, 0);
}

#[tokio::test]
async fn push_monitors_get_a_token_and_viewers_cannot_edit() {
    let t = app().await;
    let editor = router(&t.state, Role::Editor);
    let res = send(
        &editor,
        post_form(
            "/monitors/new",
            "monitor_type=push&name=Nightly+backup&interval_seconds=3600&retries=0&resend_interval=0&escalation_minutes=0",
        ),
    )
    .await;
    assert_eq!(res.status(), StatusCode::SEE_OTHER);
    let id = created_id(&res);
    let m = db::interact(&t.state.db, move |c| db::monitors::get_monitor(c, id))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(m.monitor_type, MonitorType::Push);
    let token = m.push_token.clone().unwrap();
    assert_eq!(token.len(), 32);
    let html = body_text(send(&editor, get(&format!("/monitors/{id}"))).await).await;
    assert!(html.contains(&format!("/api/push/{token}")));

    // Regenerating replaces the token.
    let res = send(&editor, htmx_post(&format!("/monitors/{id}/push-token"), "")).await;
    assert_eq!(res.status(), StatusCode::OK);
    let html = body_text(res).await;
    assert!(!html.contains(&token));

    let viewer = router(&t.state, Role::Viewer);
    for req in [
        get("/monitors/new"),
        get(&format!("/monitors/{id}/edit")),
        post_form(&format!("/monitors/{id}/pause"), ""),
        post_form("/monitors/bulk", &format!("action=delete&ids={id}")),
        Request::delete(format!("/monitors/{id}")).body(Body::empty()).unwrap(),
    ] {
        assert_eq!(send(&viewer, req).await.status(), StatusCode::FORBIDDEN);
    }
    let html = body_text(send(&viewer, get(&format!("/monitors/{id}"))).await).await;
    assert!(!html.contains("/api/push/"), "viewers never see the push secret");
    assert!(!html.contains("/edit\""), "no edit controls for viewers");
}

#[tokio::test]
async fn pause_resume_bulk_and_delete() {
    let t = app().await;
    let app = router(&t.state, Role::Editor);
    let mut ids = Vec::new();
    for name in ["a", "b", "c"] {
        let body = format!("monitor_type=group&name={name}&resend_interval=0&escalation_minutes=0");
        let res = send(&app, post_form("/monitors/new", &body)).await;
        assert_eq!(res.status(), StatusCode::SEE_OTHER);
        ids.push(created_id(&res));
    }
    let events = |id: i64| {
        let pool = t.state.db.clone();
        async move {
            db::interact(&pool, move |c| db::events::list_for_monitor(c, id, 10))
                .await
                .unwrap()
                .into_iter()
                .map(|e| (e.kind.as_str(), e.is_open()))
                .collect::<Vec<_>>()
        }
    };

    let res = send(&app, post_form(&format!("/monitors/{}/pause", ids[0]), "")).await;
    assert_eq!(location(&res), format!("/monitors/{}", ids[0]));
    assert_eq!(events(ids[0]).await, vec![("paused", true)]);
    let res = send(&app, post_form(&format!("/monitors/{}/resume", ids[0]), "return_to=%2F")).await;
    assert_eq!(location(&res), "/");
    // Resuming closes the paused interval.
    assert_eq!(events(ids[0]).await, vec![("paused", false)]);

    let body = format!(
        "action=pause&ids={}&ids={}&return_to=%2F%3Fstatus%3Dpaused",
        ids[1], ids[2]
    );
    let res = send(&app, post_form("/monitors/bulk", &body)).await;
    assert_eq!(location(&res), "/?status=paused");
    let active = db::interact(&t.state.db, |c| db::monitors::list_active_monitors(c))
        .await
        .unwrap();
    assert_eq!(active.iter().map(|m| m.id).collect::<Vec<_>>(), vec![ids[0]]);

    // Open redirects are refused.
    let res = send(
        &app,
        post_form("/monitors/bulk", "action=resume&ids=1&return_to=%2F%2Fevil.example"),
    )
    .await;
    assert_eq!(location(&res), "/");

    // "Apply to every monitor shown" uses the dashboard filter.
    let res = send(
        &app,
        post_form("/monitors/bulk", "action=delete&all=true&filter=status%3Dpaused"),
    )
    .await;
    assert_eq!(res.status(), StatusCode::SEE_OTHER);
    let left = db::interact(&t.state.db, |c| db::monitors::list_monitors(c))
        .await
        .unwrap();
    assert_eq!(left.iter().map(|m| m.id).collect::<Vec<_>>(), vec![ids[0]]);

    let res = send(
        &app,
        Request::delete(format!("/monitors/{}", ids[0]))
            .header("hx-request", "true")
            .body(Body::empty())
            .unwrap(),
    )
    .await;
    assert_eq!(res.headers().get("hx-redirect").unwrap(), "/");
    assert_eq!(
        send(&app, get(&format!("/monitors/{}", ids[0]))).await.status(),
        StatusCode::NOT_FOUND
    );
}

#[tokio::test]
async fn tags_page_crud() {
    let t = app().await;
    let app = router(&t.state, Role::Editor);
    let res = send(&app, post_form("/tags", "name=Team+%F0%9F%9A%80&color=pink")).await;
    assert_eq!(res.status(), StatusCode::SEE_OTHER);
    let html = body_text(send(&app, get("/tags")).await).await;
    assert!(html.contains("Team 🚀") && html.contains("tag-pink"));

    let dup = send(&app, htmx_post("/tags", "name=team+%F0%9F%9A%80&color=red")).await;
    assert_eq!(dup.headers().get("hx-retarget").unwrap(), "#new-tag-errors");
    assert!(body_text(dup).await.contains("already exists"));

    let id = db::interact(&t.state.db, |c| db::tags::list_tags(c)).await.unwrap()[0].id;
    let res = send(&app, htmx_post(&format!("/tags/{id}"), "name=Ops&color=%23ff0000")).await;
    assert_eq!(res.headers().get("hx-refresh").unwrap(), "true");
    let tag = db::interact(&t.state.db, move |c| db::tags::get_tag(c, id))
        .await
        .unwrap()
        .unwrap();
    assert_eq!((tag.name.as_str(), tag.color.as_str()), ("Ops", "red"));

    let res = send(
        &app,
        Request::delete(format!("/tags/{id}"))
            .header("hx-request", "true")
            .body(Body::empty())
            .unwrap(),
    )
    .await;
    assert_eq!(res.status(), StatusCode::OK);
    assert!(res.headers().get("hx-trigger").is_some());
    assert!(db::interact(&t.state.db, |c| db::tags::list_tags(c)).await.unwrap().is_empty());

    let viewer = router(&t.state, Role::Viewer);
    assert_eq!(send(&viewer, post_form("/tags", "name=x")).await.status(), StatusCode::FORBIDDEN);
    assert_eq!(send(&viewer, get("/tags")).await.status(), StatusCode::OK);
}

#[test]
fn toast_header_escapes_non_ascii() {
    let v = super::toast_header("Deleted “Café 🚀”", "success").unwrap();
    let s = v.to_str().unwrap();
    assert!(s.is_ascii());
    let parsed: serde_json::Value = serde_json::from_str(s).unwrap();
    assert_eq!(parsed["vv:toast"]["message"], "Deleted “Café 🚀”");
}

#[test]
fn safe_return_only_allows_local_paths() {
    assert_eq!(super::safe_return("/?status=down", "/"), "/?status=down");
    assert_eq!(super::safe_return("//evil.example", "/"), "/");
    assert_eq!(super::safe_return("https://evil.example", "/x"), "/x");
    assert_eq!(super::safe_return("/\\evil", "/"), "/");
    // Browsers strip tabs/newlines, turning these into `//evil.example`.
    assert_eq!(super::safe_return("/\t/evil.example", "/m/1"), "/m/1");
    assert_eq!(super::safe_return("/\n/evil.example", "/m/1"), "/m/1");
    assert_eq!(super::safe_return("/", "/m/1"), "/");
}

#[tokio::test]
async fn form_lists_channels_by_provider_name() {
    let t = app().await;
    let app = router(&t.state, Role::Editor);
    db::interact(&t.state.db, |c| db::notifications::insert_channel(c, "Ops room", "discord", "{}", false))
        .await
        .unwrap();
    let html = body_text(send(&app, get("/monitors/new")).await).await;
    assert!(html.contains("Ops room"));
    assert!(html.contains("<span class=\"list-sub\">Discord"), "provider label, not the raw kind");
}

#[tokio::test]
async fn edit_form_never_shows_the_stored_basic_auth_password() {
    let t = app().await;
    let app = router(&t.state, Role::Editor);
    let form = format!("{HTTP_FORM}&basic_auth_user=bot&basic_auth_pass=hunter2-secret");
    let res = send(&app, post_form("/monitors/new", &form)).await;
    let id = created_id(&res);
    let html = body_text(send(&app, get(&format!("/monitors/{id}/edit"))).await).await;
    assert!(!html.contains("hunter2-secret"));
    assert!(html.contains(crate::notification::SECRET_MASK));
    // Saving the untouched form keeps the password.
    let mask = url::form_urlencoded::byte_serialize(crate::notification::SECRET_MASK.as_bytes()).collect::<String>();
    let same = format!("{HTTP_FORM}&basic_auth_user=bot&basic_auth_pass={mask}");
    let res = send(&app, post_form(&format!("/monitors/{id}/edit"), &same)).await;
    assert_eq!(res.status(), StatusCode::SEE_OTHER);
    let m = db::interact(&t.state.db, move |c| db::monitors::get_monitor(c, id)).await.unwrap().unwrap();
    assert_eq!(m.basic_auth_pass.as_deref(), Some("hunter2-secret"));
    // Pointing it elsewhere without re-entering the password is refused.
    let moved = same.replace("127.0.0.1%3A9", "attacker.example");
    let res = send(&app, post_form(&format!("/monitors/{id}/edit"), &moved)).await;
    assert_eq!(res.status(), StatusCode::UNPROCESSABLE_ENTITY);
}

#[tokio::test]
async fn edit_form_never_shows_the_oauth_secret_or_private_key() {
    use crate::monitor::checkers::http_auth::test_pki::Pki;
    let t = app().await;
    let app = router(&t.state, Role::Editor);
    let enc = |s: &str| url::form_urlencoded::byte_serialize(s.as_bytes()).collect::<String>();
    let mask = enc(crate::notification::SECRET_MASK);
    let pki = Pki::new();
    // Not the BEGIN/END lines: the field's placeholder shows those.
    let key_body = pki.client_key_pem.lines().nth(1).unwrap().to_string();
    let key_body = key_body.as_str();

    // OAuth2: the secret is stored, shown as the mask, and kept when saved untouched.
    let oauth = format!(
        "{HTTP_FORM}&auth_method=oauth2&oauth_token_url=https%3A%2F%2Flogin.example.com%2Ftoken&oauth_client_id=vole&oauth_client_secret=client-secret-value&oauth_client_auth=client_secret_post"
    );
    let res = send(&app, post_form("/monitors/new", &oauth)).await;
    let id = created_id(&res);
    let html = body_text(send(&app, get(&format!("/monitors/{id}/edit"))).await).await;
    assert!(!html.contains("client-secret-value"));
    assert!(html.contains(crate::notification::SECRET_MASK));
    assert!(html.contains(r#"<option value="oauth2" selected>"#), "the saved method is selected");
    assert!(html.contains(r#"<option value="client_secret_post" selected>"#));
    let same = oauth.replace("client-secret-value", &mask);
    let res = send(&app, post_form(&format!("/monitors/{id}/edit"), &same)).await;
    assert_eq!(res.status(), StatusCode::SEE_OTHER);
    let m = db::interact(&t.state.db, move |c| db::monitors::get_monitor(c, id)).await.unwrap().unwrap();
    assert_eq!(m.oauth_client_secret.as_deref(), Some("client-secret-value"));
    // A rejected edit doesn't echo a freshly typed secret back.
    let bad = oauth.replace("client-secret-value", "typed-secret-value").replace("login.example.com", "not a host");
    let res = send(&app, post_form(&format!("/monitors/{id}/edit"), &bad)).await;
    assert_eq!(res.status(), StatusCode::UNPROCESSABLE_ENTITY);
    assert!(!body_text(res).await.contains("typed-secret-value"));

    // mTLS: the private key is never rendered, the certificate is.
    let mtls = format!(
        "{HTTP_FORM}&auth_method=mtls&tls_client_cert={}&tls_client_key={}&tls_ca_cert={}",
        enc(&pki.client_cert_pem.replace('\n', "\r\n")),
        enc(&pki.client_key_pem.replace('\n', "\r\n")),
        enc(&pki.ca_pem),
    );
    let res = send(&app, post_form("/monitors/new", &mtls)).await;
    assert_eq!(res.status(), StatusCode::SEE_OTHER, "{}", body_text(res).await);
    let id = {
        let all = db::interact(&t.state.db, |c| db::monitors::list_monitors(c)).await.unwrap();
        all.iter().find(|m| m.auth_method == "mtls").unwrap().id
    };
    let html = body_text(send(&app, get(&format!("/monitors/{id}/edit"))).await).await;
    assert!(!html.contains(key_body), "the private key must not be in the page");
    assert!(html.contains("BEGIN CERTIFICATE"));
    assert!(html.contains(crate::notification::SECRET_MASK));
    let same = mtls.replace(&enc(&pki.client_key_pem.replace('\n', "\r\n")), &mask);
    let res = send(&app, post_form(&format!("/monitors/{id}/edit"), &same)).await;
    assert_eq!(res.status(), StatusCode::SEE_OTHER);
    let m = db::interact(&t.state.db, move |c| db::monitors::get_monitor(c, id)).await.unwrap().unwrap();
    assert_eq!(m.tls_client_key.as_deref(), Some(pki.client_key_pem.trim()));
    // A pair that doesn't match is explained on the key field.
    let mismatch = mtls.replace(&enc(&pki.client_key_pem.replace('\n', "\r\n")), &enc(&crate::monitor::checkers::http_auth::test_pki::unrelated_key_pem()));
    let res = send(&app, post_form("/monitors/new", &mismatch)).await;
    assert_eq!(res.status(), StatusCode::UNPROCESSABLE_ENTITY);
    let html = body_text(res).await;
    assert!(html.contains("can’t be used together") && !html.contains(key_body));
}

const COMMON: &str = "&interval_seconds=60&timeout_seconds=10&retries=0&http_method=GET&accepted_status_codes=200-299\
&max_redirects=10&http_body_encoding=json&expiry_days=14&resend_interval=0&escalation_minutes=0&flap_detection=true";

/// A create form for each new service type, as the browser posts it.
fn service_form(kind: &str, extra: &str) -> String {
    let target = match kind {
        "tls" => "target_tls=example.com",
        "smtp" => "target_smtp=mail.example.com%3A587",
        "redis" => "target_redis=redis%3A%2F%2Fcache.local%2F2",
        "postgres" => "target_postgres=postgres%3A%2F%2Fdb.local%2Fapp",
        "mysql" => "target_mysql=mysql%3A%2F%2Fdb.local",
        "mqtt" => "target_mqtt=mqtt%3A%2F%2Fbroker.local%2Fhome%2Fstatus",
        "kafka" => "target_kafka=kafka%3A%2F%2Fbroker.local%2Forders",
        "rabbitmq" => "target_rabbitmq=amqp%3A%2F%2Fmq.local%2F%252Fprod",
        "grpc" => "target_grpc=grpc%3A%2F%2Fapi.local%3A50051",
        "radius" => "target_radius=radius%3A%2F%2Fradius.local",
        "snmp" => "target_snmp=snmp%3A%2F%2Frouter.local%2F1.3.6.1.2.1.1.1.0",
        other => panic!("{other}"),
    };
    format!("monitor_type={kind}&name=Svc+{kind}&{target}{COMMON}{extra}")
}

#[tokio::test]
async fn every_new_service_type_can_be_created_and_edited() {
    let t = app().await;
    let app = router(&t.state, Role::Editor);
    for (kind, extra) in [
        ("tls", ""),
        ("smtp", ""),
        ("redis", "&basic_auth_pass=pw"),
        ("postgres", "&basic_auth_user=vole&basic_auth_pass=pw"),
        ("mysql", ""),
        ("mqtt", "&keyword=on"),
        ("kafka", ""),
        ("rabbitmq", "&basic_auth_user=guest&basic_auth_pass=guest"),
        ("grpc", ""),
        ("radius", "&basic_auth_user=vole&basic_auth_pass=pw&secret=testing123"),
        ("snmp", "&secret=public"),
    ] {
        let res = send(&app, post_form("/monitors/new", &service_form(kind, extra))).await;
        assert_eq!(res.status(), StatusCode::SEE_OTHER, "{kind}: {}", body_text(res).await);
        let id = created_id(&res);
        let m = db::interact(&t.state.db, move |c| db::monitors::get_monitor(c, id)).await.unwrap().unwrap();
        assert_eq!(m.monitor_type.as_str(), kind);
        let html = body_text(send(&app, get(&format!("/monitors/{id}/edit"))).await).await;
        assert!(html.contains(&format!("name=\"target_{kind}\"")), "{kind}: the edit form shows its own target field");
        // The detail page renders too (icons, labels, type name).
        let res = send(&app, get(&format!("/monitors/{id}"))).await;
        assert_eq!(res.status(), StatusCode::OK, "{kind}");
    }
}

#[tokio::test]
async fn stored_service_secrets_are_never_shown_and_follow_the_destination() {
    let t = app().await;
    let app = router(&t.state, Role::Editor);
    let enc = |s: &str| url::form_urlencoded::byte_serialize(s.as_bytes()).collect::<String>();
    let mask = enc(crate::notification::SECRET_MASK);
    let form = service_form("radius", "&basic_auth_user=vole&basic_auth_pass=user-pass-1&secret=shared-secret-1");
    let id = created_id(&send(&app, post_form("/monitors/new", &form)).await);
    let html = body_text(send(&app, get(&format!("/monitors/{id}/edit"))).await).await;
    assert!(!html.contains("shared-secret-1") && !html.contains("user-pass-1"), "secrets stay out of the page");
    assert!(html.contains(crate::notification::SECRET_MASK));

    // The untouched form keeps both.
    let same = service_form("radius", &format!("&basic_auth_user=vole&basic_auth_pass={mask}&secret={mask}"));
    let res = send(&app, post_form(&format!("/monitors/{id}/edit"), &same)).await;
    assert_eq!(res.status(), StatusCode::SEE_OTHER);
    let m = db::interact(&t.state.db, move |c| db::monitors::get_monitor(c, id)).await.unwrap().unwrap();
    assert_eq!((m.secret.as_deref(), m.basic_auth_pass.as_deref()), (Some("shared-secret-1"), Some("user-pass-1")));

    // Pointing it at another server without re-entering them is refused.
    let moved = same.replace("radius.local", "attacker.example");
    let res = send(&app, post_form(&format!("/monitors/{id}/edit"), &moved)).await;
    assert_eq!(res.status(), StatusCode::UNPROCESSABLE_ENTITY);
    let html = body_text(res).await;
    assert!(!html.contains("shared-secret-1"));
}

#[tokio::test]
async fn service_forms_explain_what_is_missing() {
    let t = app().await;
    let app = router(&t.state, Role::Editor);
    let cases = [
        ("postgres", "", "Enter the user name."),
        ("radius", "&basic_auth_user=vole&basic_auth_pass=pw", "Enter the shared secret."),
        ("mqtt", "&keyword=on", "Add a topic to the address"),
    ];
    for (kind, extra, want) in cases {
        let target = match kind {
            "mqtt" => "&target_mqtt=mqtt%3A%2F%2Fbroker.local",
            _ => "",
        };
        let form = service_form(kind, extra).replace(
            &format!("&target_{kind}="),
            &if target.is_empty() { format!("&target_{kind}=") } else { format!("{target}&ignored=") },
        );
        let res = send(&app, post_form("/monitors/new", &form)).await;
        assert_eq!(res.status(), StatusCode::UNPROCESSABLE_ENTITY, "{kind}");
        assert!(body_text(res).await.contains(want), "{kind}");
    }
    let res = send(&app, post_form("/monitors/new", &service_form("redis", "").replace("redis%3A%2F%2F", "http%3A%2F%2F"))).await;
    assert_eq!(res.status(), StatusCode::UNPROCESSABLE_ENTITY);
    assert!(body_text(res).await.contains("Unsupported scheme"));
}

#[tokio::test]
async fn type_picker_and_fields_partial_know_the_new_types() {
    let t = app().await;
    let app = router(&t.state, Role::Editor);
    let html = body_text(send(&app, get("/monitors/new")).await).await;
    for label in ["TLS handshake", "SMTP", "Redis", "PostgreSQL", "MySQL / MariaDB", "MQTT", "Kafka", "RabbitMQ", "gRPC", "RADIUS", "SNMP"] {
        assert!(html.contains(label), "the type picker lists {label}");
    }
    for (kind, want) in [("redis", "User name"), ("radius", "Shared secret"), ("snmp", "Community"), ("mqtt", "Message must contain")] {
        let html = body_text(send(&app, get(&format!("/monitors/form-fields?type={kind}"))).await).await;
        assert!(html.contains(want), "{kind} shows {want}");
    }
}

#[tokio::test]
async fn the_monitor_form_never_repeats_an_element_id() {
    let t = app().await;
    let app = router(&t.state, Role::Editor);
    for uri in ["/monitors/new", "/monitors/new?type=redis", "/monitors/form-fields?type=snmp"] {
        let html = body_text(send(&app, get(uri)).await).await;
        let mut seen = std::collections::HashSet::new();
        for (at, _) in html.match_indices(" id=\"") {
            let id: String = html[at + 5..].chars().take_while(|c| *c != '"').collect();
            assert!(seen.insert(id.clone()), "{uri}: duplicate id \"{id}\"");
        }
    }
}
