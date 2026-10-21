//! `wdmm-server` — standalone binary that starts the WDMM OpenAPI server.
//!
//! Usage: `wdmm-server [--port <PORT>] [--bind global|local|<IP>]`
//!
//! Examples (from docs/openapi-smoke-test.md):
//! ```sh
//! wdmm-server
//! curl http://127.0.0.1:3000/health
//! curl -H "Authorization: Bearer <token>" http://127.0.0.1:3000/v1/accounts
//! ```

use std::path::Path;
use std::sync::Arc;
use tracing_subscriber::EnvFilter;
use wdmm_openapi::{ApiServer, BindMode, ServerConfig};
use wdmm_queue::TaskQueue;
use wdmm_storage::LibraryDatabase;

#[tokio::main]
async fn main() {
    tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::from_default_env().add_directive("info".parse().unwrap()))
        .init();

    // Simple arg parsing (no external deps)
    let args: Vec<String> = std::env::args().collect();
    let port: u16 = get_arg(&args, "--port").and_then(|p| p.parse().ok()).unwrap_or(3000);
    let bind_str = get_arg(&args, "--bind").unwrap_or_else(|| "local".into());
    let bind_mode = match bind_str.as_str() {
        "global" | "0.0.0.0" => BindMode::Global,
        "local" | "127.0.0.1" => BindMode::Local,
        _ => BindMode::Custom,
    };

    // In-memory DB for smoke-test; real deployments pass a DB path via env.
    let db_path = std::env::var("WDMM_DB_PATH").unwrap_or_else(|_| ":memory:".into());
    let db = Arc::new(
        LibraryDatabase::open(Path::new(&db_path))
            .expect("failed to open library database"),
    );
    let queue = Arc::new(TaskQueue::new());

    let config = ServerConfig {
        bind_mode,
        port,
        token: uuid::Uuid::new_v4().to_string(),
    };

    println!("Starting WDMM OpenAPI server on {}:{}", config.bind_mode.to_addr(None), port);
    println!("Bearer token: {}", config.token);
    println!();
    println!("Smoke-test examples:");
    println!("  curl http://127.0.0.1:{port}/health");
    println!("  curl -H 'Authorization: Bearer {}' http://127.0.0.1:{port}/v1/accounts", config.token);
    println!("  curl -H 'Authorization: Bearer {}' http://127.0.0.1:{port}/v1/tasks", config.token);

    let server = ApiServer::start(config, db, queue)
        .await
        .expect("failed to start API server");

    println!("Server listening on port {}", server.port());

    // Run until Ctrl-C
    tokio::signal::ctrl_c().await.expect("signal handler failed");
    println!("Shutting down…");
    server.stop().await;
}

fn get_arg(args: &[String], flag: &str) -> Option<String> {
    let pos = args.iter().position(|a| a == flag)?;
    args.get(pos + 1).cloned()
}
