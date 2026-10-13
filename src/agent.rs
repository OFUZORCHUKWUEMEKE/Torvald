//! The agent loop: Claude proposes tool calls, the sandbox executes them, repeat
//! until Claude ends its turn or we hit the turn limit.

use std::time::Duration;

use anyhow::Result;
use serde_json::{json, Value};

use crate::e2b::Sandbox;
use crate::llm::{Claude, Usage};
use crate::tools::{self, Toolbox};

const SYSTEM_PROMPT: &str = "\
You are Torvald, an autonomous software engineer. You work alone inside a disposable \
Linux sandbox with a checkout of the project you were assigned. Nobody is watching in \
real time, so make reasonable decisions yourself instead of asking questions.

How to work:
- Explore the codebase first and follow its existing conventions, structure and style.
- Make the smallest change that fully solves the task. Do not refactor unrelated code.
- Verify your work: build it and run the relevant tests, linters and type checks. \
If the project has no tests for what you changed, add focused ones.
- If something fails, read the error, fix the root cause, and re-run.
- Do not run git commit, git push, or open pull requests; the platform does that \
after you finish. Do not modify CI configuration unless the task asks for it.
- Never print, log or commit secrets.

When you are done, reply with a short summary in Markdown suitable for a pull request \
description: what changed, why, and how you verified it. If you could not complete \
the task, say exactly what is blocking you instead.";

#[derive(Debug, PartialEq)]
pub enum Stop {
    Completed,
    MaxTurns,
    Refused,
}

pub struct Outcome {
    pub stop: Stop,
    pub summary: String,
    pub turns: usize,
    pub usage: Usage,
}

pub struct Agent<'a> {
    pub claude: &'a Claude,
    pub sandbox: &'a Sandbox,
    pub workdir: &'a str,
    pub max_turns: usize,
    pub command_timeout: Duration,
    pub sandbox_ttl: Duration,
}

impl Agent<'_> {
    pub async fn run(&self, task: &str) -> Result<Outcome> {
        let tool_defs = tools::definitions();
        let toolbox = Toolbox {
            sandbox: self.sandbox,
            workdir: self.workdir,
            command_timeout: self.command_timeout,
        };
        let mut messages = vec![json!({
            "role": "user",
            "content": format!("The repository is checked out at {}.\n\n<task>\n{task}\n</task>", self.workdir),
        })];
        let mut usage = Usage::default();
        let mut summary = String::new();

        for turn in 1..=self.max_turns {
            if turn % 10 == 0 {
                // Long sessions can outlive the sandbox TTL; push it out.
                self.sandbox.keep_alive(self.sandbox_ttl).await?;
            }

            let resp = self.claude.create(SYSTEM_PROMPT, &tool_defs, &messages).await?;
            usage.add(&resp.usage);
            let text = text_of(&resp.content);
            if !text.is_empty() {
                tracing::info!(turn, "assistant: {}", first_line(&text));
                summary = text;
            }
            let tool_uses: Vec<Value> = resp
                .content
                .iter()
                .filter(|b| b["type"] == "tool_use")
                .cloned()
                .collect();
            let stop_reason = resp.stop_reason.clone().unwrap_or_default();
            messages.push(json!({ "role": "assistant", "content": resp.content }));

            match stop_reason.as_str() {
                "refusal" => {
                    return Ok(Outcome {
                        stop: Stop::Refused,
                        summary,
                        turns: turn,
                        usage,
                    })
                }
                "end_turn" | "stop_sequence" if tool_uses.is_empty() => {
                    return Ok(Outcome {
                        stop: Stop::Completed,
                        summary,
                        turns: turn,
                        usage,
                    })
                }
                _ => {}
            }

            if tool_uses.is_empty() {
                // e.g. max_tokens mid-text or pause_turn: let Claude carry on.
                messages.push(json!({ "role": "user", "content": "Continue." }));
                continue;
            }

            // Tool calls share one sandbox, so run them in order. All results go
            // back in a single user message.
            let mut results = Vec::with_capacity(tool_uses.len());
            for call in &tool_uses {
                let id = call["id"].as_str().unwrap_or_default();
                let name = call["name"].as_str().unwrap_or_default();
                let outcome = if stop_reason == "max_tokens" {
                    tools::ToolOutcome {
                        content: "Your response hit the output limit, so this call's input may be truncated and was not run. Re-issue it, splitting large files into smaller edits.".into(),
                        is_error: true,
                    }
                } else {
                    tracing::info!(turn, tool = name, "{}", describe_call(&call["input"]));
                    toolbox.execute(name, &call["input"]).await
                };
                results.push(json!({
                    "type": "tool_result",
                    "tool_use_id": id,
                    "content": outcome.content,
                    "is_error": outcome.is_error,
                }));
            }
            messages.push(json!({ "role": "user", "content": results }));
        }

        Ok(Outcome {
            stop: Stop::MaxTurns,
            summary,
            turns: self.max_turns,
            usage,
        })
    }
}

fn text_of(content: &[Value]) -> String {
    content
        .iter()
        .filter(|b| b["type"] == "text")
        .filter_map(|b| b["text"].as_str())
        .collect::<Vec<_>>()
        .join("\n")
        .trim()
        .to_string()
}

fn first_line(s: &str) -> &str {
    s.lines().next().unwrap_or_default()
}

fn describe_call(input: &Value) -> String {
    let s = input
        .get("command")
        .and_then(Value::as_str)
        .map(|c| match input.get("path").and_then(Value::as_str) {
            Some(p) => format!("{c} {p}"),
            None => c.to_string(),
        })
        .unwrap_or_else(|| input.to_string());
    first_line(&s).chars().take(160).collect()
}
