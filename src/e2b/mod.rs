//! E2B client: the control plane (`api.e2b.app`, REST) creates and kills sandboxes,
//! the data plane (`envd` inside each sandbox, Connect-RPC + REST on port 49983)
//! runs processes and moves files. E2B ships no Rust SDK, so we speak the wire
//! protocol directly — see `spec/openapi.yml` and `spec/envd/` in e2b-dev/E2B.

mod connect;

use std::collections::HashMap;
use std::time::Duration;

use anyhow::{anyhow, bail, Context, Result};
use base64::{engine::general_purpose::STANDARD as B64, Engine};
use futures_util::StreamExt;
use reqwest::{header::HeaderMap, Client, RequestBuilder, StatusCode};
use serde::Deserialize;
use serde_json::json;

const ENVD_PORT: u16 = 49983;
/// Default unprivileged user in E2B base templates.
const SANDBOX_USER: &str = "user";
/// Per-stream cap on captured process output, to bound memory on runaway commands.
const MAX_CAPTURE_BYTES: usize = 4 * 1024 * 1024;

#[derive(Clone)]
pub struct E2bClient {
    http: Client,
    api_key: String,
    api_url: String,
    domain: String,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct CreateSandboxResponse {
    #[serde(rename = "sandboxID")]
    sandbox_id: String,
    envd_access_token: Option<String>,
    domain: Option<String>,
}

pub struct SandboxSpec<'a> {
    pub template: &'a str,
    pub timeout: Duration,
    pub metadata: HashMap<String, String>,
}

impl E2bClient {
    pub fn new(api_key: String, domain: String) -> Result<Self> {
        let http = Client::builder().connect_timeout(Duration::from_secs(20)).build()?;
        let api_url = std::env::var("E2B_API_URL").unwrap_or_else(|_| format!("https://api.{domain}"));
        Ok(Self {
            http,
            api_key,
            api_url,
            domain,
        })
    }

    fn api(&self, req: RequestBuilder) -> RequestBuilder {
        req.header("X-API-KEY", &self.api_key).timeout(Duration::from_secs(60))
    }

    pub async fn create_sandbox(&self, spec: SandboxSpec<'_>) -> Result<Sandbox> {
        let body = json!({
            "templateID": spec.template,
            "timeout": spec.timeout.as_secs(),
            "metadata": spec.metadata,
        });
        let res = self
            .api(self.http.post(format!("{}/v2/sandboxes", self.api_url)))
            .json(&body)
            .send()
            .await
            .context("creating E2B sandbox")?;
        let created: CreateSandboxResponse = ok_json(res).await.context("creating E2B sandbox")?;
        let domain = created.domain.unwrap_or_else(|| self.domain.clone());
        Ok(Sandbox {
            envd_url: format!("https://{ENVD_PORT}-{}.{domain}", created.sandbox_id),
            id: created.sandbox_id,
            access_token: created.envd_access_token,
            client: self.clone(),
        })
    }

    pub async fn kill(&self, sandbox_id: &str) -> Result<()> {
        let res = self
            .api(self.http.delete(format!("{}/sandboxes/{sandbox_id}", self.api_url)))
            .send()
            .await?;
        if !res.status().is_success() && res.status() != StatusCode::NOT_FOUND {
            bail!("killing sandbox {sandbox_id}: HTTP {}", res.status());
        }
        Ok(())
    }

    /// Reset the sandbox TTL to `timeout` from now.
    pub async fn set_timeout(&self, sandbox_id: &str, timeout: Duration) -> Result<()> {
        let res = self
            .api(
                self.http
                    .post(format!("{}/sandboxes/{sandbox_id}/timeout", self.api_url)),
            )
            .json(&json!({ "timeout": timeout.as_secs() }))
            .send()
            .await?;
        if !res.status().is_success() {
            bail!("extending sandbox {sandbox_id}: HTTP {}", res.status());
        }
        Ok(())
    }
}

pub struct Sandbox {
    pub id: String,
    envd_url: String,
    access_token: Option<String>,
    client: E2bClient,
}

#[derive(Debug, Default)]
pub struct CommandOutput {
    pub stdout: String,
    pub stderr: String,
    pub exit_code: i32,
    pub timed_out: bool,
}

#[derive(Default)]
pub struct RunOpts<'a> {
    pub cwd: Option<&'a str>,
    pub envs: HashMap<String, String>,
    pub timeout: Option<Duration>,
}

#[derive(Debug, Deserialize)]
struct StartResponse {
    event: Option<ProcessEvent>,
}

#[derive(Debug, Deserialize)]
struct ProcessEvent {
    start: Option<StartEvent>,
    data: Option<DataEvent>,
    end: Option<EndEvent>,
}

#[derive(Debug, Deserialize)]
struct StartEvent {
    pid: u32,
}

#[derive(Debug, Deserialize)]
struct DataEvent {
    stdout: Option<String>,
    stderr: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct EndEvent {
    #[serde(default)]
    exit_code: i32,
    error: Option<String>,
}

impl Sandbox {
    fn envd_headers(&self) -> HeaderMap {
        let mut h = HeaderMap::new();
        let basic = B64.encode(format!("{SANDBOX_USER}:"));
        h.insert("Authorization", format!("Basic {basic}").parse().unwrap());
        h.insert("E2b-Sandbox-Id", self.id.parse().unwrap());
        h.insert("E2b-Sandbox-Port", ENVD_PORT.into());
        if let Some(tok) = &self.access_token {
            h.insert("X-Access-Token", tok.parse().unwrap());
        }
        h
    }

    /// Run `cmd` through `bash -l -c` and wait for it to finish (or time out).
    pub async fn run(&self, cmd: &str, opts: RunOpts<'_>) -> Result<CommandOutput> {
        let timeout = opts.timeout.unwrap_or(Duration::from_secs(600));
        let mut process = json!({
            "cmd": "/bin/bash",
            "args": ["-l", "-c", cmd],
            "envs": opts.envs,
        });
        if let Some(cwd) = opts.cwd {
            process["cwd"] = json!(cwd);
        }
        let body = connect::encode_envelope(&serde_json::to_vec(&json!({ "process": process }))?);

        let res = self
            .client
            .http
            .post(format!("{}/process.Process/Start", self.envd_url))
            .headers(self.envd_headers())
            .header("Content-Type", "application/connect+json")
            .header("Keepalive-Ping-Interval", "50")
            .body(body)
            .send()
            .await
            .context("starting sandbox process")?;
        if !res.status().is_success() {
            let status = res.status();
            bail!(
                "starting sandbox process: HTTP {status}: {}",
                res.text().await.unwrap_or_default()
            );
        }

        let mut out = CommandOutput::default();
        let mut pid = None;
        let mut stream = res.bytes_stream();
        let mut dec = connect::Decoder::default();

        let consume = async {
            while let Some(chunk) = stream.next().await {
                dec.push(&chunk.context("reading process stream")?);
                while let Some(frame) = dec.next_frame() {
                    if frame.is_end_stream() {
                        return connect::check_end_stream(&frame.payload);
                    }
                    let Some(ev) = serde_json::from_slice::<StartResponse>(&frame.payload)?.event else {
                        continue;
                    };
                    if let Some(s) = ev.start {
                        pid = Some(s.pid);
                    }
                    if let Some(d) = ev.data {
                        append_b64(&mut out.stdout, d.stdout.as_deref())?;
                        append_b64(&mut out.stderr, d.stderr.as_deref())?;
                    }
                    if let Some(e) = ev.end {
                        out.exit_code = e.exit_code;
                        if let Some(err) = e.error.filter(|e| !e.is_empty()) {
                            out.stderr.push_str(&err);
                        }
                    }
                }
            }
            Ok(())
        };

        match tokio::time::timeout(timeout, consume).await {
            Ok(result) => result?,
            Err(_) => {
                out.timed_out = true;
                out.exit_code = -1;
                if let Some(pid) = pid {
                    let _ = self.kill_process(pid).await;
                }
            }
        }
        Ok(out)
    }

    async fn kill_process(&self, pid: u32) -> Result<()> {
        self.client
            .http
            .post(format!("{}/process.Process/SendSignal", self.envd_url))
            .headers(self.envd_headers())
            .header("Content-Type", "application/json")
            .header("Connect-Protocol-Version", "1")
            .json(&json!({ "process": { "pid": pid }, "signal": "SIGNAL_SIGKILL" }))
            .timeout(Duration::from_secs(15))
            .send()
            .await?
            .error_for_status()?;
        Ok(())
    }

    pub async fn read_file(&self, path: &str) -> Result<Vec<u8>> {
        let res = self
            .client
            .http
            .get(format!("{}/files", self.envd_url))
            .query(&[("path", path), ("username", SANDBOX_USER)])
            .headers(self.envd_headers())
            .timeout(Duration::from_secs(60))
            .send()
            .await?;
        match res.status() {
            s if s.is_success() => Ok(res.bytes().await?.to_vec()),
            StatusCode::NOT_FOUND => Err(anyhow!("file not found: {path}")),
            s => Err(anyhow!(
                "reading {path}: HTTP {s}: {}",
                res.text().await.unwrap_or_default()
            )),
        }
    }

    /// Write (create or overwrite) a file; parent directories are created by envd.
    pub async fn write_file(&self, path: &str, data: Vec<u8>) -> Result<()> {
        let part = reqwest::multipart::Part::bytes(data).file_name(path.to_string());
        let form = reqwest::multipart::Form::new().part("file", part);
        let res = self
            .client
            .http
            .post(format!("{}/files", self.envd_url))
            .query(&[("path", path), ("username", SANDBOX_USER)])
            .headers(self.envd_headers())
            .multipart(form)
            .timeout(Duration::from_secs(120))
            .send()
            .await?;
        if !res.status().is_success() {
            let s = res.status();
            bail!("writing {path}: HTTP {s}: {}", res.text().await.unwrap_or_default());
        }
        Ok(())
    }

    pub async fn keep_alive(&self, timeout: Duration) -> Result<()> {
        self.client.set_timeout(&self.id, timeout).await
    }

    pub async fn kill(self) -> Result<()> {
        self.client.kill(&self.id).await
    }
}

fn append_b64(dst: &mut String, chunk: Option<&str>) -> Result<()> {
    if let Some(c) = chunk {
        if dst.len() < MAX_CAPTURE_BYTES {
            dst.push_str(&String::from_utf8_lossy(&B64.decode(c)?));
        }
    }
    Ok(())
}

async fn ok_json<T: serde::de::DeserializeOwned>(res: reqwest::Response) -> Result<T> {
    let status = res.status();
    if !status.is_success() {
        bail!("HTTP {status}: {}", res.text().await.unwrap_or_default());
    }
    Ok(res.json::<T>().await?)
}
