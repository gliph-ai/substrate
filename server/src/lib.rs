//! substrate-server: the Substrate cloud (docs/architecture.md).
#![allow(clippy::too_many_arguments, clippy::type_complexity)]

pub mod app;
pub mod auth;
pub mod aws;
pub mod domain;
pub mod files;
pub mod gliph;
pub mod mail;
pub mod mcp;
pub mod oauth;
pub mod relay;
pub mod safety;
pub mod web;

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use axum::extract::{DefaultBodyLimit, Path};
use axum::http::{header, HeaderMap, HeaderValue, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::Router;

pub use app::{App, AppInner};

/// The caller's address for rate limits. Behind Pushleft's edge the first
/// X-Forwarded-For hop is the client; the container is only reachable through it.
pub fn client_ip(h: &HeaderMap) -> String {
    h.get("x-forwarded-for").and_then(|v| v.to_str().ok()).and_then(|v| v.split(',').next()).map(|s| s.trim().to_string()).unwrap_or_else(|| "local".into())
}

static STATIC: &[(&str, &str, &[u8])] = &[
    ("app.css", "text/css; charset=utf-8", include_bytes!("../static/app.css")),
    ("app.js", "text/javascript; charset=utf-8", include_bytes!("../static/app.js")),
    ("favicon.svg", "image/svg+xml", include_bytes!("../static/favicon.svg")),
    ("logos/claude.svg", "image/svg+xml", include_bytes!("../static/logos/claude.svg")),
    ("logos/openai.svg", "image/svg+xml", include_bytes!("../static/logos/openai.svg")),
    ("logos/gemini.svg", "image/svg+xml", include_bytes!("../static/logos/gemini.svg")),
    ("logos/copilot.svg", "image/svg+xml", include_bytes!("../static/logos/copilot.svg")),
    ("logos/cursor.svg", "image/svg+xml", include_bytes!("../static/logos/cursor.svg")),
    ("logos/github-actions.svg", "image/svg+xml", include_bytes!("../static/logos/github-actions.svg")),
    ("logos/gitlab.svg", "image/svg+xml", include_bytes!("../static/logos/gitlab.svg")),
    ("logos/gliph.png", "image/png", include_bytes!("../static/logos/gliph.png")),
    ("logos/substrate.png", "image/png", include_bytes!("../static/logos/substrate.png")),
];

/// A short hash of the stylesheet and script, put in their URLs so a deploy
/// is never hidden behind the hour-long cache.
pub fn asset_v() -> &'static str {
    static V: std::sync::OnceLock<String> = std::sync::OnceLock::new();
    V.get_or_init(|| {
        use std::hash::{Hash, Hasher};
        let mut h = std::collections::hash_map::DefaultHasher::new();
        STATIC.iter().filter(|(n, _, _)| n.starts_with("app.")).for_each(|(_, _, b)| b.hash(&mut h));
        format!("{:x}", h.finish() & 0xffff_ffff)
    })
}

async fn static_file(Path(p): Path<String>) -> Response {
    match STATIC.iter().find(|(n, _, _)| *n == p) {
        Some((_, ct, b)) => ([(header::CONTENT_TYPE, *ct), (header::CACHE_CONTROL, "public, max-age=3600")], *b).into_response(),
        None => StatusCode::NOT_FOUND.into_response(),
    }
}

async fn security_headers(req: axum::extract::Request, next: axum::middleware::Next) -> Response {
    let mut r = next.run(req).await;
    let h = r.headers_mut();
    h.entry(header::X_CONTENT_TYPE_OPTIONS).or_insert(HeaderValue::from_static("nosniff"));
    h.entry(header::REFERRER_POLICY).or_insert(HeaderValue::from_static("strict-origin-when-cross-origin"));
    h.entry(header::X_FRAME_OPTIONS).or_insert(HeaderValue::from_static("DENY"));
    h.entry(header::STRICT_TRANSPORT_SECURITY).or_insert(HeaderValue::from_static("max-age=31536000"));
    h.entry(header::CONTENT_SECURITY_POLICY).or_insert(HeaderValue::from_static(
        "default-src 'self'; style-src 'self' 'unsafe-inline' https://fonts.googleapis.com; font-src https://fonts.gstatic.com; img-src 'self' data:; script-src 'self'; connect-src 'self'; frame-ancestors 'none'; base-uri 'none'; form-action 'self' https: http://localhost:* http://127.0.0.1:*",
    ));
    r
}

pub fn router(app: App) -> Router {
    Router::new()
        .route("/", get(web::home))
        .route("/healthz", get(web::healthz))
        .route("/static/{*p}", get(static_file))
        .route("/install", get(web::install_sh))
        .route("/install.sh", get(web::install_sh))
        .route("/downloads/substrate/{*rest}", get(web::download))
        // sign-in
        .route("/login", get(web::login_get).post(auth::login_post))
        .route("/logout", post(auth::logout))
        .route("/auth/magic", get(auth::magic_get).post(auth::magic_post))
        .route("/auth/gliph", get(auth::gliph_start))
        .route("/auth/gliph/callback", get(auth::gliph_cb))
        // developer + tester pages
        .route("/p/{pid}", get(web::project_page))
        .route("/p/{pid}/live", get(web::project_live))
        .route("/p/{pid}/icon", get(web::project_icon_file))
        .route("/p/{pid}/mode", post(web::project_mode))
        .route("/p/{pid}/settings", post(web::project_settings))
        .route("/p/{pid}/invites", post(web::invite_create))
        .route("/p/{pid}/invites/{iid}/revoke", post(web::invite_revoke))
        .route("/p/{pid}/members/{aid}/trust", post(web::member_trust))
        .route("/p/{pid}/members/{aid}/remove", post(web::member_remove))
        .route("/p/{pid}/unlink-gliph", post(web::unlink_gliph))
        .route("/c/{cid}", get(web::conversation))
        .route("/c/{cid}/live", get(web::conversation_live))
        .route("/c/{cid}/answer", post(web::answer))
        .route("/c/{cid}/release/{seq}", post(web::release_held))
        .route("/m/{mid}/{action}", post(web::message_action))
        .route("/a/{aid}", get(web::file_session))
        .route("/files/{aid}", get(web::file_signed))
        .route("/upload/{t}", get(web::upload_get).post(web::upload_post))
        .route("/i/{t}", get(web::invite_get).post(web::invite_post))
        .route("/connect", get(web::connect))
        .route("/connect/token", post(web::pat_create))
        .route("/connect/token/{prefix}/revoke", post(web::pat_revoke))
        .route("/pair", get(web::pair_get).post(web::pair_post))
        .route("/machines", get(web::machines))
        .route("/machines/{mid}/revoke", post(web::machine_revoke))
        .route("/audit", get(web::audit_page))
        // MCP + OAuth
        .route("/mcp", get(mcp::get).post(mcp::post))
        .route("/.well-known/oauth-protected-resource", get(oauth::protected_resource))
        .route("/.well-known/oauth-protected-resource/mcp", get(oauth::protected_resource))
        .route("/.well-known/oauth-authorization-server", get(oauth::auth_server))
        .route("/oauth/register", post(oauth::register))
        .route("/oauth/authorize", get(oauth::authorize).post(oauth::authorize_post))
        .route("/oauth/token", post(oauth::token_endpoint))
        // relay
        .route("/relay/pair/start", post(relay::pair_start))
        .route("/relay/pair/token", post(relay::pair_token))
        .route("/relay/token", post(relay::refresh))
        .route("/relay/ws", get(relay::ws))
        .route("/relay/report", post(relay::report))
        .route("/relay/report/attachment", post(relay::report_attachment))
        .route("/relay/attachments/{aid}", get(relay::attachment_download))
        .route("/relay/api/whoami", get(relay::api_whoami))
        .route("/relay/api/logout", post(relay::api_logout))
        .route("/relay/api/projects", get(relay::api_projects).post(relay::api_add_project))
        .route("/relay/api/projects/{handle}/bind", post(relay::api_bind))
        .route("/relay/api/projects/{handle}/unbind", post(relay::api_unbind))
        .route("/relay/api/projects/{handle}/mode", post(relay::api_mode))
        .route("/relay/api/projects/{handle}/icon", post(relay::api_icon))
        .route("/relay/api/approvals", get(relay::api_approvals))
        .route("/relay/api/inbox", get(relay::api_inbox))
        .route("/relay/api/messages/{mid}/{action}", post(relay::api_decide))
        .fallback(|| async { (StatusCode::NOT_FOUND, web::not_found()) })
        .layer(DefaultBodyLimit::max(16 * 1024 * 1024))
        .layer(axum::middleware::from_fn(security_headers))
        .with_state(app)
}

pub async fn build(db: sqlx::PgPool, cfg: app::Config) -> anyhow::Result<App> {
    sqlx::migrate!("./migrations").run(&db).await?;
    let dev = cfg.dev;
    Ok(Arc::new(AppInner {
        db,
        cfg,
        mail: mail::Mailer::from_env(dev),
        files: files::Files::from_env(&std::env::var("SUBSTRATE_FILES").unwrap_or_else(|_| "./data".into())),
        hub: Mutex::new(HashMap::new()),
        waiters: Mutex::new(HashMap::new()),
        limiter: app::Limiter::new(),
        gliph_out: Mutex::new(None),
    }))
}

