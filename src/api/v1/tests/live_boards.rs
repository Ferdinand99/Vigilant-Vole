//! `/api/v1/live-boards`: the webhook address goes in and never comes out.

use super::*;

const WEBHOOK: &str = "https://discord.com/api/webhooks/123456789012345678/SECRETtokenValue-1_x";

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

async fn create(h: &Harness, body: Value) -> Value {
    let (status, v) = h
        .json(Method::POST, "/api/v1/live-boards", &h.editor, Some(body))
        .await;
    assert_eq!(status, StatusCode::CREATED, "{v}");
    v
}

#[tokio::test]
async fn a_board_is_created_listed_changed_and_deleted_without_the_webhook_showing() {
    let h = harness().await;
    let a = push_monitor(&h, "API").await;
    let b = push_monitor(&h, "Queue").await;
    let board = create(
        &h,
        json!({"name": "Support", "webhook": WEBHOOK, "source": "items", "items": [b, a], "notify_down": true, "mention": "123"}),
    )
    .await;
    let id = board["id"].as_i64().unwrap();
    assert_eq!(board["platform"], "discord");
    assert_eq!(board["items"], json!([b, a]));
    assert_eq!(board["webhook_set"], true);
    assert_eq!(board["refresh_seconds"], 300);
    assert_eq!(board["state"], "active");

    // Nothing that is returned, from any endpoint, carries the address or the token.
    for uri in [
        "/api/v1/live-boards".to_string(),
        format!("/api/v1/live-boards/{id}"),
    ] {
        let res = h.call(Method::GET, &uri, &h.admin_read, None).await;
        assert_eq!(res.status(), StatusCode::OK);
        let text = String::from_utf8(
            to_bytes(res.into_body(), usize::MAX)
                .await
                .unwrap()
                .to_vec(),
        )
        .unwrap();
        for secret in ["SECRETtoken", "discord.com", "webhooks/"] {
            assert!(!text.contains(secret), "{uri} leaks {secret}: {text}");
        }
    }
    // The address is stored sealed.
    let stored: String = h
        .state
        .with_conn(move |c| {
            c.query_row(
                "SELECT webhook_url FROM live_boards WHERE id = ?1",
                [id],
                |r| r.get(0),
            )
        })
        .await
        .unwrap();
    assert!(crate::kit::crypto::is_sealed(&stored) && !stored.contains("SECRET"));

    // PATCH changes what it names and keeps the webhook.
    let (status, v) = h
        .json(
            Method::PATCH,
            &format!("/api/v1/live-boards/{id}"),
            &h.editor,
            Some(json!({"refresh_seconds": 120, "show_response": true})),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{v}");
    assert_eq!(
        (&v["refresh_seconds"], &v["show_response"]),
        (&json!(120), &json!(true))
    );
    assert_eq!(v["items"], json!([b, a]));
    assert_eq!(v["webhook_set"], true);
    let kept: String = h
        .state
        .with_conn(move |c| {
            c.query_row(
                "SELECT webhook_url FROM live_boards WHERE id = ?1",
                [id],
                |r| r.get(0),
            )
        })
        .await
        .unwrap();
    assert_eq!(kept, stored);

    let (status, _) = h
        .json(
            Method::DELETE,
            &format!("/api/v1/live-boards/{id}"),
            &h.editor,
            None,
        )
        .await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    let (_, v) = h
        .json(Method::GET, "/api/v1/live-boards", &h.admin_read, None)
        .await;
    assert_eq!(v["data"], json!([]));
}

#[tokio::test]
async fn viewers_can_read_but_not_change_and_bad_input_is_refused_with_fields() {
    let h = harness().await;
    let m = push_monitor(&h, "API").await;
    let body = json!({"name": "Support", "webhook": WEBHOOK, "source": "items", "items": [m]});
    let (status, _) = h
        .json(
            Method::POST,
            "/api/v1/live-boards",
            &h.viewer,
            Some(body.clone()),
        )
        .await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    let (status, _) = h
        .json(Method::GET, "/api/v1/live-boards", &h.viewer, None)
        .await;
    assert_eq!(status, StatusCode::OK);

    for (bad, field) in [
        (
            json!({"webhook": "https://evil.example/api/webhooks/1/x"}),
            "webhook",
        ),
        (json!({"webhook": null}), "webhook"),
        (json!({"items": []}), "items"),
        (json!({"items": [9999]}), "items"),
        (json!({"refresh_seconds": 5}), "refresh_seconds"),
        (json!({"mention": "@everyone"}), "mention"),
        (json!({"platform": "slack"}), "platform"),
        (json!({"name": ""}), "name"),
        (
            json!({"source": "page", "status_page_id": 4242}),
            "status_page_id",
        ),
    ] {
        let mut b = body.clone();
        for (k, v) in bad.as_object().unwrap() {
            b[k] = v.clone();
        }
        let (status, v) = h
            .json(Method::POST, "/api/v1/live-boards", &h.editor, Some(b))
            .await;
        assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{bad}: {v}");
        assert!(v["error"]["fields"][field].is_string(), "{bad}: {v}");
        // No error message repeats what was typed in the webhook field.
        assert!(!v.to_string().contains("evil.example"), "{v}");
    }

    // The same name twice.
    create(&h, body.clone()).await;
    let (status, v) = h
        .json(Method::POST, "/api/v1/live-boards", &h.editor, Some(body))
        .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{v}");
    assert!(v["error"]["fields"]["name"].is_string());
}

#[tokio::test]
async fn a_status_page_board_needs_a_published_page_without_a_password() {
    let h = harness().await;
    let (status, page) = h
        .json(
            Method::POST,
            "/api/v1/status-pages",
            &h.editor,
            Some(json!({"title": "Sylo", "slug": "sylo", "published": false})),
        )
        .await;
    assert_eq!(status, StatusCode::CREATED, "{page}");
    let page_id = page["id"].as_i64().unwrap();
    let body =
        json!({"name": "Sylo", "webhook": WEBHOOK, "source": "page", "status_page_id": page_id});
    let (status, v) = h
        .json(
            Method::POST,
            "/api/v1/live-boards",
            &h.editor,
            Some(body.clone()),
        )
        .await;
    assert_eq!(
        status,
        StatusCode::UNPROCESSABLE_ENTITY,
        "a draft is refused: {v}"
    );
    let (status, v) = h
        .json(
            Method::PATCH,
            &format!("/api/v1/status-pages/{page_id}"),
            &h.editor,
            Some(json!({"published": true})),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{v}");
    let board = create(&h, body).await;
    assert_eq!(board["source"], "page");
    assert_eq!(board["status_page_id"], page_id);
}

#[tokio::test]
async fn config_exports_leave_the_webhook_out_and_imports_keep_it() {
    let h = harness().await;
    let m = push_monitor(&h, "API").await;
    let board = create(
        &h,
        json!({"name": "Support", "webhook": WEBHOOK, "source": "items", "items": [m]}),
    )
    .await;
    let id = board["id"].as_i64().unwrap();

    for query in ["", "&secrets=1"] {
        let res = h
            .call(
                Method::GET,
                &format!("/api/v1/config?format=json{query}"),
                &h.admin,
                None,
            )
            .await;
        assert_eq!(res.status(), StatusCode::OK);
        let text = String::from_utf8(
            to_bytes(res.into_body(), usize::MAX)
                .await
                .unwrap()
                .to_vec(),
        )
        .unwrap();
        for secret in ["SECRETtoken", "discord.com", "webhooks/", "sealed:"] {
            assert!(!text.contains(secret), "export{query} leaks {secret}");
        }
        let doc: Value = serde_json::from_str(&text).unwrap();
        assert_eq!(doc["live_boards"][0]["name"], "Support");
        assert_eq!(doc["live_boards"][0]["items"], json!(["API"]));
    }

    // Applying the exported document again changes nothing and keeps the webhook.
    let before: String = h
        .state
        .with_conn(move |c| {
            c.query_row(
                "SELECT webhook_url FROM live_boards WHERE id = ?1",
                [id],
                |r| r.get(0),
            )
        })
        .await
        .unwrap();
    let res = h
        .call(Method::GET, "/api/v1/config?format=json", &h.admin, None)
        .await;
    let doc: Value =
        serde_json::from_slice(&to_bytes(res.into_body(), usize::MAX).await.unwrap()).unwrap();
    let (status, v) = h
        .json(Method::POST, "/api/v1/config", &h.admin, Some(doc))
        .await;
    assert_eq!(status, StatusCode::OK, "{v}");
    let after: String = h
        .state
        .with_conn(move |c| {
            c.query_row(
                "SELECT webhook_url FROM live_boards WHERE id = ?1",
                [id],
                |r| r.get(0),
            )
        })
        .await
        .unwrap();
    assert_eq!(before, after);

    // The same document with the webhook in it (as a GitOps file would carry
    // it) is just as unchanged: the saved sealed value stays, so nothing is
    // re-posted on every start.
    let mut with_webhook: Value = serde_json::from_slice(
        &to_bytes(
            h.call(Method::GET, "/api/v1/config?format=json", &h.admin, None)
                .await
                .into_body(),
            usize::MAX,
        )
        .await
        .unwrap(),
    )
    .unwrap();
    with_webhook["live_boards"][0]["webhook"] = json!(WEBHOOK);
    let (status, v) = h
        .json(Method::POST, "/api/v1/config", &h.admin, Some(with_webhook))
        .await;
    assert_eq!(status, StatusCode::OK, "{v}");
    let boards_plan: Vec<&Value> = v["items"]
        .as_array()
        .into_iter()
        .flatten()
        .filter(|i| i["kind"] == "live_board")
        .collect();
    assert!(
        boards_plan.iter().all(|i| i["action"] == "unchanged"),
        "{v}"
    );
    let after_again: String = h
        .state
        .with_conn(move |c| {
            c.query_row(
                "SELECT webhook_url FROM live_boards WHERE id = ?1",
                [id],
                |r| r.get(0),
            )
        })
        .await
        .unwrap();
    assert_eq!(before, after_again);

    // A board that is new needs its webhook in the document.
    let doc = json!({"live_boards": [{"name": "Other", "items": ["API"]}]});
    let (status, v) = h
        .json(
            Method::POST,
            "/api/v1/config?dry_run=1",
            &h.admin,
            Some(doc),
        )
        .await;
    assert!(
        status == StatusCode::UNPROCESSABLE_ENTITY || v.to_string().contains("webhook"),
        "{status} {v}"
    );
    let doc = json!({"live_boards": [{"name": "Other", "items": ["API"], "webhook": WEBHOOK}]});
    let (status, v) = h
        .json(Method::POST, "/api/v1/config", &h.admin, Some(doc))
        .await;
    assert_eq!(status, StatusCode::OK, "{v}");
    let (_, list) = h
        .json(Method::GET, "/api/v1/live-boards", &h.admin_read, None)
        .await;
    assert_eq!(list["data"].as_array().unwrap().len(), 2);

    // Replace mode never deletes boards: that would leave their messages behind.
    let (status, v) = h
        .json(
            Method::POST,
            "/api/v1/config?mode=replace&confirm=1",
            &h.admin,
            Some(json!({"live_boards": []})),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{v}");
    let (_, list) = h
        .json(Method::GET, "/api/v1/live-boards", &h.admin_read, None)
        .await;
    assert_eq!(list["data"].as_array().unwrap().len(), 2);
}
