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
                artifacts: s.artifacts,
                matrix: s.matrix,
                if_expr: s.if_expr,
                timeout_minutes: s.timeout_minutes,
            })
            .collect(),
        timeout_minutes: None,
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StepDefinitionInput {
    pub name: Option<String>,
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
}

/// Parse YAML where `steps` may be a map (GitHub Actions style) or a list.
pub fn parse_pipeline_yaml(yaml: &str) -> Result<PipelineDefinition, serde_yaml::Error> {
    #[derive(Deserialize)]
    struct Raw {
        name: String,
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
                    artifacts: input.artifacts,
                    matrix: input.matrix,
                    if_expr: input.if_expr,
                    timeout_minutes: input.timeout_minutes,
                });
            }
            out
        }
        _ => Vec::new(),
    };

    Ok(PipelineDefinition {
        name: raw.name,
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

    fn step(id: &str, needs: &[&str], run: &str) -> StepDefinition {
        StepDefinition {
            id: id.into(),
            name: id.into(),
            needs: needs.iter().map(|s| (*s).to_string()).collect(),
            run: Some(run.into()),
            image: None,
            labels: vec![],
            retries: 0,
            artifacts: vec![],
            matrix: None,
            if_expr: None,
            timeout_minutes: None,
        }
    }

    #[test]
    fn zero_timeout_is_rejected_and_positive_is_kept() {
        let mut s = step("a", &[], "true");
        s.timeout_minutes = Some(0);
        let def = PipelineDefinition {
            name: "t".into(),
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
