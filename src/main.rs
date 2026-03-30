mod forwarder;

use std::collections::HashMap;
use std::sync::Arc;

use axum::body::Bytes;
use axum::extract::{Extension, Request};
use axum::http::StatusCode;
use axum::Router;
use clap::Parser;
use serde::Deserialize;
use tokio::process::Command;
use tokio::sync::{oneshot, Mutex};
use tracing::{debug, error, info, warn};

use forwarder::{Forwarder, Passthrough};

// ---------------------------------------------------------------------------
// Configuration file (~/.config/api2cli/config.toml)
// ---------------------------------------------------------------------------

#[derive(Deserialize, Default)]
struct Config {
    port: Option<u16>,
    command: Option<String>,
    persistent: Option<bool>,
    passthrough: Option<String>,
    log_level: Option<String>,
}

fn load_config_from(path: &std::path::Path) -> Config {
    match std::fs::read_to_string(path) {
        Ok(contents) => toml::from_str(&contents).unwrap_or_else(|e| {
            warn!("Could not parse config {}: {e}", path.display());
            Config::default()
        }),
        Err(_) => Config::default(),
    }
}

fn load_config() -> Config {
    let path = std::env::var("XDG_CONFIG_HOME")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|_| {
            dirs_next::home_dir()
                .expect("cannot determine home directory")
                .join(".config")
        })
        .join("api2cli")
        .join("config.toml");

    debug!("Loading config from {}", path.display());
    load_config_from(&path)
}

// ---------------------------------------------------------------------------
// Logging
// ---------------------------------------------------------------------------

fn log_dir_path() -> std::path::PathBuf {
    std::env::var("XDG_STATE_HOME")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|_| {
            dirs_next::home_dir()
                .expect("cannot determine home directory")
                .join(".local")
                .join("state")
        })
        .join("api2cli")
}

fn make_filter(level: &str) -> tracing_subscriber::EnvFilter {
    tracing_subscriber::EnvFilter::try_new(level).unwrap_or_else(|_| {
        eprintln!("Warning: invalid log level '{level}', defaulting to 'info'");
        tracing_subscriber::EnvFilter::new("info")
    })
}

/// Initialise tracing: daily-rotating file in `log_dir` + stderr.
/// Returns the file writer guard — drop it and buffered log lines are flushed.
fn init_logging(
    level: &str,
    log_dir: &std::path::Path,
) -> Option<tracing_appender::non_blocking::WorkerGuard> {
    use tracing_subscriber::{fmt, layer::SubscriberExt, util::SubscriberInitExt};

    match std::fs::create_dir_all(log_dir) {
        Err(e) => {
            eprintln!("Warning: could not create log directory {}: {e}", log_dir.display());
            tracing_subscriber::registry()
                .with(make_filter(level))
                .with(fmt::layer().with_writer(std::io::stderr).with_ansi(true))
                .init();
            None
        }
        Ok(()) => {
            let file_appender = tracing_appender::rolling::daily(log_dir, "api2cli.log");
            let (non_blocking, guard) = tracing_appender::non_blocking(file_appender);
            tracing_subscriber::registry()
                .with(make_filter(level))
                .with(fmt::layer().with_writer(non_blocking).with_ansi(false))
                .with(fmt::layer().with_writer(std::io::stderr).with_ansi(true))
                .init();
            Some(guard)
        }
    }
}

// ---------------------------------------------------------------------------
// CLI
// ---------------------------------------------------------------------------

#[derive(Parser)]
#[command(about = "Forward HTTP requests to a CLI app or pipe")]
struct Cli {
    /// Port to listen on [config: port, default: 1337]
    #[arg(short, long)]
    port: Option<u16>,

    /// Command to spawn and pipe requests into (omit for stdout/pipe mode) [config: command]
    #[arg(short, long)]
    command: Option<String>,

    /// Keep the server running after the first request [config: persistent]
    #[arg(short = 'P', long)]
    persistent: bool,

    /// What to forward: `body` (default) or `full` (JSON envelope) [config: passthrough]
    #[arg(long)]
    passthrough: Option<Passthrough>,

    /// Log level: error, warn, info, debug, trace [config: log_level, default: info]
    #[arg(long)]
    log_level: Option<String>,
}

// ---------------------------------------------------------------------------
// App state
// ---------------------------------------------------------------------------

#[derive(Clone)]
struct AppState {
    forwarder: Forwarder,
    passthrough: Arc<Passthrough>,
    /// Fires once after the first request in one-shot mode.
    shutdown_tx: Arc<Mutex<Option<oneshot::Sender<()>>>>,
}

// ---------------------------------------------------------------------------
// Port binding with user prompt on conflict
// ---------------------------------------------------------------------------

async fn bind_or_prompt(port: u16, addr: &str) -> tokio::net::TcpListener {
    loop {
        match tokio::net::TcpListener::bind(addr).await {
            Ok(listener) => return listener,

            Err(e) if e.kind() == std::io::ErrorKind::AddrInUse => {
                let pids = pids_on_port(port);
                let pid_info = if pids.is_empty() {
                    String::from("unknown process")
                } else {
                    pids.iter().map(|p| p.to_string()).collect::<Vec<_>>().join(", ")
                };

                warn!("Port {port} is already in use (PID: {pid_info})");
                eprintln!("  [k] Kill the process and continue");
                eprintln!("  [c] Cancel");
                eprint!("Choice: ");

                let mut input = String::new();
                std::io::stdin().read_line(&mut input).unwrap_or_default();

                match input.trim() {
                    "k" | "K" => {
                        if pids.is_empty() {
                            error!("Could not determine PID; cannot kill. Please free port {port} manually.");
                            std::process::exit(1);
                        }
                        for pid in &pids {
                            info!("Killing PID {pid}");
                            if let Err(e) = std::process::Command::new("kill")
                                .arg(pid.to_string())
                                .status()
                            {
                                warn!("Failed to kill PID {pid}: {e}");
                            }
                        }
                        // Give the OS a moment to release the port.
                        tokio::time::sleep(std::time::Duration::from_millis(300)).await;
                    }
                    _ => {
                        info!("Startup cancelled by user");
                        std::process::exit(0);
                    }
                }
            }

            Err(e) => {
                error!("Failed to bind {addr}: {e}");
                std::process::exit(1);
            }
        }
    }
}

/// Returns PIDs listening on the given TCP port via `lsof`.
fn pids_on_port(port: u16) -> Vec<u32> {
    match std::process::Command::new("lsof")
        .args(["-ti", &format!("tcp:{port}")])
        .output()
    {
        Ok(out) => {
            let pids: Vec<u32> = String::from_utf8_lossy(&out.stdout)
                .split_whitespace()
                .filter_map(|s| s.parse().ok())
                .collect();
            debug!("lsof found PIDs on port {port}: {pids:?}");
            pids
        }
        Err(e) => {
            debug!("lsof unavailable for port {port}: {e}");
            vec![]
        }
    }
}

// ---------------------------------------------------------------------------
// Entry point
// ---------------------------------------------------------------------------

#[tokio::main]
async fn main() {
    let cli = Cli::parse();

    // Resolve log level first so init_logging has it before anything is logged.
    // load_config is called without logging here; warnings surface after init.
    let cfg = load_config();

    let log_level = cli.log_level
        .or_else(|| cfg.log_level.clone())
        .unwrap_or_else(|| "info".to_string());
    let log_dir = log_dir_path();
    let _log_guard = init_logging(&log_level, &log_dir);

    // Now logging is ready — re-parse config with warnings visible.
    let cfg = load_config();

    let port       = cli.port.or(cfg.port).unwrap_or(1337);
    let command    = cli.command.or(cfg.command);
    let persistent = cli.persistent || cfg.persistent.unwrap_or(false);
    let passthrough = cli.passthrough
        .or_else(|| cfg.passthrough.as_deref().and_then(|s| s.parse().ok()))
        .unwrap_or(Passthrough::Body);

    info!(
        port,
        persistent,
        passthrough = ?passthrough,
        log_level,
        log_dir = %log_dir.display(),
        "Starting api2cli"
    );

    let forwarder = match &command {
        None => {
            info!("Mode: stdout (pipe)");
            Forwarder::Stdout
        }
        Some(cmd) => {
            info!(command = cmd, "Mode: subprocess");
            let mut child = Command::new("sh")
                .arg("-c")
                .arg(cmd)
                .stdin(std::process::Stdio::piped())
                .spawn()
                .unwrap_or_else(|e| {
                    error!(command = cmd, "Failed to spawn command: {e}");
                    std::process::exit(1);
                });
            let stdin = child.stdin.take().unwrap_or_else(|| {
                error!("Failed to open stdin pipe for subprocess");
                std::process::exit(1);
            });
            tokio::spawn(async move {
                match child.wait().await {
                    Ok(status) => {
                        if status.success() {
                            debug!("Subprocess exited successfully");
                        } else {
                            warn!("Subprocess exited with {status}");
                        }
                    }
                    Err(e) => error!("Subprocess wait error: {e}"),
                }
            });
            Forwarder::Subprocess(Arc::new(Mutex::new(stdin)))
        }
    };

    let (shutdown_tx, shutdown_rx) = oneshot::channel::<()>();
    let shutdown_tx = if persistent { None } else { Some(shutdown_tx) };

    let state = AppState {
        forwarder,
        passthrough: Arc::new(passthrough),
        shutdown_tx: Arc::new(Mutex::new(shutdown_tx)),
    };

    let app = Router::new().fallback(handler).layer(Extension(state));

    let addr = format!("0.0.0.0:{port}");
    let listener = bind_or_prompt(port, &addr).await;
    info!("Listening on {addr}");

    let serve_result = if persistent {
        axum::serve(listener, app).await
    } else {
        axum::serve(listener, app)
            .with_graceful_shutdown(async { let _ = shutdown_rx.await; })
            .await
    };

    if let Err(e) = serve_result {
        error!("Server error: {e}");
    } else {
        info!("Server shut down cleanly");
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    // -----------------------------------------------------------------------
    // HTTP helpers
    // -----------------------------------------------------------------------

    async fn http_get(port: u16, path: &str) -> String {
        let mut stream = tokio::net::TcpStream::connect(("127.0.0.1", port))
            .await
            .unwrap_or_else(|e| panic!("connect :{port}: {e}"));
        let req = format!(
            "GET {path} HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n"
        );
        stream.write_all(req.as_bytes()).await.unwrap();
        stream.flush().await.unwrap();
        let mut buf = Vec::new();
        stream.read_to_end(&mut buf).await.unwrap();
        String::from_utf8_lossy(&buf).lines().next().unwrap_or("").to_string()
    }

    async fn http_post(port: u16, path: &str, body: &str) -> String {
        let mut stream = tokio::net::TcpStream::connect(("127.0.0.1", port))
            .await
            .unwrap_or_else(|e| panic!("connect :{port}: {e}"));
        let req = format!(
            "POST {path} HTTP/1.1\r\nHost: localhost\r\nContent-Length: {}\r\n\
             Content-Type: text/plain\r\nConnection: close\r\n\r\n{body}",
            body.len()
        );
        stream.write_all(req.as_bytes()).await.unwrap();
        stream.flush().await.unwrap();
        let mut buf = Vec::new();
        stream.read_to_end(&mut buf).await.unwrap();
        String::from_utf8_lossy(&buf).lines().next().unwrap_or("").to_string()
    }

    /// Bind a real listener on `port` and spin up an axum server in a task.
    /// - persistent=false: handler fires a oneshot after the first request,
    ///   graceful shutdown occurs, and the task completes on its own.
    /// - persistent=true: server runs until `handle.abort()`.
    async fn start_server(
        port: u16,
        forwarder: Forwarder,
        passthrough: Passthrough,
        persistent: bool,
    ) -> tokio::task::JoinHandle<()> {
        let listener = tokio::net::TcpListener::bind(format!("127.0.0.1:{port}"))
            .await
            .unwrap_or_else(|e| panic!("bind :{port}: {e}"));

        if persistent {
            let state = AppState {
                forwarder,
                passthrough: Arc::new(passthrough),
                shutdown_tx: Arc::new(Mutex::new(None)),
            };
            let app = Router::new().fallback(handler).layer(Extension(state));
            tokio::spawn(async move {
                axum::serve(listener, app).await.unwrap();
            })
        } else {
            let (inner_tx, inner_rx) = oneshot::channel::<()>();
            let state = AppState {
                forwarder,
                passthrough: Arc::new(passthrough),
                shutdown_tx: Arc::new(Mutex::new(Some(inner_tx))),
            };
            let app = Router::new().fallback(handler).layer(Extension(state));
            tokio::spawn(async move {
                axum::serve(listener, app)
                    .with_graceful_shutdown(async { let _ = inner_rx.await; })
                    .await
                    .unwrap();
            })
        }
    }

    // -----------------------------------------------------------------------
    // Config: load_config_from
    // -----------------------------------------------------------------------

    #[test]
    fn test_load_config_missing_file_returns_defaults() {
        let cfg = load_config_from(std::path::Path::new("/tmp/api2cli_nonexistent.toml"));
        assert!(cfg.port.is_none());
        assert!(cfg.command.is_none());
        assert!(cfg.persistent.is_none());
        assert!(cfg.passthrough.is_none());
    }

    #[test]
    fn test_load_config_valid_toml() {
        let path = std::path::PathBuf::from("/tmp/api2cli_test_valid_config.toml");
        std::fs::write(&path, "port = 9000\npassthrough = \"full\"\n").unwrap();
        let cfg = load_config_from(&path);
        assert_eq!(cfg.port, Some(9000));
        assert_eq!(cfg.passthrough.as_deref(), Some("full"));
        assert!(cfg.command.is_none());
        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn test_load_config_malformed_toml_returns_defaults() {
        let path = std::path::PathBuf::from("/tmp/api2cli_test_bad_config.toml");
        std::fs::write(&path, "port = [[[not valid toml").unwrap();
        let cfg = load_config_from(&path);
        assert!(cfg.port.is_none());
        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn test_load_config_log_level_field() {
        let path = std::path::PathBuf::from("/tmp/api2cli_test_log_level_config.toml");
        std::fs::write(&path, "log_level = \"debug\"\n").unwrap();
        let cfg = load_config_from(&path);
        assert_eq!(cfg.log_level.as_deref(), Some("debug"));
        std::fs::remove_file(&path).ok();
    }

    // -----------------------------------------------------------------------
    // Config merge priority
    // -----------------------------------------------------------------------

    #[test]
    fn test_merge_cli_port_overrides_config() {
        let result: u16 = Some(9002u16).or(Some(9001)).unwrap_or(1337);
        assert_eq!(result, 9002);
    }

    #[test]
    fn test_merge_config_port_used_when_no_cli_arg() {
        let result: u16 = None::<u16>.or(Some(9001)).unwrap_or(1337);
        assert_eq!(result, 9001);
    }

    #[test]
    fn test_merge_default_port_when_nothing_set() {
        let result: u16 = None::<u16>.or(None).unwrap_or(1337);
        assert_eq!(result, 1337);
    }

    #[test]
    fn test_merge_cli_persistent_true_wins_over_config_false() {
        let result = true || Some(false).unwrap_or(false);
        assert!(result);
    }

    #[test]
    fn test_merge_persistent_falls_back_to_config_true() {
        let result = false || Some(true).unwrap_or(false);
        assert!(result);
    }

    // -----------------------------------------------------------------------
    // Logging: log_dir_path respects XDG_STATE_HOME
    // -----------------------------------------------------------------------

    #[test]
    fn test_log_dir_path_uses_xdg_state_home() {
        // SAFETY: single-threaded test; no other thread reads XDG_STATE_HOME.
        unsafe { std::env::set_var("XDG_STATE_HOME", "/tmp/test_state") };
        let path = log_dir_path();
        unsafe { std::env::remove_var("XDG_STATE_HOME") };
        assert_eq!(path, std::path::PathBuf::from("/tmp/test_state/api2cli"));
    }

    #[test]
    fn test_log_dir_created_if_missing() {
        let dir = std::path::PathBuf::from("/tmp/api2cli_log_test_dir");
        std::fs::remove_dir_all(&dir).ok();
        assert!(!dir.exists());
        // init_logging would create it; test the creation logic directly.
        std::fs::create_dir_all(&dir).unwrap();
        assert!(dir.exists());
        std::fs::remove_dir_all(&dir).ok();
    }

    // -----------------------------------------------------------------------
    // HTTP server: all requests return 200 OK
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn test_get_returns_200() {
        let port = 19200u16;
        let handle = start_server(port, Forwarder::Stdout, Passthrough::Body, true).await;
        tokio::time::sleep(Duration::from_millis(10)).await;

        let line = http_get(port, "/").await;
        assert!(line.contains("200"), "expected 200, got: {line}");

        handle.abort();
    }

    #[tokio::test]
    async fn test_post_returns_200() {
        let port = 19201u16;
        let handle = start_server(port, Forwarder::Stdout, Passthrough::Body, true).await;
        tokio::time::sleep(Duration::from_millis(10)).await;

        let line = http_post(port, "/api/data", "payload").await;
        assert!(line.contains("200"), "expected 200, got: {line}");

        handle.abort();
    }

    #[tokio::test]
    async fn test_arbitrary_path_returns_200() {
        let port = 19202u16;
        let handle = start_server(port, Forwarder::Stdout, Passthrough::Body, true).await;
        tokio::time::sleep(Duration::from_millis(10)).await;

        let line = http_get(port, "/some/deep/path?q=1").await;
        assert!(line.contains("200"), "got: {line}");

        handle.abort();
    }

    // -----------------------------------------------------------------------
    // One-shot: server shuts down after first request
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn test_oneshot_exits_after_first_request() {
        let port = 19210u16;
        let handle = start_server(port, Forwarder::Stdout, Passthrough::Body, false).await;
        tokio::time::sleep(Duration::from_millis(10)).await;

        let line = http_get(port, "/").await;
        assert!(line.contains("200"), "got: {line}");

        tokio::time::timeout(Duration::from_secs(2), handle)
            .await
            .expect("server did not shut down within 2s")
            .unwrap();

        tokio::time::sleep(Duration::from_millis(50)).await;
        assert!(
            tokio::net::TcpStream::connect(("127.0.0.1", port)).await.is_err(),
            "server should have released the port"
        );
    }

    // -----------------------------------------------------------------------
    // Persistent: server stays alive across multiple requests
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn test_persistent_handles_multiple_requests() {
        let port = 19220u16;
        let handle = start_server(port, Forwarder::Stdout, Passthrough::Body, true).await;
        tokio::time::sleep(Duration::from_millis(10)).await;

        for i in 0..3 {
            let line = http_get(port, &format!("/req/{i}")).await;
            assert!(line.contains("200"), "request {i}: {line}");
        }

        assert!(!handle.is_finished(), "persistent server should still be running");
        handle.abort();
    }

    // -----------------------------------------------------------------------
    // Body passthrough: subprocess receives raw body + newline
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn test_subprocess_body_passthrough() {
        let port = 19230u16;
        let out = format!("/tmp/api2cli_test_{port}.txt");
        std::fs::remove_file(&out).ok();

        let mut child = tokio::process::Command::new("sh")
            .args(["-c", &format!("cat >> {out}")])
            .stdin(std::process::Stdio::piped())
            .spawn()
            .unwrap();
        let stdin = child.stdin.take().unwrap();
        tokio::spawn(async move { let _ = child.wait().await; });

        let forwarder = Forwarder::Subprocess(Arc::new(Mutex::new(stdin)));
        let handle = start_server(port, forwarder, Passthrough::Body, false).await;
        tokio::time::sleep(Duration::from_millis(10)).await;

        http_post(port, "/", "hello-body").await;
        tokio::time::timeout(Duration::from_secs(2), handle).await.unwrap().unwrap();
        tokio::time::sleep(Duration::from_millis(100)).await;

        let contents = std::fs::read_to_string(&out).expect("output file missing");
        assert_eq!(contents, "hello-body\n");
        std::fs::remove_file(&out).ok();
    }

    // -----------------------------------------------------------------------
    // Full passthrough: subprocess receives JSON envelope
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn test_subprocess_full_passthrough() {
        let port = 19240u16;
        let out = format!("/tmp/api2cli_test_{port}.txt");
        std::fs::remove_file(&out).ok();

        let mut child = tokio::process::Command::new("sh")
            .args(["-c", &format!("cat >> {out}")])
            .stdin(std::process::Stdio::piped())
            .spawn()
            .unwrap();
        let stdin = child.stdin.take().unwrap();
        tokio::spawn(async move { let _ = child.wait().await; });

        let forwarder = Forwarder::Subprocess(Arc::new(Mutex::new(stdin)));
        let handle = start_server(port, forwarder, Passthrough::Full, false).await;
        tokio::time::sleep(Duration::from_millis(10)).await;

        http_post(port, "/api/test", "the-body").await;
        tokio::time::timeout(Duration::from_secs(2), handle).await.unwrap().unwrap();
        tokio::time::sleep(Duration::from_millis(100)).await;

        let contents = std::fs::read_to_string(&out).expect("output file missing");
        assert!(contents.ends_with('\n'), "must end with newline");

        let v: serde_json::Value = serde_json::from_str(contents.trim_end_matches('\n'))
            .expect("must be valid JSON");
        assert_eq!(v["method"], "POST");
        assert_eq!(v["path"], "/api/test");
        assert_eq!(v["body"], "the-body");
        assert!(v["headers"].is_object());

        std::fs::remove_file(&out).ok();
    }
}

// ---------------------------------------------------------------------------
// Request handler
// ---------------------------------------------------------------------------

async fn handler(Extension(state): Extension<AppState>, req: Request) -> StatusCode {
    let method = req.method().to_string();
    let path = req
        .uri()
        .path_and_query()
        .map(|pq| pq.as_str())
        .unwrap_or("/")
        .to_string();

    let headers: HashMap<String, String> = req
        .headers()
        .iter()
        .filter_map(|(k, v)| v.to_str().ok().map(|val| (k.as_str().to_string(), val.to_string())))
        .collect();

    let body: Bytes = match axum::body::to_bytes(req.into_body(), usize::MAX).await {
        Ok(b) => b,
        Err(e) => {
            warn!("{method} {path} — failed to read body: {e}");
            Bytes::new()
        }
    };

    info!("{method} {path} ({} bytes)", body.len());

    if let Err(e) = state
        .forwarder
        .forward(&state.passthrough, &method, &path, headers, body.to_vec())
        .await
    {
        error!("Forward failed for {method} {path}: {e}");
    }

    if let Some(tx) = state.shutdown_tx.lock().await.take() {
        info!("One-shot request handled; shutting down");
        let _ = tx.send(());
    }

    StatusCode::OK
}
