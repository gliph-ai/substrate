//! Deploy watching. `substrate deploy track <id>` writes a job; the daemon
//! polls the pipeline with the developer's own tools (`gh` for GitHub
//! Actions) and the retest URL, and reports each observed stage. The cloud
//! never sees a CI or Pushleft credential. Silence for 45 minutes reports
//! `unknown`; nothing is ever reported as deployed without being observed or
//! reported by the agent.

use std::path::PathBuf;
use std::process::Command;
use std::sync::Arc;
use std::time::Duration;

use anyhow::Result;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use substrate_proto::{DeployReport, Report};

use crate::api::Api;
use crate::config::{log, write_private, Paths};

#[derive(Serialize, Deserialize, Clone, Debug, Default)]
pub struct Job {
    pub message_id: String,
    pub reply_token: String,
    pub commit: String,
    #[serde(default)]
    pub ci_url: String,
    #[serde(default)]
    pub env: String,
    #[serde(default)]
    pub verify_url: String,
    #[serde(default)]
    pub expect_sha: bool,
    #[serde(default)]
    pub cwd: String,
    #[serde(default)]
    pub stage: String,
    #[serde(default)]
    pub changed_at: i64,
    #[serde(default)]
    pub run_id: String,
}

pub fn job_path(p: &Paths, mid: &str) -> PathBuf {
    p.deploy().join(format!("{}.json", substrate_proto::safe_filename(mid)))
}

pub struct Watcher {
    paths: Paths,
    api: Arc<Api>,
}

impl Watcher {
    pub fn new(paths: Paths, api: Arc<Api>) -> Watcher {
        Watcher { paths, api }
    }

    pub async fn run(self) {
        loop {
            for job in self.jobs() {
                if let Err(e) = self.step(job).await {
                    log(&self.paths, "warn", "deploy watch", json!({"error": e.to_string()}));
                }
            }
            tokio::time::sleep(Duration::from_secs(15)).await;
        }
    }

    fn jobs(&self) -> Vec<Job> {
        std::fs::read_dir(self.paths.deploy()).into_iter().flatten().flatten()
            .filter_map(|e| std::fs::read(e.path()).ok()).filter_map(|b| serde_json::from_slice(&b).ok()).collect()
    }

    async fn report(&self, job: &mut Job, stage: &str, note: &str) -> Result<()> {
        if job.stage == stage {
            return Ok(());
        }
        let r = Report {
            message_id: job.message_id.clone(), reply_token: job.reply_token.clone(), kind: "deploy".into(), text: String::new(),
            deploy: Some(DeployReport { stage: stage.into(), commit: job.commit.clone(), ci_url: job.ci_url.clone(), env: job.env.clone(),
                verify_url: job.verify_url.clone(), run_id: job.run_id.clone(), note: note.into() }),
            attachments: vec![],
        };
        self.api.call("POST", "/relay/report", Some(serde_json::to_value(&r)?)).await?;
        job.stage = stage.into();
        job.changed_at = chrono::Utc::now().timestamp();
        log(&self.paths, "info", "deploy stage", json!({"message_id": job.message_id, "stage": stage}));
        if matches!(stage, "ready_to_retest" | "ci_failed" | "deploy_failed" | "unknown" | "not_deploying") {
            let _ = std::fs::remove_file(job_path(&self.paths, &job.message_id));
        } else {
            write_private(&job_path(&self.paths, &job.message_id), &serde_json::to_vec_pretty(job)?)?;
        }
        Ok(())
    }

    async fn step(&self, mut job: Job) -> Result<()> {
        let now = chrono::Utc::now().timestamp();
        if job.stage.is_empty() {
            return self.report(&mut job, "fix_committed", "").await;
        }
        if now - job.changed_at > 45 * 60 {
            return self.report(&mut job, "unknown", "no change observed for 45 minutes").await;
        }
        // CI and deploy: every GitHub Actions run for the commit, through the developer's
        // own `gh`. A workflow named deploy, release or publish is the deploy; the rest is CI.
        if matches!(job.stage.as_str(), "fix_committed" | "ci_running" | "ci_passed" | "deploying") && !job.ci_url.contains("gitlab") {
            let runs = gh_runs(&job.cwd, &job.commit);
            if !runs.is_empty() {
                let is_deploy = |r: &Run| ["deploy", "release", "publish"].iter().any(|k| r.name.to_lowercase().contains(k));
                let (deploy, ci): (Vec<&Run>, Vec<&Run>) = runs.iter().partition(|r| is_deploy(r));
                let done = |r: &Run| r.status == "completed";
                let failed = |r: &Run| done(r) && !matches!(r.conclusion.as_str(), "success" | "skipped" | "neutral");
                if let Some(r) = ci.first() {
                    job.run_id = r.id.clone();
                    if job.ci_url.is_empty() {
                        job.ci_url = r.url.clone();
                    }
                }
                if let Some(r) = ci.iter().copied().find(|r| failed(r)) {
                    return self.report(&mut job, "ci_failed", &r.conclusion).await;
                }
                if !ci.iter().all(|r| done(r)) {
                    return self.report(&mut job, "ci_running", "").await;
                }
                if deploy.is_empty() {
                    return self.report(&mut job, "ci_passed", "").await;
                }
                if let Some(r) = deploy.iter().copied().find(|r| failed(r)) {
                    return self.report(&mut job, "deploy_failed", &format!("{}: {}", r.name, r.conclusion)).await;
                }
                if !deploy.iter().all(|r| done(r)) {
                    return self.report(&mut job, "deploying", "").await;
                }
                // The deploy workflow finished green: that is an observed deploy. Without a
                // retest URL to check, it is ready to retest; with one, the live check decides.
                self.report(&mut job, "deployed", &deploy.iter().map(|r| r.name.as_str()).collect::<Vec<_>>().join(", ")).await?;
                if job.verify_url.is_empty() {
                    return self.report(&mut job, "ready_to_retest", "").await;
                }
            }
        }
        // Live check: the retest URL serves the new commit (when asked to
        // check for it) and answers 2xx.
        if matches!(job.stage.as_str(), "ci_passed" | "deploying" | "deployed") && !job.verify_url.is_empty() {
            if let Ok(r) = self.api.http.get(&job.verify_url).timeout(Duration::from_secs(10)).send().await {
                let ok = r.status().is_success();
                let header_sha = r.headers().iter().any(|(_, v)| v.to_str().map(|v| short(&job.commit).map(|s| v.contains(s)).unwrap_or(false)).unwrap_or(false));
                let body = r.text().await.unwrap_or_default();
                let live = ok && (!job.expect_sha || header_sha || short(&job.commit).map(|s| body.contains(s)).unwrap_or(false));
                if live && (job.expect_sha || job.stage == "deployed") {
                    if job.stage != "deployed" {
                        self.report(&mut job, "deployed", "").await?;
                    }
                    self.report(&mut job, "verified", "").await?;
                    return self.report(&mut job, "ready_to_retest", "").await;
                }
            }
        }
        Ok(())
    }
}

fn short(sha: &str) -> Option<&str> {
    (sha.len() >= 7).then(|| &sha[..7])
}

pub struct Run {
    pub name: String,
    pub status: String,
    pub conclusion: String,
    pub url: String,
    pub id: String,
}

/// Every GitHub Actions run for a commit. `gh run list --commit` matches only a full
/// sha, and agents often report a short one, so it is expanded first.
fn gh_runs(cwd: &str, commit: &str) -> Vec<Run> {
    if commit.is_empty() {
        return vec![];
    }
    let full = full_sha(cwd, commit).unwrap_or_else(|| commit.to_string());
    let mut cmd = Command::new("gh");
    cmd.args(["run", "list", "--commit", &full, "--json", "databaseId,status,conclusion,url,workflowName", "-L", "20"]);
    if !cwd.is_empty() {
        cmd.current_dir(cwd);
    }
    let Some(out) = cmd.output().ok().filter(|o| o.status.success()) else { return vec![] };
    let v: Value = serde_json::from_slice(&out.stdout).unwrap_or_default();
    v.as_array().into_iter().flatten().map(|r| Run {
        name: r["workflowName"].as_str().unwrap_or("").into(),
        status: r["status"].as_str().unwrap_or("").into(),
        conclusion: r["conclusion"].as_str().unwrap_or("").into(),
        url: r["url"].as_str().unwrap_or("").into(),
        id: r["databaseId"].as_i64().map(|i| i.to_string()).unwrap_or_default(),
    }).collect()
}

pub fn full_sha(cwd: &str, commit: &str) -> Option<String> {
    let mut cmd = Command::new("git");
    cmd.args(["rev-parse", "--verify", "--quiet", &format!("{commit}^{{commit}}")]);
    if !cwd.is_empty() {
        cmd.current_dir(cwd);
    }
    let out = cmd.output().ok().filter(|o| o.status.success())?;
    Some(String::from_utf8_lossy(&out.stdout).trim().to_string()).filter(|s| s.len() == 40)
}

#[cfg(test)]
mod tests {
    #[test]
    fn short_sha_expands_for_gh() {
        let dir = tempfile::tempdir().unwrap();
        let d = dir.path().to_str().unwrap();
        let git = |a: &[&str]| assert!(std::process::Command::new("git").args(a).current_dir(d).output().unwrap().status.success());
        git(&["init", "-q"]);
        git(&["-c", "user.email=t@t", "-c", "user.name=t", "commit", "-q", "--allow-empty", "-m", "x"]);
        let full = super::full_sha(d, "HEAD").unwrap();
        assert_eq!(super::full_sha(d, &full[..7]).as_deref(), Some(full.as_str()));
        assert!(super::full_sha(d, "nothere").is_none());
    }
}
