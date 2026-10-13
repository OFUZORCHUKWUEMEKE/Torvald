//! One task, end to end: sandbox → clone → agent → commit → push → pull request.
//!
//! Git credentials never enter the agent's shell: clone and push are run by the
//! harness with the token passed as a per-process env var, not written to disk.

use std::collections::HashMap;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::{bail, Context, Result};
use base64::{engine::general_purpose::STANDARD as B64, Engine};

use crate::agent::{Agent, Stop};
use crate::e2b::{E2bClient, RunOpts, Sandbox, SandboxSpec};
use crate::github::GitHub;
use crate::llm::{Claude, Usage};
use crate::tools::sh_quote;

const WORKDIR: &str = "/home/user/repo";

#[derive(Clone)]
pub struct Settings {
    pub template: String,
    pub max_turns: usize,
    pub command_timeout: Duration,
    pub sandbox_ttl: Duration,
    pub git_name: String,
    pub git_email: String,
}

pub struct Deps {
    pub e2b: E2bClient,
    pub claude: Claude,
    pub github: GitHub,
    pub settings: Settings,
}

#[derive(Debug, Clone)]
pub struct Task {
    /// `owner/name`
    pub repo: String,
    pub base: Option<String>,
    pub title: String,
    pub instructions: String,
    pub issue: Option<u64>,
}

#[derive(Debug)]
pub enum TaskResult {
    PullRequest { url: String },
    NoChanges { summary: String },
    Incomplete { reason: String, summary: String },
}

pub struct Report {
    pub result: TaskResult,
    pub turns: usize,
    pub usage: Usage,
}

pub async fn run_task(deps: &Deps, task: &Task) -> Result<Report> {
    let base = match &task.base {
        Some(b) => b.clone(),
        None => deps.github.default_branch(&task.repo).await?,
    };
    let branch = branch_name(task);

    let sandbox = deps
        .e2b
        .create_sandbox(SandboxSpec {
            template: &deps.settings.template,
            timeout: deps.settings.sandbox_ttl,
            metadata: HashMap::from([
                ("app".into(), "torvald".into()),
                ("repo".into(), task.repo.clone()),
                ("branch".into(), branch.clone()),
            ]),
        })
        .await?;
    tracing::info!(sandbox = %sandbox.id, repo = %task.repo, %branch, "sandbox ready");

    let result = work_in_sandbox(deps, task, &sandbox, &base, &branch).await;
    if let Err(e) = sandbox.kill().await {
        tracing::warn!(error = %e, "failed to kill sandbox (it will expire on its own)");
    }
    result
}

async fn work_in_sandbox(deps: &Deps, task: &Task, sandbox: &Sandbox, base: &str, branch: &str) -> Result<Report> {
    let s = &deps.settings;
    let git = GitRunner {
        sandbox,
        auth: B64.encode(format!("x-access-token:{}", deps.github.token())),
    };

    git.clone_repo(&task.repo, base).await.context("cloning repository")?;
    git.plain(&format!(
        "git config user.name {} && git config user.email {} && git checkout --quiet -b {}",
        sh_quote(&s.git_name),
        sh_quote(&s.git_email),
        sh_quote(branch)
    ))
    .await?;
    let base_sha = git.plain("git rev-parse HEAD").await?.trim().to_string();

    let agent = Agent {
        claude: &deps.claude,
        sandbox,
        workdir: WORKDIR,
        max_turns: s.max_turns,
        command_timeout: s.command_timeout,
        sandbox_ttl: s.sandbox_ttl,
    };
    let outcome = agent.run(&task.instructions).await?;
    tracing::info!(turns = outcome.turns, stop = ?outcome.stop, usage = ?outcome.usage, "agent finished");

    let report = |result| Report {
        result,
        turns: outcome.turns,
        usage: outcome.usage.clone(),
    };
    match outcome.stop {
        Stop::Completed => {}
        Stop::Refused => {
            return Ok(report(TaskResult::Incomplete {
                reason: "the model declined this task".into(),
                summary: outcome.summary,
            }))
        }
        Stop::MaxTurns => {
            return Ok(report(TaskResult::Incomplete {
                reason: format!("hit the {}-turn limit before finishing", s.max_turns),
                summary: outcome.summary,
            }))
        }
    }

    // Commit whatever the agent left in the tree (it may also have committed itself).
    let message = format!("{}\n\n{}", task.title, outcome.summary);
    git.plain_with_env(
        "git add -A && (git diff --cached --quiet || git commit --quiet -F - <<< \"$TORVALD_COMMIT_MSG\")",
        HashMap::from([("TORVALD_COMMIT_MSG".to_string(), message)]),
    )
    .await
    .context("committing changes")?;
    let ahead: u32 = git
        .plain(&format!("git rev-list --count {base_sha}..HEAD"))
        .await?
        .trim()
        .parse()
        .unwrap_or(0);
    if ahead == 0 {
        return Ok(report(TaskResult::NoChanges {
            summary: outcome.summary,
        }));
    }

    git.push(branch).await.context("pushing branch")?;

    let mut body = outcome.summary.clone();
    if let Some(n) = task.issue {
        body.push_str(&format!("\n\nCloses #{n}"));
    }
    body.push_str("\n\n---\n_Opened automatically by Torvald._");
    let pr = deps
        .github
        .create_pull_request(&task.repo, branch, base, &task.title, &body)
        .await?;
    Ok(report(TaskResult::PullRequest { url: pr.html_url }))
}

struct GitRunner<'a> {
    sandbox: &'a Sandbox,
    /// base64(`x-access-token:<token>`), handed to git per process only.
    auth: String,
}

impl GitRunner<'_> {
    async fn clone_repo(&self, repo: &str, base: &str) -> Result<()> {
        let cmd = format!(
            "git {AUTH_FLAG} clone --quiet --branch {} https://github.com/{repo}.git {WORKDIR}",
            sh_quote(base)
        );
        self.exec(&cmd, self.auth_env(), None).await.map(drop)
    }

    async fn push(&self, branch: &str) -> Result<()> {
        let cmd = format!(
            "git {AUTH_FLAG} push --quiet origin HEAD:refs/heads/{}",
            sh_quote(branch)
        );
        self.exec(&cmd, self.auth_env(), Some(WORKDIR)).await.map(drop)
    }

    async fn plain(&self, cmd: &str) -> Result<String> {
        self.exec(cmd, HashMap::new(), Some(WORKDIR)).await
    }

    async fn plain_with_env(&self, cmd: &str, envs: HashMap<String, String>) -> Result<String> {
        self.exec(cmd, envs, Some(WORKDIR)).await
    }

    fn auth_env(&self) -> HashMap<String, String> {
        HashMap::from([("TORVALD_GIT_AUTH".to_string(), self.auth.clone())])
    }

    async fn exec(&self, cmd: &str, envs: HashMap<String, String>, cwd: Option<&str>) -> Result<String> {
        let out = self
            .sandbox
            .run(
                cmd,
                RunOpts {
                    cwd,
                    envs,
                    timeout: Some(Duration::from_secs(600)),
                },
            )
            .await?;
        if out.exit_code != 0 {
            bail!("git step failed (exit {}): {}", out.exit_code, out.stderr.trim());
        }
        Ok(out.stdout)
    }
}

/// Expanded by the sandbox shell from a per-process env var, so the token is
/// never written into `.git/config` where the agent could read it.
const AUTH_FLAG: &str = r#"-c http.extraHeader="Authorization: Basic $TORVALD_GIT_AUTH""#;

fn branch_name(task: &Task) -> String {
    let stamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let slug: String = task
        .title
        .to_lowercase()
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
        .collect::<String>()
        .split('-')
        .filter(|s| !s.is_empty())
        .take(6)
        .collect::<Vec<_>>()
        .join("-");
    match task.issue {
        Some(n) => format!("torvald/{n}-{slug}-{stamp}"),
        None => format!("torvald/{slug}-{stamp}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn branch_names_are_git_safe() {
        let task = Task {
            repo: "o/r".into(),
            base: None,
            title: "Fix: login bug (500 on /auth)!".into(),
            instructions: String::new(),
            issue: Some(42),
        };
        let b = branch_name(&task);
        assert!(b.starts_with("torvald/42-fix-login-bug-500-on-auth-"), "{b}");
        assert!(b.chars().all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '/'));
    }
}
