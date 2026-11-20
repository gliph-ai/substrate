//! APNs pushes to Sidra: one per request that starts waiting for the developer's
//! approval, with Approve and Reject on the notification (category `substrate.approval`).
//! Token auth with the team's APNs key: the .p8 at /secrets/apns.p8 (or APNS_KEY_PATH), APNS_KEY_ID,
//! APNS_TEAM_ID. Without them nothing is sent.

use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

use base64::Engine;
use ring::signature::{EcdsaKeyPair, ECDSA_P256_SHA256_FIXED_SIGNING};
use serde_json::json;
use sqlx::PgPool;

/// What the notification says, sent in the background so the message's creation never waits on Apple.
pub fn approval_waiting(db: PgPool, owner: String, message_id: String, project: String, sender: String, text: String, held: bool) {
    tokio::spawn(async move {
        let devices = sqlx::query_as::<_, (String, String, bool, String)>("SELECT token, bundle, sandbox, client_ref FROM devices WHERE account_id=$1")
            .bind(&owner).fetch_all(&db).await.unwrap_or_default();
        if devices.is_empty() { return; }
        let Some(jwt) = jwt() else { return };
        let (title, body) = if held { (format!("Held: {project}"), format!("{sender}: {text}")) } else { (format!("{sender} asks {project}"), text) };
        let body: String = body.chars().take(240).collect();
        for (token, bundle, sandbox, client_ref) in devices {
            let host = if sandbox { "api.sandbox.push.apple.com" } else { "api.push.apple.com" };
            let payload = json!({
                "aps": {"alert": {"title": title, "body": body}, "sound": "default", "category": "substrate.approval", "thread-id": "substrate"},
                "substrateMessage": message_id, "substrateConnection": client_ref,
            });
            let res = client().post(format!("https://{host}/3/device/{token}"))
                .bearer_auth(&jwt).header("apns-topic", &bundle).header("apns-push-type", "alert")
                .header("apns-collapse-id", format!("substrate.{message_id}"))   // the same id as Sidra's own local notification
                .json(&payload).send().await;
            match res {
                Ok(r) if r.status().is_success() => {}
                Ok(r) => {
                    let status = r.status().as_u16();
                    let reason = r.text().await.unwrap_or_default();
                    tracing::warn!(status, reason, "apns refused a push");
                    // A token Apple no longer knows (app removed, or the wrong environment) is dropped.
                    if status == 410 || reason.contains("BadDeviceToken") || reason.contains("Unregistered") {
                        let _ = sqlx::query("DELETE FROM devices WHERE token=$1").bind(&token).execute(&db).await;
                    }
                }
                Err(e) => tracing::warn!(error = %e, "apns push failed"),
            }
        }
    });
}

fn client() -> &'static reqwest::Client {
    static C: OnceLock<reqwest::Client> = OnceLock::new();
    C.get_or_init(|| reqwest::Client::builder().timeout(Duration::from_secs(15)).build().expect("http client"))
}

/// The provider token, made again every 50 minutes (Apple accepts one for an hour, and refuses fresh ones more often than every 20).
fn jwt() -> Option<String> {
    static CACHE: OnceLock<Mutex<Option<(Instant, String)>>> = OnceLock::new();
    let cache = CACHE.get_or_init(|| Mutex::new(None));
    let mut c = cache.lock().unwrap();
    if let Some((at, t)) = c.as_ref() {
        if at.elapsed() < Duration::from_secs(50 * 60) { return Some(t.clone()); }
    }
    let path = std::env::var("APNS_KEY_PATH").unwrap_or_else(|_| "/secrets/apns.p8".into());
    let t = sign(&std::fs::read_to_string(path).ok()?, &std::env::var("APNS_KEY_ID").ok()?, &std::env::var("APNS_TEAM_ID").ok()?, chrono::Utc::now().timestamp())?;
    *c = Some((Instant::now(), t.clone()));
    Some(t)
}

/// An ES256 JWT from the .p8 (PKCS#8 PEM) key.
pub fn sign(p8: &str, key_id: &str, team_id: &str, iat: i64) -> Option<String> {
    let b64 = base64::engine::general_purpose::URL_SAFE_NO_PAD;
    let der = base64::engine::general_purpose::STANDARD
        .decode(p8.lines().filter(|l| !l.starts_with("-----")).collect::<String>().trim()).ok()?;
    let key = EcdsaKeyPair::from_pkcs8(&ECDSA_P256_SHA256_FIXED_SIGNING, &der, &ring::rand::SystemRandom::new()).ok()?;
    let head = b64.encode(json!({"alg": "ES256", "kid": key_id}).to_string());
    let claims = b64.encode(json!({"iss": team_id, "iat": iat}).to_string());
    let input = format!("{head}.{claims}");
    let sig = key.sign(&ring::rand::SystemRandom::new(), input.as_bytes()).ok()?;
    Some(format!("{input}.{}", b64.encode(sig.as_ref())))
}

#[cfg(test)]
mod tests {
    #[test]
    fn signs_an_es256_token() {
        let rng = ring::rand::SystemRandom::new();
        let pkcs8 = ring::signature::EcdsaKeyPair::generate_pkcs8(&ring::signature::ECDSA_P256_SHA256_FIXED_SIGNING, &rng).unwrap();
        use base64::Engine;
        let pem = format!("-----BEGIN PRIVATE KEY-----\n{}\n-----END PRIVATE KEY-----\n", base64::engine::general_purpose::STANDARD.encode(pkcs8.as_ref()));
        let t = super::sign(&pem, "KEY123", "TEAM456", 1_700_000_000).unwrap();
        let parts: Vec<_> = t.split('.').collect();
        assert_eq!(parts.len(), 3);
        let sig = base64::engine::general_purpose::URL_SAFE_NO_PAD.decode(parts[2]).unwrap();
        assert_eq!(sig.len(), 64);   // r || s, as JWS wants
        let claims = String::from_utf8(base64::engine::general_purpose::URL_SAFE_NO_PAD.decode(parts[1]).unwrap()).unwrap();
        assert!(claims.contains("TEAM456"));
    }
}
