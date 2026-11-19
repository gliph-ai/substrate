//! `substrate guard`: a Claude Code PreToolUse hook that makes a tester's deploy
//! permission real. While the agent in a session works on a Substrate message, the
//! server-stamped allowance on that delivery (production, test or none) decides
//! which push and deploy commands may run; the rest are blocked (exit 2), whatever
//! the prompt said. Outside a Substrate message nothing is checked.
//!
//! It matches command text, so it stops the agent's ordinary routes to a deploy
//! (git push, pl up, a release script), not a deliberate disguise such as a new
//! script that calls git push. A project adds its own commands in
//! `.substrate/guard.json`: {"production": ["bin/ship"], "test": ["bin/ship --staging"]}.

use std::path::Path;

use serde_json::Value;

use crate::config::Paths;
use crate::daemon::Record;

/// Commands that put a change in front of users. Matched on whole words.
pub const PRODUCTION: &[&str] = &[
    "git push", "pl up", "pl deploy", "pl release", "pl storage cp", "gh workflow run", "gh release create",
    "kubectl apply", "kubectl rollout", "kubectl set image", "helm upgrade", "helm install", "terraform apply",
    "fly deploy", "vercel --prod", "netlify deploy --prod", "npm publish", "cargo publish", "docker push",
    "wrangler deploy", "wrangler publish", "firebase deploy", "eas submit", "fastlane", "testflight",
    "bin/release", "bin/deploy", "bin/ota", "release.sh", "deploy.sh", "aws s3 cp", "aws s3 sync",
];
/// Deploys that only reach a test or staging environment.
/// A production-shaped command that names staging or --preview counts as test too.
pub const TEST: &[&str] = &["vercel deploy", "netlify deploy"];

/// The active Substrate deliveries for this session or folder, strictest first.
fn active(paths: &Paths, session: &str, cwd: &str) -> Vec<Record> {
    let dir = paths.dispatch();
    let mut out: Vec<Record> = std::fs::read_dir(&dir).into_iter().flatten().flatten()
        .filter(|e| e.path().extension().is_some_and(|x| x == "json"))
        .filter_map(|e| std::fs::read(e.path()).ok())
        .filter_map(|b| serde_json::from_slice::<Record>(&b).ok())
        .filter(|r| matches!(r.state.as_str(), "submitting" | "submitted" | "working"))
        .filter(|r| !dir.join(format!("{}.final", substrate_proto::safe_filename(&r.message_id))).exists())
        .filter(|r| (!session.is_empty() && r.agent_session == session) || (!r.cwd.is_empty() && Path::new(cwd).starts_with(&r.cwd)))
        .collect();
    out.sort_by_key(|r| rank(&r.deploy));
    out
}

fn rank(allow: &str) -> u8 {
    match allow { "production" => 2, "test" => 1, _ => 0 }
}

/// The commands in a shell line: split on ; && || | and newlines, quoted text
/// dropped (a commit message that says "push" is not a push), tokens kept.
fn segments(cmd: &str) -> Vec<Vec<String>> {
    let mut clean = String::new();
    let mut q: Option<char> = None;
    for c in cmd.chars() {
        match q {
            Some(o) if c == o => q = None,
            Some(_) => {}
            None if c == '\'' || c == '"' => q = Some(c),
            None => clean.push(if matches!(c, ';' | '|' | '&' | '\n' | '(' | ')' | '`') { '\n' } else { c }),
        }
    }
    clean.lines().map(|l| l.split_whitespace().map(String::from).collect::<Vec<_>>()).filter(|t| !t.is_empty()).collect()
}

/// A pattern matches a command when its first word is the program run (any path,
/// after env assignments and sudo) and its other words follow in order, so
/// `pl --org x up` is `pl up` and `git -C dir push` is `git push`. A pattern with
/// a slash matches a program path that contains it (bin/release, ./bin/release-desktop).
fn matches(seg: &[String], pat: &str) -> bool {
    let words: Vec<&str> = pat.split_whitespace().collect();
    let Some(start) = seg.iter().position(|t| !t.contains('=') && t != "sudo" && t != "env" && t != "exec") else { return false };
    let prog = &seg[start];
    let first_ok = if words[0].contains('/') || words[0].ends_with(".sh") { prog.contains(words[0]) } else { prog.rsplit('/').next() == Some(words[0]) };
    if !first_ok {
        return false;
    }
    let mut rest = seg[start + 1..].iter();
    words[1..].iter().all(|w| rest.any(|t| t == w || (w.starts_with("--") && t.starts_with(&format!("{w}=")))))
}

/// production, test or "" (not a deploy) for a shell command line.
pub fn classify(cmd: &str, extra_prod: &[String], extra_test: &[String]) -> &'static str {
    let segs = segments(cmd);
    let any = |pats: Vec<&str>| segs.iter().any(|s| pats.iter().any(|p| matches(s, p)));
    let prod: Vec<&str> = PRODUCTION.iter().copied().chain(extra_prod.iter().map(String::as_str)).collect();
    let test: Vec<&str> = TEST.iter().copied().chain(extra_test.iter().map(String::as_str)).collect();
    // A line that deploys to production anywhere counts as production.
    let is_test = |s: &Vec<String>| test.iter().any(|p| matches(s, p)) || s.iter().any(|t| t.contains("staging") || t == "--preview");
    if segs.iter().any(|s| prod.iter().any(|p| matches(s, p)) && !is_test(s)) {
        return "production";
    }
    if any(test) || segs.iter().any(|s| prod.iter().any(|p| matches(s, p))) {
        return "test";
    }
    ""
}

fn project_patterns(root: &str) -> (Vec<String>, Vec<String>) {
    let v: Value = std::fs::read(Path::new(root).join(".substrate/guard.json")).ok().and_then(|b| serde_json::from_slice(&b).ok()).unwrap_or_default();
    let list = |k: &str| v[k].as_array().map(|a| a.iter().filter_map(|x| x.as_str().map(String::from)).collect()).unwrap_or_default();
    (list("production"), list("test"))
}

/// Ok to run, or the reason to block. `input` is Claude Code's hook JSON.
pub fn check(paths: &Paths, input: &Value) -> Result<(), String> {
    if input["tool_name"].as_str() != Some("Bash") {
        return Ok(());
    }
    let cmd = input["tool_input"]["command"].as_str().unwrap_or("");
    let recs = active(paths, input["session_id"].as_str().unwrap_or(""), input["cwd"].as_str().unwrap_or(""));
    let Some(r) = recs.first() else { return Ok(()) };
    let (p, t) = project_patterns(&r.cwd);
    let kind = classify(cmd, &p, &t);
    if kind.is_empty() || rank(&r.deploy) >= rank(kind) {
        return Ok(());
    }
    let allowed = match r.deploy.as_str() { "test" => "to deploy to test only", _ => "no deploy" };
    Err(format!(
        "Substrate blocked this: it counts as a {kind} deploy, and the tester behind message {} was allowed {allowed}. \
Finish the change without deploying it and say so in your reply. The developer deploys it, or allows it for this tester.",
        r.message_id
    ))
}

/// Add the hook to ~/.claude/settings.json once, after a backup. Returns whether it changed.
pub fn install(exe: &str) -> anyhow::Result<bool> {
    let home = std::env::var("HOME")?;
    let path = Path::new(&home).join(".claude/settings.json");
    let raw = std::fs::read_to_string(&path).unwrap_or_else(|_| "{}".into());
    let mut v: Value = serde_json::from_str(&raw)?;
    let cmd = format!("{exe} guard");
    let list = v.as_object_mut().ok_or_else(|| anyhow::anyhow!("settings.json is not an object"))?
        .entry("hooks").or_insert_with(|| serde_json::json!({}))
        .as_object_mut().ok_or_else(|| anyhow::anyhow!("hooks is not an object"))?
        .entry("PreToolUse").or_insert_with(|| serde_json::json!([]));
    let arr = list.as_array_mut().ok_or_else(|| anyhow::anyhow!("PreToolUse is not a list"))?;
    if arr.iter().any(|h| h.to_string().contains(" guard\"")) {
        return Ok(false);
    }
    arr.push(serde_json::json!({"matcher": "Bash", "hooks": [{"type": "command", "command": cmd}]}));
    if path.exists() {
        std::fs::copy(&path, path.with_extension("json.before-substrate"))?;
    }
    std::fs::create_dir_all(path.parent().unwrap())?;
    std::fs::write(&path, serde_json::to_string_pretty(&v)? + "\n")?;
    Ok(true)
}

pub fn installed() -> bool {
    let home = std::env::var("HOME").unwrap_or_default();
    std::fs::read_to_string(Path::new(&home).join(".claude/settings.json")).is_ok_and(|s| s.contains(" guard\""))
}

#[cfg(test)]
mod tests {
    use super::classify;

    #[test]
    fn deploy_commands() {
        assert_eq!(classify("git push origin HEAD:main", &[], &[]), "production");
        assert_eq!(classify("cd x && pl --org empire up", &[], &[]), "production");
        assert_eq!(classify("git -C ../app push", &[], &[]), "production");
        assert_eq!(classify("git commit -m 'push the fix'", &[], &[]), "");
        assert_eq!(classify("FOO=1 sudo git push", &[], &[]), "production");
        assert_eq!(classify("vercel deploy", &[], &[]), "test");
        assert_eq!(classify("git push origin staging && git push origin main", &[], &[]), "production");
        assert_eq!(classify("pl up --production", &[], &[]), "production");
        assert_eq!(classify("git push origin staging", &[], &[]), "test");
        assert_eq!(classify("./bin/release-desktop", &[], &[]), "production");
        assert_eq!(classify("git status && cargo test", &[], &[]), "");
        assert_eq!(classify("echo pushing", &[], &[]), "");
        assert_eq!(classify("bin/ship now", &["bin/ship".into()], &[]), "production");
    }
}
