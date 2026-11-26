//! The work log: every Claude Code and Codex turn on this Mac, summarised here and
//! uploaded to Substrate (ported from Theasa). The Stop hooks parse the transcript
//! into `worklog/queue/`; the daemon uploads the turns, keeps their raw log, diffs and
//! images in `worklog/turns/` (the cloud asks for those only through a `request`), and
//! writes each summary with the agent's own CLI.

pub mod hook;
pub mod live;
pub mod parse;
pub mod summarize;
pub mod worker;

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::{Mutex, OnceLock};

use regex::Regex;
use sha2::{Digest, Sha256};
use substrate_proto::{LogRepo, TurnRef};

use crate::config::Paths;

/// `<root>/worklog/<sub>`, made owner-only on first use.
pub fn dir(paths: &Paths, sub: &str) -> PathBuf {
    let d = paths.root.join("worklog").join(sub);
    if !d.exists() {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::create_dir_all(&d);
        for p in [paths.root.join("worklog"), d.clone()] {
            let _ = std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o700));
        }
    }
    d
}

pub fn sha(s: &str) -> String {
    hex::encode(Sha256::digest(s.as_bytes()))[..32].to_string()
}

pub fn turn_ref_key(t: &TurnRef) -> String {
    sha(&format!("{}:{}:{}", t.agent, t.session_id, t.turn_key))
}

/// Mask secrets before anything leaves the machine or reaches the summariser.
/// ponytail: a regex list, not a secret scanner. Add patterns when one slips through.
pub fn redact(s: &str) -> String {
    static PATS: OnceLock<Vec<(Regex, &'static str)>> = OnceLock::new();
    let pats = PATS.get_or_init(|| {
        [
            (r"-----BEGIN [A-Z ]*PRIVATE KEY-----[\s\S]*?-----END [A-Z ]*PRIVATE KEY-----", "[private key]"),
            (r"\bsk-ant-[A-Za-z0-9_-]{10,}", "sk-ant-[masked]"),
            (r"\bsk-(?:proj-|live_|test_)?[A-Za-z0-9_-]{16,}", "sk-[masked]"),
            (r"\b(?:rk|pk)_(?:live|test)_[A-Za-z0-9]{10,}", "[stripe key masked]"),
            (r"\bwhsec_[A-Za-z0-9]{10,}", "whsec_[masked]"),
            (r"\bgh[pousr]_[A-Za-z0-9]{20,}", "gh_[masked]"),
            (r"\bxox[abpr]-[A-Za-z0-9-]{10,}", "xox-[masked]"),
            (r"\bAKIA[0-9A-Z]{16}\b", "AKIA[masked]"),
            (r"\bAIza[0-9A-Za-z_-]{30,}", "AIza[masked]"),
            (r"\beyJ[A-Za-z0-9_-]{10,}\.[A-Za-z0-9_-]{10,}\.[A-Za-z0-9_-]{10,}", "[jwt masked]"),
            (r"(?i)(\b(?:Bearer|Basic)\s+)[A-Za-z0-9._~+/=-]{16,}", "${1}[masked]"),
            (r#"(\b[A-Z0-9_]*(?:SECRET|TOKEN|PASSWORD|PASSWD|API_KEY|PRIVATE_KEY|ACCESS_KEY)[A-Z0-9_]*\s*[=:]\s*)["']?[^\s"'`]{4,}["']?"#, "${1}[masked]"),
            (r"(?i)(\b[a-z][a-z0-9+.-]*://[^\s:@/]+:)[^\s@/]{3,}(@)", "${1}[masked]${2}"),
        ]
        .into_iter().map(|(p, r)| (Regex::new(p).unwrap(), r)).collect()
    });
    let mut out = s.to_string();
    for (re, to) in pats {
        out = re.replace_all(&out, *to).into_owned();
    }
    out
}

fn git(cwd: &str, args: &[&str]) -> String {
    Command::new("git").arg("-C").arg(cwd).args(args).stderr(std::process::Stdio::null()).output().ok()
        .filter(|o| o.status.success()).map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string()).unwrap_or_default()
}

/// `git@host:a/b.git`, `https://Host/a/b`, `ssh://git@host:22/a/b` → `host/a/b`.
pub fn norm_remote(url: &str) -> String {
    let mut s = url.trim().trim_end_matches(".git").trim_end_matches('/').to_string();
    if let Some(i) = s.find("://").filter(|&i| s[..i].chars().all(|c| c.is_ascii_alphabetic() || c == '+')) {
        s = s[i + 3..].to_string();
    }
    if let Some(i) = s.find('@').filter(|&i| !s[..i].contains('/')) {
        s = s[i + 1..].to_string();
    }
    // host:path (scp form) or host:port/path
    if let Some(i) = s.find(':').filter(|&i| !s[..i].contains('/')) {
        let rest = &s[i + 1..];
        let digits = rest.chars().take_while(char::is_ascii_digit).count();
        s = if digits > 0 && rest[digits..].starts_with('/') { format!("{}{}", &s[..i], &rest[digits..]) } else { format!("{}/{}", &s[..i], rest) };
    }
    match s.split_once('/') {
        Some((host, rest)) => format!("{}/{rest}", host.to_lowercase()),
        None => s.to_lowercase(),
    }
}

/// A deleted agent worktree (.claude/worktrees/agent-x) still belongs to its repo.
pub fn strip_worktree(cwd: &str) -> &str {
    cwd.find("/.claude/worktrees/").map(|i| &cwd[..i]).unwrap_or(cwd)
}

/// Project identity: the git remote when there is one, else the main worktree path.
pub fn project_for(cwd_in: &str) -> LogRepo {
    static CACHE: OnceLock<Mutex<HashMap<String, LogRepo>>> = OnceLock::new();
    let cache = CACHE.get_or_init(Default::default);
    if let Some(r) = cache.lock().unwrap().get(cwd_in) {
        return r.clone();
    }
    let cwd = if Path::new(cwd_in).exists() { cwd_in } else { strip_worktree(cwd_in) };
    let top = if Path::new(cwd).exists() { git(cwd, &["rev-parse", "--show-toplevel"]) } else { String::new() };
    let base = |p: &str| p.rsplit('/').find(|x| !x.is_empty()).unwrap_or(p).to_string();
    let r = if !top.is_empty() {
        let common = git(cwd, &["rev-parse", "--path-format=absolute", "--git-common-dir"]);
        let root = common.strip_suffix("/.git").map(String::from).unwrap_or(top);
        let remote = norm_remote(&git(&root, &["config", "--get", "remote.origin.url"]));
        LogRepo {
            key: if remote.is_empty() { root.clone() } else { remote.clone() },
            name: base(&root),
            branch: git(cwd, &["rev-parse", "--abbrev-ref", "HEAD"]),
            head: git(cwd, &["rev-parse", "--short", "HEAD"]),
            root,
            remote,
        }
    } else {
        let root = strip_worktree(cwd_in).to_string();
        LogRepo { key: root.clone(), name: base(&root), root, ..Default::default() }
    };
    cache.lock().unwrap().insert(cwd_in.to_string(), r.clone());
    r
}

pub fn now_s() -> i64 {
    chrono::Utc::now().timestamp()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn redacts_secrets() {
        let s = redact("STRIPE_SECRET_KEY=sk_live_abc123456789xyz Authorization: Bearer abcdefghijklmnopqrstuvwx ghp_abcdefghijklmnopqrstuvwxyz123 postgres://u:hunter22@db/x"); // gitleaks:allow
        for gone in ["abc123456789xyz", "abcdefghijklmnopqrstuvwx ", "ghp_abcdefghij", "hunter22"] {
            assert!(!s.contains(gone), "{gone} in {s}");
        }
        assert_eq!(redact("PORT=4747 name=theasa"), "PORT=4747 name=theasa");
    }

    #[test]
    fn repo_identity() {
        assert_eq!(norm_remote("git@git.example.com:api-server/api-server.git"), "git.example.com/api-server/api-server");
        assert_eq!(norm_remote("https://GitHub.com/example-org/crm.git"), "github.com/example-org/crm");
        assert_eq!(norm_remote("ssh://git@host:2222/a/b.git"), "host/a/b");
        assert_eq!(strip_worktree("/x/api-server/.claude/worktrees/agent-1/sub"), "/x/api-server");
    }
}
