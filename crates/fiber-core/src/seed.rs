//! Demo project + complex DAG pipelines seeded on API boot.

use crate::models::{CreatePipelineRequest, CreateProjectRequest};
use crate::roles::ProjectRole;
use crate::store::Store;
use anyhow::Result;
use serde_json::{Value, json};
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
        ("matrix-build", matrix_build()),
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

/// A step the compiler will expand: `matrix` axes, and an optional `if` evaluated per cell.
fn matrix_step(
    id: &str,
    name: &str,
    needs: &[&str],
    run: &str,
    matrix: Value,
    cell_if: Option<&str>,
) -> Value {
    let mut s = step(id, name, needs, run, &["os=linux"], 0, &[]);
    s["matrix"] = matrix;
    if let Some(expr) = cell_if {
        s["if"] = json!(expr);
    }
    s
}

/// A real `matrix:`, expanded by the compiler rather than written out by hand.
///
/// The axes are `rust` and `features` rather than the usual `os`, because every cell has
/// to be runnable on the one agent a demo instance has: a `macos` cell would sit queued
/// forever and showcase nothing.
///
/// `report` needs `test` alone, and `lint` is a leaf. That is deliberate: `success()` is
/// transitive, so a `report` that also needed `lint` would be skipped by the very cell
/// this pipeline exists to show being skipped — correct behaviour, but a demo whose last
/// node is always grey reads as broken. Each step now demonstrates one thing: `test` the
/// fan-out, `report` the rewriting of `needs` onto every cell, `lint` a per-cell `if`.
fn matrix_build() -> Value {
    json!({
        "name": "matrix-build",
        "steps": [
            step("checkout", "checkout", &[], "echo ready", &["os=linux"], 0, &[]),
            matrix_step(
                "test",
                "test",
                &["checkout"],
                // Each binding is exported three ways — bare, MATRIX_*, FIBER_MATRIX_*.
                "echo \"rust=$rust features=$features\" && echo \"also MATRIX_RUST=$MATRIX_RUST\" && sleep 0.2",
                json!({ "rust": ["stable", "beta"], "features": ["default", "all"] }),
                None,
            ),
            matrix_step(
                "lint",
                "lint",
                &["checkout"],
                "echo \"clippy on $rust\" && sleep 0.2",
                json!({ "rust": ["stable", "beta"] }),
                Some("matrix.rust == 'stable'"),
            ),
            step("report", "report", &["test"], "echo all-cells-done > matrix-report.txt && cat matrix-report.txt", &["os=linux"], 0, &["matrix-report.txt"]),
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

#[cfg(test)]
mod tests {
    //! The seeded pipelines are written as raw JSON and were never checked against the
    //! compiler that has to accept them. A malformed one is not a test failure today —
    //! it is a pipeline that appears in the demo project and cannot run.

    use super::*;
    use crate::dag::compile_definition;
    use fiber_proto::PipelineDefinition;

    fn compiled(definition: &Value) -> crate::dag::CompiledDag {
        let parsed: PipelineDefinition = serde_json::from_value(definition.clone())
            .unwrap_or_else(|e| panic!("seed pipeline is not a PipelineDefinition: {e}"));
        compile_definition(&parsed)
            .unwrap_or_else(|e| panic!("seed pipeline does not compile: {e}"))
    }

    #[test]
    fn every_seeded_pipeline_compiles() {
        for (name, definition) in showcase_pipelines() {
            let dag = compiled(&definition);
            assert!(!dag.steps.is_empty(), "{name} compiled to no steps");
        }
    }

    #[test]
    fn seeded_pipeline_names_are_unique() {
        // `ensure_showcase` skips by name, so a duplicate would silently seed only one.
        let mut seen = std::collections::HashSet::new();
        for (name, _) in showcase_pipelines() {
            assert!(seen.insert(name), "two seeded pipelines are called {name}");
        }
    }

    #[test]
    fn the_matrix_pipeline_expands_into_its_cells() {
        let dag = compiled(&matrix_build());
        let ids: std::collections::HashSet<&str> =
            dag.steps.iter().map(|s| s.id.as_str()).collect();

        // Two axes of two values: four cells, not one step called `test`.
        for id in [
            "test__features_default__rust_stable",
            "test__features_default__rust_beta",
            "test__features_all__rust_stable",
            "test__features_all__rust_beta",
        ] {
            assert!(ids.contains(id), "missing matrix cell {id} in {ids:?}");
        }
        assert!(
            !ids.contains("test"),
            "the authored step should not survive expansion"
        );
    }

    #[test]
    fn a_dependency_on_a_matrix_step_is_rewritten_to_every_cell() {
        let dag = compiled(&matrix_build());
        let report = dag
            .steps
            .iter()
            .find(|s| s.id == "report")
            .expect("report step");

        // `needs: [test]` as written points at an id that no longer exists after
        // expansion; the compiler rewrites it to all four cells.
        assert_eq!(report.needs.len(), 4, "report needs: {:?}", report.needs);
        assert!(report.needs.iter().all(|n| n.starts_with("test__")));
    }

    #[test]
    fn the_showcase_run_can_actually_finish_green() {
        // `success()` is transitive: anything downstream of the deliberately skipped
        // lint cell would be skipped too, leaving the demo's terminal node grey.
        let dag = compiled(&matrix_build());
        let skippable: std::collections::HashSet<&str> = dag
            .steps
            .iter()
            .filter(|s| s.if_expr.is_some())
            .map(|s| s.id.as_str())
            .collect();
        for s in &dag.steps {
            assert!(
                !s.needs.iter().any(|n| skippable.contains(n.as_str())),
                "{} depends on conditional {:?}; the demo would end on a skip",
                s.id,
                s.needs
            );
        }
    }

    #[test]
    fn the_conditional_axis_reaches_both_cells_with_its_expression() {
        let dag = compiled(&matrix_build());
        let lint: Vec<_> = dag
            .steps
            .iter()
            .filter(|s| s.id.starts_with("lint"))
            .collect();
        assert_eq!(
            lint.len(),
            2,
            "lint should expand to one cell per rust value"
        );
        // The skip is decided at run time from `if`; what compiles is the expression
        // travelling to every cell, each carrying its own binding.
        assert!(
            lint.iter()
                .all(|s| s.if_expr.as_deref() == Some("matrix.rust == 'stable'"))
        );
        let bindings: std::collections::HashSet<&str> = lint
            .iter()
            .filter_map(|s| s.matrix.get("rust").map(String::as_str))
            .collect();
        assert_eq!(bindings, ["stable", "beta"].into_iter().collect());
    }
}
