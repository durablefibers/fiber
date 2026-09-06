//! `fiber` CLI — validate YAML, login, pipelines, runs, logs, artifacts, members,
//! secrets, agents, fibers.

use anyhow::{Context, Result, anyhow};
use clap::{CommandFactory, Parser, Subcommand};
use fiber_core::dag::{compile_definition, parse_pipeline_yaml};
use serde::Deserialize;
use serde_json::json;
use std::path::PathBuf;
use std::process::Command;

#[derive(Parser, Debug)]
#[command(name = "fiber", about = "Fiber CI CLI", version)]
struct Cli {
    #[arg(long, env = "FIBER_API_URL", default_value = "http://127.0.0.1:18080")]
    api_url: String,

    #[arg(long, env = "FIBER_TOKEN")]
    token: Option<String>,

    /// Print machine-readable JSON instead of the human summary.
    #[arg(long, global = true)]
    json: bool,

    #[command(subcommand)]
    cmd: Commands,
}

/// `fiber run --wait` and `runs retry --wait` exit with the run's outcome, so a shell or
/// another CI system can branch on it.
const EXIT_RUN_FAILED: i32 = 1;
const EXIT_WAIT_TIMEOUT: i32 = 3;

#[derive(Subcommand, Debug)]
enum Commands {
    /// Validate a fiber.yml pipeline definition
    Validate {
        #[arg(default_value = "fiber.yml")]
        path: PathBuf,
    },
    /// Login and print a session token (also writes ~/.fiber/token, mode 0600)
    Login {
        #[arg(long, env = "FIBER_ADMIN_USER", default_value = "admin")]
        username: String,
        /// Password (default `fiber`). Prefer --password-stdin to keep it out of shell history.
        /// Ignored when --password-stdin is given (so an exported FIBER_ADMIN_PASSWORD does not block it).
        #[arg(long, env = "FIBER_ADMIN_PASSWORD")]
        password: Option<String>,
        /// Read the password from stdin (first line).
        #[arg(long)]
        password_stdin: bool,
    },
    /// Start a pipeline run
    Run {
        pipeline_id: String,
        /// Block until the run finishes and exit with its outcome (1 = failed/cancelled).
        #[arg(long)]
        wait: bool,
        /// Imply --wait and stream step output as it arrives.
        #[arg(long)]
        follow: bool,
        /// Give up waiting after this many seconds (exit 3).
        #[arg(long, default_value_t = 3600)]
        timeout_secs: u64,
    },
    /// Projects
    #[command(subcommand)]
    Projects(ProjectsCmd),
    /// Pipelines — list, show, and apply a fiber.yml
    #[command(subcommand)]
    Pipelines(PipelinesCmd),
    /// Runs — list, show, cancel, retry
    #[command(subcommand)]
    Runs(RunsCmd),
    /// Print a step's logs
    Logs {
        step_run_id: String,
        /// Only this attempt (default: the whole step).
        #[arg(long)]
        attempt: Option<i32>,
        /// Keep printing new lines until the step finishes.
        #[arg(long)]
        follow: bool,
        /// Lines to show when not following.
        #[arg(long, default_value_t = 1000)]
        limit: i64,
    },
    /// Run artifacts
    #[command(subcommand)]
    Artifacts(ArtifactsCmd),
    /// Forget the saved session token
    Logout,
    /// Print a shell completion script (bash, zsh, fish, elvish, powershell)
    Completions { shell: clap_complete::Shell },
    /// Project members
    #[command(subcommand)]
    Members(MembersCmd),
    /// Project secrets
    #[command(subcommand)]
    Secrets(SecretsCmd),
    /// Manage agent registrations (list / create / update / delete / rotate)
    #[command(subcommand)]
    Agents(AgentsCmd),
    /// Fiber (durable task) commands
    #[command(subcommand)]
    Fibers(FibersCmd),
    /// Spawn fiber-agent (forwards env / flags)
    Agent {
        #[arg(long, env = "FIBER_AGENT_TOKEN")]
        token: Option<String>,
        #[arg(long, default_value = "os=linux")]
        labels: String,
        #[arg(long, default_value_t = false)]
        docker: bool,
    },
}

#[derive(Subcommand, Debug)]
enum ProjectsCmd {
    List,
    Create { name: String },
}

#[derive(Subcommand, Debug)]
enum PipelinesCmd {
    List {
        project_id: String,
    },
    Get {
        pipeline_id: String,
    },
    /// Create or update a pipeline from a fiber.yml. Matched by name within the project
    /// unless --id is given, so re-running it is an update rather than a duplicate.
    Apply {
        #[arg(default_value = "fiber.yml")]
        path: PathBuf,
        #[arg(long)]
        project_id: Option<String>,
        /// Update this pipeline regardless of the name in the file.
        #[arg(long)]
        id: Option<String>,
        /// Override the pipeline name from the file.
        #[arg(long)]
        name: Option<String>,
    },
}

#[derive(Subcommand, Debug)]
enum RunsCmd {
    List {
        project_id: String,
        #[arg(long, default_value_t = 20)]
        limit: i64,
        /// Continue after this run id (from a previous page).
        #[arg(long)]
        before: Option<String>,
    },
    Get {
        run_id: String,
    },
    Cancel {
        run_id: String,
    },
    Retry {
        run_id: String,
        /// Carry over steps that already succeeded and re-run only the rest.
        #[arg(long)]
        failed_only: bool,
        #[arg(long)]
        wait: bool,
        #[arg(long)]
        follow: bool,
        #[arg(long, default_value_t = 3600)]
        timeout_secs: u64,
    },
}

#[derive(Subcommand, Debug)]
enum ArtifactsCmd {
    List {
        run_id: String,
    },
    Download {
        artifact_id: String,
        /// Where to write it (default: the artifact's file name in the current directory).
        #[arg(long)]
        out: Option<PathBuf>,
    },
}

#[derive(Subcommand, Debug)]
enum MembersCmd {
    List {
        project_id: String,
    },
    Add {
        project_id: String,
        #[arg(long)]
        username: String,
        #[arg(long, default_value = "reader")]
        role: String,
        #[arg(long)]
        password: Option<String>,
    },
    Set {
        project_id: String,
        user_id: String,
        #[arg(long)]
        role: String,
    },
    Remove {
        project_id: String,
        user_id: String,
    },
}

#[derive(Subcommand, Debug)]
enum SecretsCmd {
    List {
        project_id: String,
    },
    Set {
        project_id: String,
        key: String,
        /// Secret value. Prefer --value-stdin to keep it out of shell history (stdin wins if both are given).
        #[arg(long)]
        value: Option<String>,
        /// Read the value from stdin (first line, trailing newline stripped).
        #[arg(long)]
        value_stdin: bool,
    },
    Delete {
        project_id: String,
        key: String,
    },
}

#[derive(Subcommand, Debug)]
enum AgentsCmd {
    List {
        #[arg(long)]
        project_id: Option<String>,
    },
    Create {
        #[arg(long)]
        name: String,
        #[arg(long, default_value = "os=linux")]
        labels: String,
        #[arg(long, default_value_t = 1)]
        concurrency: u32,
        #[arg(long)]
        project_id: Option<String>,
    },
    Update {
        agent_id: String,
        #[arg(long)]
        name: Option<String>,
        #[arg(long)]
        labels: Option<String>,
        #[arg(long)]
        concurrency: Option<u32>,
    },
    Delete {
        agent_id: String,
    },
    /// Issue a new token (prints once) and force-disconnect the agent
    Rotate {
        agent_id: String,
    },
}

#[derive(Subcommand, Debug)]
enum FibersCmd {
    List {
        project_id: String,
    },
    Create {
        project_id: String,
        #[arg(long, default_value = "ping")]
        name: String,
        #[arg(long, default_value = "{}")]
        input: String,
    },
    Get {
        fiber_id: String,
    },
    Cancel {
        fiber_id: String,
    },
}

#[tokio::main]
async fn main() -> Result<()> {
    // Rust ignores SIGPIPE, so `fiber logs … | head` would panic on the closed pipe
    // instead of exiting quietly the way every other Unix tool does.
    #[cfg(unix)]
    unsafe {
        libc::signal(libc::SIGPIPE, libc::SIG_DFL);
    }

    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::from_default_env()
                .add_directive("fiber=info".parse().unwrap()),
        )
        .init();

    let cli = Cli::parse();
    let base = cli.api_url.trim_end_matches('/').to_string();
    let token_opt = cli.token.clone();

    match cli.cmd {
        Commands::Validate { path } => {
            let yaml = std::fs::read_to_string(&path)
                .with_context(|| format!("read {}", path.display()))?;
            let def = parse_pipeline_yaml(&yaml).context("parse yaml")?;
            let compiled = compile_definition(&def).context("compile dag")?;
            println!(
                "ok: {} ({} steps, {} levels)",
                compiled.name,
                compiled.steps.len(),
                compiled.levels.len()
            );
            for (i, level) in compiled.levels.iter().enumerate() {
                println!("  L{i}: {}", level.join(", "));
            }
        }
        Commands::Login {
            username,
            password,
            password_stdin,
        } => {
            let password = if password_stdin {
                read_stdin_line().context("read password from stdin")?
            } else {
                password.unwrap_or_else(|| "fiber".to_string())
            };
            let client = reqwest::Client::new();
            let resp: LoginResp = client
                .post(format!("{base}/api/auth/login"))
                .json(&json!({ "username": username, "password": password }))
                .send()
                .await?
                .error_for_status()?
                .json()
                .await?;
            save_token(&resp.token)?;
            println!("{}", resp.token);
        }
        Commands::Run {
            pipeline_id,
            wait,
            follow,
            timeout_secs,
        } => {
            let token = resolve_token(token_opt.as_deref())?;
            let client = api_client(&token);
            let resp = api_json(
                client
                    .post(format!("{base}/api/pipelines/{pipeline_id}/runs"))
                    .json(&json!({ "trigger": "manual" })),
            )
            .await?;
            let id = resp["run"]["id"]
                .as_str()
                .ok_or_else(|| anyhow!("no run id in response"))?
                .to_string();
            if cli.json && !(wait || follow) {
                print_json(&resp)?;
            } else if !(wait || follow) {
                println!("started run {id}");
            }
            if wait || follow {
                let status =
                    wait_for_run(&client, &base, &id, follow, timeout_secs, cli.json).await?;
                exit_for_run(&status);
            }
        }
        Commands::Projects(sub) => {
            let token = resolve_token(token_opt.as_deref())?;
            let client = api_client(&token);
            match sub {
                ProjectsCmd::List => {
                    let v = api_json(client.get(format!("{base}/api/projects"))).await?;
                    if cli.json {
                        print_json(&v)?;
                    } else {
                        for p in v.as_array().cloned().unwrap_or_default() {
                            println!(
                                "{}  {}  ({})",
                                p["id"].as_str().unwrap_or("?"),
                                p["name"].as_str().unwrap_or("?"),
                                p["slug"].as_str().unwrap_or("?")
                            );
                        }
                    }
                }
                ProjectsCmd::Create { name } => {
                    let v = api_json(
                        client
                            .post(format!("{base}/api/projects"))
                            .json(&json!({ "name": name })),
                    )
                    .await?;
                    print_json(&v)?;
                }
            }
        }
        Commands::Pipelines(sub) => {
            let token = resolve_token(token_opt.as_deref())?;
            let client = api_client(&token);
            match sub {
                PipelinesCmd::List { project_id } => {
                    let v =
                        api_json(client.get(format!("{base}/api/projects/{project_id}/pipelines")))
                            .await?;
                    if cli.json {
                        print_json(&v)?;
                    } else {
                        for p in v.as_array().cloned().unwrap_or_default() {
                            println!(
                                "{}  {}",
                                p["id"].as_str().unwrap_or("?"),
                                p["name"].as_str().unwrap_or("?")
                            );
                        }
                    }
                }
                PipelinesCmd::Get { pipeline_id } => {
                    let v =
                        api_json(client.get(format!("{base}/api/pipelines/{pipeline_id}"))).await?;
                    print_json(&v)?;
                }
                PipelinesCmd::Apply {
                    path,
                    project_id,
                    id,
                    name,
                } => {
                    let yaml = std::fs::read_to_string(&path)
                        .with_context(|| format!("read {}", path.display()))?;
                    // Compile locally first: a syntax or DAG error should not need a round trip.
                    let def = parse_pipeline_yaml(&yaml).context("parse yaml")?;
                    compile_definition(&def).context("compile dag")?;
                    let name = name.unwrap_or_else(|| def.name.clone());
                    let definition = serde_json::to_value(&def)?;

                    let target = match id {
                        Some(id) => Some(id),
                        None => {
                            let project_id = project_id.clone().ok_or_else(|| {
                                anyhow!("pass --project-id (or --id to update a known pipeline)")
                            })?;
                            let existing = api_json(
                                client.get(format!("{base}/api/projects/{project_id}/pipelines")),
                            )
                            .await?;
                            existing.as_array().and_then(|a| {
                                a.iter()
                                    .find(|p| p["name"].as_str() == Some(name.as_str()))
                                    .and_then(|p| p["id"].as_str().map(str::to_string))
                            })
                        }
                    };
                    let body = json!({ "name": name, "definition": definition });
                    let (v, action) = match target {
                        Some(pid) => (
                            api_json(
                                client
                                    .put(format!("{base}/api/pipelines/{pid}"))
                                    .json(&body),
                            )
                            .await?,
                            "updated",
                        ),
                        None => {
                            let project_id = project_id.ok_or_else(|| {
                                anyhow!("pass --project-id to create a new pipeline")
                            })?;
                            (
                                api_json(
                                    client
                                        .post(format!("{base}/api/projects/{project_id}/pipelines"))
                                        .json(&body),
                                )
                                .await?,
                                "created",
                            )
                        }
                    };
                    if cli.json {
                        print_json(&v)?;
                    } else {
                        println!("{action} {} ({})", v["id"].as_str().unwrap_or("?"), name);
                    }
                }
            }
        }
        Commands::Runs(sub) => {
            let token = resolve_token(token_opt.as_deref())?;
            let client = api_client(&token);
            match sub {
                RunsCmd::List {
                    project_id,
                    limit,
                    before,
                } => {
                    let mut url = format!("{base}/api/projects/{project_id}/runs?limit={limit}");
                    if let Some(b) = before {
                        url.push_str(&format!("&before={b}"));
                    }
                    let v = api_json(client.get(url)).await?;
                    if cli.json {
                        print_json(&v)?;
                    } else {
                        for r in v["items"].as_array().cloned().unwrap_or_default() {
                            println!(
                                "{}  {:<9}  {}  {}",
                                r["id"].as_str().unwrap_or("?"),
                                r["status"].as_str().unwrap_or("?"),
                                r["created_at"].as_str().unwrap_or("?"),
                                r["trigger"].as_str().unwrap_or("")
                            );
                        }
                        if let Some(c) = v["next_cursor"].as_str() {
                            println!("(more: --before {c})");
                        }
                    }
                }
                RunsCmd::Get { run_id } => {
                    let v = api_json(client.get(format!("{base}/api/runs/{run_id}"))).await?;
                    if cli.json {
                        print_json(&v)?;
                    } else {
                        println!(
                            "run {} {}",
                            v["run"]["id"].as_str().unwrap_or("?"),
                            v["run"]["status"].as_str().unwrap_or("?")
                        );
                        for s in v["steps"].as_array().cloned().unwrap_or_default() {
                            println!(
                                "  {:<9} {:<20} {}",
                                s["status"].as_str().unwrap_or("?"),
                                s["step_id"].as_str().unwrap_or("?"),
                                s["error"].as_str().unwrap_or("")
                            );
                        }
                    }
                }
                RunsCmd::Cancel { run_id } => {
                    let v =
                        api_json(client.post(format!("{base}/api/runs/{run_id}/cancel"))).await?;
                    print_json(&v)?;
                }
                RunsCmd::Retry {
                    run_id,
                    failed_only,
                    wait,
                    follow,
                    timeout_secs,
                } => {
                    let v = api_json(
                        client
                            .post(format!("{base}/api/runs/{run_id}/retry"))
                            .json(&json!({ "failed_only": failed_only })),
                    )
                    .await?;
                    let id = v["run"]["id"]
                        .as_str()
                        .ok_or_else(|| anyhow!("no run id in response"))?
                        .to_string();
                    if cli.json && !(wait || follow) {
                        print_json(&v)?;
                    } else if !(wait || follow) {
                        println!("started run {id}");
                    }
                    if wait || follow {
                        let status =
                            wait_for_run(&client, &base, &id, follow, timeout_secs, cli.json)
                                .await?;
                        exit_for_run(&status);
                    }
                }
            }
        }
        Commands::Logs {
            step_run_id,
            attempt,
            follow,
            limit,
        } => {
            let token = resolve_token(token_opt.as_deref())?;
            let client = api_client(&token);
            let mut after: Option<i64> = None;
            loop {
                let mut url = format!("{base}/api/steps/{step_run_id}/logs?limit={limit}");
                if let Some(a) = attempt {
                    url.push_str(&format!("&attempt={a}"));
                }
                if let Some(a) = after {
                    url.push_str(&format!("&after_id={a}"));
                }
                let v = api_json(client.get(url)).await?;
                let lines = v.as_array().cloned().unwrap_or_default();
                for l in &lines {
                    if cli.json {
                        println!("{}", serde_json::to_string(l)?);
                    } else {
                        println!(
                            "[{}] {}",
                            l["stream"].as_str().unwrap_or("out"),
                            l["data"].as_str().unwrap_or("")
                        );
                    }
                    after = l["id"].as_i64().or(after);
                }
                if !follow {
                    break;
                }
                // Stop once the step itself is finished and we have drained its output.
                let step = api_json(client.get(format!("{base}/api/steps/{step_run_id}/attempts")))
                    .await
                    .unwrap_or_else(|_| json!([]));
                let done = step
                    .as_array()
                    .and_then(|a| a.last().cloned())
                    .map(|a| !a["finished_at"].is_null())
                    .unwrap_or(false);
                if done && lines.is_empty() {
                    break;
                }
                tokio::time::sleep(std::time::Duration::from_secs(2)).await;
            }
        }
        Commands::Artifacts(sub) => {
            let token = resolve_token(token_opt.as_deref())?;
            let client = api_client(&token);
            match sub {
                ArtifactsCmd::List { run_id } => {
                    let v =
                        api_json(client.get(format!("{base}/api/runs/{run_id}/artifacts"))).await?;
                    if cli.json {
                        print_json(&v)?;
                    } else {
                        for a in v.as_array().cloned().unwrap_or_default() {
                            println!(
                                "{}  {:>10}  {}",
                                a["id"].as_str().unwrap_or("?"),
                                a["size"].as_i64().unwrap_or(0),
                                a["name"].as_str().unwrap_or("?")
                            );
                        }
                    }
                }
                ArtifactsCmd::Download { artifact_id, out } => {
                    let resp = client
                        .get(format!("{base}/api/artifacts/{artifact_id}/download"))
                        .send()
                        .await?;
                    let status = resp.status();
                    if !status.is_success() {
                        return Err(anyhow!(
                            "HTTP {status}: {}",
                            resp.text().await.unwrap_or_default()
                        ));
                    }
                    let name = out.unwrap_or_else(|| {
                        PathBuf::from(
                            filename_from_disposition(&resp)
                                .unwrap_or_else(|| format!("{artifact_id}.bin")),
                        )
                    });
                    let bytes = resp.bytes().await?;
                    if let Some(parent) = name.parent().filter(|p| !p.as_os_str().is_empty()) {
                        std::fs::create_dir_all(parent)?;
                    }
                    std::fs::write(&name, &bytes)?;
                    println!("{} ({} bytes)", name.display(), bytes.len());
                }
            }
        }
        Commands::Logout => {
            let path = token_path();
            match std::fs::remove_file(&path) {
                Ok(()) => println!("removed {}", path.display()),
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                    println!("no saved token");
                }
                Err(e) => return Err(e).context("remove token"),
            }
        }
        Commands::Completions { shell } => {
            let mut cmd = Cli::command();
            let name = cmd.get_name().to_string();
            clap_complete::generate(shell, &mut cmd, name, &mut std::io::stdout());
        }
        Commands::Members(sub) => {
            let token = resolve_token(token_opt.as_deref())?;
            let client = api_client(&token);
            match sub {
                MembersCmd::List { project_id } => {
                    let v =
                        api_json(client.get(format!("{base}/api/projects/{project_id}/members")))
                            .await?;
                    print_json(&v)?;
                }
                MembersCmd::Add {
                    project_id,
                    username,
                    role,
                    password,
                } => {
                    let mut body = json!({ "username": username, "role": role });
                    if let Some(p) = password {
                        body["password"] = json!(p);
                    }
                    let v = api_json(
                        client
                            .post(format!("{base}/api/projects/{project_id}/members"))
                            .json(&body),
                    )
                    .await?;
                    print_json(&v)?;
                }
                MembersCmd::Set {
                    project_id,
                    user_id,
                    role,
                } => {
                    let v = api_json(
                        client
                            .put(format!(
                                "{base}/api/projects/{project_id}/members/{user_id}"
                            ))
                            .json(&json!({ "role": role })),
                    )
                    .await?;
                    print_json(&v)?;
                }
                MembersCmd::Remove {
                    project_id,
                    user_id,
                } => {
                    let v = api_json(client.delete(format!(
                        "{base}/api/projects/{project_id}/members/{user_id}"
                    )))
                    .await?;
                    print_json(&v)?;
                }
            }
        }
        Commands::Secrets(sub) => {
            let token = resolve_token(token_opt.as_deref())?;
            let client = api_client(&token);
            match sub {
                SecretsCmd::List { project_id } => {
                    let v =
                        api_json(client.get(format!("{base}/api/projects/{project_id}/secrets")))
                            .await?;
                    print_json(&v)?;
                }
                SecretsCmd::Set {
                    project_id,
                    key,
                    value,
                    value_stdin,
                } => {
                    let value = if value_stdin {
                        read_stdin_line().context("read secret value from stdin")?
                    } else {
                        value.context("pass --value <v> or --value-stdin")?
                    };
                    let v = api_json(
                        client
                            .post(format!("{base}/api/projects/{project_id}/secrets"))
                            .json(&json!({ "key": key, "value": value })),
                    )
                    .await?;
                    print_json(&v)?;
                }
                SecretsCmd::Delete { project_id, key } => {
                    let v = api_json(
                        client.delete(format!("{base}/api/projects/{project_id}/secrets/{key}")),
                    )
                    .await?;
                    print_json(&v)?;
                }
            }
        }
        Commands::Agents(sub) => {
            let token = resolve_token(token_opt.as_deref())?;
            let client = api_client(&token);
            match sub {
                AgentsCmd::List { project_id } => {
                    let url = match project_id {
                        Some(pid) => format!("{base}/api/agents?project_id={pid}"),
                        None => format!("{base}/api/agents"),
                    };
                    let v = api_json(client.get(url)).await?;
                    print_json(&v)?;
                }
                AgentsCmd::Create {
                    name,
                    labels,
                    concurrency,
                    project_id,
                } => {
                    let labels: Vec<String> = labels
                        .split(',')
                        .map(|s| s.trim().to_string())
                        .filter(|s| !s.is_empty())
                        .collect();
                    let mut body = json!({
                        "name": name,
                        "labels": labels,
                        "concurrency": concurrency,
                    });
                    if let Some(pid) = project_id {
                        body["project_id"] = json!(pid);
                    }
                    let v = api_json(client.post(format!("{base}/api/agents")).json(&body)).await?;
                    if let Some(t) = v["token"].as_str() {
                        eprintln!("# agent token (copy now — shown once)");
                        println!("{t}");
                        if let Some(agent) = v.get("agent") {
                            eprintln!("{}", serde_json::to_string_pretty(agent)?);
                        }
                    } else {
                        print_json(&v)?;
                    }
                }
                AgentsCmd::Update {
                    agent_id,
                    name,
                    labels,
                    concurrency,
                } => {
                    let mut body = json!({});
                    if let Some(n) = name {
                        body["name"] = json!(n);
                    }
                    if let Some(l) = labels {
                        let labels: Vec<String> = l
                            .split(',')
                            .map(|s| s.trim().to_string())
                            .filter(|s| !s.is_empty())
                            .collect();
                        body["labels"] = json!(labels);
                    }
                    if let Some(c) = concurrency {
                        body["concurrency"] = json!(c);
                    }
                    let v = api_json(
                        client
                            .put(format!("{base}/api/agents/{agent_id}"))
                            .json(&body),
                    )
                    .await?;
                    print_json(&v)?;
                }
                AgentsCmd::Delete { agent_id } => {
                    let v =
                        api_json(client.delete(format!("{base}/api/agents/{agent_id}"))).await?;
                    print_json(&v)?;
                }
                AgentsCmd::Rotate { agent_id } => {
                    let v =
                        api_json(client.post(format!("{base}/api/agents/{agent_id}/rotate-token")))
                            .await?;
                    if let Some(t) = v["token"].as_str() {
                        eprintln!("# new agent token (copy now — shown once)");
                        println!("{t}");
                        if let Some(agent) = v.get("agent") {
                            eprintln!("{}", serde_json::to_string_pretty(agent)?);
                        }
                    } else {
                        print_json(&v)?;
                    }
                }
            }
        }
        Commands::Fibers(sub) => {
            let token = resolve_token(token_opt.as_deref())?;
            let client = api_client(&token);
            match sub {
                FibersCmd::List { project_id } => {
                    let v =
                        api_json(client.get(format!("{base}/api/projects/{project_id}/fibers")))
                            .await?;
                    print_json(&v)?;
                }
                FibersCmd::Create {
                    project_id,
                    name,
                    input,
                } => {
                    let input_v: serde_json::Value = serde_json::from_str(&input)?;
                    let v = api_json(
                        client
                            .post(format!("{base}/api/projects/{project_id}/fibers"))
                            .json(&json!({ "name": name, "input": input_v })),
                    )
                    .await?;
                    print_json(&v)?;
                }
                FibersCmd::Get { fiber_id } => {
                    let v = api_json(client.get(format!("{base}/api/fibers/{fiber_id}"))).await?;
                    print_json(&v)?;
                }
                FibersCmd::Cancel { fiber_id } => {
                    let v = api_json(client.post(format!("{base}/api/fibers/{fiber_id}/cancel")))
                        .await?;
                    print_json(&v)?;
                }
            }
        }
        Commands::Agent {
            token,
            labels,
            docker,
        } => {
            let token = token
                .or_else(|| std::env::var("FIBER_AGENT_TOKEN").ok())
                .ok_or_else(|| anyhow!("set --token or FIBER_AGENT_TOKEN"))?;
            let api_ws = base
                .replacen("https://", "wss://", 1)
                .replacen("http://", "ws://", 1);
            let bin = std::env::current_exe()
                .ok()
                .and_then(|p| p.parent().map(|d| d.join("fiber-agent")))
                .filter(|p| p.exists())
                .unwrap_or_else(|| PathBuf::from("fiber-agent"));
            let status = Command::new(&bin)
                .env("FIBER_AGENT_TOKEN", token)
                .env("FIBER_API_URL", api_ws)
                .env("FIBER_AGENT_LABELS", labels)
                .env(
                    "FIBER_AGENT_USE_DOCKER",
                    if docker { "true" } else { "false" },
                )
                .status()
                .with_context(|| format!("spawn {} (cargo build -p fiber-agent)", bin.display()))?;
            if !status.success() {
                std::process::exit(status.code().unwrap_or(1));
            }
        }
    }
    Ok(())
}

/// Poll a run until it is terminal, optionally streaming step output as it appears.
///
/// Returns the final status. Polling (rather than the run WebSocket) keeps this usable
/// from a shell script behind a proxy that does not pass upgrades through.
async fn wait_for_run(
    client: &reqwest::Client,
    base: &str,
    run_id: &str,
    follow: bool,
    timeout_secs: u64,
    json_out: bool,
) -> Result<String> {
    use std::collections::HashMap;
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(timeout_secs);
    // step_run_id -> last log line id already printed.
    let mut cursors: HashMap<String, i64> = HashMap::new();
    let mut announced: HashMap<String, String> = HashMap::new();
    loop {
        let detail = api_json(client.get(format!("{base}/api/runs/{run_id}"))).await?;
        let status = detail["run"]["status"]
            .as_str()
            .unwrap_or("running")
            .to_string();
        let steps = detail["steps"].as_array().cloned().unwrap_or_default();

        for step in &steps {
            let (Some(sid), Some(step_name)) = (step["id"].as_str(), step["step_id"].as_str())
            else {
                continue;
            };
            let st = step["status"].as_str().unwrap_or("");
            if !json_out && announced.get(sid).map(String::as_str) != Some(st) {
                announced.insert(sid.to_string(), st.to_string());
                eprintln!("== {step_name}: {st}");
            }
            if !follow || matches!(st, "pending" | "queued") {
                continue;
            }
            let after = cursors.get(sid).copied();
            let mut url = format!("{base}/api/steps/{sid}/logs?limit=1000");
            if let Some(a) = after {
                url.push_str(&format!("&after_id={a}"));
            }
            let Ok(lines) = api_json(client.get(url)).await else {
                continue;
            };
            for l in lines.as_array().cloned().unwrap_or_default() {
                if json_out {
                    println!("{}", serde_json::to_string(&l)?);
                } else {
                    println!("{step_name} | {}", l["data"].as_str().unwrap_or(""));
                }
                if let Some(id) = l["id"].as_i64() {
                    cursors.insert(sid.to_string(), id);
                }
            }
        }

        if status != "running" && status != "pending" {
            if json_out {
                print_json(&detail)?;
            } else {
                println!("run {run_id} {status}");
            }
            return Ok(status);
        }
        if std::time::Instant::now() >= deadline {
            eprintln!("timed out waiting for run {run_id} after {timeout_secs}s");
            std::process::exit(EXIT_WAIT_TIMEOUT);
        }
        tokio::time::sleep(std::time::Duration::from_secs(2)).await;
    }
}

/// Succeeded exits 0; anything else exits 1, so `fiber run --wait` can gate a script.
fn exit_for_run(status: &str) {
    if status != "succeeded" {
        std::process::exit(EXIT_RUN_FAILED);
    }
}

fn filename_from_disposition(resp: &reqwest::Response) -> Option<String> {
    let raw = resp
        .headers()
        .get(reqwest::header::CONTENT_DISPOSITION)?
        .to_str()
        .ok()?;
    let name = raw.split("filename=").nth(1)?.trim().trim_matches('"');
    let name = name.rsplit(['/', '\\']).next()?;
    (!name.is_empty()).then(|| name.to_string())
}

#[derive(Deserialize)]
struct LoginResp {
    token: String,
}

fn api_client(token: &str) -> reqwest::Client {
    reqwest::Client::builder()
        .default_headers({
            let mut h = reqwest::header::HeaderMap::new();
            h.insert(
                reqwest::header::AUTHORIZATION,
                format!("Bearer {token}").parse().unwrap(),
            );
            h
        })
        .build()
        .expect("client")
}

async fn api_json(req: reqwest::RequestBuilder) -> Result<serde_json::Value> {
    let resp = req.send().await?;
    let status = resp.status();
    let text = resp.text().await.unwrap_or_default();
    if !status.is_success() {
        return Err(anyhow!("HTTP {status}: {text}"));
    }
    if text.trim().is_empty() {
        return Ok(json!({}));
    }
    Ok(serde_json::from_str(&text).unwrap_or_else(|_| json!({ "raw": text })))
}

fn print_json(v: &serde_json::Value) -> Result<()> {
    println!("{}", serde_json::to_string_pretty(v)?);
    Ok(())
}

fn resolve_token(cli_token: Option<&str>) -> Result<String> {
    if let Some(t) = cli_token {
        return Ok(t.to_string());
    }
    if let Ok(t) = std::env::var("FIBER_TOKEN") {
        return Ok(t);
    }
    load_token().context("no token; run `fiber login` or set FIBER_TOKEN")
}

fn token_path() -> PathBuf {
    dirs_token_home().join("token")
}

fn dirs_token_home() -> PathBuf {
    if let Some(h) = std::env::var_os("HOME") {
        return PathBuf::from(h).join(".fiber");
    }
    PathBuf::from(".fiber")
}

fn save_token(token: &str) -> Result<()> {
    use std::io::Write;
    let dir = dirs_token_home();
    std::fs::create_dir_all(&dir)?;
    let mut opts = std::fs::OpenOptions::new();
    opts.write(true).create(true).truncate(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
        // The session token is a bearer credential: private dir, private file.
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700))?;
        opts.mode(0o600);
    }
    let mut f = opts.open(token_path())?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        // Tighten a pre-existing file created with an older umask.
        f.set_permissions(std::fs::Permissions::from_mode(0o600))?;
    }
    f.write_all(token.as_bytes())?;
    Ok(())
}

fn read_stdin_line() -> Result<String> {
    let mut s = String::new();
    std::io::stdin().read_line(&mut s)?;
    let s = s.trim_end_matches(['\r', '\n']).to_string();
    if s.is_empty() {
        anyhow::bail!("stdin was empty");
    }
    Ok(s)
}

fn load_token() -> Result<String> {
    Ok(std::fs::read_to_string(token_path())?.trim().to_string())
}
