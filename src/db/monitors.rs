use rusqlite::{Connection, OptionalExtension, Row, named_params, params};

use super::models::{
    Heartbeat, HeartbeatStatus, Monitor, MonitorType, MonitorWithStatus, NewMonitor,
};

/// Every column of `monitors`, in one place. Use with [`row_to_monitor`]
/// (which reads by column name, so extra columns may follow).
pub const MONITOR_COLUMNS: &str = "id, name, description, monitor_type, target, parent_id,
    interval_seconds, retry_interval_seconds, timeout_seconds, retries, active, upside_down,
    http_method, http_headers, http_body, http_body_encoding, accepted_status_codes, max_redirects,
    ignore_tls, basic_auth_user, basic_auth_pass, secret, proxy_url, keyword, keyword_invert,
    json_path, json_operator, json_expected, dns_record_type, dns_resolver, dns_expected,
    docker_host, push_token, expiry_notify, expiry_days, resend_interval, escalation_minutes,
    flap_detection, run_on_agents, confirm_with_agents, source, external_id, sort_order,
    created_at, updated_at,
    auth_method, oauth_token_url, oauth_client_id, oauth_client_secret, oauth_scope,
    oauth_audience, oauth_client_auth, tls_client_cert, tls_client_key, tls_ca_cert,
    remote_browser_id, browser_selector, browser_wait, browser_viewport, browser_js_errors,
    grpc_proto, check_from_agent_id";

/// `MONITOR_COLUMNS` qualified with the `m.` table alias, for joins.
fn monitor_columns_aliased() -> String {
    MONITOR_COLUMNS
        .split(',')
        .map(|c| format!("m.{}", c.trim()))
        .collect::<Vec<_>>()
        .join(", ")
}

/// Maps a row containing (at least) all [`MONITOR_COLUMNS`] to a [`Monitor`].
pub fn row_to_monitor(row: &Row) -> rusqlite::Result<Monitor> {
    let monitor_type: String = row.get("monitor_type")?;
    Ok(Monitor {
        id: row.get("id")?,
        name: row.get("name")?,
        description: row.get("description")?,
        monitor_type: MonitorType::parse(&monitor_type).unwrap_or_default(),
        target: row.get("target")?,
        parent_id: row.get("parent_id")?,
        interval_seconds: row.get("interval_seconds")?,
        retry_interval_seconds: row.get("retry_interval_seconds")?,
        timeout_seconds: row.get("timeout_seconds")?,
        retries: row.get("retries")?,
        active: row.get("active")?,
        upside_down: row.get("upside_down")?,
        http_method: row.get("http_method")?,
        http_headers: row.get("http_headers")?,
        http_body: row.get("http_body")?,
        http_body_encoding: row.get("http_body_encoding")?,
        accepted_status_codes: row.get("accepted_status_codes")?,
        max_redirects: row.get("max_redirects")?,
        ignore_tls: row.get("ignore_tls")?,
        basic_auth_user: row.get("basic_auth_user")?,
        basic_auth_pass: row.get("basic_auth_pass")?,
        secret: row.get("secret")?,
        proxy_url: row.get("proxy_url")?,
        keyword: row.get("keyword")?,
        keyword_invert: row.get("keyword_invert")?,
        json_path: row.get("json_path")?,
        json_operator: row.get("json_operator")?,
        json_expected: row.get("json_expected")?,
        dns_record_type: row.get("dns_record_type")?,
        dns_resolver: row.get("dns_resolver")?,
        dns_expected: row.get("dns_expected")?,
        docker_host: row.get("docker_host")?,
        push_token: row.get("push_token")?,
        expiry_notify: row.get("expiry_notify")?,
        expiry_days: row.get("expiry_days")?,
        resend_interval: row.get("resend_interval")?,
        escalation_minutes: row.get("escalation_minutes")?,
        flap_detection: row.get("flap_detection")?,
        run_on_agents: row.get("run_on_agents")?,
        confirm_with_agents: row.get("confirm_with_agents")?,
        source: row.get("source")?,
        external_id: row.get("external_id")?,
        sort_order: row.get("sort_order")?,
        created_at: row.get("created_at")?,
        updated_at: row.get("updated_at")?,
        auth_method: row.get("auth_method")?,
        oauth_token_url: row.get("oauth_token_url")?,
        oauth_client_id: row.get("oauth_client_id")?,
        oauth_client_secret: row.get("oauth_client_secret")?,
        oauth_scope: row.get("oauth_scope")?,
        oauth_audience: row.get("oauth_audience")?,
        oauth_client_auth: row.get("oauth_client_auth")?,
        tls_client_cert: row.get("tls_client_cert")?,
        tls_client_key: row.get("tls_client_key")?,
        tls_ca_cert: row.get("tls_ca_cert")?,
        remote_browser_id: row.get("remote_browser_id")?,
        browser_selector: row.get("browser_selector")?,
        browser_wait: row.get("browser_wait")?,
        browser_viewport: row.get("browser_viewport")?,
        browser_js_errors: row.get("browser_js_errors")?,
        grpc_proto: row.get("grpc_proto")?,
        check_from_agent_id: row.get("check_from_agent_id")?,
    })
}

/// Shared `SET`/`VALUES` bindings for insert and update (`:name` placeholders).
macro_rules! monitor_params {
    ($m:expr, $($extra:tt)*) => {
        named_params! {
            ":name": $m.name,
            ":description": $m.description,
            ":monitor_type": $m.monitor_type.as_str(),
            ":target": $m.target,
            ":parent_id": $m.parent_id,
            ":interval_seconds": $m.interval_seconds,
            ":retry_interval_seconds": $m.retry_interval_seconds,
            ":timeout_seconds": $m.timeout_seconds,
            ":retries": $m.retries,
            ":active": $m.active,
            ":upside_down": $m.upside_down,
            ":http_method": $m.http_method,
            ":http_headers": $m.http_headers,
            ":http_body": $m.http_body,
            ":http_body_encoding": $m.http_body_encoding,
            ":accepted_status_codes": $m.accepted_status_codes,
            ":max_redirects": $m.max_redirects,
            ":ignore_tls": $m.ignore_tls,
            ":basic_auth_user": $m.basic_auth_user,
            ":basic_auth_pass": $m.basic_auth_pass,
            ":secret": $m.secret,
            ":proxy_url": $m.proxy_url,
            ":keyword": $m.keyword,
            ":keyword_invert": $m.keyword_invert,
            ":json_path": $m.json_path,
            ":json_operator": $m.json_operator,
            ":json_expected": $m.json_expected,
            ":dns_record_type": $m.dns_record_type,
            ":dns_resolver": $m.dns_resolver,
            ":dns_expected": $m.dns_expected,
            ":docker_host": $m.docker_host,
            ":push_token": $m.push_token,
            ":expiry_notify": $m.expiry_notify,
            ":expiry_days": $m.expiry_days,
            ":resend_interval": $m.resend_interval,
            ":escalation_minutes": $m.escalation_minutes,
            ":flap_detection": $m.flap_detection,
            ":run_on_agents": $m.run_on_agents,
            ":confirm_with_agents": $m.confirm_with_agents,
            ":source": $m.source,
            ":external_id": $m.external_id,
            ":sort_order": $m.sort_order,
            ":auth_method": $m.auth_method,
            ":oauth_token_url": $m.oauth_token_url,
            ":oauth_client_id": $m.oauth_client_id,
            ":oauth_client_secret": $m.oauth_client_secret,
            ":oauth_scope": $m.oauth_scope,
            ":oauth_audience": $m.oauth_audience,
            ":oauth_client_auth": $m.oauth_client_auth,
            ":tls_client_cert": $m.tls_client_cert,
            ":tls_client_key": $m.tls_client_key,
            ":tls_ca_cert": $m.tls_ca_cert,
            ":remote_browser_id": $m.remote_browser_id,
            ":browser_selector": $m.browser_selector,
            ":browser_wait": $m.browser_wait,
            ":browser_viewport": $m.browser_viewport,
            ":browser_js_errors": $m.browser_js_errors,
            ":grpc_proto": $m.grpc_proto,
            ":check_from_agent_id": $m.check_from_agent_id,
            $($extra)*
        }
    };
}

/// Inserts a monitor and attaches every `is_default` notification channel.
pub fn insert_monitor(conn: &Connection, m: &NewMonitor) -> rusqlite::Result<i64> {
    conn.execute(
        "INSERT INTO monitors (
            name, description, monitor_type, target, parent_id, interval_seconds,
            retry_interval_seconds, timeout_seconds, retries, active, upside_down, http_method,
            http_headers, http_body, http_body_encoding, accepted_status_codes, max_redirects,
            ignore_tls, basic_auth_user, basic_auth_pass, secret, proxy_url, keyword, keyword_invert,
            json_path, json_operator, json_expected, dns_record_type, dns_resolver, dns_expected,
            docker_host, push_token, expiry_notify, expiry_days, resend_interval,
            escalation_minutes, flap_detection, run_on_agents, confirm_with_agents, source,
            external_id, sort_order,
            auth_method, oauth_token_url, oauth_client_id, oauth_client_secret, oauth_scope,
            oauth_audience, oauth_client_auth, tls_client_cert, tls_client_key, tls_ca_cert,
            remote_browser_id, browser_selector, browser_wait, browser_viewport, browser_js_errors,
            grpc_proto, check_from_agent_id
        ) VALUES (
            :name, :description, :monitor_type, :target, :parent_id, :interval_seconds,
            :retry_interval_seconds, :timeout_seconds, :retries, :active, :upside_down, :http_method,
            :http_headers, :http_body, :http_body_encoding, :accepted_status_codes, :max_redirects,
            :ignore_tls, :basic_auth_user, :basic_auth_pass, :secret, :proxy_url, :keyword, :keyword_invert,
            :json_path, :json_operator, :json_expected, :dns_record_type, :dns_resolver, :dns_expected,
            :docker_host, :push_token, :expiry_notify, :expiry_days, :resend_interval,
            :escalation_minutes, :flap_detection, :run_on_agents, :confirm_with_agents, :source,
            :external_id, :sort_order,
            :auth_method, :oauth_token_url, :oauth_client_id, :oauth_client_secret, :oauth_scope,
            :oauth_audience, :oauth_client_auth, :tls_client_cert, :tls_client_key, :tls_ca_cert,
            :remote_browser_id, :browser_selector, :browser_wait, :browser_viewport, :browser_js_errors,
            :grpc_proto, :check_from_agent_id
        )",
        monitor_params!(m,),
    )?;
    let id = conn.last_insert_rowid();
    conn.execute(
        "INSERT INTO monitor_notifications (monitor_id, channel_id, escalation)
         SELECT ?1, id, 0 FROM notification_channels WHERE is_default = 1",
        params![id],
    )?;
    Ok(id)
}

/// Overwrites every writable column and bumps `updated_at`.
pub fn update_monitor(conn: &Connection, id: i64, m: &NewMonitor) -> rusqlite::Result<()> {
    conn.execute(
        "UPDATE monitors SET
            name = :name, description = :description, monitor_type = :monitor_type,
            target = :target, parent_id = :parent_id, interval_seconds = :interval_seconds,
            retry_interval_seconds = :retry_interval_seconds, timeout_seconds = :timeout_seconds,
            retries = :retries, active = :active, upside_down = :upside_down,
            http_method = :http_method, http_headers = :http_headers, http_body = :http_body,
            http_body_encoding = :http_body_encoding, accepted_status_codes = :accepted_status_codes,
            max_redirects = :max_redirects, ignore_tls = :ignore_tls,
            basic_auth_user = :basic_auth_user, basic_auth_pass = :basic_auth_pass, secret = :secret,
            proxy_url = :proxy_url, keyword = :keyword, keyword_invert = :keyword_invert,
            json_path = :json_path, json_operator = :json_operator, json_expected = :json_expected,
            dns_record_type = :dns_record_type, dns_resolver = :dns_resolver,
            dns_expected = :dns_expected, docker_host = :docker_host, push_token = :push_token,
            expiry_notify = :expiry_notify, expiry_days = :expiry_days,
            resend_interval = :resend_interval, escalation_minutes = :escalation_minutes,
            flap_detection = :flap_detection, run_on_agents = :run_on_agents,
            confirm_with_agents = :confirm_with_agents, source = :source,
            external_id = :external_id, sort_order = :sort_order,
            auth_method = :auth_method, oauth_token_url = :oauth_token_url,
            oauth_client_id = :oauth_client_id, oauth_client_secret = :oauth_client_secret,
            oauth_scope = :oauth_scope, oauth_audience = :oauth_audience,
            oauth_client_auth = :oauth_client_auth, tls_client_cert = :tls_client_cert,
            tls_client_key = :tls_client_key, tls_ca_cert = :tls_ca_cert,
            remote_browser_id = :remote_browser_id, browser_selector = :browser_selector,
            browser_wait = :browser_wait, browser_viewport = :browser_viewport,
            browser_js_errors = :browser_js_errors, grpc_proto = :grpc_proto,
            check_from_agent_id = :check_from_agent_id,
            updated_at = datetime('now')
         WHERE id = :id",
        monitor_params!(m, ":id": id),
    )?;
    Ok(())
}

pub fn delete_monitor(conn: &Connection, id: i64) -> rusqlite::Result<()> {
    conn.execute("DELETE FROM monitors WHERE id = ?1", params![id])?;
    Ok(())
}

pub fn get_monitor(conn: &Connection, id: i64) -> rusqlite::Result<Option<Monitor>> {
    conn.query_row(
        &format!("SELECT {MONITOR_COLUMNS} FROM monitors WHERE id = ?1"),
        params![id],
        row_to_monitor,
    )
    .optional()
}

pub fn list_monitors(conn: &Connection) -> rusqlite::Result<Vec<Monitor>> {
    let mut stmt = conn.prepare(&format!(
        "SELECT {MONITOR_COLUMNS} FROM monitors ORDER BY sort_order, name"
    ))?;
    let rows = stmt.query_map([], row_to_monitor)?;
    rows.collect()
}

pub fn list_active_monitors(conn: &Connection) -> rusqlite::Result<Vec<Monitor>> {
    let mut stmt = conn.prepare(&format!(
        "SELECT {MONITOR_COLUMNS} FROM monitors WHERE active = 1 ORDER BY sort_order, name"
    ))?;
    let rows = stmt.query_map([], row_to_monitor)?;
    rows.collect()
}

/// How many past heartbeats the dashboard sparkline shows per monitor.
const SPARKLINE_LEN: usize = 20;

/// Every monitor with its latest local heartbeat, recent-status strip and
/// 24 h uptime (from [`super::rollups::uptime_24h_cached`]). Remote-agent
/// heartbeats are ignored.
pub fn list_monitors_with_status(conn: &Connection) -> rusqlite::Result<Vec<MonitorWithStatus>> {
    let mut list = list_monitors_with_latest(conn)?;
    for row in &mut list {
        row.uptime_24h = super::rollups::uptime_24h_cached(conn, row.monitor.id)?;
    }
    Ok(list)
}

/// [`list_monitors_with_status`] without the 24 h uptime (`uptime_24h` is
/// `None`), for callers that don't show it: the uptime is by far the most
/// expensive part (it reads every heartbeat of the last day).
pub fn list_monitors_with_latest(conn: &Connection) -> rusqlite::Result<Vec<MonitorWithStatus>> {
    let mut stmt = conn.prepare(&format!(
        "SELECT {cols},
                h.status AS last_status, h.response_time_ms AS last_response_time_ms,
                (SELECT GROUP_CONCAT(status) FROM (
                     SELECT status FROM heartbeats WHERE monitor_id = m.id AND agent_id IS NULL
                     ORDER BY checked_at DESC, id DESC LIMIT ?1
                 )) AS recent_statuses,
                a.name AS check_from_agent_name
         FROM monitors m
         LEFT JOIN agents a ON a.id = m.check_from_agent_id
         LEFT JOIN heartbeats h ON h.id = (
             SELECT id FROM heartbeats WHERE monitor_id = m.id AND agent_id IS NULL
             ORDER BY checked_at DESC, id DESC LIMIT 1
         )
         ORDER BY m.sort_order, m.name COLLATE NOCASE",
        cols = monitor_columns_aliased()
    ))?;
    let rows = stmt.query_map(params![SPARKLINE_LEN as i64], |row| {
        let monitor = row_to_monitor(row)?;
        let status: Option<String> = row.get("last_status")?;
        let recent_statuses: Option<String> = row.get("recent_statuses")?;
        let agent_name: Option<String> = row.get("check_from_agent_name")?;
        Ok(MonitorWithStatus {
            checked_by: agent_name.filter(|_| monitor.is_agent_checked()),
            monitor,
            status: status.and_then(|s| HeartbeatStatus::parse(&s)),
            response_time_ms: row.get("last_response_time_ms")?,
            recent: parse_recent(recent_statuses),
            uptime_24h: None,
        })
    })?;
    rows.collect()
}

/// `raw` is a comma-separated list of statuses, most-recent-first (as produced by
/// the `ORDER BY checked_at DESC` subquery above). Returns a fixed-length,
/// oldest-first vec, left-padded with `None` when there isn't SPARKLINE_LEN of
/// history yet, so every monitor's sparkline renders the same width.
fn parse_recent(raw: Option<String>) -> Vec<Option<HeartbeatStatus>> {
    let mut statuses: Vec<Option<HeartbeatStatus>> = raw
        .map(|s| s.split(',').map(HeartbeatStatus::parse).collect())
        .unwrap_or_default();
    statuses.reverse();
    let missing = SPARKLINE_LEN.saturating_sub(statuses.len());
    let mut padded = vec![None; missing];
    padded.extend(statuses);
    padded
}

/// Oldest-first local heartbeats for a single monitor (agent results excluded).
pub fn list_recent_heartbeats(
    conn: &Connection,
    monitor_id: i64,
    limit: i64,
) -> rusqlite::Result<Vec<Heartbeat>> {
    let mut stmt = conn.prepare(&format!(
        "SELECT * FROM (
             SELECT {}
             FROM heartbeats WHERE monitor_id = ?1 AND agent_id IS NULL
             ORDER BY checked_at DESC, id DESC
             LIMIT ?2
         ) ORDER BY checked_at ASC, id ASC",
        super::heartbeats::COLUMNS
    ))?;
    let rows = stmt.query_map(
        params![monitor_id, limit],
        super::heartbeats::row_to_heartbeat,
    )?;
    rows.collect()
}

/// Uptime percentage over a window, e.g. `modifier = "-7 days"`. `None` when
/// there's no heartbeat history in that window yet.
pub fn uptime_since(
    conn: &Connection,
    monitor_id: i64,
    modifier: &str,
) -> rusqlite::Result<Option<f64>> {
    conn.query_row(
        "SELECT CASE WHEN COUNT(*) = 0 THEN NULL
                     ELSE ROUND(100.0 * SUM(CASE WHEN status = 'up' THEN 1 ELSE 0 END) / COUNT(*), 1)
                END
         FROM heartbeats WHERE monitor_id = ?1 AND agent_id IS NULL AND checked_at >= datetime('now', ?2)",
        params![monitor_id, modifier],
        |row| row.get(0),
    )
}

/// Pause (`false`) or resume (`true`) a monitor. Returns false when it doesn't exist.
pub fn set_active(conn: &Connection, id: i64, active: bool) -> rusqlite::Result<bool> {
    let n = conn.execute(
        "UPDATE monitors SET active = ?2, updated_at = datetime('now') WHERE id = ?1",
        params![id, active],
    )?;
    Ok(n > 0)
}

/// Ids (of `ids`) that exist, in the given order without duplicates.
pub fn existing_ids(conn: &Connection, ids: &[i64]) -> rusqlite::Result<Vec<i64>> {
    let mut out: Vec<i64> = Vec::with_capacity(ids.len());
    let mut stmt = conn.prepare("SELECT EXISTS(SELECT 1 FROM monitors WHERE id = ?1)")?;
    for id in ids {
        if !out.contains(id) && stmt.query_row(params![id], |r| r.get::<_, bool>(0))? {
            out.push(*id);
        }
    }
    Ok(out)
}

/// Sets `active` on every monitor in `ids` whose state differs. Returns the
/// ids that actually changed.
pub fn set_active_many(conn: &Connection, ids: &[i64], active: bool) -> rusqlite::Result<Vec<i64>> {
    let mut changed = Vec::new();
    let mut stmt = conn.prepare(
        "UPDATE monitors SET active = ?2, updated_at = datetime('now') WHERE id = ?1 AND active <> ?2",
    )?;
    for id in ids {
        if stmt.execute(params![id, active])? > 0 && !changed.contains(id) {
            changed.push(*id);
        }
    }
    Ok(changed)
}

/// Deletes every monitor in `ids`. Returns how many were deleted.
pub fn delete_many(conn: &Connection, ids: &[i64]) -> rusqlite::Result<usize> {
    let mut stmt = conn.prepare("DELETE FROM monitors WHERE id = ?1")?;
    let mut n = 0;
    for id in ids {
        n += stmt.execute(params![id])?;
    }
    Ok(n)
}

/// Direct children of a group, by sort order and name.
pub fn children(conn: &Connection, group_id: i64) -> rusqlite::Result<Vec<Monitor>> {
    let mut stmt = conn.prepare(&format!(
        "SELECT {MONITOR_COLUMNS} FROM monitors WHERE parent_id = ?1 ORDER BY sort_order, name COLLATE NOCASE"
    ))?;
    let rows = stmt.query_map(params![group_id], row_to_monitor)?;
    rows.collect()
}

/// Ids of every monitor below `id` (children, grandchildren, …).
pub fn descendant_ids(conn: &Connection, id: i64) -> rusqlite::Result<Vec<i64>> {
    let mut stmt = conn.prepare(
        "WITH RECURSIVE d(id) AS (
             SELECT id FROM monitors WHERE parent_id = ?1
             UNION SELECT m.id FROM monitors m JOIN d ON m.parent_id = d.id
         ) SELECT id FROM d",
    )?;
    let rows = stmt.query_map(params![id], |r| r.get(0))?;
    rows.collect()
}

/// Sets (or clears) a monitor's push token.
pub fn set_push_token(conn: &Connection, id: i64, token: Option<&str>) -> rusqlite::Result<bool> {
    let n = conn.execute(
        "UPDATE monitors SET push_token = ?2, updated_at = datetime('now') WHERE id = ?1",
        params![id, token],
    )?;
    Ok(n > 0)
}

/// A row of `monitor_expiry` (TLS certificate or domain registration).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExpiryRecord {
    /// `cert` | `domain`.
    pub kind: String,
    pub subject: Option<String>,
    pub issuer: Option<String>,
    /// UTC `YYYY-MM-DD HH:MM:SS`.
    pub expires_at: String,
    pub checked_at: String,
    /// Whole days until expiry (negative once expired).
    pub days_left: i64,
}

/// Known expiry dates of a monitor (cert first, then domain).
pub fn expiry_records(conn: &Connection, monitor_id: i64) -> rusqlite::Result<Vec<ExpiryRecord>> {
    let mut stmt = conn.prepare(
        "SELECT kind, subject, issuer, expires_at, checked_at,
                julianday(expires_at) - julianday('now') AS days_left
         FROM monitor_expiry WHERE monitor_id = ?1 ORDER BY kind",
    )?;
    let rows = stmt.query_map(params![monitor_id], |r| {
        Ok(ExpiryRecord {
            kind: r.get(0)?,
            subject: r.get(1)?,
            issuer: r.get(2)?,
            expires_at: r.get(3)?,
            checked_at: r.get(4)?,
            days_left: r
                .get::<_, Option<f64>>(5)?
                .map(|d| d.floor() as i64)
                .unwrap_or(0),
        })
    })?;
    rows.collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::test_conn;

    fn full_monitor() -> NewMonitor {
        NewMonitor {
            name: "API".into(),
            description: "public api".into(),
            monitor_type: MonitorType::JsonQuery,
            target: "https://api.example.com/health".into(),
            retry_interval_seconds: Some(15),
            upside_down: true,
            http_method: "POST".into(),
            http_headers: Some(r#"{"X-Key":"1"}"#.into()),
            http_body: Some("{}".into()),
            accepted_status_codes: "200-299".into(),
            ignore_tls: true,
            basic_auth_user: Some("u".into()),
            basic_auth_pass: Some("p".into()),
            json_path: Some("$.status".into()),
            json_operator: "!=".into(),
            json_expected: Some("down".into()),
            push_token: Some("tok".into()),
            resend_interval: 3,
            escalation_minutes: 10,
            run_on_agents: true,
            source: "config".into(),
            external_id: Some("api".into()),
            sort_order: 4,
            ..NewMonitor::default()
        }
    }

    #[test]
    fn insert_then_get_round_trips_every_field() {
        let conn = test_conn();
        let new = full_monitor();
        let id = insert_monitor(&conn, &new).unwrap();
        let got = get_monitor(&conn, id).unwrap().unwrap();
        assert_eq!(got.to_new(), new);
    }

    #[test]
    fn update_overwrites_every_field() {
        let conn = test_conn();
        let id = insert_monitor(&conn, &NewMonitor::default()).unwrap();
        let new = full_monitor();
        update_monitor(&conn, id, &new).unwrap();
        assert_eq!(get_monitor(&conn, id).unwrap().unwrap().to_new(), new);
    }

    #[test]
    fn insert_attaches_default_channels_only() {
        let conn = test_conn();
        conn.execute_batch(
            "INSERT INTO notification_channels (id, name, kind, config, is_default) VALUES
                 (1, 'a', 'discord', '{}', 1), (2, 'b', 'slack', '{}', 0);",
        )
        .unwrap();
        let id = insert_monitor(&conn, &NewMonitor::default()).unwrap();
        let linked: Vec<i64> = conn
            .prepare("SELECT channel_id FROM monitor_notifications WHERE monitor_id = ?1")
            .unwrap()
            .query_map([id], |r| r.get(0))
            .unwrap()
            .collect::<Result<_, _>>()
            .unwrap();
        assert_eq!(linked, vec![1]);
    }

    #[test]
    fn dashboard_query_maps_monitor_and_status() {
        let conn = test_conn();
        let id = insert_monitor(&conn, &full_monitor()).unwrap();
        crate::db::heartbeats::insert(&conn, id, None, HeartbeatStatus::Up, Some(42), None)
            .unwrap();
        let list = list_monitors_with_status(&conn).unwrap();
        assert_eq!(list.len(), 1);
        assert_eq!(list[0].monitor.name, "API");
        assert_eq!(list[0].status, Some(HeartbeatStatus::Up));
        assert_eq!(list[0].response_time_ms, Some(42));
    }

    fn named(conn: &Connection, name: &str, parent: Option<i64>, kind: MonitorType) -> i64 {
        insert_monitor(
            conn,
            &NewMonitor {
                name: name.into(),
                monitor_type: kind,
                parent_id: parent,
                ..NewMonitor::default()
            },
        )
        .unwrap()
    }

    #[test]
    fn dashboard_query_ignores_agent_heartbeats() {
        let conn = test_conn();
        let id = insert_monitor(&conn, &full_monitor()).unwrap();
        conn.execute(
            "INSERT INTO agents (id, name, prefix, token_hash) VALUES (7, 'remote', 'x', 'h')",
            [],
        )
        .unwrap();
        crate::db::heartbeats::insert(&conn, id, None, HeartbeatStatus::Up, Some(10), None)
            .unwrap();
        crate::db::heartbeats::insert(&conn, id, Some(7), HeartbeatStatus::Down, None, None)
            .unwrap();
        let row = &list_monitors_with_status(&conn).unwrap()[0];
        assert_eq!(row.status, Some(HeartbeatStatus::Up));
        assert_eq!(row.uptime_24h, Some(100.0));
        let lean = &list_monitors_with_latest(&conn).unwrap()[0];
        assert_eq!(lean.status, Some(HeartbeatStatus::Up));
        assert_eq!(lean.uptime_24h, None);
        assert_eq!(row.recent.iter().flatten().count(), 1);
        assert_eq!(list_recent_heartbeats(&conn, id, 10).unwrap().len(), 1);
    }

    #[test]
    fn pause_resume_and_bulk_ops() {
        let conn = test_conn();
        let a = named(&conn, "a", None, MonitorType::Http);
        let b = named(&conn, "b", None, MonitorType::Http);
        let c = named(&conn, "c", None, MonitorType::Http);
        assert!(set_active(&conn, a, false).unwrap());
        assert!(!get_monitor(&conn, a).unwrap().unwrap().active);
        assert!(!set_active(&conn, 999, false).unwrap());

        assert_eq!(existing_ids(&conn, &[c, 999, a, c]).unwrap(), vec![c, a]);
        // a is already paused, so only b and c change.
        assert_eq!(
            set_active_many(&conn, &[a, b, c, b], false).unwrap(),
            vec![b, c]
        );
        assert_eq!(list_active_monitors(&conn).unwrap().len(), 0);
        assert_eq!(set_active_many(&conn, &[a], true).unwrap(), vec![a]);

        assert_eq!(delete_many(&conn, &[a, b, 999]).unwrap(), 2);
        let left: Vec<i64> = list_monitors(&conn).unwrap().iter().map(|m| m.id).collect();
        assert_eq!(left, vec![c]);
    }

    #[test]
    fn groups_children_and_descendants() {
        let conn = test_conn();
        let root = named(&conn, "root", None, MonitorType::Group);
        let sub = named(&conn, "sub", Some(root), MonitorType::Group);
        let leaf = named(&conn, "leaf", Some(sub), MonitorType::Http);
        let other = named(&conn, "other", Some(root), MonitorType::Tcp);
        let names: Vec<String> = children(&conn, root)
            .unwrap()
            .into_iter()
            .map(|m| m.name)
            .collect();
        assert_eq!(names, vec!["other", "sub"]);
        let mut desc = descendant_ids(&conn, root).unwrap();
        desc.sort_unstable();
        assert_eq!(desc, vec![sub, leaf, other]);
        assert!(descendant_ids(&conn, leaf).unwrap().is_empty());
        // Deleting a group detaches its children (ON DELETE SET NULL).
        delete_monitor(&conn, sub).unwrap();
        assert_eq!(get_monitor(&conn, leaf).unwrap().unwrap().parent_id, None);
    }

    #[test]
    fn push_token_and_expiry_records() {
        let conn = test_conn();
        let id = named(&conn, "m", None, MonitorType::Push);
        assert!(set_push_token(&conn, id, Some("abc")).unwrap());
        assert_eq!(
            get_monitor(&conn, id)
                .unwrap()
                .unwrap()
                .push_token
                .as_deref(),
            Some("abc")
        );
        conn.execute(
            "INSERT INTO monitor_expiry (monitor_id, kind, subject, issuer, expires_at)
             VALUES (?1, 'cert', 'CN=x', 'R3', datetime('now', '+10 days', '+1 hour'))",
            params![id],
        )
        .unwrap();
        let rec = expiry_records(&conn, id).unwrap();
        assert_eq!(rec.len(), 1);
        assert_eq!((rec[0].kind.as_str(), rec[0].days_left), ("cert", 10));
    }

    #[test]
    fn http_auth_columns_round_trip() {
        let conn = test_conn();
        let m = NewMonitor {
            name: "Secure".into(),
            target: "https://api.example.com/".into(),
            auth_method: "oauth2".into(),
            oauth_token_url: Some("https://login.example.com/token".into()),
            oauth_client_id: Some("vole".into()),
            oauth_client_secret: Some("shh".into()),
            oauth_scope: Some("read write".into()),
            oauth_audience: Some("api".into()),
            oauth_client_auth: "client_secret_post".into(),
            tls_client_cert: Some("CERT".into()),
            tls_client_key: Some("KEY".into()),
            tls_ca_cert: Some("CA".into()),
            ..NewMonitor::default()
        };
        let id = insert_monitor(&conn, &m).unwrap();
        assert_eq!(get_monitor(&conn, id).unwrap().unwrap().to_new(), m);
        let off = NewMonitor {
            auth_method: "none".into(),
            oauth_client_secret: None,
            ..m.clone()
        };
        update_monitor(&conn, id, &off).unwrap();
        assert_eq!(get_monitor(&conn, id).unwrap().unwrap().to_new(), off);
        // New monitors default to no authentication.
        let plain = insert_monitor(
            &conn,
            &NewMonitor {
                name: "Plain".into(),
                ..NewMonitor::default()
            },
        )
        .unwrap();
        let plain = get_monitor(&conn, plain).unwrap().unwrap();
        assert_eq!(
            (plain.auth_method.as_str(), plain.oauth_client_auth.as_str()),
            ("none", "client_secret_basic")
        );
    }

    #[test]
    fn migration_turns_existing_basic_auth_into_the_basic_method() {
        // The 0096 migration on a table that only has what it reads.
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(
            "CREATE TABLE monitors (id INTEGER PRIMARY KEY, basic_auth_user TEXT);
             INSERT INTO monitors (id, basic_auth_user) VALUES (1, 'bot'), (2, NULL), (3, '');",
        )
        .unwrap();
        conn.execute_batch(include_str!("../../migrations/0096_http_auth.sql"))
            .unwrap();
        let methods: Vec<String> = conn
            .prepare("SELECT auth_method FROM monitors ORDER BY id")
            .unwrap()
            .query_map([], |r| r.get(0))
            .unwrap()
            .collect::<Result<_, _>>()
            .unwrap();
        assert_eq!(methods, ["basic", "none", "none"]);
        let style: String = conn
            .query_row(
                "SELECT oauth_client_auth FROM monitors WHERE id = 2",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(style, "client_secret_basic");
    }
}
