//! substrate-server binary: config from the environment, then serve.

use std::net::SocketAddr;

use substrate_server::{app, build, domain, gliph, router};

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt().json().with_env_filter(
        tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info,sqlx=warn".into()),
    ).init();
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
    axum::serve(listener, router(app).into_make_service()).with_graceful_shutdown(async {
        let _ = tokio::signal::ctrl_c().await;
    }).await?;
    Ok(())
}
