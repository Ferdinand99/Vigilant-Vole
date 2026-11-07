//! `GET /api/v1/summary`: counts, uptime and what is down, in one call.

use super::*;
use crate::db::models::CheckOutcome;
use crate::db::rollups;

async fn push_monitor(h: &Harness, name: &str) -> i64 {
    let (status, m) = h
        .json(
            Method::POST,
            "/api/v1/monitors",
            &h.editor,
            Some(json!({"name": name, "monitor_type": "push"})),
        )
        .await;
    assert_eq!(status, StatusCode::CREATED, "{m}");
    m["id"].as_i64().unwrap()
}

async fn beat(h: &Harness, id: i64, outcome: CheckOutcome) {
    h.state.scheduler.ingest_external(id, outcome, None).await;
}

async fn summary(h: &Harness, token: &str, query: &str) -> Value {
    let (status, v) = h
        .json(Method::GET, &format!("/api/v1/summary{query}"), token, None)
        .await;
    assert_eq!(status, StatusCode::OK, "{v}");
    v
}

async fn start_maintenance(h: &Harness, id: i64) {
    let window =
        json!({"title": "Update", "strategy": "manual", "active": true, "monitor_ids": [id]});
    let (status, w) = h
        .json(Method::POST, "/api/v1/maintenance", &h.editor, Some(window))
        .await;
    assert_eq!(status, StatusCode::CREATED, "{w}");
}

#[tokio::test]
async fn summary_has_the_documented_shape() {
    let h = harness().await;
    let up = push_monitor(&h, "Fine").await;
    let down = push_monitor(&h, "Broken").await;
    beat(&h, up, CheckOutcome::up(10)).await;
    beat(&h, down, CheckOutcome::up(10)).await;
    beat(&h, down, CheckOutcome::down("connection refused")).await;

    let v = summary(&h, &h.admin_read, "").await;
    assert_eq!(v["version"], env!("CARGO_PKG_VERSION"));
    assert_eq!(
        v["monitors"],
        json!({"total": 2, "up": 1, "down": 1, "pending": 0, "maintenance": 0, "paused": 0, "unknown": 0}),
        "{v}"
    );
    assert_eq!(v["uptime"]["days"], 30);
    assert_eq!(v["uptime"]["monitors_counted"], 2);
    assert!(v["uptime"]["pct"].is_number() && v["uptime"]["pct_24h"].is_number());
    let list = v["down"].as_array().unwrap();
    assert_eq!(list.len(), 1, "{v}");
    assert_eq!(list[0]["id"], down);
    assert_eq!(list[0]["name"], "Broken");
    assert_eq!(list[0]["message"], "connection refused");
    let since = list[0]["since"].as_str().unwrap();
    assert!(since.len() == 20 && since.ends_with('Z'), "{since}");
    // The same moment the monitor list gives as the start of the real status.
    let (_, m) = h
        .json(
            Method::GET,
            &format!("/api/v1/monitors/{down}"),
            &h.viewer,
            None,
        )
        .await;
    assert_eq!(m["raw_status_since"], list[0]["since"]);
    assert_eq!(m["raw_message"], list[0]["message"]);
    // Nothing else is in the document.
    let mut keys: Vec<_> = v.as_object().unwrap().keys().cloned().collect();
    keys.sort();
    assert_eq!(keys, ["down", "monitors", "uptime", "version"]);
}

#[tokio::test]
async fn summary_of_an_empty_instance_has_null_uptime() {
    let h = harness().await;
    let v = summary(&h, &h.viewer, "").await;
    assert_eq!(v["monitors"]["total"], 0);
    assert!(v["uptime"]["pct"].is_null() && v["uptime"]["pct_24h"].is_null());
    assert_eq!(v["uptime"]["monitors_counted"], 0);
    assert_eq!(v["down"], json!([]));
}

#[tokio::test]
async fn viewers_see_the_monitors_the_list_shows_and_no_more() {
    let h = harness().await;
    let a = push_monitor(&h, "Alpha").await;
    let b = push_monitor(&h, "Bravo").await;
    beat(&h, a, CheckOutcome::up(5)).await;
    beat(&h, b, CheckOutcome::down("boom")).await;

    let (_, list) = h
        .json(Method::GET, "/api/v1/monitors", &h.viewer, None)
        .await;
    let listed = list["data"].as_array().unwrap().len();
    for token in [&h.viewer, &h.admin_read, &h.editor, &h.admin] {
        let v = summary(&h, token, "").await;
        assert_eq!(v["monitors"]["total"], listed, "{v}");
        let ids: Vec<_> = v["down"]
            .as_array()
            .unwrap()
            .iter()
            .map(|d| d["id"].as_i64().unwrap())
            .collect();
        for id in ids {
            let (status, _) = h
                .json(
                    Method::GET,
                    &format!("/api/v1/monitors/{id}"),
                    &h.viewer,
                    None,
                )
                .await;
            assert_eq!(status, StatusCode::OK, "viewer cannot open {id}");
            assert!(
                list["data"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .any(|m| m["id"] == id)
            );
        }
    }
    // A read token of a viewer gets the same document as a write token.
    assert_eq!(
        summary(&h, &h.viewer, "").await,
        summary(&h, &h.admin_read, "").await
    );
}

#[tokio::test]
async fn paused_and_maintenance_monitors_are_not_down() {
    let h = harness().await;
    let up = push_monitor(&h, "Up").await;
    let paused = push_monitor(&h, "Paused").await;
    let maint = push_monitor(&h, "Maint").await;
    for id in [up, paused, maint] {
        beat(&h, id, CheckOutcome::up(5)).await;
    }
    // The paused monitor's last result is a failure; the one in maintenance
    // keeps failing during its window.
    beat(&h, paused, CheckOutcome::down("old failure")).await;
    let (status, _) = h
        .json(
            Method::POST,
            &format!("/api/v1/monitors/{paused}/pause"),
            &h.editor,
            None,
        )
        .await;
    assert_eq!(status, StatusCode::OK);
    start_maintenance(&h, maint).await;
    beat(&h, maint, CheckOutcome::down("connection refused")).await;

    let v = summary(&h, &h.viewer, "").await;
    assert_eq!(
        v["monitors"],
        json!({"total": 3, "up": 1, "down": 0, "pending": 0, "maintenance": 1, "paused": 1, "unknown": 0}),
        "{v}"
    );
    assert_eq!(v["down"], json!([]), "{v}");
    // The paused one is left out of the uptime figures; the one in
    // maintenance counts, with its up checks only (maintenance and its
    // failing checks are recorded as maintenance, which uptime ignores).
    assert_eq!(v["uptime"]["monitors_counted"], 2, "{v}");
    assert_eq!(v["uptime"]["pct"], 100.0, "{v}");
}

#[tokio::test]
async fn days_defaults_to_30_and_accepts_1_to_365() {
    let h = harness().await;
    assert_eq!(summary(&h, &h.viewer, "").await["uptime"]["days"], 30);
    for days in [1, 7, 90, 365] {
        let v = summary(&h, &h.viewer, &format!("?days={days}")).await;
        assert_eq!(v["uptime"]["days"], days);
    }
    // Out of range: a field error, as on /monitors/{id}/uptime.
    for bad in ["0", "366", "-1", "1000"] {
        let (status, body) = h
            .json(
                Method::GET,
                &format!("/api/v1/summary?days={bad}"),
                &h.viewer,
                None,
            )
            .await;
        assert_eq!(
            status,
            StatusCode::UNPROCESSABLE_ENTITY,
            "days={bad}: {body}"
        );
        assert_eq!(
            body["error"]["code"], "validation_failed",
            "days={bad}: {body}"
        );
        assert!(body["error"]["fields"]["days"].is_string(), "{body}");
    }
    // Not a whole number: the query extractor's 400.
    for bad in ["abc", "1.5", ""] {
        let (status, body) = h
            .json(
                Method::GET,
                &format!("/api/v1/summary?days={bad}"),
                &h.viewer,
                None,
            )
            .await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "days={bad}: {body}");
        assert_eq!(body["error"]["code"], "bad_request", "days={bad}: {body}");
    }
}

#[tokio::test]
async fn groups_are_not_counted_only_their_children() {
    use crate::db::heartbeats as db_heartbeats;
    use crate::db::models::HeartbeatStatus;
    let h = harness().await;
    let (status, g) = h
        .json(
            Method::POST,
            "/api/v1/monitors",
            &h.editor,
            Some(json!({"name": "Prod", "monitor_type": "group"})),
        )
        .await;
    assert_eq!(status, StatusCode::CREATED, "{g}");
    let group = g["id"].as_i64().unwrap();
    let (status, c) = h
        .json(
            Method::POST,
            "/api/v1/monitors",
            &h.editor,
            Some(json!({"name": "Web", "monitor_type": "push", "parent_id": group})),
        )
        .await;
    assert_eq!(status, StatusCode::CREATED, "{c}");
    let child = c["id"].as_i64().unwrap();
    let ok = push_monitor(&h, "Other").await;
    beat(&h, ok, CheckOutcome::up(5)).await;
    beat(&h, child, CheckOutcome::down("connection refused")).await;
    // The group records the worst of its children, as the scheduler does.
    h.state
        .with_conn(move |c| {
            db_heartbeats::insert(c, group, None, HeartbeatStatus::Down, None, Some("down"))
        })
        .await
        .unwrap();

    let v = summary(&h, &h.viewer, "").await;
    assert_eq!(
        v["monitors"],
        json!({"total": 2, "up": 1, "down": 1, "pending": 0, "maintenance": 0, "paused": 0, "unknown": 0}),
        "{v}"
    );
    let down = v["down"].as_array().unwrap();
    assert_eq!(down.len(), 1, "{v}");
    assert_eq!(down[0]["id"], child);
    // The group's own uptime is not in the mean.
    assert_eq!(v["uptime"]["monitors_counted"], 2, "{v}");
}

#[tokio::test]
async fn a_down_monitor_shows_since_and_the_longest_outage_comes_first() {
    let h = harness().await;
    let first = push_monitor(&h, "First").await;
    let second = push_monitor(&h, "Second").await;
    beat(&h, second, CheckOutcome::down("later")).await;
    beat(&h, first, CheckOutcome::down("earlier")).await;
    // Same-second checks tie: age the first monitor's outage in the database.
    h.state
        .with_conn(move |c| {
            c.execute(
                "UPDATE heartbeats SET checked_at = datetime(checked_at, '-2 hours') WHERE monitor_id = ?1",
                [first],
            )
        })
        .await
        .unwrap();
    let v = summary(&h, &h.viewer, "").await;
    let down = v["down"].as_array().unwrap();
    assert_eq!(
        down.iter()
            .map(|d| d["name"].as_str().unwrap())
            .collect::<Vec<_>>(),
        ["First", "Second"],
        "{v}"
    );
    assert!(down[0]["since"].as_str().unwrap() < down[1]["since"].as_str().unwrap());

    // Recovering takes it off the list.
    beat(&h, first, CheckOutcome::up(5)).await;
    let v = summary(&h, &h.viewer, "").await;
    assert_eq!(v["monitors"]["down"], 1);
    assert_eq!(v["down"].as_array().unwrap().len(), 1);
    assert_eq!(v["down"][0]["name"], "Second");
}

#[tokio::test]
async fn down_is_capped_at_25_and_the_message_is_cut() {
    let h = harness().await;
    let long = "x".repeat(2_000);
    for i in 0..27 {
        let id = push_monitor(&h, &format!("M{i:02}")).await;
        beat(&h, id, CheckOutcome::down(long.clone())).await;
    }
    let v = summary(&h, &h.viewer, "").await;
    assert_eq!(v["monitors"]["down"], 27, "{v}");
    let down = v["down"].as_array().unwrap();
    assert_eq!(down.len(), 25);
    assert_eq!(down[0]["message"].as_str().unwrap().chars().count(), 500);
}

#[tokio::test]
async fn pct_is_the_mean_of_the_monitors_uptime() {
    let h = harness().await;
    let a = push_monitor(&h, "A").await;
    let b = push_monitor(&h, "B").await;
    let idle = push_monitor(&h, "No data").await;
    for outcome in [
        CheckOutcome::up(5),
        CheckOutcome::up(5),
        CheckOutcome::down("x"),
    ] {
        beat(&h, a, outcome).await;
    }
    beat(&h, b, CheckOutcome::up(5)).await;

    for days in [1_i64, 30, 90] {
        let (pa, pb, p_idle, p24) = h
            .state
            .with_conn(move |c| {
                Ok((
                    rollups::uptime_pct(c, a, days)?,
                    rollups::uptime_pct(c, b, days)?,
                    rollups::uptime_pct(c, idle, days)?,
                    rollups::uptime_pct(c, a, 1)?.zip(rollups::uptime_pct(c, b, 1)?),
                ))
            })
            .await
            .unwrap();
        let (pa, pb) = (pa.unwrap(), pb.unwrap());
        assert!(p_idle.is_none());
        let expect = ((pa + pb) / 2.0 * 100.0).round() / 100.0;
        let v = summary(&h, &h.viewer, &format!("?days={days}")).await;
        assert_eq!(v["uptime"]["pct"], expect, "days={days}: {v}");
        assert_eq!(
            v["uptime"]["monitors_counted"], 2,
            "monitor without data is not counted"
        );
        let (a24, b24) = p24.unwrap();
        assert_eq!(
            v["uptime"]["pct_24h"],
            ((a24 + b24) / 2.0 * 100.0).round() / 100.0
        );
    }
    // A fixture the dashboard knows too: A = 2 of 3 up.
    assert_eq!(
        h.state
            .with_conn(move |c| rollups::uptime_pct(c, a, 30))
            .await
            .unwrap(),
        Some(100.0 * 2.0 / 3.0)
    );
}

#[tokio::test]
async fn summary_carries_an_etag_and_honours_if_none_match() {
    let h = harness().await;
    let id = push_monitor(&h, "Svc").await;
    beat(&h, id, CheckOutcome::up(5)).await;
    let res = h
        .call(Method::GET, "/api/v1/summary", &h.viewer, None)
        .await;
    assert_eq!(res.status(), StatusCode::OK);
    let etag = res.headers()[header::ETAG].to_str().unwrap().to_string();
    assert!(etag.starts_with("W/\""), "{etag}");
    let req = Request::builder()
        .uri("/api/v1/summary")
        .header(header::AUTHORIZATION, format!("Bearer {}", h.viewer))
        .header(header::IF_NONE_MATCH, &etag)
        .body(ReqBody::empty())
        .unwrap();
    assert_eq!(
        h.app.clone().oneshot(req).await.unwrap().status(),
        StatusCode::NOT_MODIFIED
    );

    // Once the data changes the old tag no longer matches: 200, new tag.
    beat(&h, id, CheckOutcome::down("boom")).await;
    let req = Request::builder()
        .uri("/api/v1/summary")
        .header(header::AUTHORIZATION, format!("Bearer {}", h.viewer))
        .header(header::IF_NONE_MATCH, &etag)
        .body(ReqBody::empty())
        .unwrap();
    let res = h.app.clone().oneshot(req).await.unwrap();
    assert_eq!(res.status(), StatusCode::OK);
    let fresh = res.headers()[header::ETAG].to_str().unwrap().to_string();
    assert_ne!(fresh, etag);
    let body: Value =
        serde_json::from_slice(&to_bytes(res.into_body(), usize::MAX).await.unwrap()).unwrap();
    assert_eq!(body["monitors"]["down"], 1);
}

#[tokio::test]
async fn summary_needs_credentials_and_works_with_a_session() {
    let h = harness().await;
    assert_eq!(
        h.call(Method::GET, "/api/v1/summary", "", None)
            .await
            .status(),
        StatusCode::UNAUTHORIZED
    );
    let bogus = format!("vv_{}", "a".repeat(43));
    assert_eq!(
        h.call(Method::GET, "/api/v1/summary", &bogus, None)
            .await
            .status(),
        StatusCode::UNAUTHORIZED
    );
    // A signed-in browser may read it, like the other API reads.
    let mut viewer = Browser::login(&h, "viewer").await;
    let (status, body) = viewer.get("/api/v1/summary?days=7").await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let v: Value = serde_json::from_str(&body).unwrap();
    assert_eq!(v["uptime"]["days"], 7);
    // Only GET exists.
    assert_eq!(
        h.json(Method::POST, "/api/v1/summary", &h.admin, None)
            .await
            .0,
        StatusCode::METHOD_NOT_ALLOWED
    );
}
