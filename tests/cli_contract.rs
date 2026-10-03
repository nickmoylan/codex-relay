use anyhow::Result;
use codex_relay::safety;
use serde_json::Value;
use std::{
    path::{Path, PathBuf},
    process::Stdio,
    time::Duration,
};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

fn command(args: &[&str], home: &Path) -> Result<tokio::process::Command> {
    let mut command = safety::command(
        Path::new(env!("CARGO_BIN_EXE_codex-relay")),
        &args.iter().map(|s| s.to_string()).collect::<Vec<_>>(),
    )?;
    command
        .env("HOME", home)
        .env("NO_COLOR", "1")
        .current_dir(home);
    Ok(command)
}

#[tokio::test]
async fn root_and_subcommand_help_version_and_usage_follow_conventions() -> Result<()> {
    let temp = tempfile::TempDir::new_in("/tmp")?;
    for args in [vec!["--help"], vec!["-h"], vec!["--version"], vec!["-V"]] {
        let output = command(&args, temp.path())?.output().await?;
        assert!(output.status.success());
        assert!(String::from_utf8_lossy(&output.stdout).contains("codex-relay"));
        assert!(output.stderr.is_empty());
    }
    for subcommand in [
        "init", "doctor", "start", "status", "result", "correct", "cancel", "mcp", "schemas",
    ] {
        let output = command(&[subcommand, "--help"], temp.path())?
            .output()
            .await?;
        assert!(output.status.success(), "{subcommand}");
        assert!(String::from_utf8_lossy(&output.stdout).contains("Usage:"));
    }
    for args in [vec!["--unknown"], vec!["--", "--version"], vec!["start"]] {
        let output = command(&args, temp.path())?.output().await?;
        assert_eq!(output.status.code(), Some(2));
        assert!(output.stdout.is_empty());
        assert!(!output.stderr.is_empty());
    }
    Ok(())
}

async fn init(home: &Path, config: &Path, state: &Path) -> Result<()> {
    let managed = home.join("managed");
    let output = command(
        &[
            "--config",
            config.to_str().unwrap(),
            "init",
            "--state-dir",
            state.to_str().unwrap(),
            "--managed-root",
            managed.to_str().unwrap(),
        ],
        home,
    )?
    .output()
    .await?;
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let value: Value = serde_json::from_slice(&output.stdout)?;
    assert_eq!(value["profiles_enabled"], 0);
    Ok(())
}

#[tokio::test]
async fn init_config_precedence_json_piping_and_path_handling_are_real() -> Result<()> {
    let temp = tempfile::TempDir::new_in("/tmp")?;
    let home = std::fs::canonicalize(temp.path())?;
    let explicit = home.join("config with spaces.json");
    let environment = home.join("environment.json");
    init(&home, &explicit, &home.join("state-a")).await?;
    init(&home, &environment, &home.join("state-b")).await?;
    let mut cmd = command(
        &["doctor", "--json", "--config", explicit.to_str().unwrap()],
        &home,
    )?;
    cmd.env("CODEX_RELAY_CONFIG", &environment);
    let output = cmd.output().await?;
    assert!(output.status.success() && output.stderr.is_empty());
    assert!(!output.stdout.contains(&0x1b));
    let value: Value = serde_json::from_slice(&output.stdout)?;
    assert_eq!(
        value["state_dir"],
        home.join("state-a").to_string_lossy().as_ref()
    );
    assert_eq!(value["isolation_verified"], false);
    let mut cmd = command(&["doctor"], &home)?;
    cmd.env("CODEX_RELAY_CONFIG", &environment);
    let value: Value = serde_json::from_slice(&cmd.output().await?.stdout)?;
    assert_eq!(
        value["state_dir"],
        home.join("state-b").to_string_lossy().as_ref()
    );
    let output = command(
        &[
            "--config=--leading.json",
            "init",
            "--state-dir",
            home.join("state-c").to_str().unwrap(),
            "--managed-root",
            home.join("managed").to_str().unwrap(),
        ],
        &home,
    )?
    .output()
    .await?;
    assert!(output.status.success());
    assert!(home.join("--leading.json").is_file());
    let output = command(
        &[
            "--config",
            explicit.to_str().unwrap(),
            "init",
            "--managed-root",
            home.join("managed").to_str().unwrap(),
        ],
        &home,
    )?
    .output()
    .await?;
    assert_eq!(output.status.code(), Some(1));
    assert!(output.stdout.is_empty());
    assert!(String::from_utf8_lossy(&output.stderr).contains("refusing to overwrite"));
    let output = command(&["schemas", "--json"], &home)?.output().await?;
    let value: Value = serde_json::from_slice(&output.stdout)?;
    assert!(value.get("task").is_some());
    Ok(())
}

#[tokio::test]
async fn default_locations_are_scoped_and_missing_config_has_actionable_error() -> Result<()> {
    let temp = tempfile::TempDir::new_in("/tmp")?;
    let home = std::fs::canonicalize(temp.path())?;
    let output = command(&["doctor", "--json"], &home)?.output().await?;
    assert_eq!(output.status.code(), Some(1));
    assert!(output.stdout.is_empty());
    assert!(String::from_utf8_lossy(&output.stderr).contains("run init or supply --config"));
    let output = command(
        &[
            "init",
            "--managed-root",
            home.join("managed").to_str().unwrap(),
        ],
        &home,
    )?
    .output()
    .await?;
    assert!(output.status.success());
    let default = if cfg!(target_os = "macos") {
        home.join("Library/Application Support/codex-relay/config.json")
    } else {
        home.join(".config/codex-relay/config.json")
    };
    assert!(default.is_file());
    let value: Value = serde_json::from_slice(
        &command(&["doctor", "--json"], &home)?
            .output()
            .await?
            .stdout,
    )?;
    assert!(value["profiles"].as_array().unwrap().is_empty());
    Ok(())
}

#[tokio::test]
async fn mcp_interrupts_keep_stdout_protocol_only_and_exit_by_signal() -> Result<()> {
    for (signal, expected) in [(libc::SIGINT, 130), (libc::SIGTERM, 143)] {
        let temp = tempfile::TempDir::new_in("/tmp")?;
        let home = std::fs::canonicalize(temp.path())?;
        let config: PathBuf = home.join("config.json");
        init(&home, &config, &home.join("state")).await?;
        let mut cmd = command(&["--config", config.to_str().unwrap(), "mcp"], &home)?;
        cmd.stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .process_group(0)
            .kill_on_drop(true);
        let mut child = cmd.spawn()?;
        let mut stdin = child.stdin.take().unwrap();
        stdin
            .write_all(b"{\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"initialize\",\"params\":{}}\n")
            .await?;
        stdin.flush().await?;
        let mut stdout = BufReader::new(child.stdout.take().unwrap());
        let mut line = String::new();
        tokio::time::timeout(Duration::from_secs(5), stdout.read_line(&mut line)).await??;
        let response: Value = serde_json::from_str(&line)?;
        assert_eq!(response["id"], 1);
        stdin.write_all(b"{\"jsonrpc\":\"2.0\",\"method\":\"notifications/initialized\"}\n{\"jsonrpc\":\"2.0\",\"id\":2,\"method\":\"tools/call\",\"params\":{\"name\":\"workers_doctor\",\"arguments\":{}}}\n").await?;
        stdin.flush().await?;
        line.clear();
        tokio::time::timeout(Duration::from_secs(5), stdout.read_line(&mut line)).await??;
        let response: Value = serde_json::from_str(&line)?;
        let cli_pid = response["result"]["structuredContent"]["process_id"]
            .as_i64()
            .expect("doctor identifies the actual Relay process") as i32;
        assert_eq!(unsafe { libc::kill(cli_pid, signal) }, 0);
        let status = tokio::time::timeout(Duration::from_secs(5), child.wait()).await??;
        assert_eq!(status.code(), Some(expected));
        line.clear();
        assert_eq!(stdout.read_line(&mut line).await?, 0);
    }
    Ok(())
}

#[tokio::test]
async fn container_wrapper_accepts_clap_normalized_program_argv() -> Result<()> {
    let temp = tempfile::TempDir::new_in("/tmp")?;
    for args in [
        vec!["__container-worker", "--", "/usr/bin/true"],
        vec!["__container-worker", "/usr/bin/true"],
    ] {
        let output = command(&args, temp.path())?.output().await?;
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert!(output.stdout.is_empty());
    }
    Ok(())
}

async fn rpc_frame(
    input: &mut tokio::process::ChildStdin,
    output: &mut BufReader<tokio::process::ChildStdout>,
    value: Value,
) -> Result<Value> {
    input
        .write_all(serde_json::to_string(&value)?.as_bytes())
        .await?;
    input.write_all(b"\n").await?;
    input.flush().await?;
    let mut line = String::new();
    tokio::time::timeout(Duration::from_secs(5), output.read_line(&mut line)).await??;
    Ok(serde_json::from_str(&line)?)
}

#[tokio::test]
async fn mcp_rejects_invalid_requests_and_preserves_notification_semantics() -> Result<()> {
    use serde_json::json;
    let temp = tempfile::TempDir::new_in("/tmp")?;
    let home = std::fs::canonicalize(temp.path())?;
    let config = home.join("config.json");
    init(&home, &config, &home.join("state")).await?;
    let mut cmd = command(&["--config", config.to_str().unwrap(), "mcp"], &home)?;
    cmd.stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    let mut child = cmd.spawn()?;
    let mut input = child.stdin.take().unwrap();
    let mut output = BufReader::new(child.stdout.take().unwrap());
    for value in [
        json!(42),
        json!(null),
        json!("text"),
        json!([]),
        json!({}),
        json!({"jsonrpc":"1.0","method":"ping"}),
        json!({"jsonrpc":"2.0","method":42}),
        json!({"jsonrpc":"2.0","method":"ping","id":true}),
        json!({"jsonrpc":"2.0","method":"ping","id":null}),
        json!({"jsonrpc":"2.0","method":"ping","id":1.5}),
        json!({"jsonrpc":"2.0","method":"initialize","id":null}),
        json!({"jsonrpc":"2.0","method":"initialize","id":-1.5}),
        json!({"jsonrpc":"2.0","method":"ping","id":[]}),
        json!({"jsonrpc":"2.0","method":"ping","id":{}}),
    ] {
        let response = rpc_frame(&mut input, &mut output, value).await?;
        assert_eq!(response["error"]["code"], -32600);
        assert_eq!(response["id"], Value::Null);
    }
    for params in [json!(42), json!("text"), json!(null), json!([])] {
        let response = rpc_frame(
            &mut input,
            &mut output,
            json!({"jsonrpc":"2.0","method":"initialize","id":"bad-params","params":params}),
        )
        .await?;
        assert_eq!(response["error"]["code"], -32602);
        assert_eq!(response["id"], "bad-params");
    }
    // Invalid notification parameters produce no response or state transition.
    input
        .write_all(b"{\"jsonrpc\":\"2.0\",\"method\":\"initialize\",\"params\":42}\n")
        .await?;
    let response = rpc_frame(
        &mut input,
        &mut output,
        json!({"jsonrpc":"2.0","method":"tools/list","id":1}),
    )
    .await?;
    assert_eq!(response["id"], 1);
    assert_eq!(response["error"]["code"], -32002);
    let response = rpc_frame(
        &mut input,
        &mut output,
        json!({"jsonrpc":"2.0","method":"initialize","id":2,"params":{}}),
    )
    .await?;
    assert!(response.get("result").is_some());
    input
        .write_all(
            b"{\"jsonrpc\":\"2.0\",\"method\":\"notifications/initialized\",\"params\":[]}\n",
        )
        .await?;
    let response = rpc_frame(
        &mut input,
        &mut output,
        json!({"jsonrpc":"2.0","method":"tools/list","id":3}),
    )
    .await?;
    assert_eq!(response["id"], 3);
    assert_eq!(response["error"]["code"], -32002);
    input.write_all(b"{\"jsonrpc\":\"2.0\",\"method\":\"notifications/initialized\"}\n{\"jsonrpc\":\"2.0\",\"method\":\"unknown-notification\"}\n").await?;
    for id in [json!("valid"), json!(7), json!(-7), json!(u64::MAX)] {
        let response = rpc_frame(
            &mut input,
            &mut output,
            json!({"jsonrpc":"2.0","method":"ping","id":id}),
        )
        .await?;
        assert_eq!(response["id"], id);
        assert_eq!(response["result"], json!({}));
    }
    // Invalid IDs remain rejected after initialization; they never become notifications.
    for id in [json!(null), json!(1.5)] {
        let response = rpc_frame(
            &mut input,
            &mut output,
            json!({"jsonrpc":"2.0","method":"ping","id":id}),
        )
        .await?;
        assert_eq!(response["error"]["code"], -32600);
        assert_eq!(response["id"], Value::Null);
        assert!(response.get("result").is_none());
    }
    let response = rpc_frame(
        &mut input,
        &mut output,
        json!({"jsonrpc":"2.0","method":"tools/list","id":8}),
    )
    .await?;
    assert_eq!(response["result"]["tools"].as_array().unwrap().len(), 6);
    input.write_all(b"{invalid json}\n").await?;
    input.flush().await?;
    let mut line = String::new();
    tokio::time::timeout(Duration::from_secs(5), output.read_line(&mut line)).await??;
    assert_eq!(
        serde_json::from_str::<Value>(&line)?["error"]["code"],
        -32700
    );
    drop(input);
    assert!(
        tokio::time::timeout(Duration::from_secs(5), child.wait())
            .await??
            .success()
    );
    Ok(())
}
