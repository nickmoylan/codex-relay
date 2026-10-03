use anyhow::Result;
use codex_relay::{docker, engine::Engine, model::*, safety};
use sha2::{Digest, Sha256};
use std::{
    collections::BTreeMap,
    path::{Path, PathBuf},
};

fn runtime() -> DockerRuntime {
    DockerRuntime {
        image: format!("sha256:{}", "a".repeat(64)),
        memory_mb: 256,
        pids_limit: 64,
        cpus: 1,
    }
}
fn profile() -> Profile {
    Profile {
        container: Some(runtime()),
        enabled: true,
        harness: Harness::Simulated,
        executable: "/usr/local/bin/codex-relay".into(),
        provider: "simulation".into(),
        model: "fixture".into(),
        reasoning: None,
        tools: vec!["file_read".into(), "file_edit".into()],
        capabilities: Capabilities {
            code_editing: true,
            reasoning: false,
            tool_use: false,
        },
        privacy: Privacy {
            confidential_code: false,
            data_collection: "deny".into(),
            zero_data_retention: false,
            allowed_providers: vec![],
            allow_fallbacks: false,
        },
        budget: Budget {
            timeout_seconds: 20,
            max_model_steps: 5,
            max_output_bytes: 8192,
            max_change_bytes: 1024 * 1024,
            max_cost_usd: None,
        },
        credential_env: vec![],
        api_base: None,
        network_hosts: vec![],
        runtime_read_roots: vec![],
    }
}
fn stamp(text: &str) -> FileStamp {
    FileStamp {
        sha256: format!("{:x}", Sha256::digest(text)),
        bytes: text.len() as u64,
        tracked: true,
        executable: false,
    }
}
fn fake_job(workspace: PathBuf) -> Job {
    Job {
        schema_version: 1,
        id: uuid::Uuid::new_v4().to_string(),
        task: Task {
            schema_version: 1,
            profile: "fixture".into(),
            workspace: Workspace {
                path: workspace.clone(),
                artifact_identity: workspace.to_string_lossy().into_owned(),
                base_revision: "a".repeat(40),
                attested_at_ms: now_ms(),
            },
            objective: "Synthetic fixture only".into(),
            writable_paths: vec!["src".into()],
            read_paths: vec!["reference.txt".into()],
            acceptance_criteria: BTreeMap::from([("done".into(), "Fixture change".into())]),
            tests: vec![],
            native_load: NativeLoad {
                active_workers: 1,
                observed_at_ms: now_ms(),
                source: "coordinator_attested".into(),
            },
            simulator: Some(Simulation::default()),
        },
        profile: profile(),
        repository_identity: workspace,
        status: JobStatus::Running,
        created_at_ms: now_ms(),
        updated_at_ms: now_ms(),
        attempt: 0,
        supervisor_pid: None,
        worker: None,
        docker_endpoint: None,
        container: None,
        broker: None,
        recovery_required: false,
        import_pending: false,
        baseline: BTreeMap::from([
            ("src/file.txt".into(), stamp("before\n")),
            ("reference.txt".into(), stamp("reference\n")),
            ("other.txt".into(), stamp("excluded\n")),
        ]),
        last_snapshot: BTreeMap::new(),
        result: None,
        correction: None,
    }
}

#[test]
fn immutable_images_and_bounded_profiles_fail_closed() -> Result<()> {
    let mut p = profile();
    safety::validate_profile(&p)?;
    for image in [
        "example:latest",
        "example:fixed",
        "sha256:abcd",
        "example@sha256:bad",
        "-option@sha256:0000000000000000000000000000000000000000000000000000000000000000",
    ] {
        p.container.as_mut().unwrap().image = image.into();
        assert!(safety::validate_profile(&p).is_err());
    }
    p = profile();
    p.container.as_mut().unwrap().pids_limit = 0;
    assert!(safety::validate_profile(&p).is_err());
    p = profile();
    p.runtime_read_roots = vec!["/opt/homebrew/Cellar".into()];
    assert!(safety::validate_profile(&p).is_err());
    p = profile();
    p.executable = "/bin/sh".into();
    assert!(safety::validate_profile(&p).is_err());
    p = profile();
    p.credential_env = vec!["OPENAI_API_KEY".into()];
    assert!(safety::validate_profile(&p).is_err());
    Ok(())
}

#[test]
fn docker_arguments_cannot_mount_owner_data_or_grant_host_network() -> Result<()> {
    let id = docker::identity(
        Path::new("/tmp/disposable/state"),
        &uuid::Uuid::new_v4().to_string(),
        0,
        "worker",
        "unix:///tmp/local.sock",
    )?;
    let args = docker::create_args(
        &id,
        &runtime(),
        Path::new("/tmp/disposable/work"),
        Path::new("/tmp/disposable/control"),
        Path::new("/tmp/disposable/scratch"),
        Path::new("/usr/local/bin/codex-relay"),
        &["literal $(touch /tmp/nope); text".into()],
    )?;
    for flag in [
        "--network=none",
        "--read-only",
        "--cap-drop=ALL",
        "--security-opt=no-new-privileges:true",
        "--pull=never",
        "--log-driver=none",
        "--restart=no",
    ] {
        assert!(args.iter().any(|s| s == flag));
    }
    assert!(!args.iter().any(|s| s.contains("docker.sock")
        || s == "--privileged"
        || s.contains("network=host")
        || s.contains("pid=host")));
    assert_eq!(args.last().unwrap(), "literal $(touch /tmp/nope); text");
    assert_eq!(args.iter().filter(|s| s.as_str() == "--mount").count(), 3);
    assert!(
        args.iter()
            .any(|s| s == "type=bind,src=/tmp/disposable/control,dst=/relay/control,readonly")
    );
    Ok(())
}

#[test]
fn sanitized_export_and_copy_validation_quarantine_escape_and_secrets() -> Result<()> {
    let tmp = tempfile::tempdir_in("/tmp")?;
    let root = std::fs::canonicalize(tmp.path())?;
    let host = root.join("host");
    std::fs::create_dir(&host)?;
    std::fs::create_dir(host.join("src"))?;
    for (path, text) in [
        ("src/file.txt", "before\n"),
        ("reference.txt", "reference\n"),
        ("other.txt", "excluded\n"),
        (".env", "never exported\n"),
    ] {
        std::fs::write(host.join(path), text)?;
    }
    std::fs::create_dir(host.join(".git"))?;
    std::fs::write(host.join(".git/config"), "owner metadata")?;
    let mut job = fake_job(host.clone());
    job.last_snapshot = job.baseline.clone();
    let work = root.join("copy");
    let before = docker::export(&job, &work)?;
    assert!(
        !work.join(".git").exists()
            && !work.join(".env").exists()
            && !work.join("other.txt").exists()
    );
    assert_eq!(before.len(), 2);
    std::fs::write(work.join("src/file.txt"), "after\n")?;
    let after = docker::inventory(&work, &before)?;
    let changes = docker::validate_changes(&job, &before, &after)?;
    assert_eq!(
        std::fs::read_to_string(host.join("src/file.txt"))?,
        "before\n"
    );
    docker::import(&job, &work, &changes)?;
    assert_eq!(
        std::fs::read_to_string(host.join("src/file.txt"))?,
        "after\n"
    );
    std::fs::write(work.join("reference.txt"), "forbidden")?;
    assert!(docker::validate_changes(&job, &before, &docker::inventory(&work, &before)?).is_err());
    std::fs::write(work.join("reference.txt"), "reference\n")?;
    std::fs::write(
        work.join("src/new.txt"),
        "sk-abcdefghijklmnopqrstuvwxyz123456",
    )?;
    assert!(docker::inventory(&work, &before).is_err());
    std::fs::remove_file(work.join("src/new.txt"))?;
    std::os::unix::fs::symlink(host.join("src/file.txt"), work.join("src/link"))?;
    assert!(docker::inventory(&work, &before).is_err());
    std::fs::remove_file(work.join("src/link"))?;
    std::fs::hard_link(work.join("src/file.txt"), work.join("src/link"))?;
    assert!(docker::inventory(&work, &before).is_err());
    Ok(())
}

#[test]
fn proxy_rejects_private_reserved_transition_and_unapproved_targets() -> Result<()> {
    for addr in [
        "127.0.0.1",
        "0.1.2.3",
        "10.2.3.4",
        "172.16.1.2",
        "192.168.1.2",
        "100.64.0.1",
        "169.254.169.254",
        "192.0.2.1",
        "198.18.0.1",
        "198.51.100.1",
        "203.0.113.1",
        "224.0.0.1",
        "255.255.255.255",
        "::1",
        "::",
        "fc00::1",
        "fe80::1",
        "::ffff:127.0.0.1",
        "2001:db8::1",
        "2002:7f00:1::1",
    ] {
        assert!(!docker::public_ip(addr.parse()?));
    }
    for addr in ["1.1.1.1", "8.8.8.8", "2606:4700:4700::1111"] {
        assert!(docker::public_ip(addr.parse()?));
    }
    let config = docker::ProxyConfig {
        hosts: BTreeMap::from([("example.com".into(), vec!["1.1.1.1:443".parse()?])]),
        timeout_seconds: 10,
    };
    config.validate()?;
    assert_eq!(
        docker::connect_host(
            b"CONNECT example.com:443 HTTP/1.1\r\nHost: example.com:443\r\n\r\n",
            &config
        )?,
        "example.com"
    );
    for header in [
        "CONNECT localhost:443 HTTP/1.1\r\n\r\n",
        "CONNECT example.com:80 HTTP/1.1\r\n\r\n",
        "GET https://example.com HTTP/1.1\r\n\r\n",
        "CONNECT example.com:443 HTTP/1.1\r\nHost: owner.local:443\r\n\r\n",
        "CONNECT example.com:443 HTTP/1.1\r\nContent-Length: 12\r\n\r\n",
    ] {
        assert!(docker::connect_host(header.as_bytes(), &config).is_err());
    }
    Ok(())
}

fn client_hello(host: &str, ech: bool) -> Vec<u8> {
    let mut names = vec![0];
    names.extend((host.len() as u16).to_be_bytes());
    names.extend(host.as_bytes());
    let mut sni = Vec::new();
    sni.extend((names.len() as u16).to_be_bytes());
    sni.extend(names);
    let mut ext = vec![0, 0];
    ext.extend((sni.len() as u16).to_be_bytes());
    ext.extend(sni);
    if ech {
        ext.extend([0xfe, 0x0d, 0, 0]);
    }
    let mut payload = vec![3, 3];
    payload.extend([0; 32]);
    payload.extend([0, 0, 2, 0x13, 1, 1, 0]);
    payload.extend((ext.len() as u16).to_be_bytes());
    payload.extend(ext);
    let mut handshake = vec![1, 0, (payload.len() >> 8) as u8, payload.len() as u8];
    handshake.extend(payload);
    let mut record = vec![22, 3, 1];
    record.extend((handshake.len() as u16).to_be_bytes());
    record.extend(handshake);
    record
}
#[test]
fn tls_hello_requires_exact_visible_hostname_and_bounded_framing() -> Result<()> {
    assert_eq!(
        docker::tls_server_name(&client_hello("example.com", false))?,
        "example.com"
    );
    assert!(docker::tls_server_name(&client_hello("example.com", true)).is_err());
    let valid = client_hello("example.com", false);
    for n in 0..valid.len() {
        assert!(docker::tls_server_name(&valid[..n]).is_err());
    }
    assert!(docker::tls_server_name(b"GET / HTTP/1.1\r\n").is_err());
    Ok(())
}

async fn fixture() -> Result<(tempfile::TempDir, Engine, Task)> {
    let tmp = tempfile::tempdir_in("/tmp")?;
    let root = std::fs::canonicalize(tmp.path())?;
    let repo = root.join("repo");
    std::fs::create_dir(&repo)?;
    safety::git(&repo, &["init", "-q"]).await?;
    std::fs::create_dir(repo.join("src"))?;
    std::fs::write(repo.join("src/file.txt"), "before\n")?;
    safety::git(&repo, &["add", "src"]).await?;
    safety::git(
        &repo,
        &[
            "-c",
            "user.name=Fixture",
            "-c",
            "user.email=fixture@example.invalid",
            "-c",
            "commit.gpgsign=false",
            "commit",
            "-qm",
            "test: disposable container fixture",
        ],
    )
    .await?;
    let managed = root.join("managed");
    std::fs::create_dir(&managed)?;
    let work = managed.join("worker");
    safety::git(
        &repo,
        &[
            "worktree",
            "add",
            "--detach",
            work.to_str().unwrap(),
            "HEAD",
        ],
    )
    .await?;
    let mut profile = profile();
    profile.container.as_mut().unwrap().image =
        std::env::var("RELAY_TEST_IMAGE").map_err(|_| {
            anyhow::anyhow!(
                "RELAY_TEST_IMAGE must name the reviewed locally built immutable fixture image"
            )
        })?;
    let config = Config {
        schema_version: 1,
        state_dir: root.join("state"),
        managed_worktree_roots: vec![managed],
        max_external_workers: 1,
        max_total_workers: 3,
        reserved_native_slots: 1,
        allow_simulated_workers: true,
        profiles: BTreeMap::from([("fixture".into(), profile)]),
    };
    let engine = Engine::open(
        config,
        std::fs::canonicalize(env!("CARGO_BIN_EXE_codex-relay"))?,
    )?;
    let mut task = fake_job(work.clone()).task;
    task.read_paths = vec![];
    task.workspace.base_revision =
        String::from_utf8(safety::git(&work, &["rev-parse", "HEAD"]).await?)?
            .trim()
            .into();
    Ok((tmp, engine, task))
}

#[tokio::test]
#[ignore = "requires explicitly authorized local Docker daemon and reviewed immutable Linux fixture image; no provider"]
async fn real_container_copy_verification_quarantine_and_whole_tree_cleanup() -> Result<()> {
    let (_tmp, engine, mut task) = fixture().await?;
    task.simulator
        .as_mut()
        .unwrap()
        .writes
        .insert("src/file.txt".into(), "after\n".into());
    task.simulator.as_mut().unwrap().descendant = true;
    task.tests.push(TestCommand {
        id: "verified".into(),
        program: "/usr/bin/true".into(),
        args: vec![],
        timeout_seconds: 3,
    });
    let job = engine.prepare(task.clone()).await?;
    engine.run(&job.id).await?;
    let finished = engine.status(&job.id).await?;
    assert_eq!(finished.status, JobStatus::Completed);
    assert!(
        finished.container.is_none() && finished.broker.is_none() && !finished.recovery_required
    );
    assert_eq!(
        std::fs::read_to_string(task.workspace.path.join("src/file.txt"))?,
        "after\n"
    );
    assert_eq!(
        finished.result.unwrap().independently_observed_tests[0].outcome,
        "passed"
    );
    // Restore the disposable fixture, then prove out-of-scope edits never touch the host worktree.
    std::fs::write(task.workspace.path.join("src/file.txt"), "before\n")?;
    task.simulator.as_mut().unwrap().writes =
        BTreeMap::from([("outside.txt".into(), "quarantined".into())]);
    let job = engine.prepare(task.clone()).await?;
    engine.run(&job.id).await?;
    let finished = engine.status(&job.id).await?;
    assert_eq!(finished.status, JobStatus::ScopeViolation);
    assert!(!task.workspace.path.join("outside.txt").exists());
    // Isolation preflight itself requires a real setsid child and attested whole-container removal.
    engine
        .isolation_probe(&task, &engine.config.profiles["fixture"])
        .await?;
    Ok(())
}

#[tokio::test]
#[ignore = "requires authorized local Docker daemon and immutable synthetic image; no provider or credentials"]
async fn real_container_cancel_timeout_orphan_and_import_reservation() -> Result<()> {
    for action in ["cancel", "timeout", "orphan"] {
        let (_tmp, mut engine, mut task) = fixture().await?;
        engine
            .config
            .profiles
            .get_mut("fixture")
            .unwrap()
            .budget
            .timeout_seconds = 3;
        task.simulator.as_mut().unwrap().delay_ms = 10_000;
        task.simulator.as_mut().unwrap().descendant = true;
        let job = engine.prepare(task).await?;
        let runner = engine.clone();
        let id = job.id.clone();
        let running = tokio::spawn(async move { runner.run(&id).await });
        tokio::time::timeout(std::time::Duration::from_secs(10), async {
            loop {
                if engine
                    .store
                    .load(&job.id)
                    .unwrap()
                    .container
                    .as_ref()
                    .is_some_and(|c| c.id.is_some())
                {
                    break;
                }
                tokio::time::sleep(std::time::Duration::from_millis(20)).await;
            }
        })
        .await?;
        let identity = engine.store.load(&job.id)?.container.unwrap();
        if action == "cancel" {
            engine.cancel(&job.id).await?;
        }
        if action == "orphan" {
            running.abort();
            let _ = running.await;
            let mut stored = engine.store.load(&job.id)?;
            stored.updated_at_ms = now_ms() - 6000;
            engine.store.save(&stored)?;
        } else {
            running.await??;
        }
        let finished = engine.status(&job.id).await?;
        assert_eq!(
            finished.status,
            match action {
                "cancel" => JobStatus::Cancelled,
                "timeout" => JobStatus::TimedOut,
                _ => JobStatus::Interrupted,
            }
        );
        assert!(finished.container.is_none() && !finished.recovery_required);
        // Cleanup is idempotent and only acts on ownership-verified immutable IDs.
        docker::Client::local(&engine.store.root)?
            .cleanup(&identity)
            .await?;
    }
    let (_tmp, engine, task) = fixture().await?;
    let job = engine.prepare(task).await?;
    let mut stored = engine.store.load(&job.id)?;
    stored.status = JobStatus::Running;
    stored.import_pending = true;
    stored.recovery_required = true;
    stored.updated_at_ms = now_ms() - 6000;
    engine.store.save(&stored)?;
    let recovered = engine.status(&job.id).await?;
    assert_eq!(recovered.status, JobStatus::Interrupted);
    assert!(recovered.recovery_required && recovered.import_pending);
    assert!(
        engine
            .correct(
                &job.id,
                "Review interrupted import".into(),
                recovered.task.native_load,
                None
            )
            .await
            .is_err()
    );
    Ok(())
}

#[tokio::test]
#[ignore = "requires authorized local Docker daemon and immutable synthetic image; fixed true fixture starts the broker without a model call"]
async fn real_broker_enabled_worker_lifecycle_uses_network_none() -> Result<()> {
    let (_tmp, mut engine, mut task) = fixture().await?;
    let p = engine.config.profiles.get_mut("fixture").unwrap();
    p.harness = Harness::Aider;
    p.executable = "/usr/bin/true".into();
    p.provider = "synthetic".into();
    p.budget.max_model_steps = 1;
    p.network_hosts = vec!["example.com".into()];
    task.simulator = None;
    task.writable_paths = vec!["src/file.txt".into()];
    task.tests = vec![TestCommand {
        id: "no_credentials".into(),
        program: "/usr/bin/true".into(),
        args: vec![],
        timeout_seconds: 3,
    }];
    let job = engine.prepare(task.clone()).await?;
    engine.run(&job.id).await?;
    let finished = engine.status(&job.id).await?;
    assert_eq!(finished.status, JobStatus::Completed);
    assert!(
        finished.container.is_none() && finished.broker.is_none() && !finished.recovery_required
    );
    assert_eq!(
        std::fs::read_to_string(task.workspace.path.join("src/file.txt"))?,
        "before\n"
    );
    Ok(())
}

#[tokio::test]
#[ignore = "requires authorized local Docker daemon, immutable synthetic image with curl, and public example.com HTTPS; no private payload or provider"]
async fn real_https_connect_sni_allowlist_and_direct_network_denial() -> Result<()> {
    let (_tmp, engine, _task) = fixture().await?;
    let client = docker::Client::local(&engine.store.root)?;
    let runtime = engine.config.profiles["fixture"]
        .container
        .as_ref()
        .unwrap();
    client.available(&runtime.image).await?;
    let dir = engine.store.root.join("synthetic-proxy");
    codex_relay::store::private_dir(&dir)?;
    for name in ["work", "broker-control", "control", "runtime", "ipc"] {
        codex_relay::store::private_dir(&dir.join(name))?;
    }
    let mut p = profile();
    p.network_hosts = vec!["example.com".into()];
    p.budget.timeout_seconds = 120;
    let config = docker::proxy_config(&p).await?;
    codex_relay::store::atomic_json(&dir.join("broker-control/proxy.json"), &config)?;
    let id = uuid::Uuid::new_v4().to_string();
    let mut broker = docker::identity(&engine.store.root, &id, 0, "broker", &client.endpoint)?;
    codex_relay::store::atomic_json(&dir.join("broker-identity.json"), &broker)?;
    client
        .create(
            &mut broker,
            runtime,
            &dir.join("work"),
            &dir.join("broker-control"),
            &dir.join("runtime"),
            Path::new("/usr/local/bin/codex-relay"),
            &["__https-proxy".into(), "/relay/control/proxy.json".into()],
            Some(&dir.join("ipc")),
            true,
            &[],
        )
        .await?;
    codex_relay::store::atomic_json(&dir.join("broker-identity.json"), &broker)?;
    let mut broker_guard = docker::Guard::new(client.clone(), broker.clone());
    let mut start = client.command(&["container".into(), "start".into(), broker.id.unwrap()])?;
    start
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null());
    assert!(start.status().await?.success());
    tokio::time::timeout(std::time::Duration::from_secs(5), async {
        loop {
            if dir.join("ipc/proxy.sock").exists() {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
    })
    .await?;
    for (stage, url, direct, success) in [
        ("allowed", "https://example.com/", false, true),
        ("unlisted", "https://www.example.com/", false, false),
        ("private", "https://169.254.169.254/", false, false),
        ("direct", "https://example.com/", true, false),
    ] {
        let mut worker = docker::identity(&engine.store.root, &id, 0, stage, &client.endpoint)?;
        let mut args = vec![
            "--silent".into(),
            "--fail".into(),
            "--max-time".into(),
            "10".into(),
        ];
        if direct {
            args.extend(["--noproxy".into(), "*".into()]);
        }
        args.push(url.into());
        codex_relay::store::atomic_json(&dir.join("worker-identity.json"), &worker)?;
        client
            .create(
                &mut worker,
                runtime,
                &dir.join("work"),
                &dir.join("control"),
                &dir.join("runtime"),
                Path::new("/usr/bin/curl"),
                &args,
                Some(&dir.join("ipc")),
                false,
                &[],
            )
            .await?;
        codex_relay::store::atomic_json(&dir.join("worker-identity.json"), &worker)?;
        let mut guard = docker::Guard::new(client.clone(), worker.clone());
        let mut attach = client.attach(&worker)?;
        attach
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null());
        let status =
            tokio::time::timeout(std::time::Duration::from_secs(15), attach.status()).await??;
        guard.cleanup().await?;
        assert_eq!(status.success(), success, "synthetic HTTPS case {stage}");
    }
    broker_guard.cleanup().await?;
    Ok(())
}

#[test]
fn explicit_docker_endpoint_rejects_nonunix_relative_and_regular_files() -> Result<()> {
    let tmp = tempfile::tempdir_in("/tmp")?;
    let root = std::fs::canonicalize(tmp.path())?;
    for endpoint in [
        "tcp://127.0.0.1:2375",
        "ssh://owner-host",
        "unix://relative",
        "unix:///tmp/absent-relay-test.sock",
    ] {
        assert!(docker::Client::at(&root.join("state"), endpoint).is_err());
    }
    let file = root.join("not-a-socket");
    std::fs::write(&file, "synthetic")?;
    assert!(
        docker::Client::at(&root.join("state"), &format!("unix://{}", file.display())).is_err()
    );
    Ok(())
}

#[tokio::test]
async fn ignored_read_references_are_exported_and_readonly_mode_changes_rejected() -> Result<()> {
    use std::os::unix::fs::PermissionsExt;
    let tmp = tempfile::tempdir_in("/tmp")?;
    let host = std::fs::canonicalize(tmp.path())?;
    safety::git(&host, &["init", "-q"]).await?;
    std::fs::create_dir(host.join("src"))?;
    std::fs::write(host.join("src/file.txt"), "before\n")?;
    std::fs::write(host.join(".gitignore"), "*.ignored\n")?;
    safety::git(&host, &["add", "src", ".gitignore"]).await?;
    std::fs::write(host.join("reference.ignored"), "read reference\n")?;
    std::fs::write(host.join("unrelated.ignored"), "do not export\n")?;
    let mut job = fake_job(host.clone());
    job.task.read_paths = vec!["reference.ignored".into()];
    job.baseline = safety::snapshot(
        &host,
        1024 * 1024,
        &job.task.writable_paths,
        &job.task.read_paths,
    )
    .await?;
    job.last_snapshot = job.baseline.clone();
    assert!(job.baseline.contains_key(Path::new("reference.ignored")));
    assert!(!job.baseline.contains_key(Path::new("unrelated.ignored")));
    let work = host.join("sanitized-copy");
    let before = docker::export(&job, &work)?;
    assert_eq!(
        std::fs::read_to_string(work.join("reference.ignored"))?,
        "read reference\n"
    );
    assert!(!work.join("unrelated.ignored").exists());
    std::fs::set_permissions(
        work.join("reference.ignored"),
        std::fs::Permissions::from_mode(0o755),
    )?;
    let after = docker::inventory(&work, &before)?;
    let changed = safety::changes(&before, &after, &job.task.writable_paths);
    assert_eq!(changed.len(), 1);
    assert_eq!(changed[0].kind, "mode_changed");
    assert!(!changed[0].in_scope);
    assert!(docker::validate_changes(&job, &before, &after).is_err());
    std::fs::set_permissions(
        host.join("reference.ignored"),
        std::fs::Permissions::from_mode(0o755),
    )?;
    assert_ne!(
        safety::snapshot(
            &host,
            1024 * 1024,
            &job.task.writable_paths,
            &job.task.read_paths
        )
        .await?,
        job.last_snapshot
    );
    Ok(())
}

#[test]
fn executable_mode_only_changes_are_validated_preserved_and_host_conflicts_rejected() -> Result<()>
{
    use std::os::unix::fs::PermissionsExt;
    let tmp = tempfile::tempdir_in("/tmp")?;
    let root = std::fs::canonicalize(tmp.path())?;
    let host = root.join("host");
    std::fs::create_dir_all(host.join("src"))?;
    std::fs::write(host.join("src/file.txt"), "before\n")?;
    std::fs::set_permissions(
        host.join("src/file.txt"),
        std::fs::Permissions::from_mode(0o755),
    )?;
    let mut job = fake_job(host.clone());
    job.baseline.retain(|p, _| p == Path::new("src/file.txt"));
    job.baseline
        .get_mut(Path::new("src/file.txt"))
        .unwrap()
        .executable = true;
    job.last_snapshot = job.baseline.clone();
    let work = root.join("copy");
    let before = docker::export(&job, &work)?;
    assert!(safety::executable(&std::fs::metadata(
        work.join("src/file.txt")
    )?));
    std::fs::set_permissions(
        work.join("src/file.txt"),
        std::fs::Permissions::from_mode(0o644),
    )?;
    let after = docker::inventory(&work, &before)?;
    let changes = docker::validate_changes(&job, &before, &after)?;
    assert_eq!(changes.len(), 1);
    assert_eq!(changes[0].kind, "mode_changed");
    assert_eq!(changes[0].before_executable, Some(true));
    assert_eq!(changes[0].after_executable, Some(false));
    assert_eq!(changes[0].before_sha256, changes[0].after_sha256);
    docker::import(&job, &work, &changes)?;
    assert!(!safety::executable(&std::fs::metadata(
        host.join("src/file.txt")
    )?));
    assert_eq!(
        std::fs::read_to_string(host.join("src/file.txt"))?,
        "before\n"
    );
    assert!(
        docker::export(&job, &root.join("conflicted-copy")).is_err(),
        "host chmod must invalidate export even when content is identical"
    );
    let legacy: FileStamp = serde_json::from_str(r#"{"sha256":"test","bytes":4,"tracked":true}"#)?;
    assert!(!legacy.executable);
    Ok(())
}

async fn await_finished(engine: &Engine, id: &str) -> Result<Job> {
    tokio::time::timeout(std::time::Duration::from_secs(30), async {
        loop {
            let job = engine.status(id).await?;
            if !job.status.active() {
                return Ok::<_, anyhow::Error>(job);
            }
            tokio::time::sleep(std::time::Duration::from_millis(25)).await;
        }
    })
    .await?
}

#[tokio::test]
#[ignore = "requires authorized local Docker daemon and rebuilt reviewed immutable image; launches real detached supervisor without owner HOME"]
async fn real_detached_start_uses_durable_endpoint_and_status() -> Result<()> {
    let (_tmp, engine, mut task) = fixture().await?;
    task.simulator
        .as_mut()
        .unwrap()
        .writes
        .insert("src/file.txt".into(), "detached change\n".into());
    let config = engine
        .store
        .root
        .parent()
        .unwrap()
        .join("detached-config.json");
    codex_relay::store::atomic_json(&config, &engine.config)?;
    let selected = docker::Client::local(&engine.store.root)?.endpoint;
    let job = engine.start(task.clone(), &config).await?;
    assert_eq!(job.docker_endpoint.as_deref(), Some(selected.as_str()));
    assert!(job.supervisor_pid.is_none() || job.supervisor_pid != Some(std::process::id()));
    let finished = await_finished(&engine, &job.id).await?;
    assert_eq!(finished.status, JobStatus::Completed);
    assert!(!finished.recovery_required);
    assert_eq!(
        std::fs::read_to_string(task.workspace.path.join("src/file.txt"))?,
        "detached change\n"
    );
    // A present /var/run alias must not become a fallback if the durable daemon is unavailable.
    // The fake stale identity names the already-removed container at the real daemon; only
    // honoring the deliberately invalid Job endpoint keeps recovery reserved here.
    tokio::time::timeout(std::time::Duration::from_secs(5), async {
        while engine.store.supervisor_alive(&job.id).unwrap() {
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
    })
    .await?;
    let completed = engine.store.load(&job.id)?;
    let mut stale = completed.clone();
    stale.status = JobStatus::Running;
    stale.updated_at_ms = now_ms() - 6000;
    stale.container = Some(docker::identity(
        &engine.store.root,
        &job.id,
        0,
        "worker",
        &selected,
    )?);
    stale.docker_endpoint = Some(format!(
        "unix://{}",
        engine.store.root.join("unavailable-daemon.sock").display()
    ));
    stale.recovery_required = true;
    engine.store.save(&stale)?;
    let blocked = engine.status(&job.id).await?;
    assert_eq!(blocked.status, JobStatus::Interrupted);
    assert!(blocked.recovery_required && blocked.container.is_some());
    assert_eq!(blocked.docker_endpoint, stale.docker_endpoint);
    engine.store.save(&completed)?;
    Ok(())
}

#[tokio::test]
#[ignore = "requires authorized local Docker daemon and rebuilt reviewed immutable image; ignored source and mode-only changes stay inside synthetic fixture"]
async fn real_container_ignored_reference_and_executable_mode_only_import() -> Result<()> {
    use std::os::unix::fs::PermissionsExt;
    let (_tmp, mut engine, mut task) = fixture().await?;
    let profile = engine.config.profiles.get_mut("fixture").unwrap();
    profile.harness = Harness::Aider;
    profile.executable = "/usr/bin/true".into();
    profile.provider = "synthetic".into();
    profile.budget.max_model_steps = 1;
    profile.budget.timeout_seconds = 60;
    task.simulator = None;
    task.writable_paths = vec!["src/file.txt".into()];
    let root = engine.store.root.parent().unwrap();
    std::fs::write(root.join("repo/.git/info/exclude"), "reference.ignored\n")?;
    std::fs::write(
        task.workspace.path.join("reference.ignored"),
        "ignored readonly source\n",
    )?;
    task.read_paths = vec!["reference.ignored".into()];
    task.tests = vec![TestCommand {
        id: "mode_only".into(),
        program: "/usr/bin/chmod".into(),
        args: vec!["755".into(), "src/file.txt".into()],
        timeout_seconds: 10,
    }];
    let job = engine.prepare(task.clone()).await?;
    assert!(job.baseline.contains_key(Path::new("reference.ignored")));
    engine.run(&job.id).await?;
    let finished = engine.status(&job.id).await?;
    assert_eq!(
        finished.status,
        JobStatus::Completed,
        "{:?}",
        finished.result
    );
    assert!(safety::executable(&std::fs::metadata(
        task.workspace.path.join("src/file.txt")
    )?));
    assert_eq!(finished.result.unwrap().changes[0].kind, "mode_changed");
    assert_eq!(
        std::fs::read_to_string(
            engine
                .store
                .job_dir(&job.id)?
                .join("attempt-0/workspace/reference.ignored")
        )?,
        "ignored readonly source\n"
    );
    // Revert only the disposable mode before another clean task; deny mode changes to read references.
    std::fs::set_permissions(
        task.workspace.path.join("src/file.txt"),
        std::fs::Permissions::from_mode(0o644),
    )?;
    task.tests[0].args = vec!["755".into(), "reference.ignored".into()];
    let job = engine.prepare(task.clone()).await?;
    engine.run(&job.id).await?;
    assert_eq!(
        engine.status(&job.id).await?.status,
        JobStatus::ScopeViolation
    );
    assert!(!safety::executable(&std::fs::metadata(
        task.workspace.path.join("reference.ignored")
    )?));
    Ok(())
}

#[tokio::test]
#[ignore = "requires authorized local Docker daemon and reviewed immutable image; concurrent chmod is synthetic and must stop copy-back"]
async fn real_container_concurrent_host_chmod_prevents_copyback() -> Result<()> {
    use std::os::unix::fs::PermissionsExt;
    let (_tmp, engine, mut task) = fixture().await?;
    task.simulator.as_mut().unwrap().delay_ms = 2000;
    task.simulator
        .as_mut()
        .unwrap()
        .writes
        .insert("src/file.txt".into(), "container change\n".into());
    let job = engine.prepare(task.clone()).await?;
    let runner = engine.clone();
    let id = job.id.clone();
    let running = tokio::spawn(async move { runner.run(&id).await });
    tokio::time::timeout(std::time::Duration::from_secs(10), async {
        loop {
            if engine
                .store
                .load(&job.id)
                .unwrap()
                .container
                .as_ref()
                .is_some_and(|c| c.id.is_some())
            {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
    })
    .await?;
    std::fs::set_permissions(
        task.workspace.path.join("src/file.txt"),
        std::fs::Permissions::from_mode(0o755),
    )?;
    assert!(running.await?.is_err());
    let finished = engine.status(&job.id).await?;
    assert_eq!(finished.status, JobStatus::Failed);
    assert_eq!(
        std::fs::read_to_string(task.workspace.path.join("src/file.txt"))?,
        "before\n"
    );
    assert!(safety::executable(&std::fs::metadata(
        task.workspace.path.join("src/file.txt")
    )?));
    assert!(
        engine
            .correct(
                &job.id,
                "Do not overwrite concurrent chmod".into(),
                task.native_load,
                None
            )
            .await
            .is_err()
    );
    Ok(())
}
