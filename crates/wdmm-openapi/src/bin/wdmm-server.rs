use axum::{
    routing::get,
    Router,
    Json,
};
use std::net::SocketAddr;
use tokio::net::TcpListener;

#[tokio::main]
async fn main() {
    let app = Router::new()
        .route("/health", get(health_handler))
        .route("/v1/accounts", get(accounts_stub))
        .route("/v1/tracks", get(tracks_stub));

    let addr = SocketAddr::from(([127, 0, 0, 1], 3000));
    println!("Starting WDMM OpenAPI server on {}", addr);
    println!("Try: curl http://127.0.0.1:3000/health");
    println!("     curl http://127.0.0.1:3000/v1/accounts");

    let listener = TcpListener::bind(addr).await.unwrap();
    axum::serve(listener, app).await.unwrap();
}

async fn health_handler() -> Json<serde_json::Value> {
    Json(serde_json::json!({
        "status": "ok",
        "service": "wdmm-openapi",
        "version": env!("CARGO_PKG_VERSION"),
    }))
}

async fn accounts_stub() -> Json<serde_json::Value> {
    Json(serde_json::json!({"accounts": []}))
}

async fn tracks_stub() -> Json<serde_json::Value> {
    Json(serde_json::json!({"tracks": []}))
}
