//! The 24/7 loop. GitHub issues are the queue: label an issue `torvald` and the
//! worker claims it, ships a PR, and reports back on the issue. All state lives
//! in labels, so the worker can restart at any time without losing work.

use std::collections::HashSet;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::Result;
use tokio::sync::Semaphore;
use tokio::task::JoinSet;

use crate::github::Issue;
use crate::pipeline::{self, Deps, Task, TaskResult};

pub struct WorkerConfig {
    pub repos: Vec<String>,
    pub poll_interval: Duration,
    pub concurrency: usize,
    pub label: String,
}

impl WorkerConfig {
    fn working_label(&self) -> String {
        format!("{}:working", self.label)
    }
    fn done_label(&self) -> String {
        format!("{}:done", self.label)
    }
    fn failed_label(&self) -> String {
        format!("{}:failed", self.label)
    }
}

pub async fn serve(deps: Arc<Deps>, cfg: Arc<WorkerConfig>) -> Result<()> {
    let slots = Arc::new(Semaphore::new(cfg.concurrency));
    let in_flight: Arc<Mutex<HashSet<(String, u64)>>> = Default::default();
    let mut jobs = JoinSet::new();
    let mut tick = tokio::time::interval(cfg.poll_interval);
    let shutdown = shutdown_signal();
    tokio::pin!(shutdown);
    tracing::info!(repos = ?cfg.repos, concurrency = cfg.concurrency, label = %cfg.label, "worker started");

    loop {
        tokio::select! {
            _ = &mut shutdown => {
                tracing::info!(running = jobs.len(), "shutting down; waiting for running tasks to finish");
                while jobs.join_next().await.is_some() {}
                return Ok(());
            }
            Some(_) = jobs.join_next(), if !jobs.is_empty() => {}
            _ = tick.tick() => {
                for repo in &cfg.repos {
                    let issues = match deps.github.open_issues_with_label(repo, &cfg.label).await {
                        Ok(issues) => issues,
                        Err(e) => {
                            tracing::warn!(%repo, error = %e, "polling failed");
                            continue;
                        }
                    };
                    for issue in issues.into_iter().filter(|i| !i.has_label(&cfg.working_label())) {
                        let key = (repo.clone(), issue.number);
                        let Ok(permit) = slots.clone().try_acquire_owned() else {
                            break; // at capacity; pick it up on a later tick
                        };
                        if !in_flight.lock().unwrap().insert(key.clone()) {
                            continue;
                        }
                        let (deps, cfg, in_flight, repo) = (deps.clone(), cfg.clone(), in_flight.clone(), repo.clone());
                        jobs.spawn(async move {
                            handle_issue(&deps, &cfg, &repo, issue).await;
                            in_flight.lock().unwrap().remove(&key);
                            drop(permit);
                        });
                    }
                }
            }
        }
    }
}

/// Resolves on Ctrl-C or SIGTERM (what `docker stop` and Kubernetes send).
async fn shutdown_signal() {
    let ctrl_c = tokio::signal::ctrl_c();
    #[cfg(unix)]
    {
        let mut term = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
            .expect("installing SIGTERM handler");
        tokio::select! {
            _ = ctrl_c => {}
            _ = term.recv() => {}
        }
    }
    #[cfg(not(unix))]
    let _ = ctrl_c.await;
}

async fn handle_issue(deps: &Deps, cfg: &WorkerConfig, repo: &str, issue: Issue) {
    let n = issue.number;
    // Claim first so other workers (and our next poll) skip it.
    if let Err(e) = deps.github.add_labels(repo, n, &[&cfg.working_label()]).await {
        tracing::warn!(%repo, issue = n, error = %e, "could not claim issue");
        return;
    }
    tracing::info!(%repo, issue = n, title = %issue.title, "picked up issue");

    let task = Task {
        repo: repo.to_string(),
        base: None,
        title: issue.title.clone(),
        instructions: format!("# {}\n\n{}", issue.title, issue.body.clone().unwrap_or_default()),
        issue: Some(n),
    };

    let (label, comment) = match pipeline::run_task(deps, &task).await {
        Ok(report) => {
            let cost = format!(
                "_{} turns · {} input / {} output tokens ({} cached)_",
                report.turns,
                report.usage.input_tokens,
                report.usage.output_tokens,
                report.usage.cache_read_input_tokens
            );
            match report.result {
                TaskResult::PullRequest { url } => (cfg.done_label(), format!("Opened {url}\n\n{cost}")),
                TaskResult::NoChanges { summary } => (
                    cfg.done_label(),
                    format!("Finished without code changes.\n\n{summary}\n\n{cost}"),
                ),
                TaskResult::Incomplete { reason, summary } => {
                    (cfg.failed_label(), format!("Stopped: {reason}.\n\n{summary}\n\n{cost}"))
                }
            }
        }
        Err(e) => {
            tracing::error!(%repo, issue = n, error = format!("{e:#}"), "task failed");
            (
                cfg.failed_label(),
                format!("Torvald hit an error and stopped:\n\n```\n{e:#}\n```"),
            )
        }
    };

    let gh = &deps.github;
    let results = [
        gh.comment(repo, n, &comment).await,
        gh.add_labels(repo, n, &[&label]).await,
        gh.remove_label(repo, n, &cfg.label).await,
        gh.remove_label(repo, n, &cfg.working_label()).await,
    ];
    for r in results.into_iter().filter_map(Result::err) {
        tracing::warn!(%repo, issue = n, error = %r, "could not report back on issue");
    }
}
