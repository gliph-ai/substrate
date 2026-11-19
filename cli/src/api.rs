//! HTTP to the cloud with the machine's credentials: access tokens refreshed
//! under the credential lock, the device-pairing flow, and the calls the CLI
//! and the daemon make.

use anyhow::{anyhow, bail, Result};
use serde_json::{json, Value};

use crate::config::{Creds, Paths, Store};

pub struct Api {
    pub server: String,
    pub http: reqwest::Client,
    store: Store,
}

impl Api {
    pub fn new(paths: &Paths, server: &str) -> Api {
        Api {
            server: server.to_string(),
            http: reqwest::Client::builder().user_agent(concat!("substrate-cli/", env!("CARGO_PKG_VERSION"))).build().unwrap(),
            store: Store::new(paths, server),
        }
    }

    pub fn store(&self) -> &Store {
        &self.store
    }

    /// A valid access token, refreshing (and rotating the refresh token) if it
    /// is within two minutes of expiry. `force` refreshes regardless.
    pub async fn access(&self, force: bool) -> Result<(String, String)> {
        let _lock = self.store.lock()?;
        let mut c = self.store.load()?.ok_or_else(|| anyhow!("not signed in; run `substrate login`"))?;
        let now = chrono::Utc::now().timestamp();
        if force || c.access_token.is_empty() || c.access_expires < now + 120 {
            let r = self.http.post(format!("{}/relay/token", self.server)).json(&json!({"refresh_token": c.refresh_token})).send().await?;
            if r.status() == 401 {
                self.store.clear()?;
                bail!("this computer was signed out of Substrate; run `substrate login` again");
            }
            if !r.status().is_success() {
                bail!("token refresh failed: {}", r.status());
            }
            let v: Value = r.json().await?;
            c.refresh_token = v["refresh_token"].as_str().unwrap_or_default().to_string();
            c.access_token = v["access_token"].as_str().unwrap_or_default().to_string();
            c.access_expires = now + v["expires_in"].as_i64().unwrap_or(3600);
            self.store.save(&c)?;
        }
        Ok((c.access_token, c.machine_id))
    }

    pub async fn call(&self, method: &str, path: &str, body: Option<Value>) -> Result<Value> {
        for attempt in 0..2 {
            let (tok, _) = self.access(attempt > 0).await?;
            let url = format!("{}{path}", self.server);
            let req = match method {
                "GET" => self.http.get(url),
                _ => self.http.post(url).json(&body.clone().unwrap_or(json!({}))),
            };
            let r = req.bearer_auth(tok).send().await?;
            if r.status() == 401 && attempt == 0 {
                continue;
            }
            let ok = r.status().is_success();
            let v: Value = r.json().await.unwrap_or(json!({}));
            if !ok {
                bail!("{}", v["error"].as_str().unwrap_or("request failed"));
            }
            return Ok(v);
        }
        bail!("unauthorized")
    }

    pub async fn upload(&self, message_id: &str, reply_token: &str, name: &str, bytes: Vec<u8>) -> Result<String> {
        let (tok, _) = self.access(false).await?;
        let r = self.http.post(format!("{}/relay/report/attachment", self.server))
            .query(&[("message_id", message_id), ("reply_token", reply_token)])
            .header("x-filename", name).bearer_auth(tok).body(bytes).send().await?;
        let ok = r.status().is_success();
        let v: Value = r.json().await.unwrap_or(json!({}));
        if !ok {
            bail!("{}", v["error"].as_str().unwrap_or("upload failed"));
        }
        Ok(v["attachment_id"].as_str().unwrap_or_default().to_string())
    }

    /// Upload a project's logo (PNG/JPEG bytes).
    pub async fn icon(&self, handle: &str, bytes: Vec<u8>) -> Result<()> {
        let (tok, _) = self.access(false).await?;
        let r = self.http.post(format!("{}/relay/api/projects/{handle}/icon", self.server)).bearer_auth(tok).body(bytes).send().await?;
        if !r.status().is_success() {
            let v: Value = r.json().await.unwrap_or(json!({}));
            bail!("{}", v["error"].as_str().unwrap_or("icon upload failed"));
        }
        Ok(())
    }

    pub async fn download(&self, url: &str) -> Result<Vec<u8>> {
        if !url.starts_with(&self.server) {
            bail!("refusing to download from outside {}", self.server);
        }
        let (tok, _) = self.access(false).await?;
        let r = self.http.get(url).bearer_auth(tok).send().await?;
        if !r.status().is_success() {
            bail!("download failed: {}", r.status());
        }
        Ok(r.bytes().await?.to_vec())
    }

    /// RFC 8628 device flow. Prints the code, opens the browser, polls.
    pub async fn pair(&self, machine_name: &str) -> Result<Creds> {
        let s: Value = self.http.post(format!("{}/relay/pair/start", self.server))
            .json(&json!({"machine_name": machine_name, "platform": std::env::consts::OS, "version": env!("CARGO_PKG_VERSION")}))
            .send().await?.error_for_status()?.json().await?;
        let code = s["user_code"].as_str().unwrap_or_default();
        let uri = s["verification_uri"].as_str().unwrap_or_default();
        println!("\n  Confirm this code in your browser:  {code}\n  {uri}\n");
        if cfg!(target_os = "macos") {
            let _ = std::process::Command::new("open").arg(uri).status();
        }
        let interval = s["interval"].as_u64().unwrap_or(3);
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(s["expires_in"].as_u64().unwrap_or(600));
        while std::time::Instant::now() < deadline {
            tokio::time::sleep(std::time::Duration::from_secs(interval)).await;
            let r = self.http.post(format!("{}/relay/pair/token", self.server)).json(&json!({"device_code": s["device_code"]})).send().await?;
            let ok = r.status().is_success();
            let v: Value = r.json().await.unwrap_or(json!({}));
            if ok {
                let c = Creds {
                    machine_id: v["machine_id"].as_str().unwrap_or_default().into(),
                    refresh_token: v["refresh_token"].as_str().unwrap_or_default().into(),
                    access_token: v["access_token"].as_str().unwrap_or_default().into(),
                    access_expires: chrono::Utc::now().timestamp() + v["expires_in"].as_i64().unwrap_or(3600),
                };
                let _lock = self.store.lock()?;
                self.store.save(&c)?;
                return Ok(c);
            }
            match v["error"].as_str() {
                Some("authorization_pending") => continue,
                Some("access_denied") => bail!("the pairing was denied in the browser"),
                _ => bail!("the pairing code expired; run `substrate login` again"),
            }
        }
        bail!("timed out waiting for the browser")
    }
}
