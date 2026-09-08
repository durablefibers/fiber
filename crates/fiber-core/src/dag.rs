use fiber_proto::{PipelineDefinition, StepDefinition};
use petgraph::algo::{is_cyclic_directed, toposort};
use petgraph::graph::{DiGraph, NodeIndex};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, HashMap, HashSet};
use thiserror::Error;

const MAX_MATRIX_CELLS: usize = 64;

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
    for step in &def.steps {
        if step.timeout_minutes == Some(0) {
            return Err(DagError::InvalidTimeout(format!("step {}", step.id)));
        }
    }

    let mut seen = HashSet::new();
    for step in &def.steps {
        if !seen.insert(step.id.clone()) {
            return Err(DagError::DuplicateStep(step.id.clone()));
        }
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

    if is_cyclic_directed(&graph) {
        return Err(DagError::Cycle);
    }

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
            image: cell.template.image.clone(),
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
            secrets: cell.template.secrets.clone(),
        });
    }

    for level in &mut levels {
        level.sort_by_key(|id| compiled.iter().position(|s| &s.id == id).unwrap_or(0));
    }

    Ok(CompiledDag {
        name: def.name.clone(),
        workspace: def.workspace.clone(),
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

/// Parse YAML where `steps` may be a map (GitHub Actions style) or a list.
pub fn parse_pipeline_yaml(yaml: &str) -> Result<PipelineDefinition, serde_yaml::Error> {
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
    }

    let raw: Raw = serde_yaml::from_str(yaml)?;
    let steps = match raw.steps {
        serde_yaml::Value::Sequence(seq) => seq
            .into_iter()
            .map(serde_yaml::from_value::<StepDefinition>)
            .collect::<Result<Vec<_>, _>>()?,
        serde_yaml::Value::Mapping(map) => {
            let mut out = Vec::new();
            for (k, v) in map {
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
                    artifacts: input.artifacts,
                    matrix: input.matrix,
                    if_expr: input.if_expr,
                    timeout_minutes: input.timeout_minutes,
                    secrets: input.secrets,
                });
            }
            out
        }
        _ => Vec::new(),
    };

    Ok(PipelineDefinition {
        name: raw.name,
        env: raw.env,
        workspace: raw.workspace,
        on: raw.on,
        steps,
        timeout_minutes: raw.timeout_minutes,
    })
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
        for good in ["apps/web", "sub", "a/b/c"] {
            let mut st = step("a", &[], "echo");
            st.working_directory = Some(good.to_string());
            let d = PipelineDefinition {
                name: "p".into(),
                env: Default::default(),
                workspace: None,
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
                on: None,
                steps: vec![st],
                timeout_minutes: None,
            };
            assert!(compile_definition(&d).is_ok(), "rejected {good:?}");
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

    #[test]
    fn zero_timeout_is_rejected_and_positive_is_kept() {
        let mut s = step("a", &[], "true");
        s.timeout_minutes = Some(0);
        let def = PipelineDefinition {
            name: "t".into(),
            env: Default::default(),
            workspace: None,
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

    #[test]
    fn detects_cycle() {
        let def = PipelineDefinition {
            name: "bad".into(),
            env: Default::default(),
            workspace: None,
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
}
