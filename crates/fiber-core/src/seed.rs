//! Demo project + complex DAG pipelines seeded on API boot.

use crate::models::{CreatePipelineRequest, CreateProjectRequest};
use crate::roles::ProjectRole;
use crate::store::Store;
use anyhow::Result;
use serde_json::{json, Value};
use uuid::Uuid;

const SHOWCASE_SLUG: &str = "showcase";

pub async fn ensure_showcase(store: &Store, owner_id: Uuid) -> Result<()> {
    let project = match store.get_project_by_slug(SHOWCASE_SLUG).await? {
        Some(p) => {
            store
                .add_project_member(p.id, owner_id, ProjectRole::Owner)
                .await?;
            p
        }
        None => {
            store
                .create_project(
                    owner_id,
                    CreateProjectRequest {
                        name: "Showcase".into(),
                        slug: Some(SHOWCASE_SLUG.into()),
                    },
                )
                .await?
        }
    };

    let existing = store.list_pipelines(project.id).await?;
    let names: std::collections::HashSet<String> =
        existing.iter().map(|p| p.name.clone()).collect();

    for (name, definition) in showcase_pipelines() {
        if names.contains(name) {
            continue;
        }
        store
            .create_pipeline(
                project.id,
                CreatePipelineRequest {
                    name: name.to_string(),
                    definition,
                },
            )
            .await?;
        tracing::info!(pipeline = name, "seeded showcase pipeline");
    }

    let secrets = store.list_secret_keys(project.id).await?;
    if !secrets.iter().any(|s| s.key == "DEMO_TOKEN") {
        store
            .upsert_secret(project.id, "DEMO_TOKEN", "showcase-demo-token")
            .await?;
    }

    Ok(())
}

fn showcase_pipelines() -> Vec<(&'static str, Value)> {
    vec![
        ("build-and-test", simple_linear()),
        ("diamond-ci", diamond_ci()),
        ("fan-out-tests", fan_out_tests()),
        ("release-with-artifacts", release_with_artifacts()),
        ("retry-and-skip", retry_and_skip()),
        ("nightly-interval", nightly_interval()),
    ]
}

fn step(
    id: &str,
    name: &str,
    needs: &[&str],
    run: &str,
    labels: &[&str],
    retries: u32,
    artifacts: &[&str],
) -> Value {
    json!({
        "id": id,
        "name": name,
        "needs": needs,
        "run": run,
        "labels": labels,
        "retries": retries,
        "artifacts": artifacts,
    })
}

fn simple_linear() -> Value {
    json!({
        "name": "build-and-test",
        "steps": [
            step("checkout", "checkout", &[], "ls -la && echo ready", &["os=linux"], 0, &[]),
            step("build", "build", &["checkout"], "echo building && sleep 0.2", &["os=linux"], 0, &[]),
            step("test", "test", &["build"], "echo testing && sleep 0.2", &["os=linux"], 0, &[]),
        ]
    })
}

fn diamond_ci() -> Value {
    json!({
        "name": "diamond-ci",
        "workspace": { "repo": "https://github.com/octocat/Hello-World.git", "ref": "master" },
        "on": { "push": { "branches": ["main", "develop"] } },
        "steps": [
            step("checkout", "checkout", &[], "ls -la && test -f README", &["os=linux"], 1, &[]),
            step("lint", "lint", &["checkout"], "echo linting && sleep 0.3", &["os=linux"], 0, &[]),
            step("unit", "unit tests", &["checkout"], "echo unit && sleep 0.4", &["os=linux"], 1, &[]),
            step("types", "typecheck", &["checkout"], "echo typecheck && sleep 0.25", &["os=linux"], 0, &[]),
            step("package", "package", &["lint", "unit", "types"], "mkdir -p dist && echo bundle > dist/app.txt", &["os=linux"], 0, &["dist/app.txt"]),
            step("smoke", "smoke", &["package"], "test -f dist/app.txt && echo smoke-ok", &["os=linux"], 0, &[]),
        ]
    })
}

/// Parallel “matrix-like” lanes with different labels after a shared checkout.
fn fan_out_tests() -> Value {
    json!({
        "name": "fan-out-tests",
        "on": { "push": { "branches": ["main"] } },
        "steps": [
            step("checkout", "checkout", &[], "echo ready", &["os=linux"], 0, &[]),
            step("test-linux", "test linux", &["checkout"], "uname -s && sleep 0.3", &["os=linux"], 1, &[]),
            step("test-docker", "test docker labels", &["checkout"], "echo docker-lane && sleep 0.35", &["os=linux", "docker=true"], 0, &[]),
            step("integration", "integration", &["checkout"], "echo integration && sleep 0.5", &["os=linux"], 0, &[]),
            step("report", "report", &["test-linux", "test-docker", "integration"], "echo green > report.txt", &["os=linux"], 0, &["report.txt"]),
        ]
    })
}

fn release_with_artifacts() -> Value {
    json!({
        "name": "release-with-artifacts",
        "workspace": { "repo": "https://github.com/octocat/Hello-World.git", "ref": "master" },
        "steps": [
            step("checkout", "checkout", &[], "test -f README && echo checked-out", &["os=linux"], 1, &[]),
            step("build", "build", &["checkout"], "mkdir -p out && printf 'v1.0.0\\n' > out/VERSION && tar -cf out/release.tar README out/VERSION && ls -la out", &["os=linux"], 0, &["out/VERSION", "out/release.tar"]),
            step("sign", "sign", &["build"], "echo signed-$(cat out/VERSION) > out/release.tar.sig && cat out/release.tar.sig", &["os=linux"], 0, &["out/release.tar.sig"]),
            step("staging", "deploy staging", &["sign"], "echo deploy staging with $(cat out/VERSION) token=${DEMO_TOKEN:-missing}", &["os=linux"], 1, &[]),
            step("canary", "canary", &["staging"], "echo canary checks && sleep 0.4 && echo healthy", &["os=linux"], 0, &[]),
            step("prod", "deploy prod", &["canary"], "echo prod $(cat out/VERSION)", &["os=linux"], 0, &[]),
        ]
    })
}

fn retry_and_skip() -> Value {
    json!({
        "name": "retry-and-skip",
        "steps": [
            step("prep", "prep", &[], "mkdir -p .fiber && echo 0 > .fiber/attempt_hint", &["os=linux"], 0, &[]),
            step("flaky", "flaky (retries)", &["prep"], "n=$(cat .fiber/attempt_hint); n=$((n+1)); echo $n > .fiber/attempt_hint; echo attempt=$n; [ \"$n\" -ge 2 ] || exit 1; echo recovered", &["os=linux"], 2, &[".fiber/attempt_hint"]),
            step("downstream", "downstream", &["flaky"], "echo only-if-flaky-ok", &["os=linux"], 0, &[]),
            step("notify", "notify", &["downstream"], "echo notify", &["os=linux"], 0, &[]),
        ]
    })
}

fn nightly_interval() -> Value {
    json!({
        "name": "nightly-interval",
        "on": { "interval_minutes": 1440 },
        "steps": [
            step("tick", "tick", &[], "echo nightly $(date -u +%Y-%m-%dT%H:%M:%SZ)", &["os=linux"], 0, &[]),
        ]
    })
}
