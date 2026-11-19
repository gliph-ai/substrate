//! Outbound email through Amazon SES v2 (the shared account gliph.sh uses,
//! sending as noreply@gliph.sh). Unconfigured, it refuses, unless the server
//! runs with SUBSTRATE_DEV=1, where the message is logged instead.

use anyhow::{bail, Result};
use serde_json::json;

use crate::aws::{self, Creds};

pub struct Mailer {
    pub ses: Option<(Creds, String)>,
    pub dev: bool,
    /// The public site, for the logo in the HTML version.
    base: String,
    http: reqwest::Client,
}

/// One email: a title, a line of context, one button, and a quiet note.
/// Rendered as HTML (table layout and inline styles, so mail clients keep it)
/// and as plain text with the same words.
pub struct Message<'a> {
    pub subject: &'a str,
    pub title: &'a str,
    pub intro: &'a str,
    pub button: &'a str,
    pub url: &'a str,
    pub note: &'a str,
    /// A one-time code to type (Sidra's sign-in), shown big instead of a button when set.
    pub code: &'a str,
}

impl Message<'_> {
    pub fn text(&self) -> String {
        if !self.code.is_empty() {
            return format!("{}\n\n{}\n\n{}\n\n{}\n", self.title, self.intro, self.code, self.note);
        }
        format!("{}\n\n{}\n\n{}: {}\n\n{}\n", self.title, self.intro, self.button, self.url, self.note)
    }

    pub fn html(&self, base: &str) -> String {
        let font = "font-family:-apple-system,BlinkMacSystemFont,'Segoe UI',Inter,Helvetica,Arial,sans-serif";
        maud::html! {
            (maud::DOCTYPE)
            html lang="en" {
                head { meta charset="utf-8"; meta name="viewport" content="width=device-width"; meta name="color-scheme" content="light"; title { (self.subject) } }
                body style="margin:0;padding:0;background:#F4F4F5" {
                    table role="presentation" width="100%" cellpadding="0" cellspacing="0" style="background:#F4F4F5" { tr { td align="center" style="padding:40px 16px" {
                        table role="presentation" width="100%" cellpadding="0" cellspacing="0" style=(format!("max-width:480px;background:#FFFFFF;border-radius:16px;{font}")) {
                            tr { td style="padding:36px 36px 8px" {
                                img src=(format!("{base}/static/logos/substrate.png")) width="44" height="44" alt="Substrate" style="display:block;border-radius:12px";
                            } }
                            tr { td style="padding:20px 36px 0;font-size:24px;line-height:30px;font-weight:600;color:#09090B;letter-spacing:-0.3px" { (self.title) } }
                            tr { td style="padding:10px 36px 0;font-size:15px;line-height:23px;color:#52525B" { (self.intro) } }
                            @if !self.code.is_empty() {
                                tr { td style="padding:24px 36px 0" {
                                    div style="display:inline-block;padding:14px 22px;border-radius:12px;background:#F4F4F5;font-family:ui-monospace,SFMono-Regular,Menlo,monospace;font-size:32px;letter-spacing:10px;font-weight:700;color:#09090B" { (self.code) }
                                } }
                            } @else {
                                tr { td style="padding:28px 36px 0" {
                                    table role="presentation" cellpadding="0" cellspacing="0" { tr { td bgcolor="#09090B" style="border-radius:10px" {
                                        a href=(self.url) style="display:inline-block;padding:13px 26px;font-size:15px;font-weight:600;color:#FFFFFF;text-decoration:none;border-radius:10px" { (self.button) }
                                    } } }
                                } }
                                tr { td style="padding:28px 36px 0;font-size:13px;line-height:20px;color:#71717A" {
                                    "Button not working? Paste this link into your browser:" br;
                                    a href=(self.url) style="color:#4F46E5;word-break:break-all" { (self.url) }
                                } }
                            }
                            tr { td style="padding:20px 36px 36px;font-size:13px;line-height:20px;color:#71717A" { (self.note) } }
                        }
                        table role="presentation" width="100%" cellpadding="0" cellspacing="0" style=(format!("max-width:480px;{font}")) {
                            tr { td align="center" style="padding:20px 0;font-size:12px;color:#A1A1AA" {
                                "Substrate is part of " a href="https://gliph.sh" style="color:#A1A1AA" { "gliph.sh" }
                                " " img src=(format!("{base}/static/logos/gliph.png")) alt="" width="14" height="14" style="vertical-align:-2px;border-radius:3px";
                            } }
                        }
                    } } }
                }
            }
        }.into_string()
    }
}

impl Mailer {
    pub fn from_env(dev: bool) -> Mailer {
        let key = std::env::var("SES_KEY_ID").unwrap_or_default();
        let secret = std::env::var("SES_SECRET").unwrap_or_default();
        let from = std::env::var("MAIL_FROM").unwrap_or_else(|_| "Substrate <noreply@gliph.sh>".into());
        let region = std::env::var("SES_REGION").unwrap_or_else(|_| "eu-west-1".into());
        let ses = (!key.is_empty() && !secret.is_empty()).then_some((Creds { key_id: key, secret, region }, from));
        let base = std::env::var("PUBLIC_URL").unwrap_or_else(|_| "https://substrate.gliph.sh".into());
        Mailer { ses, dev, base, http: reqwest::Client::new() }
    }

    pub async fn send(&self, to: &str, m: &Message<'_>) -> Result<()> {
        let (subject, body, html) = (m.subject, m.text(), m.html(&self.base));
        let Some((creds, from)) = &self.ses else {
            if self.dev {
                tracing::warn!(to, subject, body = %body, "mail not configured; logged instead (SUBSTRATE_DEV)");
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
                "Body": {
                    "Text": { "Data": body, "Charset": "UTF-8" },
                    "Html": { "Data": html, "Charset": "UTF-8" }
                }
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

#[cfg(test)]
mod tests {
    use super::Message;

    #[test]
    fn html_and_text_carry_the_link() {
        let m = Message { subject: "s", title: "Sign in", intro: "i", button: "Go", url: "https://x.test/a?t=1&n=<b>", note: "n", code: "" };
        let h = m.html("https://x.test");
        assert!(h.contains("href=\"https://x.test/a?t=1&amp;n=&lt;b&gt;\""), "{h}");
        assert!(!h.contains("<b>"));
        assert!(m.text().contains("Go: https://x.test/a?t=1&n=<b>"));
        if let Ok(p) = std::env::var("MAIL_PREVIEW") {
            let real = Message { subject: "Your Substrate sign-in link", title: "Sign in to Substrate", intro: "The button works once and expires in 15 minutes.", button: "Sign in", url: "https://substrate.gliph.sh/auth/magic?t=076844d22b324ad0f44da55f02f5a3ea3a0f6532d7c71116adccea0718a98b09", note: "Didn't ask for it? Ignore this email.", code: "" };
            std::fs::write(p, real.html("http://localhost:8790")).unwrap();
        }
    }
}
