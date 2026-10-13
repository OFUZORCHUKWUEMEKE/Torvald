//! The few GitHub REST calls Torvald needs: read issues, label/comment on them,
//! open pull requests.

use std::time::Duration;

use anyhow::{bail, Context, Result};
use reqwest::{Client, RequestBuilder};
use serde::{de::DeserializeOwned, Deserialize};
use serde_json::{json, Value};

const API: &str = "https://api.github.com";

#[derive(Clone)]
pub struct GitHub {
    http: Client,
    token: String,
}

#[derive(Debug, Deserialize)]
pub struct Issue {
    pub number: u64,
    pub title: String,
    pub body: Option<String>,
    #[serde(default)]
    pub labels: Vec<Label>,
    /// Present when the "issue" is really a pull request.
    pub pull_request: Option<Value>,
}

#[derive(Debug, Deserialize)]
pub struct Label {
    pub name: String,
}

impl Issue {
    pub fn has_label(&self, name: &str) -> bool {
        self.labels.iter().any(|l| l.name == name)
    }
}

#[derive(Debug, Deserialize)]
pub struct PullRequest {
    pub html_url: String,
}

impl GitHub {
    pub fn new(token: String) -> Result<Self> {
        let http = Client::builder()
            .user_agent("torvald")
            .timeout(Duration::from_secs(30))
            .build()?;
        Ok(Self { http, token })
    }

    pub fn token(&self) -> &str {
        &self.token
    }

    fn req(&self, rb: RequestBuilder) -> RequestBuilder {
        rb.bearer_auth(&self.token)
            .header("Accept", "application/vnd.github+json")
            .header("X-GitHub-Api-Version", "2022-11-28")
    }

    async fn send<T: DeserializeOwned>(&self, rb: RequestBuilder, what: &str) -> Result<T> {
        let res = self.req(rb).send().await.with_context(|| what.to_string())?;
        let status = res.status();
        if !status.is_success() {
            bail!("{what}: HTTP {status}: {}", res.text().await.unwrap_or_default());
        }
        res.json().await.with_context(|| what.to_string())
    }

    pub async fn default_branch(&self, repo: &str) -> Result<String> {
        let v: Value = self
            .send(self.http.get(format!("{API}/repos/{repo}")), "reading repository")
            .await?;
        Ok(v["default_branch"].as_str().unwrap_or("main").to_string())
    }

    pub async fn open_issues_with_label(&self, repo: &str, label: &str) -> Result<Vec<Issue>> {
        let issues: Vec<Issue> = self
            .send(
                self.http.get(format!("{API}/repos/{repo}/issues")).query(&[
                    ("state", "open"),
                    ("labels", label),
                    ("per_page", "50"),
                    ("sort", "created"),
                    ("direction", "asc"),
                ]),
                "listing issues",
            )
            .await?;
        Ok(issues.into_iter().filter(|i| i.pull_request.is_none()).collect())
    }

    pub async fn add_labels(&self, repo: &str, issue: u64, labels: &[&str]) -> Result<()> {
        let _: Value = self
            .send(
                self.http
                    .post(format!("{API}/repos/{repo}/issues/{issue}/labels"))
                    .json(&json!({ "labels": labels })),
                "adding labels",
            )
            .await?;
        Ok(())
    }

    pub async fn remove_label(&self, repo: &str, issue: u64, label: &str) -> Result<()> {
        let res = self
            .req(
                self.http
                    .delete(format!("{API}/repos/{repo}/issues/{issue}/labels/{label}")),
            )
            .send()
            .await?;
        // 404 just means the label was not there.
        if !res.status().is_success() && res.status().as_u16() != 404 {
            bail!("removing label {label}: HTTP {}", res.status());
        }
        Ok(())
    }

    pub async fn comment(&self, repo: &str, issue: u64, body: &str) -> Result<()> {
        let _: Value = self
            .send(
                self.http
                    .post(format!("{API}/repos/{repo}/issues/{issue}/comments"))
                    .json(&json!({ "body": body })),
                "commenting",
            )
            .await?;
        Ok(())
    }

    pub async fn create_pull_request(
        &self,
        repo: &str,
        head: &str,
        base: &str,
        title: &str,
        body: &str,
    ) -> Result<PullRequest> {
        self.send(
            self.http.post(format!("{API}/repos/{repo}/pulls")).json(&json!({
                "title": title,
                "head": head,
                "base": base,
                "body": body,
                "draft": false,
            })),
            "opening pull request",
        )
        .await
    }
}
