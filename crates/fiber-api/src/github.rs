//! GitHub API helpers (PR file lists for path filters).

use anyhow::{Context, Result};
use serde::Deserialize;
use tracing::{info, warn};

const DEFAULT_API: &str = "https://api.github.com";

/// A token for **writing** a commit status: project secrets only.
///
/// Deliberately not `resolve_token`, whose environment fallback belongs to whoever runs
/// `fiber-api`. Writing with that would let any project admin point a pipeline at a
/// repository they do not own and post statuses to it under the instance's identity.
pub async fn resolve_status_token(
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
    Ok(None)
}

/// Resolve a token for read-only GitHub API calls (the PR file list).
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
        .timeout(std::time::Duration::from_secs(15))
        .build()
        .context("http client")?;
    let base = api_base().trim_end_matches('/').to_string();
    let mut files = Vec::new();
    let mut page = 1u32;
    loop {
        let url =
            format!("{base}/repos/{owner}/{repo}/pulls/{number}/files?per_page=100&page={page}");
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

/// Public base URL of this Fiber, used to link a commit status back to the run page.
pub fn public_url() -> Option<String> {
    std::env::var("FIBER_PUBLIC_URL")
        .ok()
        .map(|s| s.trim_end_matches('/').to_string())
        .filter(|s| !s.is_empty())
}

/// One commit status to post.
pub struct CommitStatus<'a> {
    pub owner: &'a str,
    pub repo: &'a str,
    pub sha: &'a str,
    /// GitHub's vocabulary: `pending`, `success`, `failure`, or `error`.
    pub state: &'a str,
    pub context: &'a str,
    pub description: &'a str,
    pub target_url: Option<&'a str>,
}

/// Report a run's outcome against the commit that triggered it, so a pull request can
/// require it as a check.
///
/// `state` is GitHub's vocabulary: `pending`, `success`, `failure`, or `error`. Failures
/// here are logged and swallowed — a status that cannot be posted must not fail the run.
pub async fn post_commit_status(status: CommitStatus<'_>, token: &str) -> Result<()> {
    let CommitStatus {
        owner,
        repo,
        sha,
        state,
        context,
        description,
        target_url,
    } = status;
    let client = reqwest::Client::builder()
        .user_agent("fiber-ci")
        // GitHub gives a webhook delivery ~10s; never hold one open longer than that.
        .timeout(std::time::Duration::from_secs(10))
        .build()
        .context("http client")?;
    let base = api_base().trim_end_matches('/').to_string();
    let mut body = serde_json::json!({
        "state": state,
        "context": context,
        // GitHub truncates at 140 characters.
        "description": description.chars().take(140).collect::<String>(),
    });
    if let Some(url) = target_url {
        body["target_url"] = serde_json::Value::String(url.to_string());
    }
    let resp = client
        .post(format!("{base}/repos/{owner}/{repo}/statuses/{sha}"))
        .bearer_auth(token)
        .header("Accept", "application/vnd.github+json")
        .header("X-GitHub-Api-Version", "2022-11-28")
        .json(&body)
        .send()
        .await
        .with_context(|| format!("post status {owner}/{repo}@{sha}"))?;
    if !resp.status().is_success() {
        let status = resp.status();
        let text = resp.text().await.unwrap_or_default();
        anyhow::bail!("github status API {status}: {text}");
    }
    info!(%owner, %repo, %sha, %state, %context, "posted commit status");
    Ok(())
}

/// `owner/repo` from a git remote URL (`https://github.com/o/r.git`, `git@github.com:o/r`).
pub fn repo_from_remote(url: &str) -> Option<(String, String)> {
    let rest = url
        .trim_end_matches('/')
        .trim_end_matches(".git")
        .rsplit_once(':')
        .map(|(_, r)| r)
        .filter(|r| !r.starts_with("//"))
        .unwrap_or(url.trim_end_matches('/').trim_end_matches(".git"));
    let mut parts: Vec<&str> = rest.rsplit('/').take(2).collect();
    parts.reverse();
    match parts.as_slice() {
        [owner, repo] if !owner.is_empty() && !repo.is_empty() && !owner.contains(':') => {
            Some(((*owner).to_string(), (*repo).to_string()))
        }
        _ => None,
    }
}

/// A path segment safe to interpolate into a GitHub API URL.
fn is_safe_segment(s: &str) -> bool {
    !s.is_empty()
        && s.len() <= 100
        && s.chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_' || c == '.')
        && s != "."
        && s != ".."
}

/// Post the status for a run, if it came from a webhook and a token is available.
/// Silent when the run has no commit (a manual or scheduled run has nothing to report on).
pub async fn report_run_status(store: &fiber_core::Store, run: &fiber_core::Run) {
    let (Some(sha), Some(full_name)) = (run.head_sha.as_deref(), run.repo_full_name.as_deref())
    else {
        return;
    };
    let Some((owner, repo)) = full_name.split_once('/') else {
        return;
    };
    // The repository name arrived in a webhook body. Even signed, it must not be able to
    // aim the project's token at another repository or escape the URL path.
    let sha_ok = matches!(sha.len(), 40 | 64) && sha.chars().all(|c| c.is_ascii_hexdigit());
    if !is_safe_segment(owner) || !is_safe_segment(repo) || !sha_ok {
        warn!(%full_name, "refusing to post a status for an implausible repo or sha");
        return;
    }
    let token = match resolve_status_token(store, run.project_id).await {
        Ok(Some(t)) => t,
        Ok(None) => return,
        Err(e) => {
            warn!(error = %e, "resolve github token for commit status");
            return;
        }
    };
    let (state, description) = match run.status.as_str() {
        "succeeded" => ("success", "All steps passed"),
        "failed" => ("failure", "A step failed"),
        "cancelled" => ("error", "Run cancelled"),
        _ => ("pending", "Running"),
    };
    // One context per pipeline, so several pipelines on a repo report separately and can
    // be required independently.
    let Ok(Some(pipeline)) = store.get_pipeline(run.pipeline_id).await else {
        return;
    };
    // Only report on the repository this pipeline actually builds.
    let configured = fiber_core::store::value_to_definition(&pipeline.definition)
        .ok()
        .and_then(|d| d.workspace)
        .and_then(|w| repo_from_remote(&w.repo));
    match &configured {
        Some((o, r)) if o.eq_ignore_ascii_case(owner) && r.eq_ignore_ascii_case(repo) => {}
        Some((o, r)) => {
            warn!(
                run_id = %run.id, %full_name, expected = %format!("{o}/{r}"),
                "webhook repository does not match the pipeline's workspace; not posting a status"
            );
            return;
        }
        None => {
            warn!(
                run_id = %run.id,
                "pipeline has no git workspace to check the status repository against"
            );
            return;
        }
    }
    let name = pipeline.name;
    let context = format!("fiber/{name}");
    let target = public_url().map(|b| format!("{b}/p/{}/runs/{}", run.project_id, run.id));
    if let Err(e) = post_commit_status(
        CommitStatus {
            owner,
            repo,
            sha,
            state,
            context: &context,
            description,
            target_url: target.as_deref(),
        },
        &token,
    )
    .await
    {
        warn!(error = %e, run_id = %run.id, "could not post commit status");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn repo_from_remote_handles_common_forms() {
        for url in [
            "https://github.com/octocat/Hello-World.git",
            "https://github.com/octocat/Hello-World",
            "git@github.com:octocat/Hello-World.git",
            "ssh://git@github.com/octocat/Hello-World.git",
        ] {
            assert_eq!(
                repo_from_remote(url),
                Some(("octocat".into(), "Hello-World".into())),
                "{url}"
            );
        }
    }

    #[test]
    fn path_segments_that_could_escape_the_api_url_are_refused() {
        assert!(is_safe_segment("Hello-World"));
        assert!(is_safe_segment("repo.name_1"));
        assert!(!is_safe_segment(""));
        assert!(!is_safe_segment(".."));
        assert!(!is_safe_segment("a/b"));
        assert!(!is_safe_segment("a?b"));
        assert!(!is_safe_segment("a b"));
        assert!(!is_safe_segment("a%2Fb"));
    }
}
