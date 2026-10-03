use anyhow::{Context, Result, ensure};
use clap::{Parser, Subcommand};
use codex_relay::{
    engine::{Engine, public_status},
    mcp,
    model::*,
    safety, store,
};
use serde_json::json;
use std::{
    path::{Path, PathBuf},
    time::Duration,
};

#[derive(Parser)]
#[command(
    version,
    about = "Bounded coding workers for Codex: structured CLI and stdio MCP"
)]
struct Cli {
    /// Config path: flag overrides CODEX_RELAY_CONFIG, then the platform default.
    #[arg(long, global = true, env = "CODEX_RELAY_CONFIG")]
    config: Option<PathBuf>,
    /// Return structured JSON (the default for data commands); help/version stay text.
    #[arg(long, global = true)]
    json: bool,
    #[command(subcommand)]
    command: Commands,
}
#[derive(Subcommand)]
enum Commands {
    /// Create a non-secret configuration with no worker profiles enabled.
    Init {
        #[arg(long)]
        state_dir: Option<PathBuf>,
        #[arg(long)]
        managed_root: PathBuf,
    },
    Doctor,
    Start {
        #[arg(long)]
        task: PathBuf,
    },
    Status {
        #[arg(long)]
        job: Option<String>,
    },
    Result {
        #[arg(long)]
        job: String,
    },
    Correct {
        #[arg(long)]
        job: String,
        #[arg(long)]
        feedback_file: PathBuf,
        #[arg(long)]
        native_load: PathBuf,
    },
    Cancel {
        #[arg(long)]
        job: String,
    },
    Mcp,
    Schemas,
    #[command(name = "__supervise", hide = true)]
    Supervise {
        job: String,
    },
    #[command(name = "__fixture", hide = true)]
    Fixture {
        spec: PathBuf,
    },
    #[command(name = "__fixture-child", hide = true)]
    FixtureChild,
    #[command(name = "__isolation-probe", hide = true)]
    IsolationProbe {
        denied: PathBuf,
        scratch: PathBuf,
    },
    #[command(name = "__detach-probe", hide = true)]
    DetachProbe,
    #[command(name = "__container-probe", hide = true)]
    ContainerProbe {
        denied: PathBuf,
        scratch: PathBuf,
    },
    #[command(name = "__container-detached", hide = true)]
    ContainerDetached {
        marker: PathBuf,
    },
    #[command(name = "__https-proxy", hide = true)]
    HttpsProxy {
        config: PathBuf,
    },
    #[command(name = "__container-worker", hide = true)]
    ContainerWorker {
        #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
        argv: Vec<String>,
    },
}

#[tokio::main]
async fn main() {
    let exit_code = tokio::select! {
        outcome = entry() => match outcome {
            Ok(()) => 0,
            Err(error) => {
                eprintln!("codex-relay: {error}");
                1
            }
        },
        code = shutdown_signal() => {
            eprintln!("codex-relay: interrupted; accepted durable jobs remain available through status/cancel");
            code
        }
    };
    // The losing future has been dropped, so process-tree guards run before exit.
    if exit_code != 0 {
        std::process::exit(exit_code);
    }
}

async fn entry() -> Result<()> {
    let cli = Cli::parse();
    let config_path = cli.config.clone().unwrap_or_else(default_config_path);
    match &cli.command {
        Commands::Init {
            state_dir,
            managed_root,
        } => {
            let path = std::path::absolute(&config_path)?;
            ensure!(
                !path.exists(),
                "configuration already exists; refusing to overwrite it"
            );
            let config = Config {
                schema_version: SCHEMA_VERSION,
                state_dir: state_dir.clone().unwrap_or_else(default_state_path),
                managed_worktree_roots: vec![managed_root.clone()],
                max_external_workers: 1,
                max_total_workers: 3,
                reserved_native_slots: 1,
                allow_simulated_workers: false,
                profiles: Default::default(),
            };
            safety::validate_config(&config)?;
            store::private_dir(path.parent().context("configuration parent missing")?)?;
            store::atomic_json(&path, &config)?;
            println!(
                "{}",
                json!({"config":path,"profiles_enabled":0,"next":"Run doctor; add only reviewed named profiles. Live execution requires verified isolation and separately authorized credentials."})
            );
            return Ok(());
        }
        Commands::Fixture { spec } => return fixture(spec).await,
        Commands::FixtureChild => {
            tokio::time::sleep(Duration::from_secs(60)).await;
            return Ok(());
        }
        Commands::IsolationProbe { denied, scratch } => {
            ensure!(
                std::fs::read(denied).is_err(),
                "sandbox allowed denied read"
            );
            ensure!(
                std::fs::write(denied, b"overwrite").is_err(),
                "sandbox allowed denied write"
            );
            std::fs::write(scratch.join("allowed-write"), b"probe")
                .context("probe scratch write failed")?;
            let detached = safety::command(&std::env::current_exe()?, &["__detach-probe".into()])?
                .status()
                .await
                .context("probe child could not launch")?;
            ensure!(
                detached.success(),
                "sandbox permits process-group escape; external execution refused"
            );
            println!("isolated");
            return Ok(());
        }
        Commands::DetachProbe => {
            // The child is not a group leader, so failure must come from enforced isolation.
            ensure!(
                unsafe { libc::getpid() } != unsafe { libc::getpgrp() },
                "probe cannot establish a non-leader process"
            );
            ensure!(
                unsafe { libc::setsid() } == -1,
                "sandbox permits session detachment"
            );
            ensure!(
                unsafe { libc::setpgid(0, 0) } == -1,
                "sandbox permits process-group detachment"
            );
            return Ok(());
        }
        Commands::ContainerProbe { denied, scratch } => {
            ensure!(
                std::fs::read(denied).is_err(),
                "container exposed the host marker"
            );
            ensure!(
                std::fs::write(denied, b"overwrite").is_err(),
                "container allowed a host write"
            );
            std::fs::write(scratch.join("allowed-write"), b"probe")?;
            let marker = scratch.join("detached.pid");
            let mut command = safety::command(
                &std::env::current_exe()?,
                &[
                    "__container-detached".into(),
                    marker.to_string_lossy().into_owned(),
                ],
            )?;
            command
                .stdin(std::process::Stdio::null())
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null());
            let _child = command.spawn()?;
            tokio::time::timeout(Duration::from_secs(5), async {
                while !marker.is_file() {
                    tokio::time::sleep(Duration::from_millis(20)).await;
                }
            })
            .await
            .context("container detachment probe did not attest")?;
            println!("isolated");
            return Ok(());
        }
        Commands::ContainerDetached { marker } => {
            ensure!(
                unsafe { libc::setsid() } != -1,
                "container probe could not detach"
            );
            std::fs::write(marker, std::process::id().to_string())?;
            tokio::time::sleep(Duration::from_secs(60)).await;
            return Ok(());
        }
        Commands::HttpsProxy { config } => {
            return codex_relay::docker::serve_https_proxy(config).await;
        }
        Commands::ContainerWorker { argv } => {
            let code = codex_relay::docker::run_container_worker(argv).await?;
            if code != 0 {
                std::process::exit(code);
            }
            return Ok(());
        }
        Commands::Schemas => {
            println!(
                "{}",
                serde_json::to_string_pretty(
                    &json!({"task":schemars::schema_for!(Task),"config":schemars::schema_for!(Config),"result":schemars::schema_for!(WorkerResult)})
                )?
            );
            return Ok(());
        }
        _ => {}
    }
    let path = config_path;
    let path = std::fs::canonicalize(path)
        .context("configuration not found; run init or supply --config")?;
    let config: Config = store::read_json(&path, 128 * 1024)?;
    let binary = std::fs::canonicalize(std::env::current_exe()?)?;
    let engine = Engine::open(config, binary)?;
    let value = match cli.command {
        Commands::Doctor => engine.doctor().await?,
        Commands::Start { task } => public_status(
            &engine
                .start(store::read_json(&task, 128 * 1024)?, &path)
                .await?,
        ),
        Commands::Status { job } => match job {
            Some(id) => public_status(&engine.status(&id).await?),
            None => engine.all_status().await?,
        },
        Commands::Result { job } => {
            mcp::call(&engine, &path, "workers_result", json!({"job_id":job})).await?
        }
        Commands::Correct {
            job,
            feedback_file,
            native_load,
        } => {
            ensure!(
                feedback_file.metadata()?.len() <= 8192,
                "feedback file exceeds limit"
            );
            let feedback = std::fs::read_to_string(feedback_file)?;
            public_status(
                &engine
                    .correct(
                        &job,
                        feedback,
                        store::read_json(&native_load, 8192)?,
                        Some(&path),
                    )
                    .await?,
            )
        }
        Commands::Cancel { job } => engine.cancel(&job).await?,
        Commands::Mcp => return mcp::serve(engine, &path).await,
        Commands::Supervise { job } => return engine.run(&job).await,
        _ => unreachable!(),
    };
    println!("{}", serde_json::to_string_pretty(&value)?);
    Ok(())
}

fn default_config_path() -> PathBuf {
    platform_directory("XDG_CONFIG_HOME", ".config").join("codex-relay/config.json")
}

fn default_state_path() -> PathBuf {
    platform_directory("XDG_STATE_HOME", ".local/state").join("codex-relay/state")
}

fn platform_directory(variable: &str, fallback: &str) -> PathBuf {
    let home = std::env::var_os("HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("."));
    if cfg!(target_os = "macos") {
        home.join("Library/Application Support")
    } else {
        std::env::var_os(variable)
            .map(PathBuf::from)
            .filter(|p| p.is_absolute())
            .unwrap_or_else(|| home.join(fallback))
    }
}

async fn shutdown_signal() -> i32 {
    #[cfg(unix)]
    {
        if let Ok(mut terminate) =
            tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
        {
            tokio::select! {
                _ = tokio::signal::ctrl_c() => 130,
                _ = terminate.recv() => 143,
            }
        } else {
            let _ = tokio::signal::ctrl_c().await;
            130
        }
    }
    #[cfg(not(unix))]
    {
        let _ = tokio::signal::ctrl_c().await;
        130
    }
}

/// Trusted fixture: no configurable executable, credentials, network, or shell.
async fn fixture(spec: &Path) -> Result<()> {
    let simulation: Simulation = store::read_json(spec, 128 * 1024)?;
    let root = std::fs::canonicalize(std::env::current_dir()?)?;
    let scratch = std::env::var_os("TMPDIR")
        .map(PathBuf::from)
        .context("fixture scratch missing")?;
    if simulation.descendant {
        let mut cmd = safety::command(&std::env::current_exe()?, &["__fixture-child".into()])?;
        cmd.env("TMPDIR", &scratch)
            // The silent background child closes parent output pipes; RTK then observes
            // the parent exit independently of the child lifetime.
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null());
        let child = cmd.spawn()?;
        std::fs::write(
            scratch.join("descendant.pid"),
            child.id().unwrap_or_default().to_string(),
        )?;
    }
    for (path, text) in &simulation.writes {
        safety::verify_no_symlink(&root, path)?;
        let full = root.join(path);
        if let Some(parent) = full.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::write(full, text)?;
    }
    for _ in 0..simulation.emit_steps {
        println!("{}", json!({"kind":"model_step"}));
    }
    if simulation.malformed_event {
        println!("not-json");
    }
    if simulation.echo == "credential_probe" {
        eprintln!("OPENROUTER_API_KEY=sk-or-v1-SYNTHETIC_DO_NOT_PERSIST_1234567890");
        println!(
            "{}",
            json!({"kind":"untrusted","text":"Bearer SYNTHETIC_DO_NOT_PERSIST_1234567890"})
        );
    }
    if simulation.output_bytes > 0 {
        println!("{}", "x".repeat(simulation.output_bytes));
    }
    println!(
        "{}",
        json!({"kind":"worker_report","claimed_test_ids":simulation.claimed_test_ids,"completed_criteria_ids":["done"]})
    );
    tokio::time::sleep(Duration::from_millis(simulation.delay_ms)).await;
    std::process::exit(simulation.exit_code);
}
