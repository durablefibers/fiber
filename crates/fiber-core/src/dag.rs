use fiber_proto::{PipelineDefinition, StepDefinition};
use petgraph::algo::toposort;
use petgraph::graph::{DiGraph, NodeIndex};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, HashMap, HashSet};
use thiserror::Error;

const MAX_MATRIX_CELLS: usize = 64;

/// The most expanded steps one pipeline may compile to.
///
/// Only the per-step matrix was capped, so 80 steps each expanding to 64 cells was a
/// legal 5 120-step definition — and every run of it did one `INSERT` per step inside one
/// transaction, then re-planned the whole DAG under the run lock after each completion.
/// 500 is chosen as the point where those stay cheap (a single `UNNEST` insert, and
/// 500 completions x 500 steps of in-memory planning) and where the run page and the DAG
/// canvas still render; it is also ~8 full matrices, which is more than any pipeline in
/// `examples/` by an order of magnitude. A definition past it is far likelier to be a
/// generator bug than an intention, and it fails at compile time with a number in the
/// message rather than at run time with a slow transaction.
const MAX_STEPS: usize = 500;

#[derive(Debug, Error)]
pub enum DagError {
    #[error("duplicate step id: {0}")]
    DuplicateStep(String),
    #[error("unknown dependency `{dep}` on step `{step}`")]
    UnknownDependency { step: String, dep: String },
    #[error("pipeline contains a cycle")]
    Cycle,
    #[error("pipeline must have at least one step")]
    Empty,
    #[error("step `{0}` is missing a run command")]
    MissingRun(String),
    #[error("step `{step}` matrix axis `{axis}` is empty")]
    EmptyMatrixAxis { step: String, axis: String },
    #[error("step `{0}` matrix expands to more than {MAX_MATRIX_CELLS} cells")]
    MatrixTooLarge(String),
    #[error("step `{step}` env name `{name}` is not a usable environment variable name")]
    BadEnvName { step: String, name: String },
    #[error("step `{step}` env name `{name}` is reserved: FIBER_* is set by the server")]
    ReservedEnvName { step: String, name: String },
    #[error("step `{step}` working_directory `{path}` must stay inside the workspace")]
    BadWorkingDirectory { step: String, path: String },
    #[error("step `{step}` shell `{shell}` must be a bare program name")]
    BadShell { step: String, shell: String },
    #[error("`{0}`: timeout_minutes must be at least 1")]
    InvalidTimeout(String),
    #[error(
        "step `{step}` image `{image}` is not a docker image reference (`name[:tag][@digest]`, optionally registry-qualified)"
    )]
    BadImage { step: String, image: String },
    #[error(
        "workspace repo `{0}` must be an http(s), ssh, git, or file URL, an scp-like `user@host:path`, or a path"
    )]
    BadRepo(String),
    #[error("step `{step}` has an unusable condition — {message}")]
    BadIf { step: String, message: String },
    #[error("every step needs a non-empty id")]
    EmptyStepId,
    #[error("pipeline expands to {count} steps, more than the limit of {MAX_STEPS}")]
    TooManySteps { count: usize },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CompiledStep {
    pub id: String,
    pub name: String,
    pub needs: Vec<String>,
    pub run: String,
    pub image: Option<String>,
    pub labels: Vec<String>,
    pub retries: u32,
    #[serde(default)]
    pub artifacts: Vec<String>,
    /// Steps that can run in the same wave (0-based).
    pub level: usize,
    /// Matrix cell bindings (axis → value).
    #[serde(default)]
    pub matrix: BTreeMap<String, String>,
    /// Env vars to inject on the agent (`MATRIX_OS=linux`, …).
    #[serde(default)]
    pub env: Vec<(String, String)>,
    /// Copied from definition `if:`.
    #[serde(default, rename = "if")]
    pub if_expr: Option<String>,
    /// Copied from definition `timeout_minutes:` (per attempt).
    #[serde(default)]
    pub timeout_minutes: Option<u32>,
    /// Copied from definition `secrets:` — `None` means every project secret.
    #[serde(default)]
    pub secrets: Option<Vec<String>>,
    /// Copied from definition `working_directory:`, validated to stay in the workspace.
    #[serde(default)]
    pub working_directory: Option<String>,
    /// Copied from definition `shell:`.
    #[serde(default)]
    pub shell: Option<String>,
    /// Copied from definition `continue_on_error:`.
    #[serde(default)]
    pub continue_on_error: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CompiledDag {
    pub name: String,
    #[serde(default)]
    pub workspace: Option<fiber_proto::WorkspaceConfig>,
    pub steps: Vec<CompiledStep>,
    pub levels: Vec<Vec<String>>,
    /// Whole-run wall-clock limit in minutes, if any.
    #[serde(default)]
    pub timeout_minutes: Option<u32>,
}

#[derive(Clone)]
struct ExpandedCell {
    id: String,
    name: String,
    matrix: BTreeMap<String, String>,
    env: Vec<(String, String)>,
    template: StepDefinition,
}

pub fn compile_definition(def: &PipelineDefinition) -> Result<CompiledDag, DagError> {
    if def.steps.is_empty() {
        return Err(DagError::Empty);
    }
    if def.timeout_minutes == Some(0) {
        return Err(DagError::InvalidTimeout("pipeline".into()));
    }
    // The repo lands on a `git remote add` command line on the agent host, before any
    // container exists; git's `ext::` transport would run it as a command.
    if let Some(ws) = &def.workspace
        && !fiber_proto::validate::repo_url_ok(&ws.repo)
    {
        return Err(DagError::BadRepo(ws.repo.clone()));
    }
    for step in &def.steps {
        if step.timeout_minutes == Some(0) {
            return Err(DagError::InvalidTimeout(format!("step {}", step.id)));
        }
    }

    let mut seen = HashSet::new();
    for step in &def.steps {
        // An id is how needs, the snapshot, `step_runs.step_id`, and the UI all refer to
        // the step. An empty one names nothing, and the map form of `steps:` produced it
        // from any key that was not a string.
        if step.id.trim().is_empty() {
            return Err(DagError::EmptyStepId);
        }
        if !seen.insert(step.id.clone()) {
            return Err(DagError::DuplicateStep(step.id.clone()));
        }
        // A condition nobody can evaluate is a step that silently never runs.
        crate::step_if::validate(step.if_expr.as_deref()).map_err(|message| DagError::BadIf {
            step: step.id.clone(),
            message,
        })?;
        if step
            .run
            .as_ref()
            .map(|s| s.trim().is_empty())
            .unwrap_or(true)
        {
            return Err(DagError::MissingRun(step.id.clone()));
        }
    }

    let id_set: HashSet<_> = def.steps.iter().map(|s| s.id.as_str()).collect();
    for step in &def.steps {
        for dep in &step.needs {
            if !id_set.contains(dep.as_str()) {
                return Err(DagError::UnknownDependency {
                    step: step.id.clone(),
                    dep: dep.clone(),
                });
            }
        }
    }

    let expanded = expand_all(def)?;
    if expanded.len() > MAX_STEPS {
        return Err(DagError::TooManySteps {
            count: expanded.len(),
        });
    }
    let expanded_ids: HashSet<_> = expanded.iter().map(|c| c.id.as_str()).collect();
    for cell in &expanded {
        for dep in &cell.template.needs {
            // needs are rewritten to expanded ids below; validate template deps existed
            if !id_set.contains(dep.as_str()) {
                return Err(DagError::UnknownDependency {
                    step: cell.id.clone(),
                    dep: dep.clone(),
                });
            }
        }
        let _ = expanded_ids;
    }

    // base_id → all expanded cell ids (in definition order)
    let mut cells_by_base: HashMap<String, Vec<String>> = HashMap::new();
    for cell in &expanded {
        cells_by_base
            .entry(cell.template.id.clone())
            .or_default()
            .push(cell.id.clone());
    }

    let mut with_needs: Vec<(ExpandedCell, Vec<String>)> = Vec::with_capacity(expanded.len());
    for cell in expanded {
        let mut needs = Vec::new();
        for dep in &cell.template.needs {
            if let Some(dep_cells) = cells_by_base.get(dep) {
                needs.extend(dep_cells.iter().cloned());
            }
        }
        with_needs.push((cell, needs));
    }

    let mut graph: DiGraph<&str, ()> = DiGraph::new();
    let mut indices: HashMap<&str, NodeIndex> = HashMap::new();
    for (cell, _) in &with_needs {
        let idx = graph.add_node(cell.id.as_str());
        indices.insert(cell.id.as_str(), idx);
    }
    for (cell, needs) in &with_needs {
        let to = indices[cell.id.as_str()];
        for dep in needs {
            let from = indices[dep.as_str()];
            graph.add_edge(from, to, ());
        }
    }

    // `toposort` is itself the cycle check: it can only order an acyclic graph, and
    // reports `Err` for anything else — a two-step loop, a longer one, or a step that
    // needs itself. A separate `is_cyclic_directed` pass in front of it walked the
    // whole graph a second time to learn what this line already tells us.
    let order = toposort(&graph, None).map_err(|_| DagError::Cycle)?;
    let needs_map: HashMap<&str, &Vec<String>> =
        with_needs.iter().map(|(c, n)| (c.id.as_str(), n)).collect();
    let mut levels_map: HashMap<&str, usize> = HashMap::new();
    for idx in &order {
        let id = graph[*idx];
        let level = needs_map
            .get(id)
            .map(|needs| {
                needs
                    .iter()
                    .map(|d| levels_map[d.as_str()] + 1)
                    .max()
                    .unwrap_or(0)
            })
            .unwrap_or(0);
        levels_map.insert(id, level);
    }

    let max_level = levels_map.values().copied().max().unwrap_or(0);
    let mut levels: Vec<Vec<String>> = vec![Vec::new(); max_level + 1];
    let mut compiled = Vec::with_capacity(with_needs.len());

    for (cell, needs) in &with_needs {
        let level = levels_map[cell.id.as_str()];
        levels[level].push(cell.id.clone());
        compiled.push(CompiledStep {
            id: cell.id.clone(),
            name: cell.name.clone(),
            needs: needs.clone(),
            run: cell.template.run.clone().unwrap_or_default(),
            // Trimmed, and empty folded to none, so the snapshot holds exactly the value
            // that was validated and the agent runs exactly what the snapshot holds.
            image: cell
                .template
                .image
                .as_deref()
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .map(str::to_string),
            labels: cell.template.labels.clone(),
            retries: cell.template.retries,
            artifacts: cell.template.artifacts.clone(),
            level,
            matrix: cell.matrix.clone(),
            env: cell.env.clone(),
            if_expr: cell.template.if_expr.clone(),
            timeout_minutes: cell.template.timeout_minutes,
            working_directory: cell.template.working_directory.clone(),
            shell: cell.template.shell.clone(),
            continue_on_error: cell.template.continue_on_error,
            secrets: cell.template.secrets.clone(),
        });
    }

    for level in &mut levels {
        level.sort_by_key(|id| compiled.iter().position(|s| &s.id == id).unwrap_or(0));
    }

    Ok(CompiledDag {
        name: def.name.clone(),
        // Trimmed, so the snapshot holds exactly the value that was validated.
        workspace: def.workspace.clone().map(|mut ws| {
            ws.repo = ws.repo.trim().to_string();
            ws
        }),
        steps: compiled,
        levels,
        timeout_minutes: def.timeout_minutes,
    })
}

fn expand_all(def: &PipelineDefinition) -> Result<Vec<ExpandedCell>, DagError> {
    let mut out = Vec::new();
    for step in &def.steps {
        let combos = matrix_combos(step)?;
        for combo in combos {
            let (id, name, matrix, env) = if combo.is_empty() {
                (
                    step.id.clone(),
                    step.name.clone(),
                    BTreeMap::new(),
                    Vec::new(),
                )
            } else {
                let suffix = combo
                    .iter()
                    .map(|(k, v)| format!("{}_{}", sanitize(k), sanitize(v)))
                    .collect::<Vec<_>>()
                    .join("__");
                let id = format!("{}__{}", step.id, suffix);
                let pretty = combo
                    .iter()
                    .map(|(k, v)| format!("{k}: {v}"))
                    .collect::<Vec<_>>()
                    .join(", ");
                let name = format!("{} ({pretty})", step.name);
                let env = matrix_env(&combo);
                (id, name, combo, env)
            };
            // Only what the pipeline author wrote is checked. The matrix generator emits
            // FIBER_MATRIX_* itself, and the reservation exists to stop a user shadowing
            // those, not to stop us setting them.
            for (k, _) in def.env.iter().chain(step.env.iter()) {
                if !is_usable_env_name(k) {
                    return Err(DagError::BadEnvName {
                        step: step.id.clone(),
                        name: k.clone(),
                    });
                }
                if k.starts_with("FIBER_") {
                    return Err(DagError::ReservedEnvName {
                        step: step.id.clone(),
                        name: k.clone(),
                    });
                }
            }
            if let Some(wd) = &step.working_directory
                && !is_contained_relative_path(wd)
            {
                return Err(DagError::BadWorkingDirectory {
                    step: step.id.clone(),
                    path: wd.clone(),
                });
            }
            if let Some(sh) = &step.shell
                && !is_bare_program_name(sh)
            {
                return Err(DagError::BadShell {
                    step: step.id.clone(),
                    shell: sh.clone(),
                });
            }
            // The image lands on the `docker run` command line. An empty string means
            // "no image" everywhere else, so it is not an error here either.
            if let Some(img) = step.image.as_deref().map(str::trim)
                && !img.is_empty()
                && !fiber_proto::validate::image_reference_ok(img)
            {
                return Err(DagError::BadImage {
                    step: step.id.clone(),
                    image: img.to_string(),
                });
            }
            // Least specific first, so the later writer wins: the pipeline sets a baseline,
            // the step narrows it, and the matrix binding is last because it is what says
            // which cell this is — a step that could shadow it would make the logs lie.
            let mut merged: Vec<(String, String)> = def
                .env
                .iter()
                .chain(step.env.iter())
                .map(|(k, v)| (k.clone(), v.clone()))
                .collect();
            merged.extend(env);
            let env = dedupe_last_wins(merged);
            out.push(ExpandedCell {
                id,
                name,
                matrix,
                env,
                template: step.clone(),
            });
        }
    }
    // Collision check
    let mut seen = HashSet::new();
    for c in &out {
        if !seen.insert(c.id.clone()) {
            return Err(DagError::DuplicateStep(c.id.clone()));
        }
    }
    Ok(out)
}

fn matrix_combos(step: &StepDefinition) -> Result<Vec<BTreeMap<String, String>>, DagError> {
    let Some(matrix) = &step.matrix else {
        return Ok(vec![BTreeMap::new()]);
    };
    if matrix.is_empty() {
        return Ok(vec![BTreeMap::new()]);
    }
    let mut axes: Vec<(String, Vec<String>)> =
        matrix.iter().map(|(k, v)| (k.clone(), v.clone())).collect();
    axes.sort_by(|a, b| a.0.cmp(&b.0));
    for (axis, values) in &axes {
        if values.is_empty() {
            return Err(DagError::EmptyMatrixAxis {
                step: step.id.clone(),
                axis: axis.clone(),
            });
        }
    }
    let mut combos: Vec<BTreeMap<String, String>> = vec![BTreeMap::new()];
    for (axis, values) in axes {
        let mut next = Vec::new();
        for base in &combos {
            for v in &values {
                let mut m = base.clone();
                m.insert(axis.clone(), v.clone());
                next.push(m);
            }
        }
        combos = next;
        if combos.len() > MAX_MATRIX_CELLS {
            return Err(DagError::MatrixTooLarge(step.id.clone()));
        }
    }
    Ok(combos)
}

/// A path that stays inside the workspace when joined to it.
///
/// Rejects absolute paths, `..` in any position, and Windows-style roots. The agent checks
/// again before it uses the value — this is the early, legible failure, not the boundary.
fn is_contained_relative_path(p: &str) -> bool {
    let p = p.trim();
    !p.is_empty()
        && !p.starts_with('/')
        && !p.starts_with('\\')
        && !p.contains(':')
        && !p.split(['/', '\\']).any(|seg| seg == "..")
}

/// A program name, not a command line.
///
/// `bash` yes, `/bin/bash` or `bash -e` no. Keeping it a bare name means the agent invokes
/// `<shell> -c <run>` predictably, and the image decides which binary that resolves to.
fn is_bare_program_name(s: &str) -> bool {
    let s = s.trim();
    !s.is_empty()
        && s.len() <= 32
        && s.chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-' || c == '.')
        && s != "."
        && s != ".."
}

/// A name a shell and `docker --env-file` will both accept.
///
/// Deliberately stricter than POSIX: no leading digit, and nothing outside
/// `[A-Za-z0-9_]`. A name containing `=` or a newline is how an env-file line turns into
/// something other than one assignment.
fn is_usable_env_name(name: &str) -> bool {
    !name.is_empty()
        && !name.starts_with(|c: char| c.is_ascii_digit())
        && name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
}

/// Keep the last value for each key, preserving first-seen order.
///
/// The offer hands the agent a list, not a map, and the agent applies it in order — but
/// docker's `--env-file` and a shell disagree about what a repeated key means, so the
/// ambiguity is resolved here rather than left to whichever executor runs the step.
fn dedupe_last_wins(pairs: Vec<(String, String)>) -> Vec<(String, String)> {
    let mut order: Vec<String> = Vec::new();
    let mut latest: BTreeMap<String, String> = BTreeMap::new();
    for (k, v) in pairs {
        if !latest.contains_key(&k) {
            order.push(k.clone());
        }
        latest.insert(k, v);
    }
    order
        .into_iter()
        .map(|k| {
            let v = latest.remove(&k).unwrap_or_default();
            (k, v)
        })
        .collect()
}

fn matrix_env(combo: &BTreeMap<String, String>) -> Vec<(String, String)> {
    let mut env = Vec::new();
    for (k, v) in combo {
        env.push((k.clone(), v.clone()));
        env.push((format!("MATRIX_{}", k.to_ascii_uppercase()), v.clone()));
        env.push((
            format!("FIBER_MATRIX_{}", k.to_ascii_uppercase()),
            v.clone(),
        ));
    }
    env
}

fn sanitize(s: &str) -> String {
    s.chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '_' {
                c
            } else {
                '_'
            }
        })
        .collect()
}

pub fn definition_from_map(
    name: String,
    steps: HashMap<String, StepDefinitionInput>,
) -> PipelineDefinition {
    let mut ordered: Vec<_> = steps.into_iter().collect();
    ordered.sort_by(|a, b| a.0.cmp(&b.0));
    PipelineDefinition {
        name,
        env: BTreeMap::new(),
        workspace: None,
        concurrency: None,
        on: None,
        steps: ordered
            .into_iter()
            .map(|(id, s)| StepDefinition {
                id: id.clone(),
                name: s.name.unwrap_or(id),
                needs: s.needs,
                run: s.run,
                image: s.image,
                labels: s.labels,
                retries: s.retries.unwrap_or(0),
                env: s.env,
                working_directory: s.working_directory,
                shell: s.shell,
                continue_on_error: s.continue_on_error,
                artifacts: s.artifacts,
                matrix: s.matrix,
                if_expr: s.if_expr,
                timeout_minutes: s.timeout_minutes,
                secrets: s.secrets,
            })
            .collect(),
        timeout_minutes: None,
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StepDefinitionInput {
    pub name: Option<String>,
    #[serde(default)]
    pub env: BTreeMap<String, String>,
    #[serde(default)]
    pub working_directory: Option<String>,
    #[serde(default)]
    pub shell: Option<String>,
    #[serde(default)]
    pub continue_on_error: bool,
    #[serde(default)]
    pub needs: Vec<String>,
    pub run: Option<String>,
    pub image: Option<String>,
    #[serde(default)]
    pub labels: Vec<String>,
    pub retries: Option<u32>,
    #[serde(default)]
    pub artifacts: Vec<String>,
    #[serde(default)]
    pub matrix: Option<BTreeMap<String, Vec<String>>>,
    #[serde(default, rename = "if")]
    pub if_expr: Option<String>,
    #[serde(default)]
    pub timeout_minutes: Option<u32>,
    #[serde(default)]
    pub secrets: Option<Vec<String>>,
}

/// Field names each level of `fiber.yml` accepts, in the order the docs list them.
///
/// These mirror the `#[derive(Deserialize)]` structs in `fiber-proto` by hand, which is
/// the same hand-mirroring `apps/ui/src/lib/api.ts` does. `unknown_keys_name_every_field`
/// pins each list against a serialised instance of its type, so a field added to
/// `fiber-proto` and not added here fails the build's tests rather than silently
/// becoming an unknown key.
mod keys {
    pub const PIPELINE: &[&str] = &[
        "name",
        "env",
        "workspace",
        "on",
        "steps",
        "timeout_minutes",
        "concurrency",
    ];
    pub const STEP: &[&str] = &[
        "id",
        "name",
        "needs",
        "run",
        "image",
        "labels",
        "retries",
        "env",
        "working_directory",
        "shell",
        "continue_on_error",
        "artifacts",
        "matrix",
        "if",
        "timeout_minutes",
        "secrets",
    ];
    pub const WORKSPACE: &[&str] = &["repo", "ref"];
    pub const ON: &[&str] = &["push", "pull_request", "interval_minutes", "cron"];
    pub const PUSH: &[&str] = &["branches", "paths", "paths_ignore"];
    pub const PULL_REQUEST: &[&str] = &["branches", "types", "paths", "paths_ignore"];
    pub const CONCURRENCY: &[&str] = &["group", "cancel_in_progress"];
}

/// Reject a key no part of the schema reads, naming it and the nearest field that exists.
///
/// serde ignores what it does not recognise, so `continue-on-error:`, `working-directory:`,
/// `need:` and `artifact:` — every one of them a GitHub Actions spelling or a plausible
/// typo — all parsed clean and produced a step that ran without the field. The step was
/// not the step the author wrote and nothing said so. Unknown keys are the one class of
/// mistake where being permissive is indistinguishable from being wrong.
fn audit_keys(what: &str, map: &serde_yaml::Mapping, allowed: &[&str]) -> Result<(), String> {
    for key in map.keys() {
        let Some(key) = key.as_str() else {
            return Err(format!("{what}: keys must be strings"));
        };
        if allowed.contains(&key) {
            continue;
        }
        return Err(match nearest(key, allowed) {
            Some(hint) => format!("{what}: unknown field `{key}` — did you mean `{hint}`?"),
            None => format!(
                "{what}: unknown field `{key}`; valid fields are {}",
                allowed.join(", ")
            ),
        });
    }
    Ok(())
}

/// The valid field a typo most likely meant, or `None` when nothing is close.
///
/// `-` and `_` count as the same character first, so the Actions spellings
/// (`continue-on-error`, `working-directory`) resolve exactly; otherwise an edit distance
/// of at most a third of the name, which catches `need`/`artifact`/`imgae` and does not
/// invent a suggestion for a word that simply does not belong.
fn nearest<'a>(key: &str, allowed: &[&'a str]) -> Option<&'a str> {
    let norm = |s: &str| s.to_ascii_lowercase().replace('-', "_");
    let target = norm(key);
    if let Some(exact) = allowed.iter().find(|a| norm(a) == target) {
        return Some(exact);
    }
    let budget = (target.chars().count() / 3).max(1);
    allowed
        .iter()
        .map(|a| (edit_distance(&target, &norm(a)), *a))
        .filter(|(d, _)| *d <= budget)
        .min_by_key(|(d, a)| (*d, a.len()))
        .map(|(_, a)| a)
}

/// Levenshtein distance, two rows at a time. Both inputs here are field names.
fn edit_distance(a: &str, b: &str) -> usize {
    let a: Vec<char> = a.chars().collect();
    let b: Vec<char> = b.chars().collect();
    let mut prev: Vec<usize> = (0..=b.len()).collect();
    let mut cur = vec![0usize; b.len() + 1];
    for (i, ca) in a.iter().enumerate() {
        cur[0] = i + 1;
        for (j, cb) in b.iter().enumerate() {
            let sub = prev[j] + usize::from(ca != cb);
            cur[j + 1] = sub.min(prev[j + 1] + 1).min(cur[j] + 1);
        }
        std::mem::swap(&mut prev, &mut cur);
    }
    prev[b.len()]
}

fn audit_map(
    value: Option<&serde_yaml::Value>,
    what: &str,
    allowed: &[&str],
) -> Result<(), String> {
    match value {
        Some(serde_yaml::Value::Mapping(m)) => audit_keys(what, m, allowed),
        _ => Ok(()),
    }
}

/// Walk a parsed `fiber.yml` and reject the first key the schema does not read.
fn audit_pipeline_keys(root: &serde_yaml::Value) -> Result<(), String> {
    let serde_yaml::Value::Mapping(top) = root else {
        return Ok(());
    };
    audit_keys("pipeline", top, keys::PIPELINE)?;
    audit_map(top.get("workspace"), "workspace", keys::WORKSPACE)?;
    if let Some(on) = top.get("on") {
        audit_map(Some(on), "on", keys::ON)?;
        audit_map(on.get("push"), "on.push", keys::PUSH)?;
        audit_map(
            on.get("pull_request"),
            "on.pull_request",
            keys::PULL_REQUEST,
        )?;
    }
    audit_map(top.get("concurrency"), "concurrency", keys::CONCURRENCY)?;
    match top.get("steps") {
        Some(serde_yaml::Value::Sequence(seq)) => {
            for (i, step) in seq.iter().enumerate() {
                let label = step
                    .get("id")
                    .and_then(|v| v.as_str())
                    .map(|id| format!("step `{id}`"))
                    .unwrap_or_else(|| format!("step #{}", i + 1));
                audit_map(Some(step), &label, keys::STEP)?;
            }
        }
        Some(serde_yaml::Value::Mapping(map)) => {
            for (k, step) in map {
                let Some(id) = k.as_str() else {
                    return Err("steps: every step id must be a string".into());
                };
                if id.trim().is_empty() {
                    return Err("steps: a step id cannot be empty".into());
                }
                if step.get("id").is_some() {
                    return Err(format!(
                        "step `{id}`: remove `id:` — in the map form of `steps:` the key is \
                         the id"
                    ));
                }
                audit_map(Some(step), &format!("step `{id}`"), keys::STEP)?;
            }
        }
        _ => {}
    }
    Ok(())
}

/// Parse YAML where `steps` may be a map (GitHub Actions style) or a list.
///
/// Strict: a field the schema does not read is an error, because a pipeline that is wrong
/// should say so rather than run something other than what was written. This is the
/// authoring path — `fiber validate`, `POST /api/pipelines/parse-yaml`, the CLI. Reading a
/// definition that was stored before a field was renamed goes through
/// [`parse_pipeline_yaml_lenient`] instead, so an old row still executes.
pub fn parse_pipeline_yaml(yaml: &str) -> Result<PipelineDefinition, serde_yaml::Error> {
    let root: serde_yaml::Value = serde_yaml::from_str(yaml)?;
    audit_pipeline_keys(&root).map_err(<serde_yaml::Error as serde::de::Error>::custom)?;
    definition_from_yaml_value(root)
}

/// [`parse_pipeline_yaml`] without the unknown-key audit, for YAML that is already stored.
///
/// A row written before a key was added or renamed has to keep compiling: the run it
/// governs is already in the database, and failing it at execution time would turn a
/// schema tightening into an outage. Submissions get the strict parser.
pub fn parse_pipeline_yaml_lenient(yaml: &str) -> Result<PipelineDefinition, serde_yaml::Error> {
    definition_from_yaml_value(serde_yaml::from_str(yaml)?)
}

fn definition_from_yaml_value(
    root: serde_yaml::Value,
) -> Result<PipelineDefinition, serde_yaml::Error> {
    #[derive(Deserialize)]
    struct Raw {
        name: String,
        #[serde(default)]
        env: BTreeMap<String, String>,
        #[serde(default)]
        workspace: Option<fiber_proto::WorkspaceConfig>,
        #[serde(default)]
        on: Option<fiber_proto::PipelineTriggers>,
        steps: serde_yaml::Value,
        #[serde(default)]
        timeout_minutes: Option<u32>,
        #[serde(default)]
        concurrency: Option<fiber_proto::ConcurrencyConfig>,
    }

    let raw: Raw = serde_yaml::from_value(root)?;
    let steps = match raw.steps {
        serde_yaml::Value::Sequence(seq) => seq
            .into_iter()
            .map(serde_yaml::from_value::<StepDefinition>)
            .collect::<Result<Vec<_>, _>>()?,
        serde_yaml::Value::Mapping(map) => {
            let mut out = Vec::new();
            for (k, v) in map {
                // A non-string or empty key is caught by the audit on the strict path; the
                // lenient one must not turn it into an anonymous step, which `needs` could
                // never name and `compile_definition` now refuses outright.
                let id = k.as_str().unwrap_or_default().to_string();
                let mut input: StepDefinitionInput = serde_yaml::from_value(v)?;
                if input.name.is_none() {
                    input.name = Some(id.clone());
                }
                out.push(StepDefinition {
                    id: id.clone(),
                    name: input.name.unwrap_or(id),
                    needs: input.needs,
                    run: input.run,
                    image: input.image,
                    labels: input.labels,
                    retries: input.retries.unwrap_or(0),
                    env: input.env,
                    working_directory: input.working_directory,
                    shell: input.shell,
                    continue_on_error: input.continue_on_error,
                    artifacts: input.artifacts,
                    matrix: input.matrix,
                    if_expr: input.if_expr,
                    timeout_minutes: input.timeout_minutes,
                    secrets: input.secrets,
                });
            }
            out
        }
        other => {
            return Err(<serde_yaml::Error as serde::de::Error>::custom(format!(
                "steps: must be a list of steps or a map of id to step, not {}",
                yaml_kind(&other)
            )));
        }
    };

    Ok(PipelineDefinition {
        name: raw.name,
        env: raw.env,
        workspace: raw.workspace,
        on: raw.on,
        steps,
        timeout_minutes: raw.timeout_minutes,
        concurrency: raw.concurrency,
    })
}

fn yaml_kind(v: &serde_yaml::Value) -> &'static str {
    match v {
        serde_yaml::Value::Null => "null",
        serde_yaml::Value::Bool(_) => "a boolean",
        serde_yaml::Value::Number(_) => "a number",
        serde_yaml::Value::String(_) => "a string",
        serde_yaml::Value::Sequence(_) => "a list",
        serde_yaml::Value::Mapping(_) => "a map",
        serde_yaml::Value::Tagged(_) => "a tagged value",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use fiber_proto::StepDefinition;

    fn env_pipeline(pipeline: &[(&str, &str)], step_env: &[(&str, &str)]) -> PipelineDefinition {
        let mut s = step("a", &[], "echo");
        s.env = step_env
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect();
        PipelineDefinition {
            name: "p".into(),
            env: pipeline
                .iter()
                .map(|(k, v)| (k.to_string(), v.to_string()))
                .collect(),
            workspace: None,
            concurrency: None,
            on: None,
            steps: vec![s],
            timeout_minutes: None,
        }
    }

    fn env_of(d: &PipelineDefinition) -> Vec<(String, String)> {
        compile_definition(d).expect("compiles").steps[0]
            .env
            .clone()
    }

    #[test]
    fn step_env_overrides_pipeline_env() {
        let e = env_of(&env_pipeline(
            &[("SHARED", "from-pipeline"), ("ONLY_PIPELINE", "p")],
            &[("SHARED", "from-step"), ("ONLY_STEP", "s")],
        ));
        assert!(e.contains(&("SHARED".into(), "from-step".into())), "{e:?}");
        assert!(e.contains(&("ONLY_PIPELINE".into(), "p".into())));
        assert!(e.contains(&("ONLY_STEP".into(), "s".into())));
        // One entry per name: a repeated key means different things to a shell and to
        // docker --env-file, so it must not reach either.
        assert_eq!(e.iter().filter(|(k, _)| k == "SHARED").count(), 1);
    }

    #[test]
    fn a_matrix_binding_beats_both() {
        let mut s = step("a", &[], "echo");
        s.env = [("os".to_string(), "from-step".to_string())]
            .into_iter()
            .collect();
        s.matrix = Some(
            [("os".to_string(), vec!["linux".to_string()])]
                .into_iter()
                .collect(),
        );
        let d = PipelineDefinition {
            name: "p".into(),
            env: [("os".to_string(), "from-pipeline".to_string())]
                .into_iter()
                .collect(),
            workspace: None,
            concurrency: None,
            on: None,
            steps: vec![s],
            timeout_minutes: None,
        };
        let e = compile_definition(&d).expect("compiles").steps[0]
            .env
            .clone();
        assert!(e.contains(&("os".into(), "linux".into())), "{e:?}");
        assert_eq!(e.iter().filter(|(k, _)| k == "os").count(), 1);
    }

    #[test]
    fn unusable_env_names_fail_to_compile() {
        for bad in ["has space", "has=equals", "has\nnewline", "1LEADING", ""] {
            let d = env_pipeline(&[], &[(bad, "v")]);
            assert!(
                matches!(compile_definition(&d), Err(DagError::BadEnvName { .. })),
                "accepted {bad:?}"
            );
        }
    }

    #[test]
    fn a_working_directory_cannot_leave_the_workspace() {
        for bad in ["/etc", "../outside", "sub/../../outside", "C:\\windows", ""] {
            let mut st = step("a", &[], "echo");
            st.working_directory = Some(bad.to_string());
            let d = PipelineDefinition {
                name: "p".into(),
                env: Default::default(),
                workspace: None,
                concurrency: None,
                on: None,
                steps: vec![st],
                timeout_minutes: None,
            };
            assert!(
                matches!(
                    compile_definition(&d),
                    Err(DagError::BadWorkingDirectory { .. })
                ),
                "accepted {bad:?}"
            );
        }
        // A plain subdirectory is fine, including a nested one.
        for good in ["apps/ui", "sub", "a/b/c"] {
            let mut st = step("a", &[], "echo");
            st.working_directory = Some(good.to_string());
            let d = PipelineDefinition {
                name: "p".into(),
                env: Default::default(),
                workspace: None,
                concurrency: None,
                on: None,
                steps: vec![st],
                timeout_minutes: None,
            };
            assert!(compile_definition(&d).is_ok(), "rejected {good:?}");
        }
    }

    #[test]
    fn shell_must_be_a_bare_program_name() {
        for bad in ["/bin/bash", "bash -e", "bash;rm", "", ".."] {
            let mut st = step("a", &[], "echo");
            st.shell = Some(bad.to_string());
            let d = PipelineDefinition {
                name: "p".into(),
                env: Default::default(),
                workspace: None,
                concurrency: None,
                on: None,
                steps: vec![st],
                timeout_minutes: None,
            };
            assert!(
                matches!(compile_definition(&d), Err(DagError::BadShell { .. })),
                "accepted {bad:?}"
            );
        }
        for good in ["bash", "sh", "zsh", "python3"] {
            let mut st = step("a", &[], "echo");
            st.shell = Some(good.to_string());
            let d = PipelineDefinition {
                name: "p".into(),
                env: Default::default(),
                workspace: None,
                concurrency: None,
                on: None,
                steps: vec![st],
                timeout_minutes: None,
            };
            assert!(compile_definition(&d).is_ok(), "rejected {good:?}");
        }
    }

    #[test]
    fn an_image_that_docker_would_read_as_a_flag_does_not_compile() {
        // `-v/:/host` is the attached-value form docker's flag parser accepts, and it
        // would mount the agent host's root into the step.
        for bad in ["-v/:/host", "--privileged", "ubuntu --privileged", "a b"] {
            let mut st = step("a", &[], "echo");
            st.image = Some(bad.to_string());
            let d = PipelineDefinition {
                name: "p".into(),
                env: Default::default(),
                workspace: None,
                concurrency: None,
                on: None,
                steps: vec![st],
                timeout_minutes: None,
            };
            assert!(
                matches!(compile_definition(&d), Err(DagError::BadImage { .. })),
                "accepted {bad:?}"
            );
        }
        // An empty image means "no image", as it does on the agent.
        for good in [
            "",
            "rust:1.98",
            "ghcr.io/org/img@sha256:6a1f3c0f4b9d4c3a2e1d9c8b7a6f5e4d",
        ] {
            let mut st = step("a", &[], "echo");
            st.image = Some(good.to_string());
            let d = PipelineDefinition {
                name: "p".into(),
                env: Default::default(),
                workspace: None,
                concurrency: None,
                on: None,
                steps: vec![st],
                timeout_minutes: None,
            };
            assert!(compile_definition(&d).is_ok(), "rejected {good:?}");
        }
    }

    #[test]
    fn a_workspace_repo_that_git_would_run_does_not_compile() {
        for (repo, ok) in [
            ("ext::sh -c 'curl x|sh'", false),
            ("-oProxyCommand=x", false),
            ("ftp://host/repo", false),
            ("https://github.com/org/repo.git", true),
            ("git@github.com:org/repo.git", true),
        ] {
            let d = PipelineDefinition {
                name: "p".into(),
                env: Default::default(),
                workspace: Some(fiber_proto::WorkspaceConfig {
                    repo: repo.into(),
                    git_ref: None,
                }),
                concurrency: None,
                on: None,
                steps: vec![step("a", &[], "echo")],
                timeout_minutes: None,
            };
            let r = compile_definition(&d);
            assert_eq!(
                r.is_ok(),
                ok,
                "{repo:?}: {:?}",
                r.err().map(|e| e.to_string())
            );
            if !ok {
                assert!(matches!(compile_definition(&d), Err(DagError::BadRepo(_))));
            }
        }
    }

    #[test]
    fn the_fiber_prefix_is_reserved() {
        let d = env_pipeline(&[], &[("FIBER_RUN_ID", "spoofed")]);
        assert!(matches!(
            compile_definition(&d),
            Err(DagError::ReservedEnvName { .. })
        ));
        // The check is on the merged set, so setting it on the pipeline is caught too.
        let d = env_pipeline(&[("FIBER_ANYTHING", "x")], &[]);
        assert!(matches!(
            compile_definition(&d),
            Err(DagError::ReservedEnvName { .. })
        ));
    }

    fn step(id: &str, needs: &[&str], run: &str) -> StepDefinition {
        StepDefinition {
            id: id.into(),
            name: id.into(),
            needs: needs.iter().map(|s| (*s).to_string()).collect(),
            run: Some(run.into()),
            image: None,
            labels: vec![],
            retries: 0,
            env: BTreeMap::new(),
            working_directory: None,
            shell: None,
            continue_on_error: false,
            artifacts: vec![],
            matrix: None,
            if_expr: None,
            timeout_minutes: None,
            secrets: None,
        }
    }

    #[test]
    fn step_secrets_allowlist_survives_compile_and_snapshot() {
        let mut a = step("a", &[], "true");
        a.secrets = Some(vec!["NPM_TOKEN".into()]);
        let mut b = step("b", &[], "true");
        b.secrets = Some(vec![]);
        let def = PipelineDefinition {
            name: "s".into(),
            env: Default::default(),
            workspace: None,
            concurrency: None,
            on: None,
            timeout_minutes: None,
            steps: vec![a, b, step("c", &[], "true")],
        };
        let dag = compile_definition(&def).unwrap();
        let v = serde_json::to_value(&dag).unwrap();
        assert_eq!(v["steps"][0]["secrets"], serde_json::json!(["NPM_TOKEN"]));
        assert_eq!(v["steps"][1]["secrets"], serde_json::json!([]));
        // Omitted stays null — "every project secret", the historical behaviour.
        assert!(v["steps"][2]["secrets"].is_null());
    }

    fn def_of(steps: Vec<StepDefinition>) -> PipelineDefinition {
        PipelineDefinition {
            name: "t".into(),
            env: Default::default(),
            workspace: None,
            concurrency: None,
            on: None,
            steps,
            timeout_minutes: None,
        }
    }

    /// The condition is validated where the author can still see it, not skipped forever.
    #[test]
    fn an_unevaluable_if_fails_the_compile() {
        for expr in [
            "matrix.os != 'windows'",
            "success() && matrix.os == 'linux'",
            "failure()",
            "github.event_name == 'push'",
        ] {
            let mut a = step("a", &[], "true");
            a.if_expr = Some(expr.into());
            let err = compile_definition(&def_of(vec![a]))
                .expect_err(&format!("`{expr}` must not compile"));
            assert!(
                matches!(&err, DagError::BadIf { step, .. } if step == "a"),
                "{expr} gave {err:?}"
            );
            // The message has to say what was wrong, or it is the same silence in a 400.
            assert!(err.to_string().contains(expr), "{err}");
        }
        // A well-formed condition that is simply never true still compiles.
        let mut ok = step("a", &[], "true");
        ok.if_expr = Some("matrix.os == 'plan9'".into());
        assert!(compile_definition(&def_of(vec![ok])).is_ok());
    }

    #[test]
    fn an_empty_step_id_is_rejected() {
        assert!(matches!(
            compile_definition(&def_of(vec![step("", &[], "true")])),
            Err(DagError::EmptyStepId)
        ));
        assert!(matches!(
            compile_definition(&def_of(vec![step("   ", &[], "true")])),
            Err(DagError::EmptyStepId)
        ));
    }

    /// The matrix cap alone let 80 steps x 64 cells through as one definition.
    #[test]
    fn a_pipeline_past_the_step_cap_is_rejected() {
        let at_cap: Vec<StepDefinition> = (0..MAX_STEPS)
            .map(|i| step(&format!("s{i}"), &[], "true"))
            .collect();
        assert!(compile_definition(&def_of(at_cap)).is_ok());

        let over: Vec<StepDefinition> = (0..=MAX_STEPS)
            .map(|i| step(&format!("s{i}"), &[], "true"))
            .collect();
        assert!(matches!(
            compile_definition(&def_of(over)),
            Err(DagError::TooManySteps { count }) if count == MAX_STEPS + 1
        ));

        // Expanded cells, not written steps: 16 steps of 64 cells is 1 024.
        let matrixed: Vec<StepDefinition> = (0..16)
            .map(|i| {
                let mut s = step(&format!("m{i}"), &[], "true");
                s.matrix = Some(
                    [
                        ("a".to_string(), (0..8).map(|n| n.to_string()).collect()),
                        ("b".to_string(), (0..8).map(|n| n.to_string()).collect()),
                    ]
                    .into_iter()
                    .collect(),
                );
                s
            })
            .collect();
        assert!(matches!(
            compile_definition(&def_of(matrixed)),
            Err(DagError::TooManySteps { count }) if count == 1024
        ));
    }

    #[test]
    fn zero_timeout_is_rejected_and_positive_is_kept() {
        let mut s = step("a", &[], "true");
        s.timeout_minutes = Some(0);
        let def = PipelineDefinition {
            name: "t".into(),
            env: Default::default(),
            workspace: None,
            concurrency: None,
            on: None,
            steps: vec![s.clone()],
            timeout_minutes: None,
        };
        assert!(matches!(
            compile_definition(&def),
            Err(DagError::InvalidTimeout(_))
        ));
        s.timeout_minutes = Some(5);
        let def = PipelineDefinition {
            timeout_minutes: Some(0),
            steps: vec![s.clone()],
            ..def
        };
        assert!(matches!(
            compile_definition(&def),
            Err(DagError::InvalidTimeout(_))
        ));
        let def = PipelineDefinition {
            timeout_minutes: Some(30),
            ..def
        };
        let dag = compile_definition(&def).unwrap();
        assert_eq!(dag.timeout_minutes, Some(30));
        assert_eq!(dag.steps[0].timeout_minutes, Some(5));
        // The step timeout survives the JSON snapshot round-trip.
        let v = serde_json::to_value(&dag).unwrap();
        assert_eq!(v["steps"][0]["timeout_minutes"], 5);
    }

    #[test]
    fn compiles_levels() {
        let def = PipelineDefinition {
            name: "demo".into(),
            env: Default::default(),
            workspace: None,
            concurrency: None,
            on: None,
            timeout_minutes: None,
            steps: vec![
                step("a", &[], "echo a"),
                step("b", &["a"], "echo b"),
                step("c", &["a"], "echo c"),
            ],
        };
        let dag = compile_definition(&def).unwrap();
        assert_eq!(dag.levels.len(), 2);
        assert_eq!(dag.levels[0], vec!["a".to_string()]);
        assert_eq!(dag.levels[1].len(), 2);
    }

    /// A step sits one level past its *furthest* dependency, not its nearest, so an
    /// uneven diamond has to push the join out to the long branch. `toposort` may
    /// return any valid order and petgraph does not promise a stable one between
    /// versions, so this pins the property the level maths actually depends on.
    #[test]
    fn a_level_is_the_longest_path_from_a_root_not_the_shortest() {
        let def = PipelineDefinition {
            name: "diamond".into(),
            env: Default::default(),
            workspace: None,
            concurrency: None,
            on: None,
            timeout_minutes: None,
            steps: vec![
                step("a", &[], "echo a"),
                step("b", &["a"], "echo b"),
                step("c", &["b"], "echo c"),
                step("d", &["a"], "echo d"),
                step("join", &["c", "d"], "echo join"),
            ],
        };
        let dag = compile_definition(&def).unwrap();
        let level = |id: &str| dag.steps.iter().find(|s| s.id == id).unwrap().level;
        assert_eq!(level("a"), 0);
        assert_eq!(level("b"), 1);
        assert_eq!(level("d"), 1);
        assert_eq!(level("c"), 2);
        // Reachable at distance 2 through `d`, but 3 through `c` — the long one wins.
        assert_eq!(level("join"), 3);
        assert_eq!(dag.levels.len(), 4);
    }

    /// The compiled order comes from the definition, not from the graph walk, so a
    /// pipeline that declares its steps bottom-up compiles to the same thing.
    #[test]
    fn declaration_order_does_not_change_the_compiled_levels() {
        let steps = |order: [&str; 4]| {
            let by_id = |id: &str| match id {
                "a" => step("a", &[], "echo a"),
                "b" => step("b", &["a"], "echo b"),
                "c" => step("c", &["b"], "echo c"),
                _ => step("d", &["c"], "echo d"),
            };
            PipelineDefinition {
                name: "chain".into(),
                env: Default::default(),
                workspace: None,
                concurrency: None,
                on: None,
                timeout_minutes: None,
                steps: order.iter().map(|id| by_id(id)).collect(),
            }
        };
        let forward = compile_definition(&steps(["a", "b", "c", "d"])).unwrap();
        let backward = compile_definition(&steps(["d", "c", "b", "a"])).unwrap();
        let levels = |dag: &CompiledDag| {
            let mut v: Vec<(String, usize)> =
                dag.steps.iter().map(|s| (s.id.clone(), s.level)).collect();
            v.sort();
            v
        };
        assert_eq!(levels(&forward), levels(&backward));
        assert_eq!(levels(&forward).last().unwrap().1, 3);
    }

    /// The degenerate cycle: one step that needs itself. Worth its own case because
    /// it is the shape a hand-edited `fiber.yml` produces by typo, and because it is
    /// the one `toposort` could plausibly have treated as an ordinary node.
    #[test]
    fn detects_a_step_that_needs_itself() {
        let def = PipelineDefinition {
            name: "bad".into(),
            env: Default::default(),
            workspace: None,
            concurrency: None,
            on: None,
            timeout_minutes: None,
            steps: vec![step("a", &["a"], "echo a")],
        };
        assert!(matches!(compile_definition(&def), Err(DagError::Cycle)));
    }

    /// The two-node case can be caught without walking the graph at all; a longer
    /// loop is the one that needs the cycle check to be real.
    #[test]
    fn detects_a_cycle_longer_than_two_steps() {
        let def = PipelineDefinition {
            name: "bad".into(),
            env: Default::default(),
            workspace: None,
            concurrency: None,
            on: None,
            timeout_minutes: None,
            steps: vec![
                step("a", &["c"], "echo a"),
                step("b", &["a"], "echo b"),
                step("c", &["b"], "echo c"),
            ],
        };
        assert!(matches!(compile_definition(&def), Err(DagError::Cycle)));
    }

    #[test]
    fn detects_cycle() {
        let def = PipelineDefinition {
            name: "bad".into(),
            env: Default::default(),
            workspace: None,
            concurrency: None,
            on: None,
            timeout_minutes: None,
            steps: vec![step("a", &["b"], "echo a"), step("b", &["a"], "echo b")],
        };
        assert!(matches!(compile_definition(&def), Err(DagError::Cycle)));
    }

    #[test]
    fn expands_matrix_and_rewrites_needs() {
        let mut matrix = BTreeMap::new();
        matrix.insert("os".into(), vec!["linux".into(), "macos".into()]);
        let mut test = step("test", &["checkout"], "echo $MATRIX_OS");
        test.matrix = Some(matrix);
        let def = PipelineDefinition {
            name: "m".into(),
            env: Default::default(),
            workspace: None,
            concurrency: None,
            on: None,
            timeout_minutes: None,
            steps: vec![
                step("checkout", &[], "echo hi"),
                test,
                step("done", &["test"], "echo done"),
            ],
        };
        let dag = compile_definition(&def).unwrap();
        let ids: Vec<_> = dag.steps.iter().map(|s| s.id.as_str()).collect();
        assert!(ids.contains(&"test__os_linux"));
        assert!(ids.contains(&"test__os_macos"));
        let done = dag.steps.iter().find(|s| s.id == "done").unwrap();
        assert_eq!(done.needs.len(), 2);
        assert!(done.needs.contains(&"test__os_linux".into()));
        assert!(done.needs.contains(&"test__os_macos".into()));
        let linux = dag.steps.iter().find(|s| s.id == "test__os_linux").unwrap();
        assert_eq!(linux.matrix.get("os").map(String::as_str), Some("linux"));
        assert!(
            linux
                .env
                .iter()
                .any(|(k, v)| k == "MATRIX_OS" && v == "linux")
        );
    }

    #[test]
    fn parse_yaml_matrix_and_if() {
        let yaml = r#"
name: m
steps:
  t:
    run: echo $MATRIX_OS
    if: "matrix.os == 'linux'"
    matrix:
      os: [linux, windows]
"#;
        let def = parse_pipeline_yaml(yaml).unwrap();
        assert_eq!(def.steps.len(), 1);
        assert_eq!(
            def.steps[0].if_expr.as_deref(),
            Some("matrix.os == 'linux'")
        );
        let dag = compile_definition(&def).unwrap();
        assert_eq!(dag.steps.len(), 2);
    }

    /// Each of these parsed clean and produced a step missing the field that was written.
    #[test]
    fn unknown_keys_are_rejected_with_the_nearest_field() {
        let cases = [
            (
                "name: p\nsteps:\n  a:\n    run: 'true'\n    continue-on-error: true\n",
                "continue_on_error",
            ),
            (
                "name: p\nsteps:\n  a:\n    run: 'true'\n    working-directory: sub\n",
                "working_directory",
            ),
            (
                "name: p\nsteps:\n  a:\n    run: 'true'\n    artifact: [out]\n",
                "artifacts",
            ),
            (
                "name: p\nsteps:\n  a:\n    run: 'true'\n  b:\n    run: 'true'\n    need: [a]\n",
                "needs",
            ),
            (
                "name: p\nsteps:\n  - id: a\n    name: A\n    run: 'true'\n    imgae: rust\n",
                "image",
            ),
            (
                "name: p\ntimeout-minutes: 5\nsteps:\n  a:\n    run: 'true'\n",
                "timeout_minutes",
            ),
            (
                "name: p\non:\n  schedule: '0 0 * * * *'\nsteps:\n  a:\n    run: 'true'\n",
                "unknown field `schedule`",
            ),
            (
                "name: p\non:\n  push:\n    paths-ignore: ['*.md']\nsteps:\n  a:\n    run: 'true'\n",
                "paths_ignore",
            ),
            (
                "name: p\nconcurrency:\n  cancel-in-progress: true\nsteps:\n  a:\n    run: 'true'\n",
                "cancel_in_progress",
            ),
            (
                "name: p\nworkspace:\n  repo: /tmp/r\n  branch: main\nsteps:\n  a:\n    run: 'true'\n",
                "unknown field `branch`",
            ),
        ];
        for (yaml, needle) in cases {
            let err = parse_pipeline_yaml(yaml).expect_err("must be rejected");
            let msg = err.to_string();
            assert!(msg.contains(needle), "expected `{needle}` in: {msg}");
            // The lenient reader still accepts it, so stored rows keep working.
            assert!(
                parse_pipeline_yaml_lenient(yaml).is_ok(),
                "lenient parse of {yaml:?} failed"
            );
        }
    }

    #[test]
    fn the_map_form_names_the_step_and_refuses_a_second_id() {
        let err =
            parse_pipeline_yaml("name: p\nsteps:\n  build:\n    id: other\n    run: 'true'\n")
                .expect_err("rejected");
        assert!(err.to_string().contains("remove `id:`"), "{err}");
        let err =
            parse_pipeline_yaml("name: p\nsteps:\n  '':\n    run: 'true'\n").expect_err("rejected");
        assert!(err.to_string().contains("cannot be empty"), "{err}");
        // The list form keeps `id`, which is the only place it belongs.
        assert!(
            parse_pipeline_yaml("name: p\nsteps:\n  - id: a\n    name: A\n    run: 'true'\n")
                .is_ok()
        );
    }

    #[test]
    fn steps_that_are_neither_a_list_nor_a_map_are_an_error() {
        let err = parse_pipeline_yaml("name: p\nsteps: build\n").expect_err("rejected");
        assert!(err.to_string().contains("a list of steps"), "{err}");
    }

    /// The allowlists are hand-mirrored from `fiber-proto`; this is what notices when a
    /// field is added there and not here, which would make the new field an unknown key.
    #[test]
    fn unknown_keys_name_every_field() {
        fn fields(v: &serde_json::Value) -> Vec<String> {
            v.as_object()
                .expect("a struct serialises to a map")
                .keys()
                .cloned()
                .collect()
        }
        let step = serde_json::to_value(step("a", &[], "true")).unwrap();
        for f in fields(&step) {
            assert!(keys::STEP.contains(&f.as_str()), "step field `{f}` missing");
        }
        assert_eq!(keys::STEP.len(), fields(&step).len());

        let def = serde_json::to_value(def_of(vec![step_def()])).unwrap();
        for f in fields(&def) {
            assert!(keys::PIPELINE.contains(&f.as_str()), "field `{f}` missing");
        }
        assert_eq!(keys::PIPELINE.len(), fields(&def).len());

        let on = serde_json::to_value(fiber_proto::PipelineTriggers {
            push: None,
            pull_request: None,
            interval_minutes: None,
            cron: None,
        })
        .unwrap();
        assert_eq!(keys::ON.len(), fields(&on).len());
        for f in fields(&on) {
            assert!(keys::ON.contains(&f.as_str()), "on field `{f}` missing");
        }

        let push = serde_json::to_value(fiber_proto::PushTrigger {
            branches: vec![],
            paths: vec![],
            paths_ignore: vec![],
        })
        .unwrap();
        assert_eq!(keys::PUSH.len(), fields(&push).len());
        let pr = serde_json::to_value(fiber_proto::PullRequestTrigger {
            branches: vec![],
            types: vec![],
            paths: vec![],
            paths_ignore: vec![],
        })
        .unwrap();
        assert_eq!(keys::PULL_REQUEST.len(), fields(&pr).len());
        let ws = serde_json::to_value(fiber_proto::WorkspaceConfig {
            repo: "r".into(),
            git_ref: None,
        })
        .unwrap();
        assert_eq!(keys::WORKSPACE.len(), fields(&ws).len());
        for f in fields(&ws) {
            assert!(keys::WORKSPACE.contains(&f.as_str()), "`{f}` missing");
        }
        let c = serde_json::to_value(fiber_proto::ConcurrencyConfig {
            group: None,
            cancel_in_progress: false,
        })
        .unwrap();
        assert_eq!(keys::CONCURRENCY.len(), fields(&c).len());
        for f in fields(&c) {
            assert!(keys::CONCURRENCY.contains(&f.as_str()), "`{f}` missing");
        }
    }

    fn step_def() -> StepDefinition {
        step("a", &[], "true")
    }

    #[test]
    fn every_example_pipeline_still_parses_and_compiles() {
        let dir = concat!(env!("CARGO_MANIFEST_DIR"), "/../../examples");
        let mut seen = 0;
        for entry in std::fs::read_dir(dir).expect("examples/ is next to the crates") {
            let path = entry.expect("dir entry").path();
            if path.extension().and_then(|e| e.to_str()) != Some("yml") {
                continue;
            }
            let yaml = std::fs::read_to_string(&path).expect("read");
            let def = parse_pipeline_yaml(&yaml)
                .unwrap_or_else(|e| panic!("{} does not parse: {e}", path.display()));
            compile_definition(&def)
                .unwrap_or_else(|e| panic!("{} does not compile: {e}", path.display()));
            if let Some(on) = &def.on {
                crate::schedule::validate_triggers(on)
                    .unwrap_or_else(|e| panic!("{} has bad triggers: {e}", path.display()));
            }
            seen += 1;
        }
        assert!(seen >= 9, "expected the examples to be found, saw {seen}");

        let root = concat!(env!("CARGO_MANIFEST_DIR"), "/../../fiber.yml");
        let yaml = std::fs::read_to_string(root).expect("the repo's own fiber.yml");
        let def = parse_pipeline_yaml(&yaml).expect("fiber.yml parses");
        compile_definition(&def).expect("fiber.yml compiles");
    }
}
