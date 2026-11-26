//! substrate-server binary: config from the environment, then serve.

use std::net::SocketAddr;

use substrate_server::{app, build, domain, gliph, router};

fn main() -> anyhow::Result<()> {
    // Errors and request timings go to Pushleft's Errors page. Without SENTRY_DSN (local, tests) the SDK is off.
    // The environment comes from SENTRY_ENVIRONMENT, else "production" for a release build.
    let _sentry = sentry::init(sentry::ClientOptions::new().maybe_release(sentry::release_name!()).traces_sample_rate(0.1));
    tokio::runtime::Builder::new_multi_thread().enable_all().build()?.block_on(run())
}

async fn run() -> anyhow::Result<()> {
    use tracing_subscriber::prelude::*;
    tracing_subscriber::registry()
        .with(tracing_subscriber::fmt::layer().json())
        .with(tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info,sqlx=warn".into()))
        .with(sentry::integrations::tracing::ErrorLayer::new().with_filter(tracing_subscriber::filter::LevelFilter::ERROR))
        .init();
    let dev = std::env::var("SUBSTRATE_DEV").map(|v| v == "1").unwrap_or(false);
    let secret = std::env::var("SUBSTRATE_SECRET").unwrap_or_default();
    if secret.len() < 32 && !dev {
        anyhow::bail!("SUBSTRATE_SECRET must be set (64 hex characters)");
    }
    let cfg = app::Config {
        public_url: std::env::var("PUBLIC_URL").unwrap_or_else(|_| "https://substrate.gliph.sh".into()).trim_end_matches('/').to_string(),
        secret: if secret.is_empty() { b"dev-only-secret-dev-only-secret!".to_vec() } else { secret.into_bytes() },
        gliph_url: std::env::var("GLIPH_URL").unwrap_or_else(|_| "https://gliph.sh".into()).trim_end_matches('/').to_string(),
        gliph_plugin_token: std::env::var("GLIPH_PLUGIN_TOKEN").unwrap_or_default(),
        dev,
    };
    let db = sqlx::postgres::PgPoolOptions::new().max_connections(10).connect(&std::env::var("DATABASE_URL")?).await?;
    let app = build(db, cfg).await?;
    gliph::spawn(app.clone());
    substrate_server::worklog::spawn_prices(app.clone());
    {
        let app = app.clone();
        tokio::spawn(async move {
            loop {
                tokio::time::sleep(std::time::Duration::from_secs(60)).await;
                if let Err(e) = domain::housekeeping(&app).await {
                    tracing::error!(error = %e.1, "housekeeping");
                }
            }
        });
    }
    let port: u16 = std::env::var("PORT").ok().and_then(|p| p.parse().ok()).unwrap_or(8080);
    let addr = SocketAddr::from(([0, 0, 0, 0], port));
    tracing::info!(%addr, url = %app.cfg.public_url, "substrate listening");
    let listener = tokio::net::TcpListener::bind(addr).await?;
    let routes = router(app).layer(axum::middleware::from_fn(trace));
    axum::serve(listener, routes.into_make_service()).with_graceful_shutdown(async {
        let _ = tokio::signal::ctrl_c().await;
    }).await?;
    Ok(())
}

/// One Sentry transaction per request, named by its route pattern ("GET /mcp/{secret}"). The URL
/// itself never goes to Sentry: session links, OAuth codes and magic-link tokens sit in paths and queries.
async fn trace(req: axum::extract::Request, next: axum::middleware::Next) -> axum::response::Response {
    use sentry::SentryFutureExt;
    let route = req.extensions().get::<axum::extract::MatchedPath>().map(|p| p.as_str().to_string()).unwrap_or_else(|| "unmatched".into());
    let hub = std::sync::Arc::new(sentry::Hub::new_from_top(sentry::Hub::current()));
    let tx = hub.start_transaction(sentry::TransactionContext::new(&format!("{} {route}", req.method()), "http.server"));
    hub.configure_scope(|s| s.set_span(Some(tx.clone().into())));
    let res = next.run(req).bind_hub(hub).await;
    tx.set_status(match res.status().as_u16() {
        s if s < 400 => sentry::protocol::SpanStatus::Ok,
        401 | 403 => sentry::protocol::SpanStatus::PermissionDenied,
        404 => sentry::protocol::SpanStatus::NotFound,
        s if s < 500 => sentry::protocol::SpanStatus::InvalidArgument,
        _ => sentry::protocol::SpanStatus::InternalError,
    });
    tx.finish();
    res
}
