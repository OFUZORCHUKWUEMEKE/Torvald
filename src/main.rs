mod agent;
mod e2b;
mod github;
mod llm;
mod pipeline;
mod tools;
mod worker;

use std::sync::Arc;
use std::time::Duration;

use anyhow::Result;
use clap::{Args, Parser, Subcommand};

use pipeline::{Deps, Settings, Task, TaskResult};

#[derive(Parser)]
#[command(
    name = "torvald",
    version,
    about = "Autonomous coding agent that ships code from E2B sandboxes"
)]
struct Cli {
    #[command(flatten)]
    common: Common,
    #[command(subcommand)]
    command: Command,
}

#[derive(Args)]
struct Common {
    #[arg(long, env = "ANTHROPIC_API_KEY", hide_env_values = true)]
    anthropic_api_key: String,
    #[arg(long, env = "E2B_API_KEY", hide_env_values = true)]
    e2b_api_key: String,
    /// Token with contents, pull-requests and issues write access.
    #[arg(long, env = "GITHUB_TOKEN", hide_env_values = true)]
    github_token: String,

    #[arg(long, env = "TORVALD_MODEL", default_value = "claude-opus-5-5")]
    model: String,
    /// low | medium | high | xhigh | max
    #[arg(long, env = "TORVALD_EFFORT", default_value = "high")]
    effort: String,
    /// E2B template the sandbox boots from (see sandbox/e2b.Dockerfile).
    #[arg(long, env = "E2B_TEMPLATE", default_value = "base")]
    template: String,
    #[arg(long, env = "E2B_DOMAIN", default_value = "e2b.app")]
    e2b_domain: String,
    #[arg(long, env = "TORVALD_MAX_TURNS", default_value_t = 150)]
    max_turns: usize,
    /// Per-command timeout inside the sandbox, in seconds.
    #[arg(long, env = "TORVALD_COMMAND_TIMEOUT", default_value_t = 600)]
    command_timeout: u64,
    /// Sandbox lifetime in seconds; extended while the agent is working.
    #[arg(long, env = "TORVALD_SANDBOX_TTL", default_value_t = 3600)]
    sandbox_ttl: u64,
    #[arg(long, env = "TORVALD_GIT_NAME", default_value = "Torvald")]
    git_name: String,
    #[arg(long, env = "TORVALD_GIT_EMAIL", default_value = "torvald@users.noreply.github.com")]
    git_email: String,
}

#[derive(Subcommand)]
enum Command {
    /// Run a single task and open a pull request.
    Run {
        /// Repository as owner/name.
        #[arg(long)]
        repo: String,
        /// What to build or fix.
        #[arg(long)]
        task: String,
        /// Pull request title (defaults to the first line of the task).
        #[arg(long)]
        title: Option<String>,
        /// Base branch (defaults to the repository's default branch).
        #[arg(long)]
        base: Option<String>,
    },
    /// Run forever: watch repositories for labeled issues and ship them.
    Serve {
        /// Comma-separated owner/name list.
        #[arg(long, env = "TORVALD_REPOS", value_delimiter = ',', required = true)]
        repos: Vec<String>,
        #[arg(long, env = "TORVALD_POLL_INTERVAL", default_value_t = 60)]
        poll_interval: u64,
        #[arg(long, env = "TORVALD_CONCURRENCY", default_value_t = 2)]
        concurrency: usize,
        /// Issues carrying this label get picked up.
        #[arg(long, env = "TORVALD_LABEL", default_value = "torvald")]
        label: String,
    },
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "torvald=info".into()),
        )
        .init();

    let cli = Cli::parse();
    let c = cli.common;
    let deps = Arc::new(Deps {
        e2b: e2b::E2bClient::new(c.e2b_api_key, c.e2b_domain)?,
        claude: llm::Claude::new(c.anthropic_api_key, c.model, c.effort)?,
        github: github::GitHub::new(c.github_token)?,
        settings: Settings {
            template: c.template,
            max_turns: c.max_turns,
            command_timeout: Duration::from_secs(c.command_timeout),
            sandbox_ttl: Duration::from_secs(c.sandbox_ttl),
            git_name: c.git_name,
            git_email: c.git_email,
        },
    });

    match cli.command {
        Command::Run {
            repo,
            task,
            title,
            base,
        } => {
            let title = title.unwrap_or_else(|| {
                task.lines()
                    .next()
                    .unwrap_or("Torvald change")
                    .chars()
                    .take(72)
                    .collect()
            });
            let report = pipeline::run_task(
                &deps,
                &Task {
                    repo,
                    base,
                    title,
                    instructions: task,
                    issue: None,
                },
            )
            .await?;
            match report.result {
                TaskResult::PullRequest { url } => println!("Opened {url}"),
                TaskResult::NoChanges { summary } => println!("No changes.\n\n{summary}"),
                TaskResult::Incomplete { reason, summary } => {
                    println!("Incomplete: {reason}\n\n{summary}");
                    std::process::exit(2);
                }
            }
        }
        Command::Serve {
            repos,
            poll_interval,
            concurrency,
            label,
        } => {
            let cfg = worker::WorkerConfig {
                repos,
                poll_interval: Duration::from_secs(poll_interval),
                concurrency: concurrency.max(1),
                label,
            };
            worker::serve(deps, Arc::new(cfg)).await?;
        }
    }
    Ok(())
}
