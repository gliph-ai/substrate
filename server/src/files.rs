//! Attachment bytes: the Pushleft bucket in production (S3_* / AWS_* env, the
//! names `pl storage bucket create --set-secret` gives), a directory in dev.

use std::path::PathBuf;

use anyhow::{bail, Context, Result};

use crate::aws::{self, Creds};

pub enum Files {
    Disk(PathBuf),
    S3 { endpoint: String, bucket: String, creds: Creds, http: reqwest::Client },
}

impl Files {
    pub fn from_env(dev_dir: &str) -> Files {
        match std::env::var("S3_BUCKET") {
            Ok(bucket) if !bucket.is_empty() => Files::S3 {
                endpoint: std::env::var("S3_ENDPOINT").unwrap_or_default().trim_end_matches('/').to_string(),
                bucket,
                creds: Creds {
                    key_id: std::env::var("AWS_ACCESS_KEY_ID").unwrap_or_default(),
                    secret: std::env::var("AWS_SECRET_ACCESS_KEY").unwrap_or_default(),
                    region: std::env::var("AWS_REGION").unwrap_or_else(|_| "us-east-1".into()),
                },
                http: reqwest::Client::new(),
            },
            _ => Files::Disk(PathBuf::from(dev_dir)),
        }
    }

    fn s3_url(endpoint: &str, bucket: &str, key: &str) -> Result<(String, String, String)> {
        let u = url::Url::parse(endpoint).context("S3_ENDPOINT")?;
        let host = match u.port() {
            Some(p) => format!("{}:{p}", u.host_str().unwrap_or_default()),
            None => u.host_str().unwrap_or_default().to_string(),
        };
        // Path-style: works with every S3-compatible store.
        let path = format!("/{bucket}/{key}");
        Ok((format!("{}://{host}{}", u.scheme(), aws::uri_encode(&path, true)), host, path))
    }

    pub async fn put(&self, key: &str, bytes: &[u8]) -> Result<()> {
        match self {
            Files::Disk(dir) => {
                let p = dir.join(key);
                if let Some(d) = p.parent() {
                    tokio::fs::create_dir_all(d).await?;
                }
                tokio::fs::write(p, bytes).await?;
                Ok(())
            }
            Files::S3 { endpoint, bucket, creds, http } => {
                let (url, host, path) = Self::s3_url(endpoint, bucket, key)?;
                let h = aws::sign(creds, "s3", "PUT", &host, &path, &[], &[], bytes, chrono::Utc::now());
                let mut req = http.put(url).body(bytes.to_vec());
                for (k, v) in h {
                    req = req.header(k, v);
                }
                let r = req.send().await?;
                if !r.status().is_success() {
                    bail!("s3 put {}: {}", r.status(), r.text().await.unwrap_or_default());
                }
                Ok(())
            }
        }
    }

    pub async fn get(&self, key: &str) -> Result<Option<Vec<u8>>> {
        match self {
            Files::Disk(dir) => match tokio::fs::read(dir.join(key)).await {
                Ok(b) => Ok(Some(b)),
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
                Err(e) => Err(e.into()),
            },
            Files::S3 { endpoint, bucket, creds, http } => {
                let (url, host, path) = Self::s3_url(endpoint, bucket, key)?;
                let h = aws::sign(creds, "s3", "GET", &host, &path, &[], &[], b"", chrono::Utc::now());
                let mut req = http.get(url);
                for (k, v) in h {
                    req = req.header(k, v);
                }
                let r = req.send().await?;
                if r.status() == reqwest::StatusCode::NOT_FOUND {
                    return Ok(None);
                }
                if !r.status().is_success() {
                    bail!("s3 get {}", r.status());
                }
                Ok(Some(r.bytes().await?.to_vec()))
            }
        }
    }

    pub async fn delete(&self, key: &str) -> Result<()> {
        match self {
            Files::Disk(dir) => {
                let _ = tokio::fs::remove_file(dir.join(key)).await;
                Ok(())
            }
            Files::S3 { endpoint, bucket, creds, http } => {
                let (url, host, path) = Self::s3_url(endpoint, bucket, key)?;
                let h = aws::sign(creds, "s3", "DELETE", &host, &path, &[], &[], b"", chrono::Utc::now());
                let mut req = http.delete(url);
                for (k, v) in h {
                    req = req.header(k, v);
                }
                req.send().await?;
                Ok(())
            }
        }
    }
}

/// What an attachment may be. Sniffed from the bytes, never taken from the
/// client's claim. Anything else is refused: no HTML, no SVG, no executables.
pub fn sniff(bytes: &[u8], name: &str) -> Option<&'static str> {
    if bytes.starts_with(b"\x89PNG\r\n\x1a\n") {
        return Some("image/png");
    }
    if bytes.starts_with(&[0xFF, 0xD8, 0xFF]) {
        return Some("image/jpeg");
    }
    if bytes.len() > 12 && &bytes[0..4] == b"RIFF" && &bytes[8..12] == b"WEBP" {
        return Some("image/webp");
    }
    if bytes.starts_with(b"%PDF-") {
        return Some("application/pdf");
    }
    // Text: valid UTF-8, no NUL, not markup a browser would render.
    if let Ok(s) = std::str::from_utf8(bytes) {
        if s.contains('\0') {
            return None;
        }
        let head = s.trim_start().get(..64).unwrap_or(s.trim_start()).to_ascii_lowercase();
        if head.starts_with("<!doctype") || head.starts_with("<html") || head.starts_with("<svg") || head.starts_with("<?xml") {
            return None;
        }
        let lower = name.to_ascii_lowercase();
        if lower.ends_with(".md") || lower.ends_with(".markdown") {
            return Some("text/markdown");
        }
        return Some("text/plain");
    }
    None
}

pub const MAX_ATTACHMENT: usize = 10 * 1024 * 1024;

#[cfg(test)]
mod tests {
    use super::sniff;
    #[test]
    fn sniffing() {
        assert_eq!(sniff(b"\x89PNG\r\n\x1a\nxxxx", "a.png"), Some("image/png"));
        assert_eq!(sniff(b"hello log\n", "x.log"), Some("text/plain"));
        assert_eq!(sniff(b"# Title", "README.md"), Some("text/markdown"));
        assert_eq!(sniff(b"<svg onload=alert(1)>", "x.png"), None);
        assert_eq!(sniff(b"<!DOCTYPE html><script>", "x.txt"), None);
        assert_eq!(sniff(b"MZ\x90\x00\x03\x00\x00\x00\xff\xfe", "x.exe"), None);
    }
}
