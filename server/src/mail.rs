//! Outbound email through Amazon SES v2 (the shared account gliph.sh uses,
//! sending as noreply@gliph.sh). Unconfigured, it refuses, unless the server
//! runs with SUBSTRATE_DEV=1, where the message is logged instead.

use anyhow::{bail, Result};
use serde_json::json;

use crate::aws::{self, Creds};

pub struct Mailer {
    pub ses: Option<(Creds, String)>,
    pub dev: bool,
    http: reqwest::Client,
}

impl Mailer {
    pub fn from_env(dev: bool) -> Mailer {
        let key = std::env::var("SES_KEY_ID").unwrap_or_default();
        let secret = std::env::var("SES_SECRET").unwrap_or_default();
        let from = std::env::var("MAIL_FROM").unwrap_or_else(|_| "Substrate <noreply@gliph.sh>".into());
        let region = std::env::var("SES_REGION").unwrap_or_else(|_| "eu-west-1".into());
        let ses = (!key.is_empty() && !secret.is_empty()).then_some((Creds { key_id: key, secret, region }, from));
        Mailer { ses, dev, http: reqwest::Client::new() }
    }

    pub async fn send(&self, to: &str, subject: &str, body: &str) -> Result<()> {
        let Some((creds, from)) = &self.ses else {
            if self.dev {
                tracing::warn!(to, subject, body, "mail not configured; logged instead (SUBSTRATE_DEV)");
                return Ok(());
            }
            bail!("email is not configured on this server");
        };
        let host = format!("email.{}.amazonaws.com", creds.region);
        let path = "/v2/email/outbound-emails";
        let payload = serde_json::to_vec(&json!({
            "FromEmailAddress": from,
            "Destination": { "ToAddresses": [to] },
            "Content": { "Simple": {
                "Subject": { "Data": subject, "Charset": "UTF-8" },
                "Body": { "Text": { "Data": body, "Charset": "UTF-8" } }
            }}
        }))?;
        let h = aws::sign(creds, "ses", "POST", &host, path, &[],
            &[("content-type".into(), "application/json".into())], &payload, chrono::Utc::now());
        let mut req = self.http.post(format!("https://{host}{path}")).header("content-type", "application/json").body(payload);
        for (k, v) in h {
            req = req.header(k, v);
        }
        let r = req.send().await?;
        if !r.status().is_success() {
            bail!("ses {}: {}", r.status(), r.text().await.unwrap_or_default());
        }
        Ok(())
    }
}
