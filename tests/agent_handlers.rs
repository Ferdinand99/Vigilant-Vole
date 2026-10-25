//! The agent settings handlers through the real app (router, sessions, CSRF
//! and the engine): disabling and deleting an agent that checks monitors
//! for the server (`check_from_agent_id`).

#[path = "common/mod.rs"]
mod common;
#[allow(dead_code)] // shared with the golden tests; this binary uses a subset
#[path = "golden/harness.rs"]
mod harness;

use std::time::Duration;

use axum::body::Body;
use axum::http::{Method, Request, StatusCode, header};
use serde_json::{Value, json};
use vigilant_vole::auth;
use vigilant_vole::db::{self, agents, models::MonitorType, models::NewMonitor, monitors};

use common::DataDir;
use harness::{ADMIN, ADMIN_PASSWORD, App, ORIGIN, SITE_NAME};

/// A fresh instance with its engine running, the admin signed in.
async fn app(label: &str) -> App {
    let mut app = App::start_with_engine(DataDir::new(&format!("agents-{label}"))).await;
    let res = app
        .post_form(
            "/setup",
            &[
                ("username", ADMIN),
                ("password", ADMIN_PASSWORD),
                ("confirm", ADMIN_PASSWORD),
                ("site_name", SITE_NAME),
                ("timezone", "UTC"),
            ],
        )
        .await;
    assert_eq!(res.status, StatusCode::SEE_OTHER, "setup: {}", res.text());
    app
}

/// An agent named `home` (returns its id and token) and `n` monitors it checks.
async fn home_agent(app: &App, n: usize) -> (i64, String, Vec<i64>) {
    app.state
        .with_conn(move |c| {
            let token = agents::generate_token();
            let id = agents::insert_agent(c, "home", "", &token)?;
            let mut ids = Vec::new();
            for i in 0..n {
                ids.push(monitors::insert_monitor(
                    c,
                    &NewMonitor {
                        name: format!("nas-{i}"),
                        monitor_type: MonitorType::Tcp,
                        target: format!("10.0.3.{}:445", i + 7),
                        check_from_agent_id: Some(id),
                        ..NewMonitor::default()
                    },
                )?);
            }
            Ok((id, token.token, ids))
        })
        .await
        .unwrap_or_else(|_| panic!("seed agent"))
}

/// Signs in as a new user with `role` (the admin's session is dropped).
async fn sign_in_as(app: &mut App, name: &str, role: auth::Role) {
    let hash = auth::hash_password(ADMIN_PASSWORD);
    let name_owned = name.to_string();
    app.state
        .with_conn(move |c| db::users::insert_user(c, &name_owned, &hash, role))
        .await
        .unwrap_or_else(|_| panic!("seed user"));
    app.clear_cookies();
    let res = app
        .post_form(
            "/login",
            &[("username", name), ("password", ADMIN_PASSWORD)],
        )
        .await;
    assert_eq!(
        res.status,
        StatusCode::SEE_OTHER,
        "login {name}: {}",
        res.text()
    );
}

/// `POST /settings/agents/{id}/active` as the agents page sends it.
async fn set_active(app: &mut App, id: i64, active: bool) -> harness::Res {
    let form: &[(&str, &str)] = if active { &[("active", "true")] } else { &[] };
    app.post_form(&format!("/settings/agents/{id}/active"), form)
        .await
}

/// `DELETE /settings/agents/{id}` as the agents page's htmx button sends it.
async fn delete(app: &mut App, id: i64) -> harness::Res {
    let req = Request::builder()
        .method(Method::DELETE)
        .uri(format!("/settings/agents/{id}"))
        .header(header::ORIGIN, ORIGIN)
        .header("hx-request", "true")
        .body(Body::empty())
        .unwrap();
    app.send(req).await
}

async fn latest(app: &App, id: i64) -> Option<(String, String)> {
    app.state
        .with_conn(move |c| db::heartbeats::latest_local(c, id))
        .await
        .unwrap_or_else(|_| panic!("latest heartbeat"))
        .map(|h| (h.status.as_str().to_string(), h.message.unwrap_or_default()))
}

/// Waits until the monitor's latest own heartbeat is `status` with `message`.
async fn wait_for(app: &App, id: i64, status: &str, message: &str) {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    loop {
        let got = latest(app, id).await;
        if got
            .as_ref()
            .is_some_and(|(s, m)| s == status && m == message)
        {
            return;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "monitor {id}: want {status} “{message}”, got {got:?}"
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

async fn monitor(app: &App, id: i64) -> db::models::Monitor {
    app.state
        .with_conn(move |c| monitors::get_monitor(c, id))
        .await
        .unwrap_or_else(|_| panic!("monitor"))
        .unwrap()
}

#[tokio::test]
async fn disabling_an_agent_takes_its_monitors_down_and_enabling_lets_them_recover() {
    let mut app = app("toggle").await;
    let (agent, token, ids) = home_agent(&app, 1).await;
    let id = ids[0];
    app.state.scheduler.reload(id).await;

    let res = set_active(&mut app, agent, false).await;
    assert_eq!(res.status, StatusCode::SEE_OTHER, "{}", res.text());
    assert_eq!(res.header("location"), "/settings/agents");
    wait_for(&app, id, "down", "Agent home is disabled").await;

    // A disabled agent's results are refused.
    let result = |status: &str| {
        Request::post("/api/agent/v1/results")
            .header(header::AUTHORIZATION, format!("Bearer {token}"))
            .header(header::CONTENT_TYPE, "application/json")
            .body(Body::from(
                json!([{"monitor_id": id, "status": status, "message": "445 open"}]).to_string(),
            ))
            .unwrap()
    };
    assert_eq!(app.send(result("up")).await.status, StatusCode::FORBIDDEN);

    let res = set_active(&mut app, agent, true).await;
    assert_eq!(res.status, StatusCode::SEE_OTHER, "{}", res.text());
    let res = app.send(result("up")).await;
    assert_eq!(res.status, StatusCode::OK, "{}", res.text());
    assert_eq!(res.json()["accepted"], 1);
    wait_for(&app, id, "up", "445 open").await;
    app.state.scheduler.shutdown().await;
}

#[tokio::test]
async fn deleting_an_agent_pauses_its_monitors_and_says_how_many() {
    let mut app = app("delete").await;
    let (agent, _, ids) = home_agent(&app, 2).await;
    for id in &ids {
        app.state.scheduler.reload(*id).await;
        assert!(app.state.scheduler.is_running(*id));
    }
    // The agents page warns before deleting.
    let page = app.get("/settings/agents").await.text();
    assert!(
        page.contains("The 2 monitors it checks are paused."),
        "delete confirmation doesn't warn"
    );
    assert!(page.contains("(2 checked only by this agent)"), "{page}");

    let res = delete(&mut app, agent).await;
    assert_eq!(res.status, StatusCode::OK, "{}", res.text());
    let toast: Value = serde_json::from_str(&res.header("hx-trigger")).unwrap();
    assert_eq!(
        toast["vv:toast"]["message"],
        "Agent deleted. 2 monitors it checked are paused."
    );
    for id in &ids {
        let m = monitor(&app, *id).await;
        assert!(!m.active && m.check_from_agent_id.is_none(), "{m:?}");
        assert!(!app.state.scheduler.is_running(*id));
    }
    assert_eq!(delete(&mut app, agent).await.status, StatusCode::NOT_FOUND);
    app.state.scheduler.shutdown().await;
}

#[tokio::test]
async fn only_admins_can_disable_or_delete_an_agent() {
    let mut app = app("roles").await;
    let (agent, _, ids) = home_agent(&app, 1).await;
    for (name, role) in [("vic", auth::Role::Viewer), ("eve", auth::Role::Editor)] {
        sign_in_as(&mut app, name, role).await;
        assert_eq!(
            set_active(&mut app, agent, false).await.status,
            StatusCode::FORBIDDEN,
            "{name} disables"
        );
        assert_eq!(
            delete(&mut app, agent).await.status,
            StatusCode::FORBIDDEN,
            "{name} deletes"
        );
    }
    let still = app
        .state
        .with_conn(move |c| agents::get_agent(c, agent))
        .await
        .unwrap_or_else(|_| panic!("agent"));
    assert!(still.is_some_and(|a| a.active));
    let m = monitor(&app, ids[0]).await;
    assert!(m.active && m.check_from_agent_id == Some(agent));
    app.state.scheduler.shutdown().await;
}
