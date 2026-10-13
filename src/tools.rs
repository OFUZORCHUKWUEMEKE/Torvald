//! Executes Claude's tool calls inside the E2B sandbox.
//!
//! We expose Anthropic's built-in `bash` and `str_replace_based_edit_tool` tools —
//! the model is trained on their schemas — and implement them against the sandbox.
//! The sandbox is the security boundary: it is disposable and holds no secrets.

use std::time::Duration;

use anyhow::{anyhow, bail, Result};
use serde_json::{json, Value};

use crate::e2b::{RunOpts, Sandbox};

/// Keep tool results small enough not to flood the context window.
const MAX_RESULT_CHARS: usize = 30_000;
const STATE_DIR: &str = "/tmp/.torvald";

pub fn definitions() -> Vec<Value> {
    vec![
        json!({ "type": "bash_20250124", "name": "bash" }),
        json!({ "type": "text_editor_20250728", "name": "str_replace_based_edit_tool", "max_characters": MAX_RESULT_CHARS }),
    ]
}

pub struct ToolOutcome {
    pub content: String,
    pub is_error: bool,
}

impl ToolOutcome {
    fn ok(content: impl Into<String>) -> Self {
        Self {
            content: content.into(),
            is_error: false,
        }
    }
    fn err(content: impl Into<String>) -> Self {
        Self {
            content: content.into(),
            is_error: true,
        }
    }
}

pub struct Toolbox<'a> {
    pub sandbox: &'a Sandbox,
    pub workdir: &'a str,
    pub command_timeout: Duration,
}

impl Toolbox<'_> {
    pub async fn execute(&self, name: &str, input: &Value) -> ToolOutcome {
        let result = match name {
            "bash" => self.bash(input).await,
            "str_replace_based_edit_tool" => self.edit(input).await,
            other => Err(anyhow!("unknown tool: {other}")),
        };
        match result {
            Ok(out) => out,
            Err(e) => ToolOutcome::err(format!("{e:#}")),
        }
    }

    async fn bash(&self, input: &Value) -> Result<ToolOutcome> {
        if input.get("restart").and_then(Value::as_bool) == Some(true) {
            self.sandbox
                .run(&format!("rm -rf {STATE_DIR}"), RunOpts::default())
                .await?;
            return Ok(ToolOutcome::ok("shell restarted"));
        }
        let command = input
            .get("command")
            .and_then(Value::as_str)
            .ok_or_else(|| anyhow!("missing `command`"))?;

        let out = self
            .sandbox
            .run(
                &persistent_shell(command, self.workdir),
                RunOpts {
                    timeout: Some(self.command_timeout),
                    ..Default::default()
                },
            )
            .await?;

        let mut text = String::new();
        text.push_str(&out.stdout);
        if !out.stderr.is_empty() {
            if !text.is_empty() && !text.ends_with('\n') {
                text.push('\n');
            }
            text.push_str(&out.stderr);
        }
        if out.timed_out {
            text.push_str(&format!(
                "\n[command timed out after {}s and was killed]",
                self.command_timeout.as_secs()
            ));
        } else if out.exit_code != 0 {
            text.push_str(&format!("\n[exit code {}]", out.exit_code));
        }
        if text.trim().is_empty() {
            text = "(no output)".into();
        }
        Ok(ToolOutcome {
            content: truncate(&text),
            is_error: out.timed_out,
        })
    }

    async fn edit(&self, input: &Value) -> Result<ToolOutcome> {
        let command = str_field(input, "command")?;
        let path = checked_path(str_field(input, "path")?)?;

        match command {
            "view" => {
                let is_dir = self
                    .sandbox
                    .run(&format!("test -d {}", sh_quote(path)), RunOpts::default())
                    .await?
                    .exit_code
                    == 0;
                if is_dir {
                    let out = self
                        .sandbox
                        .run(
                            &format!("find {} -maxdepth 2 -not -path '*/.git/*' -not -path '*/node_modules/*' -not -path '*/target/*' | sort | head -500", sh_quote(path)),
                            RunOpts::default(),
                        )
                        .await?;
                    return Ok(ToolOutcome::ok(truncate(&out.stdout)));
                }
                let text = self.read_text(path).await?;
                let lines: Vec<&str> = text.lines().collect();
                let (start, end) = match input.get("view_range").and_then(Value::as_array) {
                    Some(r) if r.len() == 2 => {
                        let s = r[0].as_i64().unwrap_or(1).max(1) as usize;
                        let e = r[1].as_i64().unwrap_or(-1);
                        let e = if e < 0 {
                            lines.len()
                        } else {
                            (e as usize).min(lines.len())
                        };
                        (s, e)
                    }
                    _ => (1, lines.len()),
                };
                let numbered: String = lines
                    .iter()
                    .enumerate()
                    .skip(start.saturating_sub(1))
                    .take(end.saturating_sub(start.saturating_sub(1)))
                    .map(|(i, l)| format!("{:>6}\t{l}\n", i + 1))
                    .collect();
                Ok(ToolOutcome::ok(truncate(&numbered)))
            }
            "create" => {
                let text = str_field(input, "file_text")?;
                self.sandbox.write_file(path, text.as_bytes().to_vec()).await?;
                Ok(ToolOutcome::ok(format!("File created: {path}")))
            }
            "str_replace" => {
                let old = str_field(input, "old_str")?;
                let new = input.get("new_str").and_then(Value::as_str).unwrap_or("");
                let text = self.read_text(path).await?;
                match text.matches(old).count() {
                    0 => bail!("old_str not found in {path}"),
                    1 => {}
                    n => bail!("old_str matches {n} times in {path}; include more context to make it unique"),
                }
                self.sandbox
                    .write_file(path, text.replacen(old, new, 1).into_bytes())
                    .await?;
                Ok(ToolOutcome::ok(format!("Edited {path}")))
            }
            "insert" => {
                let line = input
                    .get("insert_line")
                    .and_then(Value::as_u64)
                    .ok_or_else(|| anyhow!("missing `insert_line`"))? as usize;
                let new = str_field(input, "insert_text")?;
                let text = self.read_text(path).await?;
                let mut lines: Vec<&str> = text.lines().collect();
                if line > lines.len() {
                    bail!("insert_line {line} is past end of file ({} lines)", lines.len());
                }
                lines.insert(line, new.trim_end_matches('\n'));
                let mut joined = lines.join("\n");
                if text.ends_with('\n') || text.is_empty() {
                    joined.push('\n');
                }
                self.sandbox.write_file(path, joined.into_bytes()).await?;
                Ok(ToolOutcome::ok(format!("Inserted text after line {line} of {path}")))
            }
            other => bail!("unsupported command: {other}"),
        }
    }

    async fn read_text(&self, path: &str) -> Result<String> {
        let bytes = self.sandbox.read_file(path).await?;
        String::from_utf8(bytes).map_err(|_| anyhow!("{path} is not a UTF-8 text file"))
    }
}

/// Every envd process is a fresh `bash -c`, but the bash tool promises a persistent
/// session. Emulate it by saving cwd + exported env after each command and
/// restoring them before the next.
fn persistent_shell(command: &str, workdir: &str) -> String {
    format!(
        r#"mkdir -p {STATE_DIR}
[ -f {STATE_DIR}/env ] && source {STATE_DIR}/env >/dev/null 2>&1
cd "$(cat {STATE_DIR}/cwd 2>/dev/null || echo {workdir})" 2>/dev/null || cd {workdir}
{{
{command}
}}
__torvald_ec=$?
pwd > {STATE_DIR}/cwd
export -p > {STATE_DIR}/env 2>/dev/null
exit $__torvald_ec"#,
        workdir = sh_quote(workdir),
    )
}

fn str_field<'a>(input: &'a Value, key: &str) -> Result<&'a str> {
    input
        .get(key)
        .and_then(Value::as_str)
        .ok_or_else(|| anyhow!("missing `{key}`"))
}

fn checked_path(path: &str) -> Result<&str> {
    if !path.starts_with('/') {
        bail!("path must be absolute: {path}");
    }
    if path.split('/').any(|seg| seg == "..") {
        bail!("path must not contain `..`: {path}");
    }
    Ok(path)
}

pub fn sh_quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', r"'\''"))
}

/// Keep the head and tail of long output; the middle is usually the least useful.
pub fn truncate(s: &str) -> String {
    if s.len() <= MAX_RESULT_CHARS {
        return s.to_string();
    }
    let half = MAX_RESULT_CHARS / 2;
    let head_end = floor_char_boundary(s, half);
    let tail_start = floor_char_boundary(s, s.len() - half);
    format!(
        "{}\n\n[... {} characters truncated ...]\n\n{}",
        &s[..head_end],
        tail_start - head_end,
        &s[tail_start..]
    )
}

fn floor_char_boundary(s: &str, mut i: usize) -> usize {
    while !s.is_char_boundary(i) {
        i -= 1;
    }
    i
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn quoting_survives_single_quotes() {
        assert_eq!(sh_quote("it's"), r"'it'\''s'");
    }

    #[test]
    fn rejects_relative_and_traversal_paths() {
        assert!(checked_path("src/main.rs").is_err());
        assert!(checked_path("/home/user/../etc/passwd").is_err());
        assert!(checked_path("/home/user/repo/src/main.rs").is_ok());
    }

    #[test]
    fn truncate_keeps_head_and_tail_on_char_boundaries() {
        let s = "é".repeat(MAX_RESULT_CHARS);
        let t = truncate(&s);
        assert!(t.contains("characters truncated"));
        assert!(t.starts_with('é') && t.ends_with('é'));
    }

    #[test]
    fn persistent_shell_wraps_command() {
        let script = persistent_shell("cargo test", "/home/user/repo");
        assert!(script.contains("\ncargo test\n"));
        assert!(script.contains("'/home/user/repo'"));
    }
}
