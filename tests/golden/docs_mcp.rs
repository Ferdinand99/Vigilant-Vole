//! Generated API documentation (OpenAPI, Markdown, llms.txt) and the MCP
//! server's handshake and tool list.

use axum::body::Body;
use axum::http::{Method, Request, header};
use insta::assert_snapshot;
use serde_json::{Value, json};

use crate::harness::{App, admin_token, read_token};
use crate::norm;

fn pretty(v: &Value) -> String {
    norm::text(&serde_json::to_string_pretty(v).unwrap())
}

#[tokio::test]
async fn openapi_document() {
    let mut app = App::seeded("openapi").await;
    let res = app.api(Method::GET, "/api/v1/openapi.json", None).await;
    assert_eq!(res.status, 200);
    let doc = res.json();
    assert_eq!(doc["info"]["version"], norm::VERSION);
    assert_snapshot!(
        "openapi_json",
        format!(
            "content-type: {}\n\n{}",
            res.header("content-type"),
            pretty(&doc)
        )
    );
}

#[tokio::test]
async fn markdown_reference_and_llms_txt() {
    let mut app = App::seeded("api-md").await;
    // Signed-in pages (outside /api/, so a session, not a token).
    let md = app.get("/api-docs.md").await;
    let llms = app.get("/llms.txt").await;
    assert_eq!((md.status.as_u16(), llms.status.as_u16()), (200, 200));
    assert_eq!(md.body, llms.body, "llms.txt serves the same document");
    assert_snapshot!(
        "api_docs_md",
        format!(
            "/api-docs.md content-type: {}\n/llms.txt content-type: {}\n\n{}",
            md.header("content-type"),
            llms.header("content-type"),
            norm::text(&md.text())
        )
    );
}

async fn mcp(app: &mut App, token: &str, body: Value) -> Value {
    let req = Request::post("/mcp")
        .header(header::AUTHORIZATION, format!("Bearer {token}"))
        .header(header::CONTENT_TYPE, "application/json")
        .header(header::ACCEPT, "application/json, text/event-stream")
        .body(Body::from(body.to_string()))
        .unwrap();
    let res = app.send(req).await;
    assert_eq!(res.status, 200, "{}", res.text());
    res.json()
}

#[tokio::test]
async fn mcp_handshake_and_tools() {
    let mut app = App::seeded("mcp").await;
    app.clear_cookies();
    let (write, read) = (admin_token(), read_token());
    let init = mcp(
        &mut app,
        &write,
        json!({"jsonrpc": "2.0", "id": 1, "method": "initialize", "params": {
            "protocolVersion": "2025-06-18",
            "capabilities": {},
            "clientInfo": {"name": "golden", "version": "1"}
        }}),
    )
    .await;
    assert_snapshot!("mcp_initialize_write", pretty(&init));
    let init_read = mcp(
        &mut app,
        &read,
        json!({"jsonrpc": "2.0", "id": 1, "method": "initialize", "params": {
            "protocolVersion": "2024-11-05",
            "capabilities": {},
            "clientInfo": {"name": "golden", "version": "1"}
        }}),
    )
    .await;
    assert_snapshot!("mcp_initialize_read", pretty(&init_read));

    let tools = mcp(
        &mut app,
        &write,
        json!({"jsonrpc": "2.0", "id": 2, "method": "tools/list"}),
    )
    .await;
    assert_snapshot!("mcp_tools_list_write", pretty(&tools));
    let tools = mcp(
        &mut app,
        &read,
        json!({"jsonrpc": "2.0", "id": 2, "method": "tools/list"}),
    )
    .await;
    assert_snapshot!("mcp_tools_list_read", pretty(&tools));

    let resources = mcp(
        &mut app,
        &read,
        json!({"jsonrpc": "2.0", "id": 3, "method": "resources/list"}),
    )
    .await;
    assert_snapshot!("mcp_resources_list", pretty(&resources));
}
