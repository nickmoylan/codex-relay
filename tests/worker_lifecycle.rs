use anyhow::Result;
use codex_relay::{adapter, engine::Engine, mcp, model::*, safety, store};
use serde_json::json;
use std::{
    collections::BTreeMap,
    path::{Path, PathBuf},
    time::Duration,
};

struct Fixture {
    _tmp: tempfile::TempDir,
    root: PathBuf,
    managed: PathBuf,
    config_path: PathBuf,
    engine: Engine,
}
impl Fixture {
    async fn new() -> Result<Self> {
        let tmp = tempfile::TempDir::new_in("/tmp")?;
        let root = std::fs::canonicalize(tmp.path())?;
        let repo = root.join("repo");
        std::fs::create_dir(&repo)?;
        safety::git(&repo, &["init", "-q"]).await?;
        std::fs::create_dir(repo.join("src"))?;
        std::fs::write(repo.join("src/tracked.txt"), "before\n")?;
        std::fs::write(repo.join(".gitignore"), "*.ignored\n")?;
        safety::git(&repo, &["add", "src", ".gitignore"]).await?;
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
                "test: initial fixture",
            ],
        )
        .await?;
        let managed = root.join("managed");
        std::fs::create_dir(&managed)?;
        let worktree = managed.join("worker-a");
        safety::git(
            &repo,
            &[
                "worktree",
                "add",
                "--detach",
                worktree.to_str().unwrap(),
                "HEAD",
            ],
        )
        .await?;
        let binary = std::fs::canonicalize(env!("CARGO_BIN_EXE_codex-relay"))?;
        let p = Profile {
            enabled: true,
            container: None,
            harness: Harness::Simulated,
            executable: binary.clone(),
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
                timeout_seconds: 3,
                max_model_steps: 5,
                max_output_bytes: 8192,
                max_change_bytes: 1024 * 1024,
                max_cost_usd: None,
            },
            credential_env: vec![],
            api_base: None,
            network_hosts: vec![],
            runtime_read_roots: vec!["/opt/homebrew/Cellar".into(), "/opt/homebrew/lib".into()],
        };
        let config = Config {
            schema_version: 1,
            state_dir: root.join("state"),
            managed_worktree_roots: vec![managed.clone()],
            max_external_workers: 1,
            max_total_workers: 3,
            reserved_native_slots: 1,
            allow_simulated_workers: true,
            profiles: BTreeMap::from([("fixture".into(), p)]),
        };
        let config_path = root.join("config.json");
        store::atomic_json(&config_path, &config)?;
        let engine = Engine::open(config, binary)?;
        Ok(Self {
            _tmp: tmp,
            root,
            managed,
            config_path,
            engine,
        })
    }
    async fn task(&self) -> Result<Task> {
        let path = self.managed.join("worker-a");
        let revision = String::from_utf8(safety::git(&path, &["rev-parse", "HEAD"]).await?)?
            .trim()
            .to_string();
        Ok(Task {
            schema_version: 1,
            profile: "fixture".into(),
            workspace: Workspace {
                path: path.clone(),
                artifact_identity: path.to_string_lossy().into_owned(),
                base_revision: revision,
                attested_at_ms: now_ms(),
            },
            objective: "Update the assigned text while preserving the fixture behavior".into(),
            writable_paths: vec!["src".into()],
            read_paths: vec![],
            acceptance_criteria: BTreeMap::from([(
                "done".into(),
                "Assigned change is complete".into(),
            )]),
            tests: vec![],
            native_load: load(),
            simulator: Some(Simulation::default()),
        })
    }
    async fn run(&self, task: Task) -> Result<Job> {
        let job = self.engine.prepare(task).await?;
        self.engine.run(&job.id).await?;
        self.engine.status(&job.id).await
    }
}
fn load() -> NativeLoad {
    NativeLoad {
        active_workers: 1,
        observed_at_ms: now_ms(),
        source: "coordinator_attested".into(),
    }
}

#[tokio::test]
async fn records_tracked_and_untracked_changes_and_observed_tests() -> Result<()> {
    let f = Fixture::new().await?;
    let mut task = f.task().await?;
    task.simulator.as_mut().unwrap().writes = BTreeMap::from([
        ("src/tracked.txt".into(), "after\n".into()),
        ("src/new.txt".into(), "new\n".into()),
        (
            "src/new.ignored".into(),
            "ignored source is still reviewed\n".into(),
        ),
    ]);
    task.simulator.as_mut().unwrap().claimed_test_ids = vec!["check".into(), "invented".into()];
    task.tests = vec![TestCommand {
        id: "check".into(),
        program: "/usr/bin/true".into(),
        args: vec![],
        timeout_seconds: 1,
    }];
    let job = f.run(task).await?;
    assert_eq!(job.status, JobStatus::Completed);
    let r = job.result.unwrap();
    assert_eq!(r.changes.len(), 3);
    assert!(
        r.changes
            .iter()
            .any(|c| c.path == Path::new("src/new.ignored") && !c.tracked)
    );
    assert!(
        r.changes
            .iter()
            .any(|c| c.path == Path::new("src/new.txt") && !c.tracked)
    );
    assert_eq!(r.worker_claimed_test_ids, ["check"]);
    assert_eq!(r.independently_observed_tests[0].outcome, "passed");
    assert_eq!(
        r.correctness,
        "requires_coordinator_review_and_integration_gate"
    );
    Ok(())
}

#[tokio::test]
async fn exit_zero_is_not_test_correctness() -> Result<()> {
    let f = Fixture::new().await?;
    let mut task = f.task().await?;
    task.tests = vec![TestCommand {
        id: "fails".into(),
        program: "/usr/bin/false".into(),
        args: vec![],
        timeout_seconds: 1,
    }];
    task.simulator.as_mut().unwrap().claimed_test_ids = vec!["fails".into()];
    let job = f.run(task).await?;
    assert_eq!(job.status, JobStatus::Failed);
    let r = job.result.unwrap();
    assert_eq!(r.process_exit_code, Some(0));
    assert_eq!(r.independently_observed_tests[0].exit_code, Some(1));
    Ok(())
}

#[tokio::test]
async fn oversized_changes_are_quarantined_before_verification() -> Result<()> {
    let mut f = Fixture::new().await?;
    f.engine
        .config
        .profiles
        .get_mut("fixture")
        .unwrap()
        .budget
        .max_change_bytes = 1024;
    let mut task = f.task().await?;
    task.simulator
        .as_mut()
        .unwrap()
        .writes
        .insert("src/tracked.txt".into(), "x".repeat(2048));
    task.tests = vec![TestCommand {
        id: "check".into(),
        program: "/usr/bin/true".into(),
        args: vec![],
        timeout_seconds: 2,
    }];
    let job = f.run(task).await?;
    assert_eq!(job.status, JobStatus::ScopeViolation);
    assert!(job.result.unwrap().independently_observed_tests.is_empty());
    Ok(())
}

#[tokio::test]
async fn malformed_events_and_output_and_step_limits_are_distinct() -> Result<()> {
    for (which, status) in [
        (0, JobStatus::Failed),
        (1, JobStatus::OutputLimit),
        (2, JobStatus::StepLimit),
    ] {
        let f = Fixture::new().await?;
        let mut task = f.task().await?;
        let spec = task.simulator.as_mut().unwrap();
        match which {
            0 => spec.malformed_event = true,
            1 => spec.output_bytes = 32 * 1024,
            _ => spec.emit_steps = 10,
        }
        assert_eq!(f.run(task).await?.status, status);
    }
    Ok(())
}

#[tokio::test]
async fn scope_violation_is_quarantined() -> Result<()> {
    let f = Fixture::new().await?;
    let mut task = f.task().await?;
    task.simulator
        .as_mut()
        .unwrap()
        .writes
        .insert("outside.txt".into(), "unauthorized\n".into());
    let job = f.run(task).await?;
    assert_eq!(job.status, JobStatus::ScopeViolation);
    assert!(job.result.unwrap().changes.iter().any(|c| !c.in_scope));
    Ok(())
}

#[tokio::test]
async fn traversal_symlink_and_hardlink_escape_are_rejected() -> Result<()> {
    let f = Fixture::new().await?;
    let mut task = f.task().await?;
    task.writable_paths = vec!["../escape".into()];
    assert!(f.engine.prepare(task).await.is_err());
    let mut task = f.task().await?;
    task.writable_paths = vec!["/tmp/escape".into()];
    assert!(f.engine.prepare(task).await.is_err());
    let root = f.managed.join("worker-a");
    let denied = f.root.join("denied.txt");
    std::fs::write(&denied, "keep\n")?;
    std::os::unix::fs::symlink(&denied, root.join("src/link.txt"))?;
    let mut task = f.task().await?;
    task.writable_paths = vec!["src/link.txt".into()];
    assert!(f.engine.prepare(task).await.is_err());
    std::fs::remove_file(root.join("src/link.txt"))?;
    std::fs::hard_link(&denied, root.join("src/link.txt"))?;
    let mut task = f.task().await?;
    task.writable_paths = vec!["src/link.txt".into()];
    assert!(f.engine.prepare(task).await.is_err());
    assert_eq!(std::fs::read_to_string(denied)?, "keep\n");
    Ok(())
}

#[tokio::test]
async fn symlink_swap_after_assignment_cannot_escape_fixture() -> Result<()> {
    let f = Fixture::new().await?;
    let mut task = f.task().await?;
    task.simulator
        .as_mut()
        .unwrap()
        .writes
        .insert("src/new.txt".into(), "replacement".into());
    let job = f.engine.prepare(task).await?;
    let outside = f.root.join("outside");
    std::fs::create_dir(&outside)?;
    std::fs::remove_dir_all(f.managed.join("worker-a/src"))?;
    std::os::unix::fs::symlink(&outside, f.managed.join("worker-a/src"))?;
    f.engine.run(&job.id).await?;
    assert_eq!(
        f.engine.status(&job.id).await?.status,
        JobStatus::ScopeViolation
    );
    assert!(!outside.join("new.txt").exists());
    Ok(())
}

#[tokio::test]
async fn one_correction_preserves_prior_evidence_and_rejects_second() -> Result<()> {
    let f = Fixture::new().await?;
    let mut task = f.task().await?;
    task.simulator
        .as_mut()
        .unwrap()
        .writes
        .insert("src/tracked.txt".into(), "changed\n".into());
    let first = f.run(task).await?;
    f.engine
        .correct(
            &first.id,
            "Check the assigned change once more".into(),
            load(),
            None,
        )
        .await?;
    f.engine.run(&first.id).await?;
    let corrected = f.engine.status(&first.id).await?;
    assert_eq!(corrected.attempt, 1);
    assert!(
        f.engine
            .store
            .job_dir(&first.id)?
            .join("result-0.json")
            .exists()
    );
    assert!(
        f.engine
            .correct(&first.id, "Again".into(), load(), None)
            .await
            .is_err()
    );
    Ok(())
}

#[tokio::test]
async fn correction_rejects_concurrent_manual_edits() -> Result<()> {
    let f = Fixture::new().await?;
    let first = f.run(f.task().await?).await?;
    std::fs::write(
        first.task.workspace.path.join("src/tracked.txt"),
        "someone else's change",
    )?;
    assert!(
        f.engine
            .correct(&first.id, "Correct".into(), load(), None)
            .await
            .is_err()
    );
    Ok(())
}

#[tokio::test]
async fn durable_state_survives_new_engine_and_recovers_abandoned_job() -> Result<()> {
    let f = Fixture::new().await?;
    let job = f.run(f.task().await?).await?;
    let second = Engine::open(f.engine.config.clone(), f.engine.binary.clone())?;
    assert_eq!(second.status(&job.id).await?.status, JobStatus::Completed);
    let mut record = second.store.load(&job.id)?;
    record.status = JobStatus::Running;
    record.worker = None;
    record.updated_at_ms = now_ms() - 6000;
    second.store.save(&record)?;
    assert_eq!(second.status(&job.id).await?.status, JobStatus::Interrupted);
    assert!(!second.status(&job.id).await?.recovery_required);
    Ok(())
}

#[tokio::test]
async fn admission_enforces_combined_load_and_disjoint_repository_ownership() -> Result<()> {
    let mut f = Fixture::new().await?;
    f.engine.config.max_external_workers = 2;
    f.engine.config.max_total_workers = 3;
    let mut task = f.task().await?;
    task.native_load.active_workers = 3;
    assert!(f.engine.prepare(task).await.is_err());
    let mut task = f.task().await?;
    task.native_load.observed_at_ms = now_ms() - 61_000;
    assert!(f.engine.prepare(task).await.is_err());
    let mut task = f.task().await?;
    task.native_load.source = "automatic".into();
    assert!(f.engine.prepare(task).await.is_err());
    let first = f.engine.prepare(f.task().await?).await?;
    let b = f.managed.join("worker-b");
    safety::git(
        &f.root.join("repo"),
        &["worktree", "add", "--detach", b.to_str().unwrap(), "HEAD"],
    )
    .await?;
    let mut task = f.task().await?;
    task.workspace.path = b.clone();
    task.workspace.artifact_identity = b.to_string_lossy().into_owned();
    assert!(f.engine.prepare(task.clone()).await.is_err());
    task.writable_paths = vec!["other.txt".into()];
    assert!(f.engine.prepare(task).await.is_ok());
    let status = f.engine.all_status().await?;
    assert_eq!(status["concurrency"]["external_active"], 2);
    assert_eq!(f.engine.status(&first.id).await?.status, JobStatus::Queued);
    Ok(())
}

async fn descendant_pid(f: &Fixture, id: &str) -> Result<i32> {
    let path = f
        .engine
        .store
        .job_dir(id)?
        .join("attempt-0/tmp/descendant.pid");
    tokio::time::timeout(Duration::from_secs(3), async {
        loop {
            if let Ok(s) = std::fs::read_to_string(&path)
                && let Ok(pid) = s.trim().parse()
            {
                return pid;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .map_err(Into::into)
}
async fn assert_dead(pid: i32) -> Result<()> {
    tokio::time::timeout(Duration::from_secs(3), async {
        loop {
            let out = safety::command(
                Path::new("/bin/ps"),
                &["-p".into(), pid.to_string(), "-o".into(), "stat=".into()],
            )
            .unwrap()
            .output()
            .await
            .unwrap();
            let state = String::from_utf8_lossy(&out.stdout);
            if state.trim().is_empty() || state.trim().starts_with('Z') {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await?;
    Ok(())
}

#[tokio::test]
async fn cancellation_and_timeout_kill_descendants() -> Result<()> {
    for cancel in [true, false] {
        let mut f = Fixture::new().await?;
        f.engine
            .config
            .profiles
            .get_mut("fixture")
            .unwrap()
            .budget
            .timeout_seconds = 1;
        let mut task = f.task().await?;
        task.simulator.as_mut().unwrap().delay_ms = 5000;
        task.simulator.as_mut().unwrap().descendant = true;
        let job = f.engine.prepare(task).await?;
        let engine = f.engine.clone();
        let id = job.id.clone();
        let running = tokio::spawn(async move { engine.run(&id).await });
        let pid = descendant_pid(&f, &job.id).await?;
        if cancel {
            f.engine.cancel(&job.id).await?;
        }
        running.await??;
        assert_eq!(
            f.engine.status(&job.id).await?.status,
            if cancel {
                JobStatus::Cancelled
            } else {
                JobStatus::TimedOut
            }
        );
        assert_dead(pid).await?;
    }
    Ok(())
}

#[tokio::test]
async fn successful_parent_exit_also_cleans_descendants() -> Result<()> {
    let f = Fixture::new().await?;
    let mut task = f.task().await?;
    task.simulator.as_mut().unwrap().descendant = true;
    task.simulator.as_mut().unwrap().delay_ms = 100;
    let job = f.engine.prepare(task).await?;
    let engine = f.engine.clone();
    let id = job.id.clone();
    let running = tokio::spawn(async move { engine.run(&id).await });
    let pid = descendant_pid(&f, &job.id).await?;
    running.await??;
    assert_eq!(f.engine.status(&job.id).await?.status, JobStatus::Completed);
    assert_dead(pid).await?;
    Ok(())
}

#[tokio::test]
async fn raw_stdout_stderr_and_secret_values_are_not_persisted() -> Result<()> {
    let f = Fixture::new().await?;
    let mut task = f.task().await?;
    task.simulator.as_mut().unwrap().echo = "credential_probe".into();
    let job = f.run(task).await?;
    for entry in std::fs::read_dir(f.engine.store.job_dir(&job.id)?)? {
        let p = entry?.path();
        if p.is_file() {
            let bytes = std::fs::read(p)?;
            assert!(!String::from_utf8_lossy(&bytes).contains("SYNTHETIC_DO_NOT_PERSIST"));
        }
    }
    assert!(!serde_json::to_string(&job.result)?.contains("SYNTHETIC_DO_NOT_PERSIST"));
    Ok(())
}

#[tokio::test]
async fn rejects_unattested_dirty_and_nonlinked_workspaces() -> Result<()> {
    let f = Fixture::new().await?;
    let mut task = f.task().await?;
    task.workspace.artifact_identity = "made-up".into();
    assert!(f.engine.prepare(task).await.is_err());
    let mut task = f.task().await?;
    task.workspace.attested_at_ms = now_ms() - 301_000;
    assert!(f.engine.prepare(task).await.is_err());
    let mut task = f.task().await?;
    task.workspace.path = f.root.join("repo");
    task.workspace.artifact_identity = task.workspace.path.to_string_lossy().into_owned();
    assert!(f.engine.prepare(task).await.is_err());
    let task = f.task().await?;
    std::fs::write(task.workspace.path.join("unrelated.txt"), "preserve")?;
    assert!(f.engine.prepare(task).await.is_err());
    Ok(())
}

#[tokio::test]
async fn profiles_reject_privacy_fallback_and_unenforceable_caps() -> Result<()> {
    let f = Fixture::new().await?;
    let mut p = f.engine.config.profiles["fixture"].clone();
    p.privacy.allow_fallbacks = true;
    assert!(safety::validate_profile(&p).is_err());
    p.privacy.allow_fallbacks = false;
    p.budget.max_cost_usd = Some(1.0);
    assert!(safety::validate_profile(&p).is_err());
    p.budget.max_cost_usd = None;
    p.harness = Harness::Muse;
    p.provider = "meta".into();
    p.model = "muse-spark-1.3-contributor".into();
    assert!(safety::validate_profile(&p).is_err());
    p = f.engine.config.profiles["fixture"].clone();
    p.runtime_read_roots = vec![f.root.join("private-home")];
    assert!(safety::validate_profile(&p).is_err());
    Ok(())
}

#[tokio::test]
async fn aider_openrouter_args_pin_model_and_private_routing_without_shells() -> Result<()> {
    let f = Fixture::new().await?;
    let mut job = f.engine.prepare(f.task().await?).await?;
    job.profile.harness = Harness::Aider;
    job.profile.provider = "openrouter".into();
    job.profile.model = "anthropic/example-model".into();
    job.profile.privacy.allowed_providers = vec!["anthropic".into()];
    job.profile.privacy.zero_data_retention = true;
    job.profile.credential_env = vec!["OPENROUTER_API_KEY".into()];
    job.profile.network_hosts = vec!["openrouter.ai".into()];
    job.profile.budget.max_model_steps = 1;
    job.task.writable_paths = vec!["src/tracked.txt".into()];
    safety::validate_profile(&job.profile)?;
    let scratch = f.root.join("adapter");
    std::fs::create_dir(&scratch)?;
    let invocation = adapter::build(&job, &scratch)?;
    assert!(invocation.args.contains(&"--no-git".into()));
    assert!(
        invocation
            .args
            .contains(&"--no-suggest-shell-commands".into())
    );
    assert_eq!(
        invocation
            .args
            .iter()
            .filter(|s| *s == "openrouter/anthropic/example-model")
            .count(),
        3
    );
    let settings: serde_json::Value = store::read_json(&scratch.join("model-settings.json"), 8192)?;
    assert_eq!(
        settings[0]["extra_params"]["extra_body"]["provider"]["allow_fallbacks"],
        false
    );
    assert_eq!(
        settings[0]["extra_params"]["extra_body"]["provider"]["data_collection"],
        "deny"
    );
    assert_eq!(
        settings[0]["extra_params"]["extra_body"]["provider"]["zdr"],
        true
    );
    Ok(())
}

#[tokio::test]
async fn state_symlinks_job_traversal_and_unknown_tool_fields_fail() -> Result<()> {
    let f = Fixture::new().await?;
    assert!(f.engine.store.load("../../escape").is_err());
    assert!(
        mcp::call(
            &f.engine,
            &f.config_path,
            "workers_doctor",
            json!({"credential":"secret"})
        )
        .await
        .is_err()
    );
    let job = f.engine.prepare(f.task().await?).await?;
    let path = f.engine.store.job_dir(&job.id)?.join("job.json");
    std::fs::remove_file(&path)?;
    std::os::unix::fs::symlink(&f.config_path, &path)?;
    assert!(f.engine.store.load(&job.id).is_err());
    Ok(())
}

#[tokio::test]
async fn disabled_external_profiles_never_launch() -> Result<()> {
    let mut f = Fixture::new().await?;
    f.engine.config.profiles.get_mut("fixture").unwrap().enabled = false;
    assert!(
        f.engine
            .prepare(f.task().await?)
            .await
            .unwrap_err()
            .to_string()
            .contains("disabled")
    );
    assert!(f.engine.store.jobs()?.is_empty());
    Ok(())
}

#[tokio::test]
async fn cancellation_before_launch_never_starts_the_worker() -> Result<()> {
    let f = Fixture::new().await?;
    let job = f.engine.prepare(f.task().await?).await?;
    f.engine.cancel(&job.id).await?;
    f.engine.run(&job.id).await?;
    assert_eq!(f.engine.status(&job.id).await?.status, JobStatus::Cancelled);
    assert!(!f.engine.store.job_dir(&job.id)?.join("attempt-0").exists());
    Ok(())
}

struct Rpc {
    child: tokio::process::Child,
    input: tokio::process::ChildStdin,
    output: tokio::io::BufReader<tokio::process::ChildStdout>,
    next: u64,
}
impl Rpc {
    async fn open(f: &Fixture) -> Result<Self> {
        let mut cmd = safety::command(
            &f.engine.binary,
            &[
                "--config".into(),
                f.config_path.to_string_lossy().into_owned(),
                "mcp".into(),
            ],
        )?;
        cmd.stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::null())
            .kill_on_drop(true);
        let mut child = cmd.spawn()?;
        let input = child.stdin.take().unwrap();
        let output = tokio::io::BufReader::new(child.stdout.take().unwrap());
        Ok(Self {
            child,
            input,
            output,
            next: 1,
        })
    }
    async fn request(
        &mut self,
        method: &str,
        params: serde_json::Value,
    ) -> Result<serde_json::Value> {
        use tokio::io::{AsyncBufReadExt, AsyncWriteExt};
        let id = self.next;
        self.next += 1;
        self.input
            .write_all(
                format!(
                    "{}\n",
                    json!({"jsonrpc":"2.0","id":id,"method":method,"params":params})
                )
                .as_bytes(),
            )
            .await?;
        self.input.flush().await?;
        let mut line = String::new();
        tokio::time::timeout(Duration::from_secs(10), self.output.read_line(&mut line)).await??;
        let reply: serde_json::Value = serde_json::from_str(&line)?;
        assert_eq!(reply["id"], id);
        assert_eq!(reply["jsonrpc"], "2.0");
        Ok(reply)
    }
    async fn initialize(&mut self) -> Result<()> {
        use tokio::io::AsyncWriteExt;
        let r=self.request("initialize",json!({"protocolVersion":"2025-06-18","capabilities":{},"clientInfo":{"name":"fixture","version":"1"}})).await?;
        assert_eq!(r["result"]["serverInfo"]["name"], "codex-relay");
        self.input
            .write_all(b"{\"jsonrpc\":\"2.0\",\"method\":\"notifications/initialized\"}\n")
            .await?;
        self.input.flush().await?;
        Ok(())
    }
    async fn close(mut self) -> Result<()> {
        use tokio::io::AsyncWriteExt;
        self.input.shutdown().await?;
        drop(self.input);
        assert!(
            tokio::time::timeout(Duration::from_secs(5), self.child.wait())
                .await??
                .success()
        );
        Ok(())
    }
}

fn check_refs(value: &serde_json::Value, root: &serde_json::Value) {
    if let Some(reference) = value.get("$ref").and_then(serde_json::Value::as_str) {
        assert!(
            root.pointer(reference.trim_start_matches('#')).is_some(),
            "unresolved schema reference {reference}"
        );
    }
    match value {
        serde_json::Value::Object(map) => {
            for v in map.values() {
                check_refs(v, root)
            }
        }
        serde_json::Value::Array(list) => {
            for v in list {
                check_refs(v, root)
            }
        }
        _ => {}
    }
}

#[tokio::test]
async fn real_stdio_mcp_protocol_smoke_exercises_all_six_tools() -> Result<()> {
    let f = Fixture::new().await?;
    let mut rpc = Rpc::open(&f).await?;
    let before = rpc.request("tools/list", json!({})).await?;
    assert!(before.get("error").is_some());
    rpc.initialize().await?;
    let listed = rpc.request("tools/list", json!({})).await?;
    let tools = listed["result"]["tools"].as_array().unwrap();
    assert_eq!(tools.len(), 6);
    for tool in tools {
        check_refs(&tool["inputSchema"], &tool["inputSchema"]);
    }
    let doctor = rpc
        .request(
            "tools/call",
            json!({"name":"workers_doctor","arguments":{}}),
        )
        .await?;
    assert_eq!(doctor["result"]["isError"], false);
    let mut task = f.task().await?;
    task.simulator
        .as_mut()
        .unwrap()
        .writes
        .insert("src/new.txt".into(), "new\n".into());
    let started = rpc
        .request(
            "tools/call",
            json!({"name":"workers_start","arguments":{"task":task}}),
        )
        .await?;
    assert_eq!(started["result"]["isError"], false);
    let id = started["result"]["structuredContent"]["job_id"]
        .as_str()
        .unwrap()
        .to_string();
    for _ in 0..100 {
        let status = rpc
            .request(
                "tools/call",
                json!({"name":"workers_status","arguments":{"job_id":id}}),
            )
            .await?;
        if status["result"]["structuredContent"]["result_available"] == true {
            break;
        }
        tokio::time::sleep(Duration::from_millis(30)).await;
    }
    let result = rpc
        .request(
            "tools/call",
            json!({"name":"workers_result","arguments":{"job_id":id}}),
        )
        .await?;
    assert_eq!(
        result["result"]["structuredContent"]["result"]["status"],
        "completed"
    );
    let corrected=rpc.request("tools/call",json!({"name":"workers_correct","arguments":{"job_id":id,"feedback":"Check the requested edit once","native_load":load()}})).await?;
    assert_eq!(corrected["result"]["isError"], false);
    let cancelled = rpc
        .request(
            "tools/call",
            json!({"name":"workers_cancel","arguments":{"job_id":id}}),
        )
        .await?;
    assert_eq!(cancelled["result"]["isError"], false);
    rpc.close().await?;
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            let j = f.engine.status(&id).await.unwrap();
            if !j.status.active() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(30)).await;
        }
    })
    .await?;
    Ok(())
}

#[tokio::test]
async fn mcp_disconnect_does_not_lose_a_running_job() -> Result<()> {
    let f = Fixture::new().await?;
    let mut rpc = Rpc::open(&f).await?;
    rpc.initialize().await?;
    let mut task = f.task().await?;
    task.simulator.as_mut().unwrap().delay_ms = 500;
    let started = rpc
        .request(
            "tools/call",
            json!({"name":"workers_start","arguments":{"task":task}}),
        )
        .await?;
    let id = started["result"]["structuredContent"]["job_id"]
        .as_str()
        .unwrap()
        .to_string();
    rpc.close().await?;
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            let job = f.engine.status(&id).await.unwrap();
            if !job.status.active() {
                assert_eq!(job.status, JobStatus::Completed);
                break;
            }
            tokio::time::sleep(Duration::from_millis(30)).await;
        }
    })
    .await?;
    assert!(f.engine.store.load(&id)?.result.is_some());
    Ok(())
}

#[tokio::test]
#[ignore = "requires host permission to initialize Seatbelt; never runs a provider"]
async fn real_os_isolation_probe() -> Result<()> {
    let f = Fixture::new().await?;
    f.engine
        .isolation_probe(&f.task().await?, &f.engine.config.profiles["fixture"])
        .await?;
    Ok(())
}

#[tokio::test]
async fn simulated_tests_cannot_launch_host_programs_or_internal_commands() -> Result<()> {
    let f = Fixture::new().await?;
    let marker = f.root.join("outside-worktree-must-not-exist");
    for hidden in [
        "__container-worker",
        "__fixture",
        "__fixture-child",
        "__supervise",
        "__isolation-probe",
        "__detach-probe",
        "__container-probe",
        "__container-detached",
        "__https-proxy",
    ] {
        let mut task = f.task().await?;
        task.tests = vec![TestCommand {
            id: "escape".into(),
            program: f.engine.binary.clone(),
            args: vec![
                hidden.into(),
                "/usr/bin/touch".into(),
                marker.to_string_lossy().into_owned(),
            ],
            timeout_seconds: 1,
        }];
        assert!(f.engine.prepare(task).await.is_err(), "accepted {hidden}");
        assert!(!marker.exists());
    }
    for (program, args) in [
        (
            PathBuf::from("/usr/bin/touch"),
            vec![marker.to_string_lossy().into_owned()],
        ),
        (
            PathBuf::from("/usr/bin/true"),
            vec!["unexpected-argument".into()],
        ),
        (f.engine.binary.clone(), vec![]),
    ] {
        let mut task = f.task().await?;
        task.tests = vec![TestCommand {
            id: "escape".into(),
            program,
            args,
            timeout_seconds: 1,
        }];
        assert!(f.engine.prepare(task).await.is_err());
        assert!(!marker.exists());
    }
    assert_eq!(
        std::fs::read_to_string(f.managed.join("worker-a/src/tracked.txt"))?,
        "before\n"
    );
    assert_eq!(
        f.engine.all_status().await?["jobs"]
            .as_array()
            .unwrap()
            .len(),
        0
    );
    Ok(())
}
