//! Router tests for incident updates: the public timeline, the Atom feed
//! (including password protection) and the editor endpoints.

use super::router_tests::{app, get, login, seed, send};
use super::{invalidate_views};
use crate::db::incidents as db_incidents;
use crate::db::models::Role;
use crate::db::status_pages::extras;
use axum::http::StatusCode;

async fn first_incident(t: &super::router_tests::TestApp, pid: i64) -> i64 {
    t.state.with_conn(move |c| db_incidents::list_for_page(c, pid)).await.unwrap()[0].id
}

#[tokio::test]
async fn the_public_page_shows_the_timeline_of_updates() {
    let t = app().await;
    let (pid, _) = seed(&t, true, None).await;
    let iid = first_incident(&t, pid).await;
    t.state
        .with_conn(move |c| {
            db_incidents::add_update(c, pid, iid, "investigating", "Looking into it")?;
            db_incidents::add_update(c, pid, iid, "identified", "A bad deploy, **rolling back**")?;
            Ok(())
        })
        .await
        .unwrap();
    invalidate_views();
    let (status, _, body) = get(&t, "/status/acme", None, None).await;
    assert_eq!(status, StatusCode::OK);
    assert!(body.contains("incident-updates"), "{body}");
    let (investigating, identified) = (body.find("Investigating").unwrap(), body.find("Identified").unwrap());
    assert!(identified < investigating, "newest update first");
    assert!(body.contains("<strong>rolling back</strong>"));
    // The JSON twin carries the raw text.
    let (_, _, json) = get(&t, "/api/status-page/acme", None, None).await;
    assert!(json.contains("\"updates\"") && json.contains("Looking into it"), "{json}");
    // Resolving through an update moves the incident to the history.
    t.state.with_conn(move |c| db_incidents::add_update(c, pid, iid, "resolved", "Fixed")).await.unwrap();
    invalidate_views();
    let (_, _, body) = get(&t, "/status/acme", None, None).await;
    assert!(body.contains("Resolved") && body.contains("Fixed"), "{body}");
}

#[tokio::test]
async fn the_atom_feed_lists_incidents_with_their_updates() {
    let t = app().await;
    let (pid, _) = seed(&t, true, None).await;
    let iid = first_incident(&t, pid).await;
    t.state
        .with_conn(move |c| db_incidents::add_update(c, pid, iid, "monitoring", "Watching <closely> & waiting"))
        .await
        .unwrap();
    let (status, headers, body) = get(&t, "/status/acme/feed.xml", Some("status.example.com"), None).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert!(headers.get("content-type").unwrap_or("").starts_with("application/atom+xml"));
    assert!(body.starts_with("<?xml version=\"1.0\""));
    assert!(body.contains("<feed xmlns=\"http://www.w3.org/2005/Atom\""));
    assert!(body.contains(&format!("<id>http://status.example.com/status/acme#incident-{iid}</id>")), "{body}");
    assert!(body.contains("<title>Heads up</title>"));
    assert!(body.contains("<category term=\"warning\"/>") && body.contains("<category term=\"monitoring\"/>"));
    // Update text is escaped for XML and sanitised as HTML first.
    assert!(body.contains("Watching &amp;lt;closely&amp;gt; &amp;amp; waiting"), "{body}");
    assert!(!body.contains("<script>"), "no raw script in the feed");
    // Every timestamp is RFC 3339 in UTC.
    for tag in ["updated", "published"] {
        let start = body.find(&format!("<{tag}>")).unwrap() + tag.len() + 2;
        let stamp = &body[start..start + 20];
        assert!(stamp.ends_with('Z') && stamp.as_bytes()[10] == b'T', "{stamp}");
    }
    // The page links to its feed.
    let (_, _, page) = get(&t, "/status/acme", None, None).await;
    assert!(page.contains("href=\"/status/acme/feed.xml\""), "{page}");
}

#[tokio::test]
async fn feeds_respect_publishing_and_passwords() {
    let t = app().await;
    let (pid, _) = seed(&t, false, None).await;
    // Unpublished pages have no feed, not even for a logged-in editor.
    let (status, _, _) = get(&t, "/status/acme/feed.xml", None, None).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    let cookie = login(&t, "ed", Role::Editor).await;
    let (status, _, _) = get(&t, "/status/acme/feed.xml", None, Some(&cookie)).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    let (status, _, _) = get(&t, "/status/nope/feed.xml", None, None).await;
    assert_eq!(status, StatusCode::NOT_FOUND);

    // Published and password protected: 401 and nothing from the page.
    t.state.with_conn(move |c| c.execute("UPDATE status_pages SET published = 1 WHERE id = ?1", [pid])).await.unwrap();
    let hash = crate::auth::hash_password("correct horse");
    t.state.with_conn(move |c| extras::set_password_hash(c, pid, Some(&hash))).await.unwrap();
    invalidate_views();
    let (status, headers, body) = get(&t, "/status/acme/feed.xml", None, None).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
    assert_eq!(headers.get("cache-control"), Some("no-store"));
    assert!(!body.contains("Heads up") && !body.contains("<feed"), "{body}");
    // The page itself doesn't advertise a feed to people who can't read it.
    let (_, _, page) = get(&t, "/status/acme", None, None).await;
    assert!(!page.contains("feed.xml"), "{page}");
}

#[tokio::test]
async fn editors_post_and_delete_updates() {
    let t = app().await;
    let (pid, _) = seed(&t, true, None).await;
    let iid = first_incident(&t, pid).await;
    let ed = login(&t, "ed", Role::Editor).await;
    let viewer = login(&t, "view", Role::Viewer).await;
    let url = format!("/status-pages/{pid}/incidents/{iid}/updates");

    // The incident list has the form and no timeline yet.
    let (status, _, body) = get(&t, &format!("/status-pages/{pid}/incidents"), None, Some(&ed)).await;
    assert_eq!(status, StatusCode::OK);
    assert!(body.contains(&format!("action=\"{url}\"")), "{body}");

    // Viewers can't post.
    let (status, _, _) = send(&t, "POST", &url, &viewer, "status=identified&body=Nope").await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    // Bad input is turned back with a message, nothing is stored.
    for bad in ["status=weird&body=x", "status=identified&body=+++", "status=identified"] {
        let (status, _, _) = send(&t, "POST", &url, &ed, bad).await;
        assert!(status.is_redirection(), "{bad}: {status}");
    }
    assert!(t.state.with_conn(move |c| db_incidents::list_updates(c, iid)).await.unwrap().is_empty());

    let (status, _, _) = send(&t, "POST", &url, &ed, "status=identified&body=Found+the+cause").await;
    assert!(status.is_redirection(), "{status}");
    let updates = t.state.with_conn(move |c| db_incidents::list_updates(c, iid)).await.unwrap();
    assert_eq!((updates.len(), updates[0].status.as_str(), updates[0].body.as_str()), (1, "identified", "Found the cause"));
    let (_, _, body) = get(&t, &format!("/status-pages/{pid}/incidents"), None, Some(&ed)).await;
    assert!(body.contains("Found the cause") && body.contains(&format!("hx-delete=\"{url}/{}\"", updates[0].id)), "{body}");

    // Another page's incident can't be reached through this one.
    let (status, _, _) = send(&t, "POST", &format!("/status-pages/{}/incidents/{iid}/updates", pid + 99), &ed, "status=identified&body=x").await;
    assert_eq!(status, StatusCode::NOT_FOUND);

    let uid = updates[0].id;
    let (status, _, _) = send(&t, "DELETE", &format!("{url}/{uid}"), &viewer, "").await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    let (status, _, _) = send(&t, "DELETE", &format!("{url}/{uid}"), &ed, "").await;
    assert!(status.is_success(), "{status}");
    assert!(t.state.with_conn(move |c| db_incidents::list_updates(c, iid)).await.unwrap().is_empty());
}
