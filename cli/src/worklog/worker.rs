//! The daemon's half of the work log: upload queued turns, keep their local part, and
//! write summaries (two at a time, retried after 30 s and 2 min, then failed).
//!
//! On disk, under `worklog/`: `queue/` (hook batches), `turns/<key>.json` (the turn
//! with its log, diffs and image bytes), `jobs/<key>.json` (summary jobs and the report
//! still to post), `icons/<sha of repo key>` (when the repo's logo was last sent).

use std::path::PathBuf;
use std::sync::atomic::{AtomicI64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::Result;
use base64::Engine;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use substrate_proto::{LogAttachment, LogBatch, LogRepo, LogSummaryReport, LogTurn, TurnRef};

use super::hook::Batch;
use super::parse::Turn;
use super::{dir, now_s, sha, summarize, turn_ref_key};
use crate::api::Api;
use crate::config::{log, write_private, Paths};

/// A turn as the Mac keeps it.
#[derive(Serialize, Deserialize, Clone, Debug, Default)]
pub struct Stored {
    pub repo: LogRepo,
    pub turn: Turn,
    /// "" | skipped | queued | done | failed
    #[serde(default)]
    pub summary: String,
}

#[derive(Serialize, Deserialize, Clone, Debug, Default)]
pub struct Job {
    pub turn: TurnRef,
    /// queued | running | done | failed
    pub status: String,
    pub attempts: u32,
    pub next_at: i64,
    /// The state the cloud has not heard yet.
    #[serde(default)]
    pub report: Option<LogSummaryReport>,
}

pub fn turn_ref(t: &Turn) -> TurnRef {
    TurnRef { agent: t.agent.clone(), session_id: t.session_id.clone(), turn_key: t.turn_key.clone() }
}

fn store_path(paths: &Paths, r: &TurnRef) -> PathBuf {
    dir(paths, "turns").join(format!("{}.json", turn_ref_key(r)))
}

fn job_path(paths: &Paths, r: &TurnRef) -> PathBuf {
    dir(paths, "jobs").join(format!("{}.json", turn_ref_key(r)))
}

pub fn load(paths: &Paths, r: &TurnRef) -> Option<Stored> {
    std::fs::read(store_path(paths, r)).ok().and_then(|b| serde_json::from_slice(&b).ok())
}

fn save(paths: &Paths, s: &Stored) {
    let _ = write_private(&store_path(paths, &turn_ref(&s.turn)), &serde_json::to_vec(s).unwrap_or_default());
}

fn report(turn: &TurnRef, status: &str, error: &str) -> LogSummaryReport {
    LogSummaryReport { turn: turn.clone(), status: status.into(), error: error.chars().take(1000).collect(), ..Default::default() }
}

fn jobs(paths: &Paths) -> Vec<(PathBuf, Job)> {
    std::fs::read_dir(dir(paths, "jobs")).into_iter().flatten().flatten().map(|e| e.path())
        .filter(|p| p.extension().is_some_and(|x| x == "json"))
        .filter_map(|p| std::fs::read(&p).ok().and_then(|b| serde_json::from_slice::<Job>(&b).ok()).map(|j| (p, j))).collect()
}

/// The upload form: no log, no diffs, no image bytes. An image the Mac holds gets its index.
pub fn to_wire(t: &Turn, summary_status: &str) -> LogTurn {
    let mut n = 0;
    let attachments = t.attachments.iter().map(|a| {
        let image = (!a.data.is_empty()).then(|| { n += 1; n - 1 });
        LogAttachment { dir: a.dir.clone(), kind: a.kind.clone(), name: a.name.clone(), url: a.url.clone(), media_type: a.media_type.clone(), count: a.count, image }
    }).collect();
    LogTurn {
        id: turn_ref(t), started_at: t.started_at.clone(), ended_at: t.ended_at.clone(), duration_ms: t.duration_ms, tool_ms: t.tool_ms,
        prompt: t.prompt.clone(), final_text: t.final_text.clone(), model: t.model.clone(), tokens: t.tokens, model_calls: t.model_calls,
        tools: t.tools.clone(), files: t.files.clone(), attachments, cwd: t.cwd.clone(), summary_status: summary_status.into(),
    }
}

pub struct Worker {
    pub paths: Paths,
    api: Arc<Api>,
    running: AtomicUsize,
    /// Unix seconds: after a failed upload, wait before the next try.
    pause_until: AtomicI64,
    /// One read-modify-write of a job file at a time.
    jobs_lock: Mutex<()>,
}

impl Worker {
    pub fn new(paths: Paths, api: Arc<Api>) -> Arc<Worker> {
        let w = Arc::new(Worker { paths, api, running: AtomicUsize::new(0), pause_until: AtomicI64::new(0), jobs_lock: Mutex::new(()) });
        // A crash mid-job leaves running jobs: put them back in the queue.
        for (p, mut j) in jobs(&w.paths) {
            if j.status == "running" {
                j.status = "queued".into();
                let _ = write_private(&p, &serde_json::to_vec(&j).unwrap_or_default());
            }
        }
        w
    }

    fn update_job(&self, r: &TurnRef, f: impl FnOnce(&mut Job)) {
        let _g = self.jobs_lock.lock().unwrap();
        let p = job_path(&self.paths, r);
        let mut j: Job = std::fs::read(&p).ok().and_then(|b| serde_json::from_slice(&b).ok()).unwrap_or_else(|| Job { turn: r.clone(), ..Default::default() });
        f(&mut j);
        if j.status == "done" && j.report.is_none() {
            let _ = std::fs::remove_file(&p);
        } else {
            let _ = write_private(&p, &serde_json::to_vec(&j).unwrap_or_default());
        }
    }

    pub fn enqueue(&self, r: &TurnRef) {
        self.update_job(r, |j| {
            *j = Job { turn: r.clone(), status: "queued".into(), attempts: 0, next_at: 0, report: Some(report(r, "queued", "")) };
        });
    }

    pub async fn run(self: Arc<Self>) {
        loop {
            if now_s() >= self.pause_until.load(Ordering::Relaxed) {
                if let Err(e) = self.upload().await {
                    self.pause(&e.to_string());
                } else if let Err(e) = self.post_reports().await {
                    self.pause(&e.to_string());
                }
            }
            self.start_jobs();
            tokio::time::sleep(Duration::from_secs(3)).await;
        }
    }

    fn pause(&self, why: &str) {
        log(&self.paths, "warn", "work log upload paused for a minute", json!({"error": why}));
        self.pause_until.store(now_s() + 60, Ordering::Relaxed);
    }

    /// Queue files, oldest first. A file goes only after the cloud took it (or refused it as bad).
    async fn upload(&self) -> Result<()> {
        let mut files: Vec<PathBuf> = std::fs::read_dir(dir(&self.paths, "queue"))?.flatten().map(|e| e.path()).filter(|p| p.extension().is_some_and(|x| x == "json")).collect();
        files.sort();
        for f in files {
            let Some(b) = std::fs::read(&f).ok().and_then(|b| serde_json::from_slice::<Batch>(&b).ok()) else {
                let _ = std::fs::remove_file(&f);
                continue;
            };
            // A Stop hook reads the last turn again: send a turn only when it grew, or when
            // a backfill now asks for the summary of a turn imported without one.
            let mut send = vec![];
            let mut summarise = vec![];
            let mut keep = vec![];
            for t in &b.turns {
                let old = load(&self.paths, &turn_ref(t));
                let grew = old.as_ref().is_none_or(|o| o.turn.ended_at != t.ended_at || o.turn.model_calls != t.model_calls);
                let wants = b.summarize && old.as_ref().is_none_or(|o| grew || matches!(o.summary.as_str(), "" | "skipped"));
                if !grew && !wants {
                    continue;
                }
                let status = if wants { "queued".to_string() } else { old.as_ref().map(|o| o.summary.clone()).filter(|s| !s.is_empty()).unwrap_or_else(|| "skipped".into()) };
                send.push(to_wire(t, &status));
                keep.push(Stored { repo: b.repo.clone(), turn: t.clone(), summary: status });
                if wants {
                    summarise.push(turn_ref(t));
                }
            }
            if !send.is_empty() {
                let body = serde_json::to_value(LogBatch { repo: b.repo.clone(), turns: send })?;
                let (st, v) = self.api.post_status("/relay/log/turns", &body).await?;
                if st == 400 || st == 413 {
                    log(&self.paths, "warn", "work log batch refused", json!({"file": f.to_string_lossy(), "error": v["error"]}));
                } else if !(200..300).contains(&st) {
                    anyhow::bail!("upload failed: {st} {}", v["error"].as_str().unwrap_or(""));
                } else {
                    // Kept only once the cloud has the turn: a failed upload is sent again whole.
                    for s in &keep {
                        save(&self.paths, s);
                    }
                    for r in &summarise {
                        self.enqueue(r);
                    }
                    self.icon(&b.repo).await;
                }
            }
            let _ = std::fs::remove_file(&f);
        }
        Ok(())
    }

    /// The repo's logo, once a day, sent again only when it changed.
    async fn icon(&self, repo: &LogRepo) {
        let mark = dir(&self.paths, "icons").join(sha(&repo.key));
        let prev = std::fs::read_to_string(&mark).unwrap_or_default();
        let mut it = prev.split(' ');
        let (at, prev_sha) = (it.next().and_then(|x| x.parse::<i64>().ok()).unwrap_or(0), it.next().unwrap_or("").to_string());
        if now_s() - at < 86400 || repo.root.is_empty() {
            return;
        }
        let root = repo.root.clone();
        let icon = tokio::task::spawn_blocking(move || crate::icon::detect(&root)).await.ok().flatten();
        let isha = icon.as_ref().map(|b| sha(&hex::encode(b))).unwrap_or_else(|| "none".into());
        if let Some(b) = icon.filter(|_| isha != prev_sha) {
            let mime = if b.starts_with(&[0xFF, 0xD8, 0xFF]) { "image/jpeg" } else { "image/png" };
            let body = json!({"key": repo.key, "mime": mime, "data": base64::engine::general_purpose::STANDARD.encode(&b)});
            if !matches!(self.api.post_status("/relay/log/icon", &body).await, Ok((200..=299, _))) {
                return; // try again with the next upload
            }
        }
        let _ = std::fs::write(&mark, format!("{} {isha}", now_s()));
    }

    async fn post_reports(&self) -> Result<()> {
        for (_, j) in jobs(&self.paths) {
            let Some(rep) = j.report.clone() else { continue };
            let (st, v) = self.api.post_status("/relay/log/summary", &serde_json::to_value(&rep)?).await?;
            if !(200..300).contains(&st) && st != 400 && st != 404 {
                anyhow::bail!("summary upload failed: {st} {}", v["error"].as_str().unwrap_or(""));
            }
            // 404: the cloud does not know the turn (deleted there); drop the report.
            self.update_job(&rep.turn, |j| {
                if j.report.as_ref() == Some(&rep) {
                    j.report = None;
                }
            });
        }
        Ok(())
    }

    fn start_jobs(self: &Arc<Self>) {
        let mut due: Vec<Job> = jobs(&self.paths).into_iter().map(|x| x.1).filter(|j| j.status == "queued" && j.next_at <= now_s()).collect();
        due.sort_by_key(|j| std::cmp::Reverse(j.next_at)); // newest work first, like Theasa
        for j in due {
            if self.running.load(Ordering::SeqCst) >= 2 {
                return;
            }
            self.running.fetch_add(1, Ordering::SeqCst);
            self.update_job(&j.turn, |j| {
                j.status = "running".into();
                j.report = Some(report(&j.turn, "running", ""));
            });
            let me = self.clone();
            tokio::spawn(async move {
                me.summarise(&j.turn).await;
                me.running.fetch_sub(1, Ordering::SeqCst);
            });
        }
    }

    async fn summarise(&self, r: &TurnRef) {
        let res = match load(&self.paths, r) {
            Some(s) => summarize::summarise_with(&r.agent, &summarize::build_prompt(&s.turn, &s.repo.name), &dir(&self.paths, "work")).await.map(|o| (s, o)),
            None => Err(anyhow::anyhow!("the turn is not on this Mac any more")),
        };
        match res {
            Ok((mut s, o)) => {
                s.summary = "done".into();
                save(&self.paths, &s);
                self.update_job(r, |j| {
                    j.status = "done".into();
                    j.report = Some(LogSummaryReport { turn: r.clone(), status: "done".into(), error: String::new(), summary: Some(o.summary), model: o.model, tokens: o.tokens, cost: o.cost });
                });
            }
            Err(e) => {
                let err = e.to_string();
                log(&self.paths, "warn", "summary failed", json!({"turn": r.turn_key, "error": err}));
                self.update_job(r, |j| {
                    j.attempts += 1;
                    if j.attempts < 3 {
                        j.status = "queued".into();
                        j.next_at = now_s() + if j.attempts == 1 { 30 } else { 120 };
                        j.report = Some(report(r, "queued", &err));
                    } else {
                        j.status = "failed".into();
                        j.report = Some(report(r, "failed", &err));
                    }
                });
                if let Some(mut s) = load(&self.paths, r).filter(|_| self.job_status(r) == "failed") {
                    s.summary = "failed".into();
                    save(&self.paths, &s);
                }
            }
        }
    }

    fn job_status(&self, r: &TurnRef) -> String {
        std::fs::read(job_path(&self.paths, r)).ok().and_then(|b| serde_json::from_slice::<Job>(&b).ok()).map(|j| j.status).unwrap_or_default()
    }

    // ---------- the cloud's requests ----------

    pub fn resummarize(&self, r: &TurnRef) -> (u16, Value) {
        if load(&self.paths, r).is_none() {
            return (404, json!({"error": "this turn is not on the Mac any more"}));
        }
        self.enqueue(r);
        (200, json!({"ok": true}))
    }

    pub fn retry_failed(&self) -> (u16, Value) {
        let failed: Vec<TurnRef> = jobs(&self.paths).into_iter().filter(|(_, j)| j.status == "failed").map(|(_, j)| j.turn).collect();
        for r in &failed {
            self.enqueue(r);
        }
        (200, json!({"retried": failed.len()}))
    }
}

/// op `turn_local`: what stays on the Mac.
pub fn turn_local(paths: &Paths, r: &TurnRef) -> (u16, Value) {
    let Some(s) = load(paths, r) else { return (404, json!({"error": "this turn is not on the Mac any more"})) };
    let images: Vec<Value> = s.turn.attachments.iter().filter(|a| !a.data.is_empty()).map(|a| json!({"name": a.name, "media_type": a.media_type})).collect();
    (200, json!({"log": s.turn.log, "diffs": s.turn.diffs, "images": images}))
}

const RASTER: &[&str] = &["image/png", "image/jpeg", "image/gif", "image/webp"];

/// op `turn_image`: one image, raster formats only.
pub fn turn_image(paths: &Paths, r: &TurnRef, index: usize) -> (u16, Value) {
    let Some(s) = load(paths, r) else { return (404, json!({"error": "this turn is not on the Mac any more"})) };
    match s.turn.attachments.iter().filter(|a| !a.data.is_empty()).nth(index) {
        Some(a) if RASTER.contains(&a.media_type.as_str()) => (200, json!({"media_type": a.media_type, "data": a.data})),
        _ => (404, json!({"error": "no such image"})),
    }
}

/// `substrate log status`: (queued batch files, summary jobs queued, failed, local store bytes).
pub fn counts(paths: &Paths) -> (usize, usize, usize, u64) {
    let q = std::fs::read_dir(dir(paths, "queue")).map(|d| d.count()).unwrap_or(0);
    let js = jobs(paths);
    let size = std::fs::read_dir(dir(paths, "turns")).into_iter().flatten().flatten().filter_map(|e| e.metadata().ok()).map(|m| m.len()).sum();
    (q, js.iter().filter(|(_, j)| matches!(j.status.as_str(), "queued" | "running")).count(), js.iter().filter(|(_, j)| j.status == "failed").count(), size)
}
