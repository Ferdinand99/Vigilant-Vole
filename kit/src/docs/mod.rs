//! The API reference: a product lists its endpoints as [`RouteDoc`]s in a
//! [`DocsSpec`]; the kit turns that table into the OpenAPI document
//! ([`openapi_document`]) and the Markdown reference ([`markdown`]), so the
//! page, the OpenAPI document and the Markdown can't drift apart.

pub mod markdown;
pub mod page;

use axum::http::HeaderMap;
use serde_json::{Map, Value, json};

/// What the product contributes to its API reference.
pub struct DocsSpec {
    pub routes: &'static [RouteDoc],
    pub openapi_title: &'static str,
    pub openapi_description: &'static str,
    /// Description of the bearer API token scheme.
    pub bearer_description: &'static str,
    /// Machine credentials besides API tokens and sessions that some routes
    /// take (Vigilant Vole: remote agent tokens, push tokens). Empty for
    /// none; the OpenAPI document declares only the schemes listed here.
    pub credentials: &'static [Credential],
    /// JSON example of the error envelope.
    pub error_example: &'static str,
    /// The product's own error codes. The kit lists the generic ones
    /// ([`GENERIC_ERROR_CODES`]); an entry with one of their codes replaces
    /// that row, any other is added (see [`error_codes`]).
    pub error_codes: &'static [ErrorCode],
    /// The endpoint that takes and returns a whole configuration document
    /// as YAML, if the product has one.
    pub config_document: Option<ConfigDocument>,
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
    /// Translation key of the `/api-docs` page's subtitle (the product's own
    /// words, e.g. "Automate Vigilant Vole with a token …").
    pub page_subtitle: &'static str,
    /// Where the kit serves the OpenAPI document ("/api/v1/openapi.json").
    pub openapi_path: &'static str,
    /// Base path of the REST API ("/api/v1"), shown on the page.
    pub api_base: &'static str,
    /// The endpoint `curl` examples call ("/api/v1/monitors").
    pub example_path: &'static str,
    /// The Prometheus endpoint API tokens can scrape, if the product has one.
    pub metrics_path: Option<&'static str>,
    /// Host (and port) the reference names when a request has no `Host`
    /// header and no public URL is configured ("localhost:3001"); `None`
    /// says `localhost`.
    pub fallback_host: Option<&'static str>,
}

/// A machine credential some of the product's routes take instead of an API
/// token. Routes name it by its [`access`](Self::access).
#[derive(Debug, Clone, Copy)]
pub struct Credential {
    /// The [`RouteDoc::access`] of the routes that take it ("agent token").
    pub access: &'static str,
    /// How the Markdown reference describes it
    /// ("remote agent token (`Authorization: Bearer vva_…`)").
    pub words: &'static str,
    /// How it is sent as a bearer token, or `None` when it travels in the
    /// URL (its routes then declare no security scheme).
    pub bearer: Option<BearerScheme>,
}

/// A bearer token scheme of its own in the OpenAPI document.
#[derive(Debug, Clone, Copy)]
pub struct BearerScheme {
    /// Name in `components.securitySchemes` ("agentToken").
    pub name: &'static str,
    /// Its description ("Remote agent token (vva_…)").
    pub description: &'static str,
    /// The variable `curl` examples send, after the product's `env_prefix`
    /// ("AGENT_TOKEN" for `$VV_AGENT_TOKEN`).
    pub variable: &'static str,
}

/// An endpoint that takes and returns a configuration document as YAML
/// (JSON works too).
#[derive(Debug, Clone, Copy)]
pub struct ConfigDocument {
    /// Its path ("/api/v1/config").
    pub path: &'static str,
    /// The file the `curl` examples save to and send ("vigilant.yml").
    pub file: &'static str,
    /// How the Markdown reference describes the request body.
    pub body: &'static str,
}

/// One row of the error code table.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ErrorCode {
    /// The `error.code` value ("not_found").
    pub code: &'static str,
    /// Its HTTP status.
    pub status: u16,
    /// When it happens, for the Markdown reference.
    pub when: &'static str,
    /// Also in the short list on the `/api-docs` page.
    pub on_page: bool,
}

/// The error codes of the kit's REST conventions, in table order. The last
/// one, `internal`, always closes the table.
pub const GENERIC_ERROR_CODES: &[ErrorCode] = &[
    ErrorCode {
        code: "bad_request",
        status: 400,
        when: "malformed query, cursor or body",
        on_page: true,
    },
    ErrorCode {
        code: "unauthorized",
        status: 401,
        when: "missing, invalid, expired or revoked token",
        on_page: true,
    },
    ErrorCode {
        code: "forbidden",
        status: 403,
        when: "your role or the token's scope doesn't allow it",
        on_page: true,
    },
    ErrorCode {
        code: "not_found",
        status: 404,
        when: "no such object or route",
        on_page: true,
    },
    ErrorCode {
        code: "conflict",
        status: 409,
        when: "clashes with existing data or the object's state",
        on_page: true,
    },
    ErrorCode {
        code: "unsupported_media_type",
        status: 415,
        when: "body without `Content-Type: application/json`",
        on_page: false,
    },
    ErrorCode {
        code: "validation_failed",
        status: 422,
        when: "invalid fields; `fields` maps each field to its message",
        on_page: true,
    },
    ErrorCode {
        code: "internal",
        status: 500,
        when: "server error; try again",
        on_page: false,
    },
];

/// The error code table: the kit's generic codes merged with the product's
/// ([`DocsSpec::error_codes`]). A product code replaces the kit's row with
/// the same code, in place. Any other goes after the last row with the same
/// or a lower status, in the product's order; `internal` stays last.
pub fn error_codes() -> Vec<ErrorCode> {
    merge_error_codes(GENERIC_ERROR_CODES, spec().error_codes)
}

fn merge_error_codes(generic: &[ErrorCode], product: &[ErrorCode]) -> Vec<ErrorCode> {
    let Some((last, generic)) = generic.split_last() else {
        return product.to_vec();
    };
    let mut out = generic.to_vec();
    let mut closing = *last;
    for &code in product {
        if code.code == closing.code {
            closing = code;
        } else if let Some(row) = out.iter_mut().find(|r| r.code == code.code) {
            *row = code;
        } else {
            let at = out
                .iter()
                .rposition(|r| r.status <= code.status)
                .map_or(0, |i| i + 1);
            out.insert(at, code);
        }
    }
    out.push(closing);
    out
}

/// The product's credential for routes with this `access`, if it has one.
pub fn credential(access: &str) -> Option<&'static Credential> {
    spec().credentials.iter().find(|c| c.access == access)
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

    /// The product's configuration document endpoint
    /// ([`DocsSpec::config_document`]) takes and returns YAML (JSON works
    /// too).
    pub fn is_yaml(&self) -> bool {
        spec()
            .config_document
            .is_some_and(|doc| doc.path == self.path)
    }

    /// The status code of a successful call.
    /// POST creates (201) only on a collection, i.e. a path that also lists;
    /// actions like `/pause`, `/test` or `/bulk` answer 200.
    pub fn success_status(&self) -> u16 {
        match self.method {
            "DELETE" => 204,
            "POST"
                if self
                    .path
                    .strip_prefix(spec().api_base)
                    .is_some_and(|rest| rest.starts_with('/'))
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

/// This instance's address: the configured base URL, else the request's
/// host, else the product's [`DocsSpec::fallback_host`].
pub fn instance_url(base_url: Option<String>, headers: &HeaderMap) -> String {
    base_url.unwrap_or_else(|| {
        let host = headers
            .get(axum::http::header::HOST)
            .and_then(|h| h.to_str().ok())
            .unwrap_or_else(|| spec().fallback_host.unwrap_or("localhost"));
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

/// The OpenAPI 3.1 document, assembled from the product's routes.
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
        let security = match (r.access, credential(r.access)) {
            ("public", _) => json!([]),
            (_, Some(c)) => match c.bearer {
                Some(scheme) => json!([{ scheme.name: [] }]),
                None => json!([]),
            },
            _ => json!([{"bearerAuth": []}, {"sessionCookie": []}]),
        };
        op.insert("security".into(), security);
        op.insert("x-access".into(), json!(r.access));
        let entry = paths.entry(r.path.to_string()).or_insert_with(|| json!({}));
        if let Some(obj) = entry.as_object_mut() {
            obj.insert(r.method_lower(), Value::Object(op));
        }
    }
    let mut schemes = Map::new();
    schemes.insert(
        "bearerAuth".into(),
        json!({"type": "http", "scheme": "bearer", "description": spec().bearer_description}),
    );
    for scheme in spec().credentials.iter().filter_map(|c| c.bearer) {
        schemes.insert(
            scheme.name.into(),
            json!({"type": "http", "scheme": "bearer", "description": scheme.description}),
        );
    }
    schemes.insert(
        "sessionCookie".into(),
        json!({"type": "apiKey", "in": "cookie", "name": crate::config::get().session_cookie}),
    );
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
            "securitySchemes": schemes,
            "responses": {
                "Error": {
                    "description": "Error",
                    "content": {"application/json": {"example": example(spec().error_example)}},
                },
            },
        },
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    const fn code(code: &'static str, status: u16, when: &'static str) -> ErrorCode {
        ErrorCode {
            code,
            status,
            when,
            on_page: false,
        }
    }

    #[test]
    fn product_error_codes_replace_or_join_the_generic_ones() {
        let product = [
            code("conflict", 409, "a clash, say"),
            code("too_large", 413, "too big"),
            code("invalid_document", 422, "unreadable"),
            code("delivery_failed", 502, "refused"),
            code("teapot", 100, "first of all"),
        ];
        let merged = merge_error_codes(GENERIC_ERROR_CODES, &product);
        let order: Vec<(&str, u16)> = merged.iter().map(|c| (c.code, c.status)).collect();
        assert_eq!(
            order,
            [
                ("teapot", 100),
                ("bad_request", 400),
                ("unauthorized", 401),
                ("forbidden", 403),
                ("not_found", 404),
                ("conflict", 409),
                ("too_large", 413),
                ("unsupported_media_type", 415),
                ("validation_failed", 422),
                ("invalid_document", 422),
                ("delivery_failed", 502),
                ("internal", 500),
            ]
        );
        assert_eq!(merged[5].when, "a clash, say");
        assert!(!merged[5].on_page, "the product's row replaces the kit's");
        assert_eq!(
            merge_error_codes(GENERIC_ERROR_CODES, &[]),
            GENERIC_ERROR_CODES
        );
        let closing = merge_error_codes(GENERIC_ERROR_CODES, &[code("internal", 500, "oops")]);
        assert_eq!(closing.last().map(|c| c.when), Some("oops"));
    }

    #[test]
    fn credentials_and_the_config_document_are_the_products() {
        // The test configuration has agent tokens but no configuration
        // document.
        let agent = credential("agent token").expect("agent token");
        assert_eq!(agent.bearer.map(|b| b.name), Some("agentToken"));
        assert!(credential("push token").is_none());
        let route = r("POST", "/api/v1/config", "admin", "Config", "Apply.");
        assert!(!route.is_yaml());
        let doc = openapi_document("/");
        let schemes = doc["components"]["securitySchemes"].as_object().unwrap();
        assert!(schemes.contains_key("agentToken"));
        assert_eq!(
            instance_url(None, &HeaderMap::new()),
            "http://localhost:3001"
        );
    }
}
