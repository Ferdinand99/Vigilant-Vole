//! A machine without CA certificates (scratch/distroless images) must not
//! crash the binary: it warns and uses the bundled roots.

use std::io::{BufRead, BufReader};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

const BIN: &str = env!("CARGO_BIN_EXE_vigilant-vole");

/// Points the TLS libraries at a directory with no certificates.
fn without_system_certs(cmd: &mut Command, dir: &std::path::Path) {
    cmd.env("SSL_CERT_FILE", dir.join("none.pem"))
        .env("SSL_CERT_DIR", dir.join("none"))
        .env("RUST_LOG", "info")
        .env("NO_COLOR", "1");
}

#[test]
fn the_agent_starts_and_warns_without_ca_certificates() {
    let dir = std::env::temp_dir().join(format!("vv-nocerts-agent-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let mut cmd = Command::new(BIN);
    cmd.args(["agent", "--server", "https://127.0.0.1:1", "--token", "vva_x", "--once"]);
    without_system_certs(&mut cmd, &dir);
    let out = cmd.output().expect("run the binary");
    let text = format!("{}{}", String::from_utf8_lossy(&out.stdout), String::from_utf8_lossy(&out.stderr));
    assert!(!text.contains("panicked"), "{text}");
    assert!(!text.contains("No CA certificates were loaded"), "{text}");
    assert!(text.contains("no CA certificates found"), "no warning: {text}");
    // The server is unreachable: a clean error exit, not a crash (101).
    assert_eq!(out.status.code(), Some(1), "{text}");
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn the_server_starts_and_warns_without_ca_certificates() {
    let dir = std::env::temp_dir().join(format!("vv-nocerts-serve-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let port = std::net::TcpListener::bind("127.0.0.1:0").unwrap().local_addr().unwrap().port();
    let mut cmd = Command::new(BIN);
    cmd.arg("serve").env("DATA_DIR", dir.join("data")).env("PORT", port.to_string());
    without_system_certs(&mut cmd, &dir);
    let mut child = cmd.stdout(Stdio::piped()).stderr(Stdio::piped()).spawn().expect("spawn");
    // Logs go to stdout; read until the server says it is listening.
    let stdout = child.stdout.take().unwrap();
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        for line in BufReader::new(stdout).lines().map_while(Result::ok) {
            if tx.send(line).is_err() {
                break;
            }
        }
    });
    let started = Instant::now();
    let mut seen = String::new();
    let mut listening = false;
    while started.elapsed() < Duration::from_secs(30) {
        match rx.recv_timeout(Duration::from_millis(200)) {
            Ok(line) => {
                seen.push_str(&line);
                seen.push('\n');
                if line.contains("listening on") {
                    listening = true;
                    break;
                }
            }
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {
                if child.try_wait().ok().flatten().is_some() {
                    break;
                }
            }
            Err(_) => break,
        }
    }
    let _ = child.kill();
    let _ = child.wait();
    let _ = std::fs::remove_dir_all(&dir);
    assert!(listening, "the server never started: {seen}");
    assert!(seen.contains("no CA certificates found"), "no warning: {seen}");
}
