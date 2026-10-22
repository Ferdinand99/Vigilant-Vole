//! The API reference: a product lists its endpoints as [`RouteDoc`]s in a
//! [`DocsSpec`]; the kit turns that table into the OpenAPI document
//! ([`openapi_document`]) and the Markdown reference ([`markdown`]), so the
//! page, the OpenAPI document and the Markdown can't drift apart.

pub mod markdown;

use axum::http::HeaderMap;
use serde_json::{Map, Value, json};

/// What the product contributes to its API reference.
pub struct DocsSpec {
    pub routes: &'static [RouteDoc],
    pub openapi_title: &'static str,
    pub openapi_description: &'static str,
    /// Description of the bearer API token scheme.
    pub bearer_description: &'static str,
    /// Description of the remote agent token scheme.
    pub agent_token_description: &'static str,
    /// JSON example of the error envelope.
    pub error_example: &'static str,
    /// Name of the configuration file the examples save and send.
    pub config_file: &'static str,
    /// How the Markdown names a remote agent's credential.
    pub agent_token_access: &'static str,
    /// Placeholder a path segment like `{id}` gets in a `curl` example, for
    /// the segments the generic rules don't know.
    pub curl_path_param: fn(route: &RouteDoc, segment: &str) -> Option<String>,
    /// Markdown before the error section (title, authentication,
    /// conventions).
    pub markdown_intro: fn(out: &mut String, base_url: &str),
    /// Markdown between the error section and the endpoints.
    pub markdown_middle: fn(out: &mut String),
    /// Markdown after the endpoints.
    pub markdown_outro: fn(out: &mut String),
}

/// The product's reference spec.
pub fn spec() -> &'static DocsSpec {
    crate::config::get().docs
}

/// The product's endpoints.
pub fn routes() -> &'static [RouteDoc] {
    spec().routes
}

/// One documented endpoint.
#[derive(Debug, Clone, Copy)]
pub struct RouteDoc {
    pub method: &'static str,
    pub path: &'static str,
    /// Minimum access: `viewer`, `editor`, `admin`, or a machine credential
    /// (`public`, `push token`, `agent token`, `metrics token`).
    pub access: &'static str,
    pub group: &'static str,
    pub summary: &'static str,
    /// Query parameters, `name: description` per entry.
    pub query: &'static [&'static str],
    pub request: &'static str,
    pub response: &'static str,
}

pub const fn r(
    method: &'static str,
    path: &'static str,
    access: &'static str,
    group: &'static str,
    summary: &'static str,
) -> RouteDoc {
    RouteDoc {
        method,
        path,
        access,
        group,
        summary,
        query: &[],
        request: "",
        response: "",
    }
}

impl RouteDoc {
    pub const fn q(mut self, query: &'static [&'static str]) -> Self {
        self.query = query;
        self
    }
    pub const fn req(mut self, request: &'static str) -> Self {
        self.request = request;
        self
    }
    pub const fn res(mut self, response: &'static str) -> Self {
        self.response = response;
        self
    }

    /// Lowercase method, for CSS-free badges and OpenAPI keys.
    pub fn method_lower(&self) -> String {
        self.method.to_ascii_lowercase()
    }

    /// Anchor id on the docs page.
    pub fn anchor(&self) -> String {
        let mut s = format!("{}-{}", self.method_lower(), self.path);
        s.retain(|c| c.is_ascii_alphanumeric() || c == '-' || c == '/');
        s.replace('/', "-").trim_matches('-').to_string()
    }

    pub fn is_public(&self) -> bool {
        !matches!(self.access, "viewer" | "editor" | "admin")
    }

    /// `/api/v1/config` takes and returns YAML (JSON works too).
    pub fn is_yaml(&self) -> bool {
        self.path == "/api/v1/config"
    }

    /// The status code of a successful call.
    /// POST creates (201) only on a collection, i.e. a path that also lists;
    /// actions like `/pause`, `/test` or `/bulk` answer 200.
    pub fn success_status(&self) -> u16 {
        match self.method {
            "DELETE" => 204,
            "POST"
                if self.path.starts_with("/api/v1/")
                    && !self.is_yaml()
                    && routes()
                        .iter()
                        .any(|r| r.method == "GET" && r.path == self.path) =>
            {
                201
            }
            _ => 200,
        }
    }

    /// Status class for the access badge.
    pub fn access_class(&self) -> &'static str {
        match self.access {
            "admin" => "status-down",
            "editor" => "status-pending",
            "viewer" => "status-up",
            _ => "status-info",
        }
    }
}

/// Groups in display order.
pub fn groups() -> Vec<&'static str> {
    let mut out: Vec<&'static str> = Vec::new();
    for r in routes() {
        if !out.contains(&r.group) {
            out.push(r.group);
        }
    }
    out
}

/// This instance's address: the configured base URL, else the request's host.
pub fn instance_url(base_url: Option<String>, headers: &HeaderMap) -> String {
    base_url.unwrap_or_else(|| {
        let host = headers
            .get(axum::http::header::HOST)
            .and_then(|h| h.to_str().ok())
            .unwrap_or("localhost:3001");
        format!("http://{host}")
    })
}

/// `{id}` style path parameters.
pub fn path_params(path: &str) -> Vec<&str> {
    path.split('/')
        .filter_map(|seg| seg.strip_prefix('{').and_then(|s| s.strip_suffix('}')))
        .collect()
}

/// JSON Schema type of a path parameter.
pub fn path_param_type(name: &str) -> &'static str {
    if matches!(name, "kind" | "token" | "slug") {
        "string"
    } else {
        "integer"
    }
}

pub fn example(text: &str) -> Value {
    serde_json::from_str(text).unwrap_or_else(|_| Value::String(text.to_string()))
}

/// The OpenAPI 3.1 document, assembled from [`ROUTES`].
pub fn openapi_document(server_url: &str) -> Value {
    let mut paths = Map::new();
    for r in routes() {
        let mut op = Map::new();
        op.insert("summary".into(), json!(r.summary));
        op.insert("tags".into(), json!([r.group]));
        op.insert("operationId".into(), json!(r.anchor()));
        let mut params: Vec<Value> = path_params(r.path)
            .into_iter()
            .map(|p| json!({"name": p, "in": "path", "required": true, "schema": {"type": path_param_type(p)}}))
            .collect();
        for q in r.query {
            let (name, desc) = q.split_once(':').unwrap_or((q, ""));
            params.push(json!({"name": name.trim(), "in": "query", "required": false, "description": desc.trim(), "schema": {"type": "string"}}));
        }
        if !params.is_empty() {
            op.insert("parameters".into(), Value::Array(params));
        }
        if !r.request.is_empty() {
            let content = if r.is_yaml() {
                json!({
                    "application/yaml": {"schema": {"type": "string"}, "example": r.request},
                    "application/json": {"schema": {"type": "object"}},
                })
            } else {
                json!({"application/json": {"schema": {}, "example": example(r.request)}})
            };
            op.insert(
                "requestBody".into(),
                json!({"required": true, "content": content}),
            );
        }
        let ok_code = r.success_status().to_string();
        let mut ok = json!({"description": "Success"});
        if !r.response.is_empty() {
            let ct = if r.is_yaml() && r.method == "GET" {
                "application/yaml"
            } else {
                "application/json"
            };
            ok["content"] = json!({ ct: {"example": example(r.response)} });
        }
        let mut responses = Map::new();
        responses.insert(ok_code, ok);
        if !r.is_public() {
            responses.insert(
                "401".into(),
                json!({"$ref": "#/components/responses/Error"}),
            );
            responses.insert(
                "403".into(),
                json!({"$ref": "#/components/responses/Error"}),
            );
        }
        if r.path.contains('{') {
            responses.insert(
                "404".into(),
                json!({"$ref": "#/components/responses/Error"}),
            );
        }
        if matches!(r.method, "POST" | "PUT" | "PATCH") {
            responses.insert(
                "422".into(),
                json!({"$ref": "#/components/responses/Error"}),
            );
        }
        op.insert("responses".into(), Value::Object(responses));
        let security = match r.access {
            "public" | "push token" => json!([]),
            "agent token" => json!([{"agentToken": []}]),
            _ => json!([{"bearerAuth": []}, {"sessionCookie": []}]),
        };
        op.insert("security".into(), security);
        op.insert("x-access".into(), json!(r.access));
        let entry = paths.entry(r.path.to_string()).or_insert_with(|| json!({}));
        if let Some(obj) = entry.as_object_mut() {
            obj.insert(r.method_lower(), Value::Object(op));
        }
    }
    json!({
        "openapi": "3.1.0",
        "info": {
            "title": spec().openapi_title,
            "version": crate::config::get().version,
            "description": spec().openapi_description,
        },
        "servers": [{"url": server_url}],
        "tags": groups().iter().map(|g| json!({"name": g})).collect::<Vec<_>>(),
        "paths": paths,
        "components": {
            "securitySchemes": {
                "bearerAuth": {"type": "http", "scheme": "bearer", "description": spec().bearer_description},
                "agentToken": {"type": "http", "scheme": "bearer", "description": spec().agent_token_description},
                "sessionCookie": {"type": "apiKey", "in": "cookie", "name": crate::config::get().session_cookie},
            },
            "responses": {
                "Error": {
                    "description": "Error",
                    "content": {"application/json": {"example": example(spec().error_example)}},
                },
            },
        },
    })
}
