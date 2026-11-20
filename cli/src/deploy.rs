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
    /// The CI URL was found by the watcher, not given by the agent: it links, but
    /// does not pin which pipeline is read.
    #[serde(default)]
    pub ci_url_found: bool,
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
        // CI and deploy: every run for the commit, through the developer's own `gh`
        // (GitHub Actions) or `glab` (GitLab pipeline jobs). A workflow named deploy,
        // release or publish, or a GitLab job in a deploy stage, is the deploy; the rest is CI.
        if matches!(job.stage.as_str(), "fix_committed" | "ci_running" | "ci_passed" | "deploying") {
            // `gh` first (github.com and GitHub Enterprise), `glab` when it cannot read the
            // repo; a CI URL that is not GitHub's picks `glab` first.
            let (gh, gl) = (|| gh_runs(&job.cwd, &job.commit), || glab_runs(&job.cwd, &job.commit, if job.ci_url_found { "" } else { &job.ci_url }));
            let runs = if !job.ci_url.is_empty() && !job.ci_url.contains("github") { gl().or_else(gh) } else { gh().or_else(gl) };
            if runs.is_none() && now - job.changed_at < 20 {
                log(&self.paths, "warn", "deploy watch: neither gh nor glab can read this repo's CI", json!({"message_id": job.message_id, "cwd": job.cwd}));
            }
            let runs = runs.unwrap_or_default();
            if runs.is_empty() && !job.ci_url_found && pipeline_of(&job.ci_url).is_some() && now - job.changed_at < 20 {
                log(&self.paths, "warn", "deploy watch: the pipeline given has no jobs for this commit", json!({"message_id": job.message_id, "ci_url": job.ci_url, "commit": job.commit}));
            }
            if !runs.is_empty() {
                let is_deploy = |r: &Run| r.deploy || ["deploy", "release", "publish"].iter().any(|k| r.name.to_lowercase().contains(k));
                let (deploy, ci): (Vec<&Run>, Vec<&Run>) = runs.iter().partition(|r| is_deploy(r));
                // With an environment tracked, a GitHub run named for another one (staging
                // while tracking production) is not this deploy. A GitLab job's environment
                // is not in its API record, so there the target stays unconfirmed: CI is
                // followed, and the live check (a retest URL) decides the deploy.
                let deploy: Vec<&Run> = deploy.into_iter().filter(|r| job.env.is_empty() || (!r.gitlab && for_env(&r.name, &job.env))).collect();
                let done = |r: &Run| r.status == "completed";
                let failed = |r: &Run| done(r) && !matches!(r.conclusion.as_str(), "success" | "skipped" | "neutral");
                if let Some(r) = ci.first() {
                    job.run_id = r.id.clone();
                    if job.ci_url.is_empty() {
                        job.ci_url = r.url.clone();
                        job.ci_url_found = true;
                    }
                }
                if let Some(r) = ci.iter().copied().find(|r| failed(r)) {
                    return self.report(&mut job, "ci_failed", &r.conclusion).await;
                }
                if !ci.iter().all(|r| done(r)) {
                    return self.report(&mut job, "ci_running", "").await;
                }
                // Only a deploy run that succeeded is a deploy; one that was skipped,
                // waits for a manual start or failed allowed is not.
                if !deploy.iter().any(|r| r.conclusion == "success") && deploy.iter().all(|r| done(r) && !failed(r)) {
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
    /// GitLab: the job sits in a deploy stage.
    pub deploy: bool,
    pub gitlab: bool,
}

/// Every GitHub Actions run for a commit. `gh run list --commit` matches only a full
/// sha, and agents often report a short one, so it is expanded first. None when
/// `gh` cannot read the repo (not on GitHub, or not logged in).
fn gh_runs(cwd: &str, commit: &str) -> Option<Vec<Run>> {
    if commit.is_empty() {
        return Some(vec![]);
    }
    let full = full_sha(cwd, commit).unwrap_or_else(|| commit.to_string());
    let mut cmd = Command::new("gh");
    cmd.args(["run", "list", "--commit", &full, "--json", "databaseId,status,conclusion,url,workflowName", "-L", "20"]);
    if !cwd.is_empty() {
        cmd.current_dir(cwd);
    }
    let out = cmd.output().ok().filter(|o| o.status.success())?;
    let v: Value = serde_json::from_slice(&out.stdout).unwrap_or_default();
    Some(v.as_array().into_iter().flatten().map(|r| Run {
        name: r["workflowName"].as_str().unwrap_or("").into(),
        status: r["status"].as_str().unwrap_or("").into(),
        conclusion: r["conclusion"].as_str().unwrap_or("").into(),
        url: r["url"].as_str().unwrap_or("").into(),
        id: r["databaseId"].as_i64().map(|i| i.to_string()).unwrap_or_default(),
        deploy: false,
        gitlab: false,
    }).collect())
}

/// The jobs of every GitLab pipeline for a commit, through `glab api` in the repo
/// (`:id` is the repo's project), every page; a trigger job is replaced by the jobs
/// of the pipeline it started. Retried jobs are left out by GitLab. None when
/// `glab` cannot read the repo.
fn glab_runs(cwd: &str, commit: &str, ci_url: &str) -> Option<Vec<Run>> {
    if commit.is_empty() {
        return Some(vec![]);
    }
    let full = full_sha(cwd, commit).unwrap_or_else(|| commit.to_string());
    let api = |path: &str| -> Option<Vec<Value>> {
        let mut cmd = Command::new("glab");
        cmd.args(["api", "--paginate", path]);
        if !cwd.is_empty() {
            cmd.current_dir(cwd);
        }
        let out = cmd.output().ok().filter(|o| o.status.success())?;
        Some(json_items(&out.stdout))
    };
    let mut runs = vec![];
    // (project, pipeline, url, under a deploy-stage trigger, depth)
    // A tracked pipeline URL is that pipeline. Otherwise newest first: the first per ref
    // and source is the current one, older ones (a rerun's predecessors) are superseded.
    let mut seen = std::collections::HashSet::new();
    // A pipeline the agent named is the one read, and only when it ran this commit;
    // another commit's pipeline tells nothing, so no runs (the caller logs it).
    let tracked = pipeline_of(ci_url);
    if let Some((proj, id)) = &tracked {
        let p = api(&format!("projects/{proj}/pipelines/{id}"))?;
        if !p.first().and_then(|p| p["sha"].as_str()).is_some_and(|s| s.starts_with(&full)) {
            return Some(vec![]);
        }
    }
    let mut todo: Vec<(String, i64, String, bool, u8)> = match tracked {
        Some((proj, id)) => vec![(proj, id, ci_url.into(), false, 0)],
        None => api(&format!("projects/:id/pipelines?sha={full}&per_page=100&order_by=id&sort=desc"))?.iter()
            .filter(|p| p["source"] != "parent_pipeline" && seen.insert(format!("{}|{}", p["ref"], p["source"])))
            .map(|p| (":id".into(), p["id"].as_i64().unwrap_or(0), p["web_url"].as_str().unwrap_or("").into(), false, 0)).collect(),
    };
    while let Some((proj, pid, url, deploy, depth)) = todo.pop() {
        for j in api(&format!("projects/{proj}/pipelines/{pid}/jobs?per_page=100"))? {
            let mut r = gitlab_run(&j, pid, &url);
            r.deploy |= deploy;
            runs.push(r);
        }
        for b in api(&format!("projects/{proj}/pipelines/{pid}/bridges?per_page=100"))? {
            let d = &b["downstream_pipeline"];
            match d["id"].as_i64() {
                // ponytail: two levels of child pipelines; deeper ones count as their trigger.
                Some(id) if depth < 2 => {
                    // A multi-project trigger names its project only in the pipeline's URL.
                    let p = d["project_id"].as_i64().map(|i| i.to_string())
                        .or_else(|| pipeline_of(d["web_url"].as_str().unwrap_or("")).map(|x| x.0)).unwrap_or(proj.clone());
                    todo.push((p, id, url.clone(), deploy || is_deploy_stage(&b), depth + 1))
                }
                _ => {
                    let mut r = gitlab_run(&b, pid, &url);
                    r.deploy |= deploy;
                    runs.push(r);
                }
            }
        }
    }
    Some(runs)
}

/// Whether a deploy run named `name` can be the deploy to `env`.
fn for_env(name: &str, env: &str) -> bool {
    const ENVS: [&[&str]; 5] = [&["production", "prod", "live"], &["staging", "stage"], &["preview", "review"], &["development", "dev"], &["test", "qa", "acceptance"]];
    let (name, env) = (name.to_lowercase(), env.to_lowercase());
    let words: Vec<&str> = name.split(|c: char| !c.is_alphanumeric()).collect();
    let group = |e: &str| ENVS.iter().find(|g| g.contains(&e));
    if env.is_empty() || name.contains(&env) {
        return true;
    }
    let mine = group(&env);
    !words.iter().any(|w| group(w).is_some_and(|g| Some(g) != mine))
}

/// The project (URL-encoded path, as the API takes it) and id of a GitLab pipeline
/// URL: `https://host/group/repo/-/pipelines/42` → ("group%2Frepo", 42).
fn pipeline_of(url: &str) -> Option<(String, i64)> {
    let (repo, id) = url.split_once("/-/pipelines/")?;
    let id = id.split(|c: char| !c.is_ascii_digit()).next()?.parse().ok()?;
    let path = repo.split_once("://").map(|x| x.1).unwrap_or(repo).split_once('/')?.1;
    Some((path.replace('/', "%2F"), id))
}

fn is_deploy_stage(j: &Value) -> bool {
    j["stage"].as_str().unwrap_or("").to_lowercase().contains("deploy")
}

/// `--paginate` prints one JSON array per page back to back: their items, in order.
fn json_items(bytes: &[u8]) -> Vec<Value> {
    serde_json::Deserializer::from_slice(bytes).into_iter::<Value>().map_while(|v| v.ok())
        .flat_map(|v| match v { Value::Array(a) => a, v => vec![v] }).collect()
}

/// A GitLab job as a run. A manual job that may be skipped counts as skipped; a
/// blocking one (not allowed to fail) holds the pipeline, so it waits. An
/// allowed failure is neutral: it fails nothing, and deploys nothing.
fn gitlab_run(j: &Value, pipeline: i64, url: &str) -> Run {
    let st = j["status"].as_str().unwrap_or("");
    let optional = j["allow_failure"].as_bool().unwrap_or(false);
    let done = matches!(st, "success" | "failed" | "canceled" | "skipped") || (st == "manual" && optional);
    let conclusion = match st {
        "success" => "success",
        "failed" if optional => "neutral",
        "failed" => "failure",
        "canceled" => "cancelled",
        "skipped" | "manual" => "skipped",
        _ => "",
    };
    Run {
        name: j["name"].as_str().unwrap_or("").into(),
        status: if done { "completed" } else { "in_progress" }.into(),
        conclusion: conclusion.into(),
        url: url.into(),
        id: pipeline.to_string(),
        deploy: is_deploy_stage(j),
        gitlab: true,
    }
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

    #[test]
    fn gitlab_jobs_map_to_runs() {
        let run = |v: serde_json::Value| super::gitlab_run(&v, 7, "u");
        let r = run(serde_json::json!({"name": "api", "stage": "test", "status": "running"}));
        assert_eq!((r.status.as_str(), r.deploy), ("in_progress", false));
        let r = run(serde_json::json!({"name": "pushleft", "stage": "deploy", "status": "success"}));
        assert_eq!((r.status.as_str(), r.conclusion.as_str(), r.deploy), ("completed", "success", true));
        assert_eq!(run(serde_json::json!({"status": "failed", "allow_failure": true})).conclusion, "neutral");
        assert_eq!(run(serde_json::json!({"status": "failed"})).conclusion, "failure");
        let r = run(serde_json::json!({"status": "manual", "allow_failure": true}));
        assert_eq!((r.status.as_str(), r.conclusion.as_str()), ("completed", "skipped"));
        assert_eq!(run(serde_json::json!({"status": "manual", "allow_failure": false})).status, "in_progress");
        assert_eq!(run(serde_json::json!({"status": "pending"})).status, "in_progress");
        assert_eq!(super::pipeline_of("https://git.erip.me/tuli/tuli/-/pipelines/138327"), Some(("tuli%2Ftuli".into(), 138327)));
        assert_eq!(super::pipeline_of("https://github.com/a/b/actions/runs/1"), None);
        assert!(super::for_env("pushleft", "production"));
        assert!(super::for_env("Deploy", "production"));
        assert!(super::for_env("deploy-prod", "production"));
        assert!(!super::for_env("deploy-staging", "production"));
        assert!(!super::for_env("Deploy production", "staging"));
        assert!(super::for_env("deploy-staging", ""));
        assert_eq!(super::json_items(b"[{\"a\":1},{\"a\":2}]\n[{\"a\":3}]").len(), 3);
    }
}
