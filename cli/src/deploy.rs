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
        // CI: GitHub Actions through the developer's own `gh`.
        if matches!(job.stage.as_str(), "fix_committed" | "ci_running") && !job.ci_url.contains("gitlab") {
            if let Some((status, conclusion, url, id)) = gh_run(&job.cwd, &job.commit) {
                job.run_id = id;
                if job.ci_url.is_empty() {
                    job.ci_url = url;
                }
                match (status.as_str(), conclusion.as_str()) {
                    ("completed", "success") => return self.report(&mut job, "ci_passed", "").await,
                    ("completed", c) if !c.is_empty() => return self.report(&mut job, "ci_failed", c).await,
                    _ => return self.report(&mut job, "ci_running", "").await,
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

/// The newest GitHub Actions run for a commit: (status, conclusion, url, id).
fn gh_run(cwd: &str, commit: &str) -> Option<(String, String, String, String)> {
    if commit.is_empty() {
        return None;
    }
    let mut cmd = Command::new("gh");
    cmd.args(["run", "list", "--commit", commit, "--json", "databaseId,status,conclusion,url", "-L", "1"]);
    if !cwd.is_empty() {
        cmd.current_dir(cwd);
    }
    let out = cmd.output().ok()?;
    if !out.status.success() {
        return None;
    }
    let v: Value = serde_json::from_slice(&out.stdout).ok()?;
    let r = v.as_array()?.first()?;
    Some((
        r["status"].as_str().unwrap_or("").into(),
        r["conclusion"].as_str().unwrap_or("").into(),
        r["url"].as_str().unwrap_or("").into(),
        r["databaseId"].as_i64().map(|i| i.to_string()).unwrap_or_default(),
    ))
}
