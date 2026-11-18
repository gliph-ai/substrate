//! Shared state, configuration and the small helpers every module uses.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use chrono::{DateTime, Utc};
use hmac::{Hmac, Mac};
use rand::RngCore;
use sha2::{Digest, Sha256};
use sqlx::PgPool;
use substrate_proto::CloudFrame;
use tokio::sync::{broadcast, mpsc};

use crate::files::Files;
use crate::mail::Mailer;

#[derive(Clone)]
pub struct Config {
    /// https://substrate.gliph.sh (no trailing slash)
    pub public_url: String,
    pub secret: Vec<u8>,
    /// https://gliph.sh, for Sign in with Gliph and the plugin.
    pub gliph_url: String,
    pub gliph_plugin_token: String,
    /// Log magic links instead of failing when no mailer is configured.
    pub dev: bool,
}

impl Config {
    pub fn secure_cookies(&self) -> bool {
        self.public_url.starts_with("https://")
    }
    pub fn host(&self) -> String {
        url::Url::parse(&self.public_url)
            .ok()
            .and_then(|u| u.host_str().map(|h| match u.port() {
                Some(p) => format!("{h}:{p}"),
                None => h.to_string(),
            }))
            .unwrap_or_default()
    }
}

pub type App = Arc<AppInner>;

pub struct AppInner {
    pub db: PgPool,
    pub cfg: Config,
    pub mail: Mailer,
    pub files: Files,
    /// machine id → its live socket's outbox.
    pub hub: Mutex<HashMap<String, mpsc::UnboundedSender<CloudFrame>>>,
    /// conversation id → wakes long-polls and live pages on a new event.
    pub waiters: Mutex<HashMap<String, broadcast::Sender<i64>>>,
    pub limiter: Limiter,
    /// Lines for the Gliph plugin task to post: (channel id, text).
    pub gliph_out: Mutex<Option<mpsc::UnboundedSender<GliphOut>>>,
}

pub enum GliphOut {
    /// `agent`: the brand Gliph draws beside the line (its allowlist:
    /// claude, openai, gemini, cursor, copilot, github, gitlab).
    Say { channel: String, body: String, agent: Option<&'static str> },
    Resubscribe,
}

impl AppInner {
    pub fn waiter(&self, conv: &str) -> broadcast::Receiver<i64> {
        let mut w = self.waiters.lock().unwrap();
        w.entry(conv.to_string()).or_insert_with(|| broadcast::channel(16).0).subscribe()
    }
    pub fn wake(&self, conv: &str, seq: i64) {
        let w = self.waiters.lock().unwrap();
        if let Some(tx) = w.get(conv) {
            let _ = tx.send(seq);
        }
    }
    pub fn send_machine(&self, machine: &str, f: CloudFrame) -> bool {
        let hub = self.hub.lock().unwrap();
        hub.get(machine).map(|tx| tx.send(f).is_ok()).unwrap_or(false)
    }
    pub fn machine_online(&self, machine: &str) -> bool {
        !machine.is_empty() && self.hub.lock().unwrap().contains_key(machine)
    }
    pub fn gliph(&self, o: GliphOut) {
        if let Some(tx) = self.gliph_out.lock().unwrap().as_ref() {
            let _ = tx.send(o);
        }
    }
    /// HMAC-SHA256 under the server secret, hex.
    pub fn sign(&self, data: &str) -> String {
        let mut m = Hmac::<Sha256>::new_from_slice(&self.cfg.secret).expect("hmac key");
        m.update(data.as_bytes());
        hex::encode(m.finalize().into_bytes())
    }
    pub fn verify(&self, data: &str, sig: &str) -> bool {
        let mut m = Hmac::<Sha256>::new_from_slice(&self.cfg.secret).expect("hmac key");
        m.update(data.as_bytes());
        hex::decode(sig).map(|s| m.verify_slice(&s).is_ok()).unwrap_or(false)
    }
}

/// A random secret, hex. 32 bytes unless asked.
pub fn token_n(n: usize) -> String {
    let mut b = vec![0u8; n];
    rand::thread_rng().fill_bytes(&mut b);
    hex::encode(b)
}
pub fn token() -> String {
    token_n(32)
}

pub fn id(prefix: &str) -> String {
    format!("{prefix}_{}", token_n(8))
}

pub fn hash(s: &str) -> String {
    hex::encode(Sha256::digest(s.as_bytes()))
}

pub fn now() -> DateTime<Utc> {
    Utc::now()
}

/// A token bucket per key. Small and in-process: there is one web instance.
pub struct Limiter {
    buckets: Mutex<HashMap<String, (f64, std::time::Instant)>>,
}

impl Limiter {
    pub fn new() -> Self {
        Limiter { buckets: Mutex::new(HashMap::new()) }
    }
    /// `rate` tokens per second, up to `burst`.
    pub fn allow(&self, key: &str, rate: f64, burst: f64) -> bool {
        let mut b = self.buckets.lock().unwrap();
        if b.len() > 50_000 {
            b.clear(); // ponytail: crude bound; an LRU if this ever fills in practice
        }
        let nowi = std::time::Instant::now();
        let e = b.entry(key.to_string()).or_insert((burst, nowi));
        e.0 = (e.0 + nowi.duration_since(e.1).as_secs_f64() * rate).min(burst);
        e.1 = nowi;
        if e.0 >= 1.0 {
            e.0 -= 1.0;
            true
        } else {
            false
        }
    }
}

impl Default for Limiter {
    fn default() -> Self {
        Self::new()
    }
}

/// Write an audit row. Never fails the caller: an audit failure is logged.
pub async fn audit(app: &AppInner, account: &str, project: &str, actor: &str, action: &str, target: &str, detail: &str) {
    if let Err(e) = sqlx::query("INSERT INTO audit (id, account_id, project_id, actor, action, target, detail) VALUES ($1,$2,$3,$4,$5,$6,$7)")
        .bind(id("au")).bind(account).bind(project).bind(actor).bind(action).bind(target).bind(detail)
        .execute(&app.db).await
    {
        tracing::error!(error = %e, action, "audit write failed");
    }
}

/// An error a handler can return: a status and a message safe to show.
#[derive(Debug)]
pub struct Fail(pub axum::http::StatusCode, pub String);

impl Fail {
    pub fn new(code: u16, msg: impl Into<String>) -> Self {
        Fail(axum::http::StatusCode::from_u16(code).unwrap(), msg.into())
    }
}

impl From<sqlx::Error> for Fail {
    fn from(e: sqlx::Error) -> Self {
        tracing::error!(error = %e, "database");
        Fail::new(500, "internal error")
    }
}
impl From<anyhow::Error> for Fail {
    fn from(e: anyhow::Error) -> Self {
        tracing::error!(error = %e, "internal");
        Fail::new(500, "internal error")
    }
}

impl axum::response::IntoResponse for Fail {
    fn into_response(self) -> axum::response::Response {
        (self.0, axum::Json(serde_json::json!({ "error": self.1 }))).into_response()
    }
}
