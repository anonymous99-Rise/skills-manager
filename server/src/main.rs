mod auth;
mod backup;
mod core;
mod invoke;
mod ops;
mod tools;

use std::sync::Arc;

use crate::core::skill_store::SkillStore;

pub struct AppState {
    pub store: Arc<SkillStore>,
    pub auth: auth::AuthService,
}

#[tokio::main]
async fn main() {
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info")).init();

    // Data home: the core resolves everything under $HOME, so let the
    // deployment pin it to a volume with SM_HOME.
    if let Ok(home) = std::env::var("SM_HOME") {
        std::env::set_var("HOME", &home);
        std::fs::create_dir_all(&home).expect("failed to create SM_HOME");
    }

    let store: Arc<SkillStore> = core::app_state::initialize_cli_store()
        .expect("failed to initialize the skills store");
    log::info!(
        "store ready: central repo at {}",
        core::central_repo::base_dir().display()
    );

    let auth = auth::AuthService::new();

    let state = Arc::new(AppState { store, auth });

    let api = invoke::router();
    let auth_routes = auth::router();

    let app = axum::Router::new()
        .nest("/api/auth", auth_routes)
        .nest("/api", api)
        .fallback(fallback)
        .with_state(state);

    let port: u16 = std::env::var("SM_PORT")
        .ok()
        .and_then(|p| p.parse().ok())
        .unwrap_or(8080);
    let addr = std::net::SocketAddr::from(([0, 0, 0, 0], port));
    log::info!("listening on {addr}");

    let listener = tokio::net::TcpListener::bind(addr).await.expect("bind failed");
    axum::serve(listener, app).await.expect("serve failed");
}

async fn fallback(req: axum::http::Request<axum::body::Body>) -> axum::response::Response {
    use tower::Service;
    static STATIC_DIR: std::sync::OnceLock<Option<std::path::PathBuf>> = std::sync::OnceLock::new();
    let dir = STATIC_DIR.get_or_init(|| {
        std::env::var("SM_STATIC_DIR").ok().filter(|d| !d.is_empty()).map(std::path::PathBuf::from)
    });

    let Some(dir) = dir else {
        return axum::http::Response::builder()
            .status(404)
            .body(axum::body::Body::from(
                "static frontend not configured (set SM_STATIC_DIR)",
            ))
            .unwrap();
    };

    // Serve real files; anything else falls back to index.html (SPA routing).
    let mut svc = tower_http::services::ServeDir::new(dir)
        .append_index_html_on_directories(true)
        .fallback(tower_http::services::ServeFile::new(dir.join("index.html")));
    match svc.call(req).await {
        Ok(resp) => resp.map(axum::body::Body::new),
        Err(_) => axum::http::Response::builder()
            .status(500)
            .body(axum::body::Body::from("static serve error"))
            .unwrap(),
    }
}
