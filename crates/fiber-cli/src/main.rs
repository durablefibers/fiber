//! `fiber` CLI — validate YAML, login, runs, members, secrets, agents, fibers.

use anyhow::{Context, Result, anyhow};
use clap::{Parser, Subcommand};
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

    #[command(subcommand)]
    cmd: Commands,
}

#[derive(Subcommand, Debug)]
enum Commands {
    /// Validate a fiber.yml pipeline definition
    Validate {
        #[arg(default_value = "fiber.yml")]
        path: PathBuf,
    },
    /// Login and print a session token (also writes ~/.fiber/token)
    Login {
        #[arg(long, env = "FIBER_ADMIN_USER", default_value = "admin")]
        username: String,
        #[arg(long, env = "FIBER_ADMIN_PASSWORD", default_value = "fiber")]
        password: String,
    },
    /// Start a pipeline run
    Run { pipeline_id: String },
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
        #[arg(long)]
        value: String,
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
        Commands::Login { username, password } => {
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
        Commands::Run { pipeline_id } => {
            let token = resolve_token(token_opt.as_deref())?;
            let client = api_client(&token);
            let resp = api_json(
                client
                    .post(format!("{base}/api/pipelines/{pipeline_id}/runs"))
                    .json(&json!({ "trigger": "manual" })),
            )
            .await?;
            let id = resp["run"]["id"].as_str().unwrap_or("?");
            println!("started run {id}");
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
                } => {
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
    let dir = dirs_token_home();
    std::fs::create_dir_all(&dir)?;
    std::fs::write(token_path(), token)?;
    Ok(())
}

fn load_token() -> Result<String> {
    Ok(std::fs::read_to_string(token_path())?.trim().to_string())
}
