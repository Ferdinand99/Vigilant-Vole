//! The product text of the API reference (`/api-docs.md`, `/llms.txt`): the
//! introduction, the sections about monitor types, configuration as code and
//! common tasks. The kit's [`crate::kit::docs::markdown`] adds the error
//! section and one section per endpoint, from the route table.

use std::fmt::Write as _;

use crate::db::models::MonitorType;
use crate::kit::docs::markdown::{example_body, shell_quote};

/// Title, authentication and conventions.
pub fn intro(out: &mut String, base: &str) {
    header(out, base);
    authentication(out);
    conventions(out);
}

/// Between the error section and the endpoints.
pub fn middle(out: &mut String) {
    monitor_types(out);
}

/// After the endpoints.
pub fn outro(out: &mut String) {
    config_as_code(out);
    common_tasks(out);
}

fn header(out: &mut String, base: &str) {
    let version = env!("CARGO_PKG_VERSION");
    let _ = write!(
        out,
        "# Vigilant Vole API\n\n\
         > REST API of a Vigilant Vole uptime monitor (version {version}), generated from the server's own route table. \
         Everything the web app can do (monitors, tags, notification channels, status pages, incidents, maintenance, \
         settings, configuration as code) can be done with these JSON endpoints.\n\n\
         - Instance: {base}\n\
         - API base URL: {base}/api/v1\n\
         - Browsable reference: {base}/api-docs (sign in) · OpenAPI 3.1: {base}/api/v1/openapi.json\n\
         - The web interface is translated; this document, field names, values and error codes are English.\n\n\
         Examples use two shell variables; never paste a real token into a chat or commit it:\n\n\
         ```bash\n\
         export VV_URL=\"{base}\"\n\
         export VV_TOKEN=\"<API token from Settings → API tokens>\"\n\
         curl -H \"Authorization: Bearer $VV_TOKEN\" \"$VV_URL/api/v1/me\"\n\
         ```\n\n"
    );
}

fn authentication(out: &mut String) {
    out.push_str(
        "## Authentication\n\n\
         - Create a token in the web app under **Settings → API tokens** (`/settings/tokens`). Tokens start with `vv_`, \
         are shown once and can expire (30, 90, 365 days or never).\n\
         - Send it on every request: `Authorization: Bearer $VV_TOKEN`.\n\
         - Scope `read` acts as a **viewer** (every GET); scope `write` acts as the token's owner with their role.\n\
         - Roles: **viewer** reads everything; **editor** also changes monitors, tags, notification channels, status pages, \
         incidents and maintenance and exports configuration; **admin** also changes settings, users, remote browsers, \
         agents and imports configuration. Each endpoint below names the least role it needs.\n\
         - API tokens work only under `/api/` and for `/metrics`; they can't open the web interface.\n\
         - `/metrics` (Prometheus) takes the token as a Bearer token or as the HTTP Basic password (any user name).\n\
         - Machine credentials are separate: push monitors carry their own token in the URL, remote agents send an \
         agent token (`vva_…`).\n\n",
    );
}

fn conventions(out: &mut String) {
    out.push_str(
        "## Conventions\n\n\
         - JSON in and out; send bodies with `Content-Type: application/json`. `/api/v1/config` also speaks YAML.\n\
         - Lists return `{\"data\": [...], \"next_cursor\": \"42\" | null}`. Pass `?limit=` (1–500, default 50) and \
         `?cursor=<next_cursor>` for the next page; stop when `next_cursor` is `null`.\n\
         - GETs send a weak `ETag` and answer `If-None-Match` with `304 Not Modified`.\n\
         - Times are UTC strings `YYYY-MM-DD HH:MM:SS` (except `raw_status_since`, which is RFC 3339).\n\
         - Monitors keep checking during maintenance. `status` is what they record and status pages show \
         (`maintenance` during a window); `raw_status` on monitors and heartbeats is the real result (`up`, `down`, \
         `pending` or `null`), equal to `status` outside maintenance. Monitors also carry `maintenance` (a window \
         covers them now), `raw_status_since` and `raw_message`.\n\
         - Secrets (passwords, webhook URLs, keys) are returned masked as `\"••••••\"`. Send `\"••••••\"` back (or leave \
         the field out of a PATCH) to keep the stored value.\n\
         - `POST` that creates returns `201 Created` with a `Location` header; `DELETE` returns `204 No Content`.\n\
         - `PUT` replaces an object (fields left out go back to defaults); `PATCH` changes only the fields you send.\n\
         - Cookie-authenticated writes must come from the app's own pages; token requests are exempt.\n\n",
    );
}

fn monitor_types(out: &mut String) {
    let types: Vec<String> = MonitorType::all()
        .iter()
        .map(|t| format!("`{}`", t.as_str()))
        .collect();
    let _ = write!(
        out,
        "## Monitor types\n\n`monitor_type` is one of: {}.\n\n",
        types.join(", ")
    );
}

fn config_as_code(out: &mut String) {
    out.push_str(
        "## Config as code\n\n\
         The whole setup (settings, tags, notification channels, monitors, status pages, maintenance) is one YAML or \
         JSON document. Export needs an editor (secrets only for admins with `secrets=1`); import needs an admin. An \
         import is one transaction: on any error you get `422` with every problem and nothing is written. Objects are \
         matched by name (status pages by slug); masked secrets keep their stored values.\n\n\
         ```bash\n\
         # Export (YAML; add format=json for JSON)\n\
         curl -H \"Authorization: Bearer $VV_TOKEN\" \"$VV_URL/api/v1/config\" -o vigilant.yml\n\n\
         # Preview what an import would change (nothing is written)\n\
         curl -X POST -H \"Authorization: Bearer $VV_TOKEN\" -H \"Content-Type: application/yaml\" \\\n  \
         --data-binary @vigilant.yml \"$VV_URL/api/v1/config?dry_run=1\"\n\n\
         # Apply (merge: create and update, never delete)\n\
         curl -X POST -H \"Authorization: Bearer $VV_TOKEN\" -H \"Content-Type: application/yaml\" \\\n  \
         --data-binary @vigilant.yml \"$VV_URL/api/v1/config\"\n\n\
         # Replace: also deletes what the document doesn't list (in the sections it contains); needs confirm=1\n\
         curl -X POST -H \"Authorization: Bearer $VV_TOKEN\" -H \"Content-Type: application/yaml\" \\\n  \
         --data-binary @vigilant.yml \"$VV_URL/api/v1/config?mode=replace&confirm=1\"\n\
         ```\n\n",
    );
}

fn common_tasks(out: &mut String) {
    let monitor = shell_quote(example_body("POST", "/api/v1/monitors"));
    let bulk = shell_quote(example_body("POST", "/api/v1/monitors/bulk"));
    let page = shell_quote(example_body("POST", "/api/v1/status-pages"));
    let _ = write!(
        out,
        "## Common tasks\n\n\
         ### Find monitors that are down\n\n\
         ```bash\n\
         curl -H \"Authorization: Bearer $VV_TOKEN\" \"$VV_URL/api/v1/monitors?status=down\"\n\
         ```\n\n\
         ### Create an HTTP monitor\n\n\
         Tags are names (created if new) or `{{\"name\", \"value\"}}`; notifications are channel ids or \
         `{{\"channel_id\", \"escalation\"}}` (list them with `GET /api/v1/notification-channels`).\n\n\
         ```bash\n\
         curl -X POST -H \"Authorization: Bearer $VV_TOKEN\" -H \"Content-Type: application/json\" \"$VV_URL/api/v1/monitors\" -d '{monitor}'\n\
         ```\n\n\
         ### Pause and resume a monitor\n\n\
         ```bash\n\
         curl -X POST -H \"Authorization: Bearer $VV_TOKEN\" \"$VV_URL/api/v1/monitors/7/pause\"\n\
         curl -X POST -H \"Authorization: Bearer $VV_TOKEN\" \"$VV_URL/api/v1/monitors/7/resume\"\n\
         ```\n\n\
         ### Bulk edit: run on remote agents, change the interval\n\n\
         Pick monitors with `ids` or `filter` (the list's filters: `status`, `type`, `tag`, `q`; `{{}}` is every monitor). \
         Every key of `set` is optional (left out or `null` keeps each monitor's value): `run_on_agents`, \
         `confirm_with_agents` (booleans); `interval_seconds`, `retry_interval_seconds`, `timeout_seconds`, `retries` \
         (integers); `channels` (`{{\"add\": [...], \"remove\": [...]}}` or `{{\"replace\": [...]}}`, entries are ids, names or \
         `{{\"id\"|\"name\", \"escalation\"}}`); `tags` (`{{\"add\": [...], \"remove\": [...], \"clear\": true}}`). Monitors a \
         setting doesn't apply to come back in `skipped` with a reason.\n\n\
         ```bash\n\
         curl -X POST -H \"Authorization: Bearer $VV_TOKEN\" -H \"Content-Type: application/json\" \"$VV_URL/api/v1/monitors/bulk\" -d '{bulk}'\n\
         ```\n\n\
         ### Create a status page\n\n\
         Then publish it and change its look with `PATCH /api/v1/status-pages/{{id}}`; it is served at `/status/<slug>`.\n\n\
         ```bash\n\
         curl -X POST -H \"Authorization: Bearer $VV_TOKEN\" -H \"Content-Type: application/json\" \"$VV_URL/api/v1/status-pages\" -d '{page}'\n\
         ```\n"
    );
}

#[cfg(test)]
mod tests {
    use crate::api::v1::ROUTES;
    use crate::kit::docs::markdown::{curl, document};

    const BASE: &str = "https://status.example.com";

    #[test]
    fn lists_every_documented_route() {
        let doc = document(BASE);
        for r in ROUTES {
            assert!(
                doc.contains(&format!("### {} {}\n", r.method, r.path)),
                "missing {} {}",
                r.method,
                r.path
            );
        }
        assert!(doc.contains("### POST /api/v1/monitors/bulk\n"));
        assert_eq!(
            doc.matches("\n### ").count(),
            ROUTES.len() + 5,
            "one heading per route plus the common tasks"
        );
    }

    #[test]
    fn explains_authentication_conventions_and_errors() {
        let doc = document(BASE);
        for needle in [
            "## Authentication",
            "Settings → API tokens",
            "Authorization: Bearer $VV_TOKEN",
            "HTTP Basic password",
            "## Conventions",
            "next_cursor",
            "If-None-Match",
            "YYYY-MM-DD HH:MM:SS",
            "\"••••••\"",
            "## Errors",
            "\"error\": {",
            "`validation_failed` | 422",
            "## Config as code",
            "dry_run=1",
            "## Common tasks",
            "## Endpoints: Monitors",
            "`real_browser`",
        ] {
            assert!(doc.contains(needle), "missing {needle:?}");
        }
        assert!(doc.contains(&format!("version {}", env!("CARGO_PKG_VERSION"))));
    }

    #[test]
    fn uses_the_instance_base_url() {
        let doc = document("https://vole.example.org/");
        assert!(doc.contains("- API base URL: https://vole.example.org/api/v1\n"));
        assert!(doc.contains("export VV_URL=\"https://vole.example.org\""));
        assert!(!doc.contains("status.example.com/api-docs"));
    }

    #[test]
    fn is_deterministic() {
        assert_eq!(document(BASE), document(BASE));
    }

    #[test]
    fn never_contains_a_token() {
        let doc = document(BASE);
        for prefix in std::iter::once(crate::kit::config::get().token_prefix).chain(
            crate::kit::config::get()
                .other_token_prefixes
                .iter()
                .copied(),
        ) {
            for (i, _) in doc.match_indices(prefix) {
                let rest = &doc[i + prefix.len()..];
                let run = rest
                    .chars()
                    .take_while(|c| c.is_ascii_alphanumeric() || *c == '-' || *c == '_')
                    .count();
                assert!(
                    run < 8,
                    "token-like text at {i}: {}",
                    &doc[i..(i + 20).min(doc.len())]
                );
            }
        }
    }

    #[test]
    fn curl_examples_quote_bodies_for_the_shell() {
        let incident = ROUTES
            .iter()
            .find(|r| r.method == "POST" && r.path == "/api/v1/status-pages/{id}/incidents")
            .expect("route");
        let cmd = curl(incident);
        assert!(cmd.starts_with("curl -X POST -H \"Authorization: Bearer $VV_TOKEN\""));
        assert!(cmd.contains("\"$VV_URL/api/v1/status-pages/1/incidents\""));
        assert!(cmd.contains("We'\\''re looking into it."));
        let push = ROUTES
            .iter()
            .find(|r| r.path == "/api/push/{token}")
            .expect("route");
        assert_eq!(curl(push), "curl \"$VV_URL/api/push/$VV_PUSH_TOKEN\"");
    }
}
