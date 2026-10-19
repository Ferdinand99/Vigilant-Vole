//! Docker container checker (bollard).
//!
//! - `target`: container name or id.
//! - `docker_host`: empty = local default (`/var/run/docker.sock` or
//!   `DOCKER_HOST` on Unix, `//./pipe/docker_engine` on Windows);
//!   `unix:///path/docker.sock`, a bare `/path`, `npipe:////./pipe/name`, or
//!   `tcp://host:2375` / `http://host:2375`. TLS (`https://`, port 2376) is
//!   not supported — put a socket proxy in front instead.
//!
//! Up when the container is running and, if it has a healthcheck, healthy.
//! Health "starting" returns `Pending`. Message: state · health · restarts.

use std::collections::HashMap;
use std::sync::{Mutex, OnceLock};
use std::time::Instant;

use bollard::models::{ContainerInspectResponse, ContainerStateStatusEnum, HealthStatusEnum};
use bollard::{API_DEFAULT_VERSION, Docker};

use super::util;
use crate::db::models::{CheckOutcome, HeartbeatStatus, Monitor};

/// Seconds bollard waits per request; the monitor timeout still bounds the check.
const CLIENT_TIMEOUT_SECS: u64 = 60;

fn client_for(host: &str) -> Result<Docker, String> {
    static CLIENTS: OnceLock<Mutex<HashMap<String, Docker>>> = OnceLock::new();
    let cache = CLIENTS.get_or_init(|| Mutex::new(HashMap::new()));
    if let Ok(map) = cache.lock()
        && let Some(client) = map.get(host)
    {
        return Ok(client.clone());
    }
    let docker = connect(host)?;
    if let Ok(mut map) = cache.lock() {
        map.insert(host.to_string(), docker.clone());
    }
    Ok(docker)
}

fn connect(host: &str) -> Result<Docker, String> {
    let fail = |e: bollard::errors::Error| {
        format!("Cannot connect to Docker at {}: {e}", display_host(host))
    };
    if host.is_empty() {
        return Docker::connect_with_local_defaults().map_err(fail);
    }
    if host.starts_with("tcp://") || host.starts_with("http://") {
        return Docker::connect_with_http(host, CLIENT_TIMEOUT_SECS, API_DEFAULT_VERSION)
            .map_err(fail);
    }
    if host.starts_with("https://") {
        return Err("TLS Docker hosts (https://) are not supported; use tcp:// behind a socket proxy, or unix://".into());
    }
    if host.starts_with("unix://") || host.starts_with('/') {
        #[cfg(unix)]
        {
            return Docker::connect_with_unix(host, CLIENT_TIMEOUT_SECS, API_DEFAULT_VERSION)
                .map_err(fail);
        }
        #[cfg(not(unix))]
        {
            return Err("unix:// Docker sockets are not available on this platform".into());
        }
    }
    if host.starts_with("npipe://") {
        #[cfg(windows)]
        {
            return Docker::connect_with_named_pipe(host, CLIENT_TIMEOUT_SECS, API_DEFAULT_VERSION)
                .map_err(fail);
        }
        #[cfg(not(windows))]
        {
            return Err("npipe:// Docker hosts are only available on Windows".into());
        }
    }
    Err(format!(
        "Unsupported Docker host \"{host}\" (use unix:///var/run/docker.sock or tcp://host:2375)"
    ))
}

fn display_host(host: &str) -> &str {
    if host.is_empty() {
        "the local socket"
    } else {
        host
    }
}

pub async fn check(monitor: &Monitor) -> CheckOutcome {
    let name = monitor.target.trim().trim_start_matches('/');
    if name.is_empty() {
        return CheckOutcome::down("No container name configured");
    }
    let host = monitor.docker_host.as_deref().map(str::trim).unwrap_or("");
    let docker = match client_for(host) {
        Ok(d) => d,
        Err(e) => return CheckOutcome::down(e),
    };
    let start = Instant::now();
    match docker.inspect_container(name, None).await {
        Ok(info) => {
            let (status, message) = evaluate(&info);
            CheckOutcome {
                status,
                response_time_ms: Some(util::elapsed_ms(start)),
                message: Some(message),
                expiry: None,
            }
        }
        Err(bollard::errors::Error::DockerResponseServerError {
            status_code: 404, ..
        }) => CheckOutcome::down(format!("Container \"{name}\" not found")),
        Err(bollard::errors::Error::DockerResponseServerError {
            status_code,
            message,
        }) => CheckOutcome::down(format!(
            "Docker API error {status_code}: {}",
            util::truncate(&message, 120)
        )),
        Err(err) => CheckOutcome::down(format!(
            "Cannot reach Docker at {}: {}",
            display_host(host),
            util::root_cause(&err)
        )),
    }
}

/// Decide the status of an inspected container. Pure, for testing.
pub fn evaluate(info: &ContainerInspectResponse) -> (HeartbeatStatus, String) {
    let state = info.state.clone().unwrap_or_default();
    let restarts = info.restart_count.unwrap_or(0);
    let restarts_text = match restarts {
        0 => String::new(),
        1 => " · 1 restart".to_string(),
        n => format!(" · {n} restarts"),
    };
    let status = state.status.unwrap_or(ContainerStateStatusEnum::EMPTY);
    // A paused container reports running=true, so the status must agree.
    let running = state.running.unwrap_or(false)
        && matches!(
            status,
            ContainerStateStatusEnum::RUNNING | ContainerStateStatusEnum::EMPTY
        );

    if !running {
        let label = match status {
            ContainerStateStatusEnum::EMPTY => "not running".to_string(),
            other => other.to_string(),
        };
        let code = match (status, state.exit_code) {
            (ContainerStateStatusEnum::EXITED | ContainerStateStatusEnum::DEAD, Some(code)) => {
                format!(" (exit code {code})")
            }
            _ => String::new(),
        };
        let error = state
            .error
            .as_deref()
            .filter(|e| !e.is_empty())
            .map(|e| format!(": {}", util::truncate(e, 100)))
            .unwrap_or_default();
        return (
            HeartbeatStatus::Down,
            format!("Container {label}{code}{error}{restarts_text}"),
        );
    }

    let health = state.health.as_ref();
    match health.and_then(|h| h.status) {
        Some(HealthStatusEnum::HEALTHY) => (
            HeartbeatStatus::Up,
            format!("running · healthy{restarts_text}"),
        ),
        Some(HealthStatusEnum::STARTING) => (
            HeartbeatStatus::Pending,
            format!("running · health: starting{restarts_text}"),
        ),
        Some(HealthStatusEnum::UNHEALTHY) => {
            let streak = health
                .and_then(|h| h.failing_streak)
                .filter(|s| *s > 0)
                .map(|s| format!(" ({s} failed checks)"))
                .unwrap_or_default();
            let output = health
                .and_then(|h| h.log.as_ref())
                .and_then(|log| log.last())
                .and_then(|r| r.output.as_deref())
                .map(|o| util::snippet(o, 100))
                .filter(|o| !o.is_empty())
                .map(|o| format!(": {o}"))
                .unwrap_or_default();
            (
                HeartbeatStatus::Down,
                format!("running · unhealthy{streak}{output}{restarts_text}"),
            )
        }
        // No healthcheck configured.
        _ => (HeartbeatStatus::Up, format!("running{restarts_text}")),
    }
}

#[cfg(test)]
mod tests {
    use bollard::models::{ContainerState, Health, HealthcheckResult};

    use super::*;

    fn info(state: ContainerState, restarts: i64) -> ContainerInspectResponse {
        ContainerInspectResponse {
            state: Some(state),
            restart_count: Some(restarts),
            ..Default::default()
        }
    }

    fn running(health: Option<Health>) -> ContainerState {
        ContainerState {
            status: Some(ContainerStateStatusEnum::RUNNING),
            running: Some(true),
            health,
            ..Default::default()
        }
    }

    #[test]
    fn running_without_healthcheck_is_up() {
        assert_eq!(
            evaluate(&info(running(None), 0)),
            (HeartbeatStatus::Up, "running".into())
        );
        assert_eq!(
            evaluate(&info(running(None), 2)),
            (HeartbeatStatus::Up, "running · 2 restarts".into())
        );
    }

    #[test]
    fn healthy_and_starting() {
        let h = |s| {
            Some(Health {
                status: Some(s),
                ..Default::default()
            })
        };
        assert_eq!(
            evaluate(&info(running(h(HealthStatusEnum::HEALTHY)), 0)).0,
            HeartbeatStatus::Up
        );
        assert_eq!(
            evaluate(&info(running(h(HealthStatusEnum::STARTING)), 0)),
            (
                HeartbeatStatus::Pending,
                "running · health: starting".into()
            )
        );
    }

    #[test]
    fn unhealthy_shows_last_output() {
        let health = Health {
            status: Some(HealthStatusEnum::UNHEALTHY),
            failing_streak: Some(3),
            log: Some(vec![HealthcheckResult {
                output: Some("curl: (7) refused\n".into()),
                ..Default::default()
            }]),
        };
        assert_eq!(
            evaluate(&info(running(Some(health)), 1)),
            (
                HeartbeatStatus::Down,
                "running · unhealthy (3 failed checks): curl: (7) refused · 1 restart".into()
            )
        );
    }

    #[test]
    fn exited_reports_code() {
        let state = ContainerState {
            status: Some(ContainerStateStatusEnum::EXITED),
            running: Some(false),
            exit_code: Some(137),
            ..Default::default()
        };
        assert_eq!(
            evaluate(&info(state, 0)),
            (
                HeartbeatStatus::Down,
                "Container exited (exit code 137)".into()
            )
        );
    }

    #[test]
    fn paused_is_down() {
        let state = ContainerState {
            status: Some(ContainerStateStatusEnum::PAUSED),
            running: Some(true),
            paused: Some(true),
            ..Default::default()
        };
        assert_eq!(
            evaluate(&info(state, 0)),
            (HeartbeatStatus::Down, "Container paused".into())
        );
    }

    #[test]
    fn bad_hosts_rejected() {
        assert!(
            connect("https://x:2376")
                .unwrap_err()
                .contains("not supported")
        );
        assert!(
            connect("ftp://x")
                .unwrap_err()
                .starts_with("Unsupported Docker host")
        );
    }
}
