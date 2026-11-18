//! Where the relay keeps things, and its credentials.
//!
//! Credentials (machine id, refresh token, the current access token) live in
//! the macOS Keychain under service `sh.gliph.substrate`, falling back to a
//! 0600 file when no Keychain is available. Every read-refresh-write happens
//! under a file lock, so the daemon and a CLI command can never both spend
//! the same refresh token (which the cloud treats as theft and revokes).

use std::fs::{self, File, OpenOptions};
use std::io::Write;
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use anyhow::{Context, Result};
use fs2::FileExt;
use serde::{Deserialize, Serialize};

const SERVICE: &str = "sh.gliph.substrate";

#[derive(Clone, Debug)]
pub struct Paths {
    pub root: PathBuf,
}

impl Paths {
    pub fn default_root() -> PathBuf {
        if let Ok(d) = std::env::var("SUBSTRATE_HOME") {
            return PathBuf::from(d);
        }
        let home = std::env::var("HOME").unwrap_or_else(|_| ".".into());
        if cfg!(target_os = "macos") {
            PathBuf::from(home).join("Library/Application Support/substrate")
        } else {
            PathBuf::from(home).join(".local/share/substrate")
        }
    }
    pub fn new(root: PathBuf) -> Result<Paths> {
        for d in ["", "dispatch", "deploy", "workspace", "logs"] {
            let p = root.join(d);
            fs::create_dir_all(&p).with_context(|| format!("creating {}", p.display()))?;
            fs::set_permissions(&p, fs::Permissions::from_mode(0o700))?;
        }
        Ok(Paths { root })
    }
    pub fn dispatch(&self) -> PathBuf { self.root.join("dispatch") }
    pub fn deploy(&self) -> PathBuf { self.root.join("deploy") }
    pub fn workspace(&self) -> PathBuf { self.root.join("workspace") }
    pub fn logs(&self) -> PathBuf { self.root.join("logs") }
    pub fn status(&self) -> PathBuf { self.root.join("status.json") }
    pub fn config(&self) -> PathBuf { self.root.join("config.json") }
    fn creds_file(&self) -> PathBuf { self.root.join("credentials.json") }
    fn lock_file(&self) -> PathBuf { self.root.join("credentials.lock") }
}

#[derive(Serialize, Deserialize, Default, Clone, Debug)]
pub struct Config {
    #[serde(default)]
    pub server: String,
}

impl Config {
    pub fn load(p: &Paths) -> Config {
        let mut c: Config = fs::read(p.config()).ok().and_then(|b| serde_json::from_slice(&b).ok()).unwrap_or_default();
        if let Ok(s) = std::env::var("SUBSTRATE_SERVER") {
            c.server = s;
        }
        if c.server.is_empty() {
            c.server = "https://substrate.gliph.sh".into();
        }
        c.server = c.server.trim_end_matches('/').to_string();
        c
    }
    pub fn save(&self, p: &Paths) -> Result<()> {
        write_private(&p.config(), &serde_json::to_vec_pretty(self)?)
    }
}

#[derive(Serialize, Deserialize, Default, Clone, Debug)]
pub struct Creds {
    pub machine_id: String,
    pub refresh_token: String,
    #[serde(default)]
    pub access_token: String,
    /// Unix seconds.
    #[serde(default)]
    pub access_expires: i64,
}

/// Write a file only this user can read, atomically.
pub fn write_private(path: &Path, bytes: &[u8]) -> Result<()> {
    let tmp = path.with_extension("tmp");
    {
        let mut f = OpenOptions::new().write(true).create(true).truncate(true).mode(0o600).open(&tmp)?;
        f.write_all(bytes)?;
        f.sync_all()?;
    }
    fs::rename(&tmp, path)?;
    Ok(())
}

pub struct Store {
    paths: Paths,
    account: String,
    keychain: bool,
}

impl Store {
    pub fn new(paths: &Paths, server: &str) -> Store {
        let keychain = cfg!(target_os = "macos") && std::env::var("SUBSTRATE_NO_KEYCHAIN").is_err();
        Store { paths: paths.clone(), account: server.to_string(), keychain }
    }

    pub fn backend(&self) -> &'static str {
        if self.keychain { "macOS Keychain" } else { "file (0600)" }
    }

    pub fn lock(&self) -> Result<File> {
        let f = OpenOptions::new().create(true).truncate(false).write(true).mode(0o600).open(self.paths.lock_file())?;
        f.lock_exclusive()?;
        Ok(f)
    }

    pub fn load(&self) -> Result<Option<Creds>> {
        if self.keychain {
            let out = Command::new("/usr/bin/security").args(["find-generic-password", "-s", SERVICE, "-a", &self.account, "-w"])
                .stderr(Stdio::null()).output();
            if let Ok(o) = out {
                if o.status.success() {
                    let s = String::from_utf8_lossy(&o.stdout);
                    return Ok(serde_json::from_str(s.trim()).ok());
                }
                return Ok(None);
            }
        }
        match fs::read(self.paths.creds_file()) {
            Ok(b) => Ok(serde_json::from_slice(&b).ok()),
            Err(_) => Ok(None),
        }
    }

    pub fn save(&self, c: &Creds) -> Result<()> {
        let json = serde_json::to_string(c)?;
        if self.keychain {
            // The secret goes over stdin to `security -i`, never on argv where
            // other processes could read it.
            let cmd = format!("add-generic-password -U -s {SERVICE} -a {} -l Substrate -w {}\n", shell_quote(&self.account), shell_quote(&json));
            let mut child = Command::new("/usr/bin/security").arg("-i").stdin(Stdio::piped()).stdout(Stdio::null()).stderr(Stdio::piped()).spawn()?;
            child.stdin.as_mut().unwrap().write_all(cmd.as_bytes())?;
            drop(child.stdin.take());
            let out = child.wait_with_output()?;
            if out.status.success() && out.stderr.is_empty() {
                return Ok(());
            }
            anyhow::bail!("could not write to the Keychain: {}", String::from_utf8_lossy(&out.stderr).trim());
        }
        write_private(&self.paths.creds_file(), json.as_bytes())
    }

    pub fn clear(&self) -> Result<()> {
        if self.keychain {
            let _ = Command::new("/usr/bin/security").args(["delete-generic-password", "-s", SERVICE, "-a", &self.account]).stdout(Stdio::null()).stderr(Stdio::null()).status();
        }
        let _ = fs::remove_file(self.paths.creds_file());
        Ok(())
    }
}

fn shell_quote(s: &str) -> String {
    format!("\"{}\"", s.replace('\\', "\\\\").replace('"', "\\\""))
}

#[derive(Serialize, Deserialize, Default, Clone, Debug)]
pub struct Status {
    pub connected: bool,
    pub since: String,
    pub last_heartbeat: String,
    pub pid: u32,
    pub version: String,
    pub error: String,
    pub projects: Vec<ProjectStatus>,
}

#[derive(Serialize, Deserialize, Default, Clone, Debug)]
pub struct ProjectStatus {
    pub handle: String,
    pub pane_id: String,
    pub state: String,
    pub queued: usize,
}

impl Status {
    pub fn load(p: &Paths) -> Option<Status> {
        fs::read(p.status()).ok().and_then(|b| serde_json::from_slice(&b).ok())
    }
    pub fn save(&self, p: &Paths) {
        let _ = write_private(&p.status(), &serde_json::to_vec_pretty(self).unwrap_or_default());
    }
}

/// One JSON line to logs/daemon.log, rotated at 5 MB.
pub fn log(p: &Paths, level: &str, msg: &str, fields: serde_json::Value) {
    let path = p.logs().join("daemon.log");
    if fs::metadata(&path).map(|m| m.len() > 5 * 1024 * 1024).unwrap_or(false) {
        let _ = fs::rename(&path, p.logs().join("daemon.log.1"));
    }
    let mut line = serde_json::json!({"ts": chrono::Utc::now().to_rfc3339(), "level": level, "msg": msg});
    if let (Some(o), Some(f)) = (line.as_object_mut(), fields.as_object()) {
        for (k, v) in f {
            o.insert(k.clone(), v.clone());
        }
    }
    if let Ok(mut f) = OpenOptions::new().create(true).append(true).mode(0o600).open(&path) {
        let _ = writeln!(f, "{line}");
    }
    if std::env::var("SUBSTRATE_LOG_STDERR").is_ok() {
        eprintln!("{line}");
    }
}
