//! GitHub API helpers (PR file lists for path filters).

use anyhow::{Context, Result};
use serde::Deserialize;
use tracing::{info, warn};

const DEFAULT_API: &str = "https://api.github.com";

/// Project secrets that may hold a GitHub token, in the order they are tried.
const PROJECT_TOKEN_SECRETS: [&str; 2] = ["GITHUB_TOKEN", "FIBER_GITHUB_TOKEN"];
/// Environment variables that used to be an instance-wide fallback. Read now only to
/// tell the operator they are no longer used.
const INSTANCE_TOKEN_ENV: [&str; 2] = ["FIBER_GITHUB_TOKEN", "GITHUB_TOKEN"];

/// Which token a GitHub call made for a project uses.
#[derive(Debug, PartialEq, Eq)]
enum TokenChoice {
    /// The project's own secret.
    Project(String),
    /// The project has none, so the call is not made. `instance_token_ignored` says the
    /// process environment holds one that was deliberately not used.
    None { instance_token_ignored: bool },
}

/// The project's own token, or nothing — never the instance's.
///
/// The token is spent on a repository named by a webhook body (`repository.full_name`).
/// The signature on that body proves only that the sender holds the project's webhook
/// secret, and a project admin sets that secret themselves; any logged-in user can create
/// a project and be its admin. With an instance-wide fallback, such a user could sign a
/// `pull_request` payload naming a repository they cannot see and read from the response
/// (`files_source`, `changed_files`, which path-filtered pipelines started) what the
/// instance's token can. Requiring the payload's repository to match a pipeline's
/// `workspace.repo` would not help: the same user writes the pipeline.
///
/// A project's secret is set by the same people who set its webhook secret, so spending
/// it on a repository they name gives them nothing they did not have.
fn select_token(
    project_secrets: &[Option<String>],
    instance_env: &[Option<String>],
) -> TokenChoice {
    fn first(values: &[Option<String>]) -> Option<String> {
        values
            .iter()
            .flatten()
            .map(|v| v.trim())
            .find(|v| !v.is_empty())
            .map(str::to_string)
    }
    match first(project_secrets) {
        Some(token) => TokenChoice::Project(token),
        None => TokenChoice::None {
            instance_token_ignored: first(instance_env).is_some(),
        },
    }
}

/// A token for GitHub API calls made on a project's behalf — the PR file list and commit
/// statuses: the project secret `GITHUB_TOKEN` / `FIBER_GITHUB_TOKEN`, and nothing else.
/// See [`select_token`] for why the process environment is not a fallback.
pub async fn resolve_project_token(
    store: &fiber_core::Store,
    project_id: uuid::Uuid,
) -> Result<Option<String>> {
    let mut project_secrets = Vec::with_capacity(PROJECT_TOKEN_SECRETS.len());
    for key in PROJECT_TOKEN_SECRETS {
        project_secrets.push(store.get_secret_plain(project_id, key).await?);
    }
    let instance_env = INSTANCE_TOKEN_ENV.map(|key| std::env::var(key).ok());
    match select_token(&project_secrets, &instance_env) {
        TokenChoice::Project(token) => Ok(Some(token)),
        TokenChoice::None {
            instance_token_ignored,
        } => {
            if instance_token_ignored {
                // Once: this is an upgrade note, not an event.
                static WARNED: std::sync::Once = std::sync::Once::new();
                WARNED.call_once(|| {
                    warn!(
                        %project_id,
                        "FIBER_GITHUB_TOKEN / GITHUB_TOKEN in the environment is no longer used as a fallback; \
                         set a project secret GITHUB_TOKEN for PR path filters and commit statuses"
                    );
                });
            }
            Ok(None)
        }
    }
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
        let Some(path) = pull_request_files_path(owner, repo, number, page) else {
            anyhow::bail!("refusing to list files for an implausible repository or pull request");
        };
        let url = format!("{base}{path}");
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

/// The API path listing a pull request's files, or `None` when a segment could not have
/// come from a real repository.
///
/// The owner, name, and number arrive in a webhook body. The signature proves the body
/// came from whoever holds the project's secret — which a project admin sets themselves —
/// so it cannot be allowed to steer the token at a different path.
fn pull_request_files_path(owner: &str, repo: &str, number: i64, page: u32) -> Option<String> {
    if !is_safe_segment(owner) || !is_safe_segment(repo) || number <= 0 {
        return None;
    }
    Some(format!(
        "/repos/{owner}/{repo}/pulls/{number}/files?per_page=100&page={page}"
    ))
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
    let token = match resolve_project_token(store, run.project_id).await {
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

    fn some(s: &str) -> Option<String> {
        Some(s.to_string())
    }

    /// The hole: a project with no token of its own borrowed the instance's, for a
    /// repository its own webhook payload named.
    #[test]
    fn the_instance_token_is_never_chosen() {
        assert_eq!(
            select_token(&[None, None], &[some("ghp_instance"), None]),
            TokenChoice::None {
                instance_token_ignored: true
            }
        );
        assert_eq!(
            select_token(&[some("  "), None], &[None, some("ghp_instance")]),
            TokenChoice::None {
                instance_token_ignored: true
            },
            "a blank project secret is not a token, and still does not fall through"
        );
        assert_eq!(
            select_token(&[None, None], &[None, some(" ")]),
            TokenChoice::None {
                instance_token_ignored: false
            }
        );
    }

    #[test]
    fn a_project_secret_is_used_trimmed_and_in_order() {
        assert_eq!(
            select_token(&[some(" ghp_a\n"), some("ghp_b")], &[some("ghp_instance")]),
            TokenChoice::Project("ghp_a".into())
        );
        assert_eq!(
            select_token(&[some(""), some("ghp_b")], &[]),
            TokenChoice::Project("ghp_b".into())
        );
    }

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

    #[test]
    fn the_pull_request_files_path_refuses_what_the_status_path_refuses() {
        assert_eq!(
            pull_request_files_path("octocat", "Hello-World", 7, 2).as_deref(),
            Some("/repos/octocat/Hello-World/pulls/7/files?per_page=100&page=2")
        );
        // A signed body from a project admin must not be able to aim the instance token
        // at another path.
        assert!(pull_request_files_path("a/b/..", "x", 1, 1).is_none());
        assert!(pull_request_files_path("octocat", "..", 1, 1).is_none());
        assert!(pull_request_files_path("octocat", "Hello-World", 0, 1).is_none());
        assert!(pull_request_files_path("octocat", "Hello-World", -5, 1).is_none());
    }
}
