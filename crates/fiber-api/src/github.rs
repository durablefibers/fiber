//! GitHub API helpers (PR file lists for path filters).

use anyhow::{Context, Result};
use serde::Deserialize;
use tracing::{info, warn};

const DEFAULT_API: &str = "https://api.github.com";

/// Resolve a token for GitHub API calls.
/// Order: project secrets `GITHUB_TOKEN` / `FIBER_GITHUB_TOKEN`, then env
/// `FIBER_GITHUB_TOKEN` / `GITHUB_TOKEN`.
pub async fn resolve_token(
    store: &fiber_core::Store,
    project_id: uuid::Uuid,
) -> Result<Option<String>> {
    for key in ["GITHUB_TOKEN", "FIBER_GITHUB_TOKEN"] {
        if let Some(v) = store.get_secret_plain(project_id, key).await? {
            let v = v.trim().to_string();
            if !v.is_empty() {
                return Ok(Some(v));
            }
        }
    }
    for key in ["FIBER_GITHUB_TOKEN", "GITHUB_TOKEN"] {
        if let Ok(v) = std::env::var(key) {
            let v = v.trim().to_string();
            if !v.is_empty() {
                return Ok(Some(v));
            }
        }
    }
    Ok(None)
}

pub fn api_base() -> String {
    std::env::var("FIBER_GITHUB_API_URL")
        .ok()
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| DEFAULT_API.into())
}

/// `owner/repo` from a webhook payload.
pub fn repo_full_name(payload: &serde_json::Value) -> Option<(String, String)> {
    if let Some(full) = payload
        .pointer("/repository/full_name")
        .and_then(|v| v.as_str())
    {
        let mut parts = full.splitn(2, '/');
        if let (Some(o), Some(r)) = (parts.next(), parts.next()) {
            if !o.is_empty() && !r.is_empty() {
                return Some((o.to_string(), r.to_string()));
            }
        }
    }
    let owner = payload
        .pointer("/repository/owner/login")
        .and_then(|v| v.as_str())?;
    let name = payload
        .pointer("/repository/name")
        .and_then(|v| v.as_str())?;
    Some((owner.to_string(), name.to_string()))
}

#[derive(Debug, Deserialize)]
struct PrFile {
    filename: String,
}

/// List paths changed in a pull request (paginated).
pub async fn list_pull_request_files(
    owner: &str,
    repo: &str,
    number: i64,
    token: &str,
) -> Result<Vec<String>> {
    let client = reqwest::Client::builder()
        .user_agent("fiber-ci")
        .build()
        .context("http client")?;
    let base = api_base().trim_end_matches('/').to_string();
    let mut files = Vec::new();
    let mut page = 1u32;
    loop {
        let url = format!(
            "{base}/repos/{owner}/{repo}/pulls/{number}/files?per_page=100&page={page}"
        );
        let resp = client
            .get(&url)
            .bearer_auth(token)
            .header("Accept", "application/vnd.github+json")
            .header("X-GitHub-Api-Version", "2022-11-28")
            .send()
            .await
            .with_context(|| format!("github list files {owner}/{repo}#{number}"))?;
        if !resp.status().is_success() {
            let status = resp.status();
            let body = resp.text().await.unwrap_or_default();
            anyhow::bail!("github API {status}: {body}");
        }
        let batch: Vec<PrFile> = resp.json().await.context("parse pr files")?;
        let n = batch.len();
        for f in batch {
            files.push(f.filename);
        }
        if n < 100 {
            break;
        }
        page += 1;
        if page > 30 {
            warn!(%owner, %repo, number, "pr files pagination capped at 3000");
            break;
        }
    }
    info!(
        %owner,
        %repo,
        number,
        count = files.len(),
        "fetched pull request changed files"
    );
    Ok(files)
}
