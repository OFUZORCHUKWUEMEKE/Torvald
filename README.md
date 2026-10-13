# Torvald

An autonomous coding agent written in Rust. It runs each task in a fresh
[E2B](https://e2b.dev) sandbox, uses Claude to write and test code, and opens a
pull request. Run it as a long-lived worker and it ships code around the clock:
label a GitHub issue `torvald` and a PR shows up.

## How it works

```
 GitHub issues ──poll──▶ worker (torvald serve) ──▶ pipeline (one per task)
  label: torvald           │ concurrency limit          │
                           │ claims via labels          ├─ 1. create E2B sandbox
                           ▼                            ├─ 2. git clone (token never stored)
                  comment + label result                ├─ 3. agent loop ◀──▶ Claude API
                                                        │      bash / file-edit tools run
                                                        │      inside the sandbox via envd
                                                        ├─ 4. commit, push branch
                                                        ├─ 5. open pull request
                                                        └─ 6. kill sandbox
```

| Module | Job |
|---|---|
| `src/e2b/` | E2B client. Control plane (`POST /v2/sandboxes`, kill, extend TTL) over REST; data plane talks to `envd` inside the sandbox: processes over Connect-RPC (JSON codec, streamed), files over `/files`. E2B ships no Rust SDK, so this speaks the wire protocol directly. |
| `src/llm.rs` | Claude Messages API client: adaptive thinking, prompt caching, retries with backoff, server-side refusal fallback. |
| `src/tools.rs` | Anthropic's built-in `bash` and `str_replace_based_edit_tool` tools, executed in the sandbox. Shell cwd/env persist between calls. |
| `src/agent.rs` | The loop: call Claude → run tool calls → feed results back, until Claude finishes or the turn limit hits. |
| `src/pipeline.rs` | One task end to end: sandbox → clone → agent → commit → push → PR. |
| `src/worker.rs` | The 24/7 loop: polls repos, claims issues, runs tasks in parallel, reports back, shuts down cleanly on SIGTERM. |

### Design decisions

- **The sandbox is the security boundary.** Each task gets a fresh, disposable VM;
  the agent can run anything inside it. Nothing the agent does touches the host.
- **Secrets stay out of the agent's reach.** The GitHub token is only passed as an
  environment variable to the specific `git clone` / `git push` processes the
  harness runs. It is never written to `.git/config` or to the agent's shell.
- **The harness owns irreversible actions.** The agent edits and tests code; the
  harness commits, pushes and opens the PR. A human merges.
- **State lives in GitHub labels.** `torvald` → `torvald:working` →
  `torvald:done` / `torvald:failed`. The worker is stateless and can restart at
  any time. To retry a failed issue, add the `torvald` label again.
- **Only trusted people can start work.** Issue text is untrusted input, but only
  users with triage access can add labels, so only they can hand an issue to the
  agent.
- **Append-only conversation.** Claude's responses are stored verbatim (including
  thinking blocks), which keeps prompt caching effective over long sessions.

## Quick start

1. Get keys: Anthropic API key, E2B API key, and a GitHub fine-grained token with
   **Contents**, **Pull requests** and **Issues** read/write on your target repos.
2. Build a sandbox template with your project's toolchains (optional; the default
   `base` template works for simple repos):
   ```sh
   e2b template create torvald-dev -d sandbox/e2b.Dockerfile
   export E2B_TEMPLATE=torvald-dev
   ```
3. Run a one-off task:
   ```sh
   cp .env.example .env   # fill it in, then: set -a; . ./.env; set +a
   cargo run --release -- run --repo owner/repo \
     --task "Add a /health endpoint that returns 200 and a unit test for it"
   ```
4. Run it 24/7:
   ```sh
   cargo run --release -- serve --repos owner/repo,owner/other --concurrency 2
   # or: docker build -t torvald . && docker run --env-file .env --restart=always torvald
   ```
   Then label any issue `torvald`.

All options: `torvald --help`. Every flag also reads from an env var
(`TORVALD_MODEL`, `TORVALD_EFFORT`, `TORVALD_MAX_TURNS`, …).

## Roadmap

What it takes to go from this MVP to a product that ships safely around the clock:

1. **Feedback loop on PRs** – subscribe to CI results and review comments
   (GitHub webhooks), resume the task in a new sandbox on the same branch, fix, and
   push. This is the biggest step toward "merged" rather than "opened".
2. **Durable job queue** – move from label polling to Postgres (or Redis) with
   leases and retries, so many workers can share the load and crashed runs are
   picked up again automatically. Add webhooks instead of polling.
3. **Sandbox templates per repo** – a `.torvald.toml` in each repo naming the
   template, setup command (install deps) and verify command (tests/lint). Run the
   verify command in the harness before pushing and refuse to open a red PR.
4. **Cost and safety budgets** – per-task token and wall-clock caps, a daily spend
   limit, and alerts. Usage is already tracked per task.
5. **Long-horizon context** – enable server-side compaction or context editing
   for very long tasks, and a planning step that splits big issues into smaller
   PRs.
6. **Observability** – structured logs exist; add OpenTelemetry traces per task,
   store transcripts, and a small dashboard of runs, cost, success rate.
7. **More triggers** – Slack, Linear, cron ("upgrade dependencies every Monday"),
   and an HTTP API.
