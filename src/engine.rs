use crate::{
    adapter, docker,
    model::*,
    safety,
    store::{self, Store},
};
use anyhow::{Context, Result, ensure};
use fs2::FileExt;
use serde_json::{Value, json};
use std::{
    path::{Path, PathBuf},
    process::Stdio,
    time::{Duration, Instant},
};
use tokio::{io::AsyncReadExt, sync::mpsc};

#[derive(Clone)]
pub struct Engine {
    pub config: Config,
    pub store: Store,
    pub binary: PathBuf,
}

#[derive(Default)]
struct Events {
    bytes: usize,
    steps: u32,
    malformed: usize,
    tests: Vec<String>,
    criteria: Vec<String>,
    buffer: Vec<u8>,
}
struct Outcome {
    status: JobStatus,
    exit_code: Option<i32>,
    events: Events,
    elapsed_ms: u64,
}

impl Engine {
    pub fn open(config: Config, binary: PathBuf) -> Result<Self> {
        safety::validate_config(&config)?;
        let store = Store::open(&config.state_dir)?;
        Ok(Self {
            config,
            store,
            binary,
        })
    }

    pub async fn prepare(&self, task: Task) -> Result<Job> {
        ensure!(
            serde_json::to_vec(&task)?.len() <= 128 * 1024,
            "task exceeds its structured byte limit"
        );
        let profile = self
            .config
            .profiles
            .get(&task.profile)
            .context("unknown profile")?
            .clone();
        ensure!(
            profile.enabled,
            "profile is disabled; external execution and credentials require a separate authorized handoff"
        );
        if profile.harness == Harness::Simulated {
            ensure!(
                self.config.allow_simulated_workers
                    && (profile.container.is_some() || profile.executable == self.binary),
                "simulation is a test-only capability and must use this binary"
            );
            ensure!(
                task.tests.iter().all(|test| {
                    [Path::new("/usr/bin/true"), Path::new("/usr/bin/false")]
                        .contains(&test.program.as_path())
                        && test.args.is_empty()
                }),
                "simulated verification permits only true/false with empty argv"
            );
            let s = task
                .simulator
                .as_ref()
                .context("simulation scenario required")?;
            ensure!(
                s.delay_ms <= 60_000
                    && s.writes.len() <= 16
                    && s.output_bytes <= 8 * 1024 * 1024
                    && s.emit_steps <= 200,
                "unbounded simulation"
            );
            for (path, text) in &s.writes {
                safety::clean_relative(path)?;
                ensure!(
                    text.len() <= 64 * 1024 && !safety::contains_secret(text),
                    "invalid simulation text"
                );
            }
            ensure!(
                ["", "credential_probe"].contains(&s.echo.as_str()),
                "simulation supports only the fixed credential-output probe"
            );
        } else {
            ensure!(
                task.simulator.is_none(),
                "simulation settings cannot be applied to external harnesses"
            );
            ensure!(
                profile.container.is_some() || profile.executable.is_file(),
                "harness is not installed at the configured path"
            );
            if profile.harness == Harness::Muse {
                ensure!(
                    profile.credential_env.is_empty(),
                    "Muse native shell can inherit API-key environment; that credential handoff is unsupported until a reviewed broker is available"
                );
                anyhow::bail!(
                    "Muse subscription-session isolation is not implemented; the adapter is prepared but live Muse starts remain blocked"
                );
            }
        }
        let (_, repository_identity) = safety::validate_workspace(&self.config, &task).await?;
        ensure!(
            safety::git(
                &task.workspace.path,
                &[
                    "status",
                    "--porcelain",
                    "--untracked-files=all",
                    "--ignore-submodules=all"
                ]
            )
            .await?
            .is_empty(),
            "start requires a clean supplied worktree; preserve existing work in another attached workspace"
        );
        if profile.container.is_some() || profile.harness != Harness::Simulated {
            self.isolation_probe(&task, &profile).await?;
        }
        let docker_endpoint = if profile.container.is_some() {
            Some(docker::Client::local(&self.store.root)?.endpoint)
        } else {
            None
        };
        let baseline = safety::snapshot(
            &task.workspace.path,
            512 * 1024 * 1024,
            &task.writable_paths,
            &task.read_paths,
        )
        .await?;
        let _lock = self.store.transaction()?;
        let jobs = self.store.jobs()?;
        self.check_admission(&task, &repository_identity, &jobs, None)?;
        let job = Job {
            schema_version: SCHEMA_VERSION,
            id: uuid::Uuid::new_v4().to_string(),
            task,
            profile,
            repository_identity,
            status: JobStatus::Queued,
            created_at_ms: now_ms(),
            updated_at_ms: now_ms(),
            attempt: 0,
            supervisor_pid: None,
            worker: None,
            docker_endpoint,
            container: None,
            broker: None,
            recovery_required: false,
            import_pending: false,
            baseline: baseline.clone(),
            last_snapshot: baseline,
            result: None,
            correction: None,
        };
        self.store.create(&job)?;
        Ok(job)
    }

    fn check_admission(
        &self,
        task: &Task,
        repo: &Path,
        jobs: &[Job],
        exclude: Option<&str>,
    ) -> Result<()> {
        let native = &task.native_load;
        ensure!(
            native.active_workers <= 32,
            "native worker count exceeds supported capacity"
        );
        ensure!(
            native.source == "coordinator_attested"
                && native.observed_at_ms <= now_ms() + 5000
                && now_ms().saturating_sub(native.observed_at_ms) <= 60_000,
            "a fresh coordinator-attested native worker count is required; automatic Codex activity discovery is unavailable"
        );
        let active = jobs
            .iter()
            .filter(|j| {
                Some(j.id.as_str()) != exclude && (j.status.active() || j.recovery_required)
            })
            .collect::<Vec<_>>();
        ensure!(
            active.len() < self.config.max_external_workers,
            "external worker capacity reached"
        );
        let native_count = native.active_workers.max(self.config.reserved_native_slots);
        ensure!(
            active.len() + native_count < self.config.max_total_workers,
            "combined native/external worker capacity reached"
        );
        for other in active {
            ensure!(
                other.task.workspace.path != task.workspace.path,
                "a worktree can have only one active worker"
            );
            ensure!(
                other.repository_identity != repo
                    || !safety::overlaps(&other.task.writable_paths, &task.writable_paths),
                "writable ownership overlaps an active/recovery-blocked job"
            );
        }
        Ok(())
    }

    pub async fn start(&self, task: Task, config_path: &Path) -> Result<Job> {
        let job = self.prepare(task).await?;
        if let Err(error) = self.spawn_supervisor(&job.id, config_path).await {
            self.finish_failure(&job.id, JobStatus::Failed, "supervisor could not start")?;
            return Err(error);
        }
        Ok(job)
    }

    async fn spawn_supervisor(&self, id: &str, config_path: &Path) -> Result<()> {
        let mut cmd = safety::command(
            &self.binary,
            &[
                "--config".into(),
                config_path.to_string_lossy().into_owned(),
                "__supervise".into(),
                id.into(),
            ],
        )?;
        let record = self.store.load(id)?;
        if record.profile.harness != Harness::Simulated {
            for name in &record.profile.credential_env {
                let value = std::env::var(name).map_err(|_| {
                    anyhow::anyhow!("named credential is unavailable; no credential was created")
                })?;
                cmd.env(name, value);
            }
        }
        cmd.stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .process_group(0);
        let child = cmd.spawn().context("cannot launch supervisor")?;
        let _lock = self.store.transaction()?;
        let mut job = self.store.load(id)?;
        if job.status == JobStatus::Queued {
            job.supervisor_pid = child.id();
        }
        self.store.save(&job)?;
        // Per-job supervisor remains responsible for its worker after CLI/MCP exit.
        // Dropping Child does not kill it; there is no persistent global daemon.
        Ok(())
    }

    pub async fn correct(
        &self,
        id: &str,
        feedback: String,
        native_load: NativeLoad,
        config_path: Option<&Path>,
    ) -> Result<Job> {
        ensure!(
            !feedback.is_empty() && feedback.len() <= 8192 && !safety::contains_secret(&feedback),
            "feedback must be bounded and contain no credentials"
        );
        let old = self.status(id).await?;
        ensure!(
            !old.status.active() && !old.recovery_required && old.attempt == 0,
            "only one correction is permitted, after a safely terminated attempt"
        );
        let current = safety::snapshot(
            &old.task.workspace.path,
            512 * 1024 * 1024,
            &old.task.writable_paths,
            &old.task.read_paths,
        )
        .await?;
        ensure!(
            current == old.last_snapshot,
            "worktree changed since the last result; coordinator must review concurrent changes"
        );
        let _lock = self.store.transaction()?;
        let mut job = self.store.load(id)?;
        ensure!(
            job.attempt == 0 && !job.status.active() && !job.recovery_required,
            "correction already claimed or recovery required"
        );
        job.task.native_load = native_load;
        self.check_admission(
            &job.task,
            &job.repository_identity,
            &self.store.jobs()?,
            Some(id),
        )?;
        job.attempt = 1;
        job.correction = Some(feedback);
        job.result = None;
        job.status = JobStatus::Queued;
        job.updated_at_ms = now_ms();
        job.worker = None;
        job.container = None;
        job.broker = None;
        let cancel = self.store.job_dir(id)?.join("cancel");
        if cancel.exists() {
            std::fs::remove_file(cancel)?;
        }
        self.store.save(&job)?;
        drop(_lock);
        if let Some(path) = config_path {
            self.spawn_supervisor(id, path).await?;
        }
        Ok(job)
    }

    pub async fn status(&self, id: &str) -> Result<Job> {
        let mut job = self.store.load(id)?;
        if (job.status.active() || job.recovery_required)
            && now_ms().saturating_sub(job.updated_at_ms) > 5000
            && !self.store.supervisor_alive(id)?
        {
            let _lock = self.store.transaction()?;
            job = self.store.load(id)?;
            if (job.status.active() || job.recovery_required) && !self.store.supervisor_alive(id)? {
                let mut blocked = job.import_pending;
                if let Some(container) = &job.container {
                    let container_blocked = match self.job_docker_client(&job) {
                        Ok(client) => client.cleanup(container).await.is_err(),
                        Err(_) => true,
                    };
                    blocked |= container_blocked;
                    if !container_blocked {
                        job.container = None;
                    }
                }
                if let Some(broker) = &job.broker {
                    let broker_blocked = match self.job_docker_client(&job) {
                        Ok(client) => client.cleanup(broker).await.is_err(),
                        Err(_) => true,
                    };
                    blocked |= broker_blocked;
                    if !broker_blocked {
                        job.broker = None;
                    }
                }
                if let Some(worker) = &job.worker {
                    if process_birth(worker.pid).await.as_deref() == Some(worker.birth.as_str()) {
                        kill_group(worker.pid);
                    } else if group_exists(worker.pid) {
                        blocked = true;
                    }
                }
                job.recovery_required = blocked;
                job.status = JobStatus::Interrupted;
                job.updated_at_ms = now_ms();
                job.result = Some(empty_result(
                    JobStatus::Interrupted,
                    if blocked {
                        "supervisor disappeared and process cleanup or a pending copy-back cannot be verified; workspace remains reserved for manual recovery"
                    } else {
                        "supervisor disappeared; no automatic replay or harness-session resume was attempted"
                    },
                ));
                self.store.save(&job)?;
            }
        }
        Ok(job)
    }

    pub async fn all_status(&self) -> Result<Value> {
        let ids = self
            .store
            .jobs()?
            .into_iter()
            .map(|j| j.id)
            .collect::<Vec<_>>();
        let mut jobs = vec![];
        for id in ids {
            jobs.push(self.status(&id).await?);
        }
        let external_active = jobs
            .iter()
            .filter(|j| j.status.active() || j.recovery_required)
            .count();
        let latest_native = jobs
            .iter()
            .max_by_key(|j| j.task.native_load.observed_at_ms)
            .map(|j| j.task.native_load.clone());
        let native_stale = latest_native
            .as_ref()
            .is_none_or(|n| now_ms().saturating_sub(n.observed_at_ms) > 60_000);
        Ok(
            json!({"jobs":jobs.iter().map(public_status).collect::<Vec<_>>(), "concurrency":{
                "external_active":external_active, "external_limit":self.config.max_external_workers,
                "total_limit":self.config.max_total_workers, "reserved_native_slots":self.config.reserved_native_slots,
                "native_observation":latest_native, "native_observation_stale":native_stale,"native_awareness":"coordinator_attestation_only",
                "scope":"this shared state directory; separate installations and unreported native work are not automatically visible"
            }}),
        )
    }

    pub async fn cancel(&self, id: &str) -> Result<Value> {
        let job = self.status(id).await?;
        if job.status.active() {
            self.store.cancel(id)?;
        }
        Ok(
            json!({"job_id":id,"cancel_requested":job.status.active(),"status":job.status,"completion":"request recorded; status/result confirms process-tree cleanup"}),
        )
    }

    pub async fn doctor(&self) -> Result<Value> {
        let docker_client = docker::Client::local(&self.store.root).ok();
        let profiles = self.config.profiles.iter().map(|(name,p)| json!({
            "name":name,"enabled":p.enabled,"harness":p.harness,"provider":p.provider,"model":p.model,
            "runtime":if p.container.is_some(){"docker"}else{"native"},
            "container":p.container,"executable_installed":if p.container.is_some(){None}else{Some(p.executable.is_file())},"credential_env_names":p.credential_env,
            "privacy":p.privacy,"budget":p.budget,"native_step_cap":p.harness == Harness::Muse,
            "live_provider_verified":false
        })).collect::<Vec<_>>();
        Ok(
            json!({"version":env!("CARGO_PKG_VERSION"),"process_id":std::process::id(),"schema_version":SCHEMA_VERSION,"now_ms":now_ms(),
            "docker":{"cli_installed":docker::cli_installed(),"local_endpoint_available":docker_client.is_some(),"daemon_reachable":null,"image_present":null,"isolation_verified":false,"probe":"required before each container start","network":"restricted public HTTPS CONNECT via private Unix IPC; worker network none"},
            "rtk_available":safety::rtk().is_ok(),"sandbox_backend":if cfg!(target_os="macos") {"seatbelt"} else {"unsupported_fail_closed"},
            "git_available":Path::new("/usr/bin/git").is_file(),
            "isolation_verified":false,"probe":"not run by doctor; required before each external start",
            "live_setup":"unverified; doctor success is not permission or proof of runnable external workers",
            "isolation":"enforced probe required before every external start; no host-home access or unsandboxed fallback",
            "managed_roots":self.config.managed_worktree_roots,"state_dir":self.store.root,"profiles":profiles,
            "capabilities":{"managed_worktree_creation":false,"native_codex_discovery":false,"monetary_cap":false,"harness_session_resume":false,
                "durable_job_status":true,"mcp_stdio":true,"one_correction":true},
            "safety":"no credentials are inspected by doctor; external profiles are disabled in the shipped config"}),
        )
    }

    pub async fn run(&self, id: &str) -> Result<()> {
        let running = self.store.running_lock(id)?;
        running
            .try_lock_exclusive()
            .context("another supervisor already owns this job")?;
        let mut job = {
            let _lock = self.store.transaction()?;
            let mut j = self.store.load(id)?;
            ensure!(j.status == JobStatus::Queued, "job is not queued");
            if self.store.cancelled(id)? {
                j.status = JobStatus::Cancelled;
                j.updated_at_ms = now_ms();
                j.result = Some(empty_result(
                    JobStatus::Cancelled,
                    "cancelled before execution",
                ));
                self.store.save(&j)?;
                return Ok(());
            }
            let current = self
                .config
                .profiles
                .get(&j.task.profile)
                .context("profile was removed before execution")?;
            ensure!(
                current.enabled
                    && serde_json::to_value(current)? == serde_json::to_value(&j.profile)?,
                "profile changed or was disabled before execution; refresh the task contract"
            );
            ensure!(
                j.profile.harness != Harness::Simulated || self.config.allow_simulated_workers,
                "simulation permission was removed"
            );
            j.status = JobStatus::Running;
            j.supervisor_pid = Some(std::process::id());
            j.updated_at_ms = now_ms();
            self.store.save(&j)?;
            j
        };
        let run = self.run_attempt(&mut job).await;
        if run.is_err() {
            let _ = self.cleanup_containers(id).await;
            self.finish_failure(id, JobStatus::Failed, "attempt failed validation or execution; diagnostics intentionally omit worker/config values")?;
        }
        run
    }

    async fn run_attempt(&self, job: &mut Job) -> Result<()> {
        if job.profile.container.is_some() {
            return self.run_container_attempt(job).await;
        }
        let scratch = self
            .store
            .job_dir(&job.id)?
            .join(format!("attempt-{}", job.attempt));
        store::private_dir(&scratch)?;
        store::private_dir(&scratch.join("home"))?;
        store::private_dir(&scratch.join("tmp"))?;
        let ownership=self.store.jobs()?.iter().filter(|j|j.status.active() || j.recovery_required).map(|j|json!({"job_id":j.id,"repository":j.repository_identity,"writable_paths":j.task.writable_paths})).collect::<Vec<_>>();
        store::atomic_json(&scratch.join("ownership.json"), &ownership)?;
        let invocation = adapter::build(job, &scratch)?;
        let started = Instant::now();
        let outcome = self
            .run_process(
                job,
                &scratch,
                &invocation.executable,
                &invocation.args,
                Duration::from_secs(job.profile.budget.timeout_seconds),
                true,
            )
            .await?;
        let mut result = empty_result(outcome.status.clone(), "");
        result.process_exit_code = outcome.exit_code;
        result.output_bytes = outcome.events.bytes;
        result.model_steps_observed = outcome.events.steps;
        result.malformed_events = outcome.events.malformed;
        result.worker_claimed_test_ids = outcome.events.tests;
        result.completed_criteria_claims = outcome.events.criteria;
        if result.status == JobStatus::Completed
            && result.malformed_events > 0
            && job.profile.harness != Harness::Aider
        {
            result.status = JobStatus::Failed;
            result
                .blockers
                .push("malformed structured worker event; raw contents discarded".into());
        }
        if self.store.cancelled(&job.id)? {
            result.status = JobStatus::Cancelled;
        }
        match safety::snapshot(
            &job.task.workspace.path,
            512 * 1024 * 1024,
            &job.task.writable_paths,
            &job.task.read_paths,
        )
        .await
        {
            Ok(after) => {
                result.changes = safety::changes(&job.baseline, &after, &job.task.writable_paths);
                let changed_bytes: u64 = result
                    .changes
                    .iter()
                    .filter_map(|c| after.get(&c.path))
                    .map(|f| f.bytes)
                    .sum();
                job.last_snapshot = after;
                if result.changes.iter().any(|c| !c.in_scope) {
                    result.status = JobStatus::ScopeViolation;
                    result
                        .blockers
                        .push("out-of-scope change detected; no automatic integration".into());
                }
                if changed_bytes > job.profile.budget.max_change_bytes as u64 {
                    result.status = JobStatus::ScopeViolation;
                    result
                        .blockers
                        .push("changed-file byte budget exceeded".into());
                }
            }
            Err(_) => {
                result.status = JobStatus::ScopeViolation;
                result.blockers.push("post-run inventory rejected symlink, hardlink, sensitive content, or an oversized source; result quarantined".into());
            }
        }
        if result.status == JobStatus::Completed {
            for test in &job.task.tests {
                if self.store.cancelled(&job.id)? {
                    result.status = JobStatus::Cancelled;
                    break;
                }
                let remaining = Duration::from_secs(job.profile.budget.timeout_seconds)
                    .saturating_sub(started.elapsed());
                if remaining.is_zero() {
                    result.status = JobStatus::TimedOut;
                    break;
                }
                let mut test_job = job.clone();
                test_job.profile.budget.max_output_bytes = job
                    .profile
                    .budget
                    .max_output_bytes
                    .saturating_sub(result.output_bytes);
                let observed = self
                    .run_process(
                        &test_job,
                        &scratch,
                        &test.program,
                        &test.args,
                        remaining.min(Duration::from_secs(test.timeout_seconds)),
                        false,
                    )
                    .await?;
                result.output_bytes = result.output_bytes.saturating_add(observed.events.bytes);
                let success = observed.status == JobStatus::Completed;
                result.independently_observed_tests.push(ObservedTest {
                    id: test.id.clone(),
                    exit_code: observed.exit_code,
                    outcome: if success {
                        "passed"
                    } else {
                        "failed_or_bounded"
                    }
                    .into(),
                    elapsed_ms: observed.elapsed_ms,
                });
                if !success {
                    result.status = observed.status;
                    if result.status == JobStatus::Completed {
                        result.status = JobStatus::Failed;
                    }
                    break;
                }
            }
            // Verification programs are sandboxed too; examine their side effects before finalizing.
            let after = safety::snapshot(
                &job.task.workspace.path,
                512 * 1024 * 1024,
                &job.task.writable_paths,
                &job.task.read_paths,
            )
            .await?;
            result.changes = safety::changes(&job.baseline, &after, &job.task.writable_paths);
            let changed_bytes: u64 = result
                .changes
                .iter()
                .filter_map(|c| after.get(&c.path))
                .map(|f| f.bytes)
                .sum();
            if result.changes.iter().any(|c| !c.in_scope)
                || changed_bytes > job.profile.budget.max_change_bytes as u64
            {
                result.status = JobStatus::ScopeViolation;
                result.blockers.push("verification changed files outside scope or exceeded the changed-file byte budget".into());
            }
            job.last_snapshot = after;
        }
        if result.status != JobStatus::Completed && result.blockers.is_empty() {
            result.blockers.push(
                "attempt did not complete within the required lifecycle and verification contract"
                    .into(),
            );
        }
        job.status = result.status.clone();
        job.updated_at_ms = now_ms();
        job.worker = None;
        job.result = Some(result);
        let _lock = self.store.transaction()?;
        store::atomic_json(
            &self
                .store
                .job_dir(&job.id)?
                .join(format!("result-{}.json", job.attempt)),
            &job.result,
        )?;
        self.store.save(job)?;
        Ok(())
    }

    fn job_docker_client(&self, job: &Job) -> Result<docker::Client> {
        let endpoint = job
            .docker_endpoint
            .as_deref()
            .context("job has no durable local Docker endpoint; refresh the task contract")?;
        docker::Client::at(&self.store.root, endpoint)
    }

    async fn cleanup_containers(&self, id: &str) -> Result<()> {
        let mut job = self.store.load(id)?;
        if job.container.is_none() && job.broker.is_none() {
            return Ok(());
        }
        let client = self.job_docker_client(&job)?;
        if let Some(identity) = &job.container {
            client.cleanup(identity).await?;
            job.container = None;
        }
        if let Some(identity) = &job.broker {
            client.cleanup(identity).await?;
            job.broker = None;
        }
        job.recovery_required = job.import_pending;
        let _lock = self.store.transaction()?;
        self.store.save(&job)?;
        Ok(())
    }

    async fn start_broker(&self, job: &Job, scratch: &Path) -> Result<docker::Guard> {
        let config = docker::proxy_config(&job.profile).await?;
        let client = self.job_docker_client(job)?;
        let runtime = job
            .profile
            .container
            .as_ref()
            .context("broker requires Docker runtime")?;
        let dir = scratch.join("broker");
        store::private_dir(&dir)?;
        for path in ["workspace", "control", "runtime", "ipc"] {
            store::private_dir(&dir.join(path))?;
        }
        store::atomic_json(&dir.join("control/proxy.json"), &config)?;
        let mut identity = docker::identity(
            &self.store.root,
            &job.id,
            job.attempt,
            "broker",
            &client.endpoint,
        )?;
        {
            let _lock = self.store.transaction()?;
            let mut stored = self.store.load(&job.id)?;
            ensure!(
                stored.broker.is_none(),
                "prior broker cleanup is unverified"
            );
            stored.broker = Some(identity.clone());
            stored.recovery_required = true;
            self.store.save(&stored)?;
        }
        client
            .create(
                &mut identity,
                runtime,
                &dir.join("workspace"),
                &dir.join("control"),
                &dir.join("runtime"),
                Path::new("/usr/local/bin/codex-relay"),
                &["__https-proxy".into(), "/relay/control/proxy.json".into()],
                Some(&dir.join("ipc")),
                true,
                &[],
            )
            .await?;
        {
            let _lock = self.store.transaction()?;
            let mut stored = self.store.load(&job.id)?;
            stored.broker = Some(identity.clone());
            self.store.save(&stored)?;
        }
        let guard = docker::Guard::new(client.clone(), identity.clone());
        let mut command = client.command(&[
            "container".into(),
            "start".into(),
            identity.id.clone().context("broker identity missing")?,
        ])?;
        command.stdout(Stdio::null()).stderr(Stdio::null());
        ensure!(
            tokio::time::timeout(Duration::from_secs(5), command.status())
                .await??
                .success(),
            "HTTPS broker did not start"
        );
        tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                if dir.join("ipc/proxy.sock").exists() {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        })
        .await
        .context("HTTPS broker socket not ready")?;
        Ok(guard)
    }

    async fn run_container_attempt(&self, job: &mut Job) -> Result<()> {
        let scratch = self
            .store
            .job_dir(&job.id)?
            .join(format!("attempt-{}", job.attempt));
        store::private_dir(&scratch)?;
        let control = scratch.join("control");
        let runtime = scratch.join("runtime");
        let work = scratch.join("workspace");
        store::private_dir(&control)?;
        store::private_dir(&runtime)?;
        store::private_dir(&runtime.join("home"))?;
        store::private_dir(&runtime.join("tmp"))?;
        let actual = safety::snapshot(
            &job.task.workspace.path,
            512 * 1024 * 1024,
            &job.task.writable_paths,
            &job.task.read_paths,
        )
        .await?;
        ensure!(
            actual == job.last_snapshot,
            "managed worktree changed before container export"
        );
        let before = docker::export(job, &work)?;
        let ownership = self
            .store
            .jobs()?
            .iter()
            .filter(|j| j.status.active() || j.recovery_required)
            .map(|j| json!({"job_id":j.id,"writable_paths":j.task.writable_paths}))
            .collect::<Vec<_>>();
        store::atomic_json(&control.join("ownership.json"), &ownership)?;
        let invocation = adapter::build(job, &control)?;
        let prompt = control.join("prompt.txt");
        let text = std::fs::read_to_string(&prompt)?;
        std::fs::write(
            prompt,
            text.replace(&control.to_string_lossy().to_string(), docker::CONTROL)
                .replace(
                    &job.task.workspace.path.to_string_lossy().to_string(),
                    docker::WORKSPACE,
                ),
        )?;
        let args = docker::rewrite_args(&invocation.args, &job.task.workspace.path, &control)?;
        let started = Instant::now();
        let mut broker_guard = if job.profile.network_hosts.is_empty() {
            None
        } else {
            Some(self.start_broker(job, &scratch).await?)
        };
        let outcome = self
            .run_process(
                job,
                &scratch,
                &invocation.executable,
                &args,
                Duration::from_secs(job.profile.budget.timeout_seconds),
                true,
            )
            .await?;
        let mut result = empty_result(outcome.status, "");
        result.process_exit_code = outcome.exit_code;
        result.output_bytes = outcome.events.bytes;
        result.model_steps_observed = outcome.events.steps;
        result.malformed_events = outcome.events.malformed;
        result.worker_claimed_test_ids = outcome.events.tests;
        result.completed_criteria_claims = outcome.events.criteria;
        if result.status == JobStatus::Completed
            && result.malformed_events > 0
            && job.profile.harness != Harness::Aider
        {
            result.status = JobStatus::Failed;
        }
        let valid = docker::inventory(&work, &before)
            .and_then(|after| docker::validate_changes(job, &before, &after));
        if valid.is_err() {
            result.status = JobStatus::ScopeViolation;
            result.blockers.push("container copy failed scope, link, sensitive-content or byte-budget validation; managed worktree was not imported".into());
        }
        if result.status == JobStatus::Completed {
            for test in &job.task.tests {
                let remaining = Duration::from_secs(job.profile.budget.timeout_seconds)
                    .saturating_sub(started.elapsed());
                if remaining.is_zero() {
                    result.status = JobStatus::TimedOut;
                    break;
                }
                let mut test_job = job.clone();
                test_job.profile.budget.max_output_bytes = job
                    .profile
                    .budget
                    .max_output_bytes
                    .saturating_sub(result.output_bytes);
                let args = docker::rewrite_args(&test.args, &job.task.workspace.path, &control)?;
                let outcome = self
                    .run_process(
                        &test_job,
                        &scratch,
                        &test.program,
                        &args,
                        remaining.min(Duration::from_secs(test.timeout_seconds)),
                        false,
                    )
                    .await?;
                result.output_bytes = result.output_bytes.saturating_add(outcome.events.bytes);
                let success = outcome.status == JobStatus::Completed;
                result.independently_observed_tests.push(ObservedTest {
                    id: test.id.clone(),
                    exit_code: outcome.exit_code,
                    outcome: if success {
                        "passed"
                    } else {
                        "failed_or_bounded"
                    }
                    .into(),
                    elapsed_ms: outcome.elapsed_ms,
                });
                if !success {
                    result.status = outcome.status;
                    break;
                }
            }
        }
        if let Some(guard) = &mut broker_guard {
            guard.cleanup().await?;
            let _lock = self.store.transaction()?;
            let mut stored = self.store.load(&job.id)?;
            stored.broker = None;
            stored.recovery_required = stored.container.is_some();
            self.store.save(&stored)?;
        }
        // Containers are gone before filesystem inspection. Tests use the same boundary and copy.
        match docker::inventory(&work, &before)
            .and_then(|after| docker::validate_changes(job, &before, &after))
        {
            Ok(changes) => {
                result.changes = changes;
                if result.status == JobStatus::Completed {
                    let _lock = self.store.transaction()?;
                    let mut stored = self.store.load(&job.id)?;
                    ensure!(
                        stored.container.is_none() && !stored.recovery_required,
                        "container cleanup is unverified"
                    );
                    ensure!(
                        stored.status == JobStatus::Running,
                        "job ownership changed before import"
                    );
                    for other in
                        self.store.jobs()?.iter().filter(|j| {
                            j.id != job.id && (j.status.active() || j.recovery_required)
                        })
                    {
                        ensure!(
                            other.repository_identity != job.repository_identity
                                || !safety::overlaps(
                                    &other.task.writable_paths,
                                    &job.task.writable_paths
                                ),
                            "scope ownership changed before import"
                        );
                    }
                    ensure!(
                        safety::snapshot(
                            &job.task.workspace.path,
                            512 * 1024 * 1024,
                            &job.task.writable_paths,
                            &job.task.read_paths,
                        )
                        .await?
                            == actual,
                        "managed worktree changed during container attempt; import refused"
                    );
                    if self.store.cancelled(&job.id)? {
                        result.status = JobStatus::Cancelled;
                    } else {
                        if !result.changes.is_empty() {
                            stored.import_pending = true;
                            stored.recovery_required = true;
                            self.store.save(&stored)?;
                            job.import_pending = true;
                        }
                        docker::import(job, &work, &result.changes)?;
                    }
                }
            }
            Err(_) => {
                result.status = JobStatus::ScopeViolation;
                result
                    .blockers
                    .push("final container copy failed validation; changes quarantined".into());
            }
        }
        job.last_snapshot = safety::snapshot(
            &job.task.workspace.path,
            512 * 1024 * 1024,
            &job.task.writable_paths,
            &job.task.read_paths,
        )
        .await?;
        job.import_pending = false;
        if result.status == JobStatus::Completed {
            result.changes =
                safety::changes(&job.baseline, &job.last_snapshot, &job.task.writable_paths);
        }
        if result.status != JobStatus::Completed && result.blockers.is_empty() {
            result.blockers.push("container attempt did not complete its bounded verification contract; changes remain in disposable copy".into());
        }
        job.status = result.status.clone();
        job.updated_at_ms = now_ms();
        job.worker = None;
        job.container = None;
        job.broker = None;
        job.recovery_required = false;
        job.result = Some(result);
        let _lock = self.store.transaction()?;
        store::atomic_json(
            &self
                .store
                .job_dir(&job.id)?
                .join(format!("result-{}.json", job.attempt)),
            &job.result,
        )?;
        self.store.save(job)?;
        Ok(())
    }

    async fn container_isolation_probe(&self, runtime: &DockerRuntime) -> Result<()> {
        let client = docker::Client::local(&self.store.root)?;
        client.available(&runtime.image).await?;
        // Serialize the synthetic probe and persist its cleanup identity before create.
        let _lock = self.store.transaction()?;
        let record = self.store.root.join("container-probe.json");
        if record.exists() {
            let old: ContainerIdentity = store::read_json(&record, 8192)?;
            client.cleanup(&old).await?;
            std::fs::remove_file(&record)?;
        }
        let id = uuid::Uuid::new_v4().to_string();
        let dir = self.store.root.join(format!("container-probe-{id}"));
        store::private_dir(&dir)?;
        for path in ["workspace", "control", "runtime"] {
            store::private_dir(&dir.join(path))?;
        }
        let denied = dir.join("denied-host-marker");
        std::fs::write(&denied, b"synthetic isolation marker")?;
        let mut identity = docker::identity(&self.store.root, &id, 0, "probe", &client.endpoint)?;
        store::atomic_json(&record, &identity)?;
        let create = client
            .create(
                &mut identity,
                runtime,
                &dir.join("workspace"),
                &dir.join("control"),
                &dir.join("runtime"),
                Path::new("/usr/local/bin/codex-relay"),
                &[
                    "__container-probe".into(),
                    denied.to_string_lossy().into_owned(),
                    docker::SCRATCH.into(),
                ],
                None,
                false,
                &[],
            )
            .await;
        if create.is_err() {
            client.cleanup(&identity).await?;
            std::fs::remove_file(&record)?;
            let _ = std::fs::remove_dir_all(&dir);
            create?;
        }
        store::atomic_json(&record, &identity)?;
        let mut guard = docker::Guard::new(client.clone(), identity.clone());
        let mut command = client.attach(&identity)?;
        command.stdout(Stdio::piped()).stderr(Stdio::null());
        let mut child = command.spawn()?;
        let mut stdout = child.stdout.take().context("probe stdout missing")?;
        let probe = tokio::time::timeout(Duration::from_secs(5), async {
            let mut bytes = Vec::new();
            (&mut stdout).take(64).read_to_end(&mut bytes).await?;
            ensure!(
                child.wait().await?.success() && bytes == b"isolated\n",
                "container isolation attestation absent or invalid"
            );
            Ok::<_, anyhow::Error>(())
        })
        .await;
        guard.cleanup().await?;
        ensure!(
            std::fs::read(&denied)? == b"synthetic isolation marker",
            "container modified denied host marker"
        );
        std::fs::remove_file(&record)?;
        let _ = std::fs::remove_dir_all(&dir);
        probe.context("container isolation probe timed out")??;
        Ok(())
    }

    async fn run_process(
        &self,
        job: &Job,
        scratch: &Path,
        program: &Path,
        args: &[String],
        timeout: Duration,
        worker_events: bool,
    ) -> Result<Outcome> {
        let started = Instant::now();
        let simulated = job.profile.harness == Harness::Simulated;
        let mut container_guard = None;
        let mut cmd = if let Some(runtime) = &job.profile.container {
            let client = self.job_docker_client(job)?;
            client.available(&runtime.image).await?;
            let stage = if worker_events { "worker" } else { "test" };
            let mut identity = docker::identity(
                &self.store.root,
                &job.id,
                job.attempt,
                stage,
                &client.endpoint,
            )?;
            {
                let _lock = self.store.transaction()?;
                let mut stored = self.store.load(&job.id)?;
                ensure!(
                    stored.container.is_none()
                        && (!stored.recovery_required || stored.broker.is_some()),
                    "prior container cleanup is unverified"
                );
                stored.container = Some(identity.clone());
                stored.recovery_required = true;
                self.store.save(&stored)?;
            }
            let broker = self.store.load(&job.id)?.broker.is_some();
            let ipc = scratch.join("broker/ipc");
            let keys = if worker_events {
                job.profile.credential_env.as_slice()
            } else {
                &[]
            };
            client
                .create(
                    &mut identity,
                    runtime,
                    &scratch.join("workspace"),
                    &scratch.join("control"),
                    &scratch.join("runtime"),
                    program,
                    args,
                    if broker && worker_events {
                        Some(&ipc)
                    } else {
                        None
                    },
                    false,
                    keys,
                )
                .await?;
            {
                let _lock = self.store.transaction()?;
                let mut stored = self.store.load(&job.id)?;
                stored.container = Some(identity.clone());
                self.store.save(&stored)?;
            }
            let command = client.attach(&identity)?;
            container_guard = Some(docker::Guard::new(client, identity));
            command
        } else if simulated {
            if worker_events {
                ensure!(
                    program == self.binary
                        && args
                            == [
                                "__fixture".to_string(),
                                scratch
                                    .join("simulation.json")
                                    .to_string_lossy()
                                    .into_owned()
                            ],
                    "native fixture worker must use the exact built-in invocation"
                );
            } else {
                ensure!(
                    [Path::new("/usr/bin/true"), Path::new("/usr/bin/false")].contains(&program)
                        && args.is_empty(),
                    "simulated verification permits only true/false with empty argv"
                );
            }
            safety::command(program, args)?
        } else {
            let peers = resolve_peers(&job.profile).await?;
            let policy = safety::seatbelt_policy(
                &job.task.workspace.path,
                scratch,
                &job.task.writable_paths,
                &job.profile,
                program,
                &peers,
            )?;
            let policy_file = scratch.join("sandbox.sb");
            std::fs::write(&policy_file, policy)?;
            let mut vector = vec![
                "-f".into(),
                policy_file.to_string_lossy().into_owned(),
                program.to_string_lossy().into_owned(),
            ];
            vector.extend(args.iter().cloned());
            safety::command(Path::new("/usr/bin/sandbox-exec"), &vector)?
        };
        cmd.current_dir(&job.task.workspace.path)
            .env("HOME", scratch.join("home"))
            .env("XDG_CONFIG_HOME", scratch.join("home/config"))
            .env("TMPDIR", scratch.join("tmp"))
            .env("TMP", scratch.join("tmp"))
            .env("TEMP", scratch.join("tmp"))
            .env("NO_COLOR", "1")
            .env("CI", "true")
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .process_group(0)
            .kill_on_drop(true);
        // A future authorized operator supplies only the profile's named keys. No other environment is inherited.
        if !simulated && worker_events && job.profile.container.is_none() {
            for name in &job.profile.credential_env {
                let value = std::env::var(name).map_err(|_| {
                    anyhow::anyhow!(
                        "a named credential is unavailable; no credential was created or inspected"
                    )
                })?;
                cmd.env(name, value);
            }
        }
        let mut child = cmd.spawn().context("worker process could not launch")?;
        let pid = child.id().context("missing worker pid")?;
        let _tree = ProcessTree(pid);
        let birth = process_birth(pid)
            .await
            .unwrap_or_else(|| "unavailable".into());
        {
            let _lock = self.store.transaction()?;
            let mut stored = self.store.load(&job.id)?;
            stored.worker = Some(ProcessIdentity { pid, birth });
            stored.updated_at_ms = now_ms();
            self.store.save(&stored)?;
        }
        let (tx, mut rx) = mpsc::channel::<(bool, Vec<u8>)>(16);
        let stdout = child.stdout.take().context("missing stdout")?;
        let stderr = child.stderr.take().context("missing stderr")?;
        let a = tokio::spawn(pump(stdout, tx.clone(), true));
        let b = tokio::spawn(pump(stderr, tx, false));
        let mut events = Events::default();
        let mut status;
        let mut exit_code = None;
        let mut channel_open = true;
        let mut ticker = tokio::time::interval(Duration::from_millis(25));
        loop {
            tokio::select! {
                chunk=rx.recv(),if channel_open=> {
                    if let Some((stdout,data))=chunk {
                        events.bytes=events.bytes.saturating_add(data.len());
                        if events.bytes > job.profile.budget.max_output_bytes {status=JobStatus::OutputLimit;break;}
                        if stdout && worker_events && job.profile.harness != Harness::Aider { consume_events(&mut events,&data,job); }
                        if events.steps > job.profile.budget.max_model_steps {status=JobStatus::StepLimit;break;}
                    } else {channel_open=false;}
                }
                _=ticker.tick()=> {
                    if self.store.cancelled(&job.id)? {status=JobStatus::Cancelled;break;}
                    if started.elapsed() >= timeout {status=JobStatus::TimedOut;break;}
                    if let Some(exit)=child.try_wait()? {
                        exit_code=exit.code(); status=if exit.success() {JobStatus::Completed} else {JobStatus::Failed}; break;
                    }
                }
            }
        }
        if let Some(guard) = &mut container_guard {
            guard.cleanup().await.context(
                "whole-container cleanup could not be verified; ownership remains reserved",
            )?;
            let _lock = self.store.transaction()?;
            let mut stored = self.store.load(&job.id)?;
            stored.container = None;
            stored.recovery_required = stored.broker.is_some();
            self.store.save(&stored)?;
        }
        // Always kill the whole inherited group, including descendants left after a successful parent exit.
        kill_group(pid);
        let _ = tokio::time::timeout(Duration::from_secs(2), child.wait()).await;
        let _ = tokio::time::timeout(Duration::from_secs(2), a).await;
        let _ = tokio::time::timeout(Duration::from_secs(2), b).await;
        while let Ok((stdout, data)) = rx.try_recv() {
            events.bytes = events.bytes.saturating_add(data.len());
            if events.bytes <= job.profile.budget.max_output_bytes
                && stdout
                && worker_events
                && job.profile.harness != Harness::Aider
            {
                consume_events(&mut events, &data, job);
            }
        }
        if events.bytes > job.profile.budget.max_output_bytes && status == JobStatus::Completed {
            status = JobStatus::OutputLimit;
        }
        if events.steps > job.profile.budget.max_model_steps && status == JobStatus::Completed {
            status = JobStatus::StepLimit;
        }
        if !events.buffer.is_empty() && worker_events && job.profile.harness != Harness::Aider {
            events.malformed += 1;
        }
        Ok(Outcome {
            status,
            exit_code,
            events,
            elapsed_ms: started.elapsed().as_millis() as u64,
        })
    }

    fn finish_failure(&self, id: &str, status: JobStatus, blocker: &str) -> Result<()> {
        let _lock = self.store.transaction()?;
        let mut job = self.store.load(id)?;
        job.status = status.clone();
        job.updated_at_ms = now_ms();
        job.result = Some(empty_result(status, blocker));
        self.store.save(&job)
    }

    pub async fn isolation_probe(&self, task: &Task, profile: &Profile) -> Result<()> {
        if let Some(runtime) = &profile.container {
            return self.container_isolation_probe(runtime).await;
        }
        ensure!(
            cfg!(target_os = "macos"),
            "no supported OS isolation backend; execution refused"
        );
        let dir = self
            .store
            .root
            .join(format!("probe-{}", uuid::Uuid::new_v4()));
        store::private_dir(&dir)?;
        let scratch = dir.join("scratch");
        store::private_dir(&scratch)?;
        let denied = dir.join("denied-marker");
        std::fs::write(&denied, b"synthetic probe only")?;
        let policy = safety::seatbelt_policy(
            &task.workspace.path,
            &scratch,
            &task.writable_paths,
            profile,
            &self.binary,
            &[],
        )?;
        let path = dir.join("probe.sb");
        std::fs::write(&path, policy)?;
        let mut cmd = safety::command(
            Path::new("/usr/bin/sandbox-exec"),
            &[
                "-f".into(),
                path.to_string_lossy().into_owned(),
                self.binary.to_string_lossy().into_owned(),
                "__isolation-probe".into(),
                denied.to_string_lossy().into_owned(),
                scratch.to_string_lossy().into_owned(),
            ],
        )?;
        let outcome = tokio::time::timeout(Duration::from_secs(5), cmd.output()).await;
        // Never accept a merely present sandbox executable or successful unrestricted run.
        let ok = matches!(outcome,Ok(Ok(ref o)) if o.status.success() && o.stdout == b"isolated\n");
        let _ = std::fs::remove_dir_all(&dir);
        if !ok {
            let detail = match outcome {
                Ok(Ok(output)) => {
                    let stderr = String::from_utf8_lossy(&output.stderr);
                    let known = [
                        "sandbox permits session detachment",
                        "sandbox permits process-group detachment",
                        "probe cannot establish a non-leader process",
                        "RTK is required",
                        "probe scratch write failed",
                        "probe child could not launch",
                        "sandbox allowed denied read",
                        "sandbox allowed denied write",
                    ]
                    .into_iter()
                    .find(|message| stderr.contains(message));
                    format!(
                        "launcher status {}; {}",
                        output.status,
                        known.unwrap_or("attestation absent or invalid")
                    )
                }
                Ok(Err(_)) => "sandbox launcher unavailable".into(),
                Err(_) => "probe exceeded five-second timeout".into(),
            };
            anyhow::bail!(
                "OS isolation probe failed ({detail}): external start refused; no unsandboxed fallback. See docs/isolation.md"
            );
        }
        Ok(())
    }
}

fn consume_events(events: &mut Events, data: &[u8], job: &Job) {
    for byte in data {
        if *byte == b'\n' {
            let parsed = serde_json::from_slice::<Value>(&events.buffer);
            events.buffer.clear();
            match parsed {
                Ok(v) => {
                    if v.get("kind").and_then(Value::as_str) == Some("model_step") {
                        events.steps = events.steps.saturating_add(1);
                    }
                    if v.get("kind").and_then(Value::as_str) == Some("worker_report") {
                        for (field, target) in [
                            ("claimed_test_ids", &mut events.tests),
                            ("completed_criteria_ids", &mut events.criteria),
                        ] {
                            if let Some(ids) = v.get(field).and_then(Value::as_array) {
                                for id in ids.iter().take(32).filter_map(Value::as_str) {
                                    let known = if field == "claimed_test_ids" {
                                        job.task.tests.iter().any(|t| t.id == id)
                                    } else {
                                        job.task.acceptance_criteria.contains_key(id)
                                    };
                                    if known && !target.iter().any(|s| s == id) {
                                        target.push(id.into());
                                    }
                                }
                            }
                        }
                    }
                    // Unknown valid JSON events are counted as bytes, never persisted or trusted.
                }
                Err(_) => events.malformed += 1,
            }
        } else if events.buffer.len() < 8192 {
            events.buffer.push(*byte);
        } else {
            events.buffer.clear();
            events.malformed += 1;
        }
    }
}

async fn pump(
    mut input: impl tokio::io::AsyncRead + Unpin,
    tx: mpsc::Sender<(bool, Vec<u8>)>,
    stdout: bool,
) {
    let mut buffer = [0; 4096];
    loop {
        match input.read(&mut buffer).await {
            Ok(0) | Err(_) => break,
            Ok(n) => {
                if tx.send((stdout, buffer[..n].to_vec())).await.is_err() {
                    break;
                }
            }
        }
    }
}

pub fn public_status(job: &Job) -> Value {
    json!({"job_id":job.id,"status":job.status,"attempt":job.attempt,"profile":job.task.profile,
    "workspace":job.task.workspace.path,"writable_paths":job.task.writable_paths,"updated_at_ms":job.updated_at_ms,
    "recovery_required":job.recovery_required,"result_available":job.result.is_some()})
}

fn empty_result(status: JobStatus, blocker: &str) -> WorkerResult {
    WorkerResult {status,process_exit_code:None,changes:vec![],worker_claimed_test_ids:vec![],
    independently_observed_tests:vec![],completed_criteria_claims:vec![],output_bytes:0,model_steps_observed:0,malformed_events:0,
    blockers:if blocker.is_empty(){vec![]}else{vec![blocker.into()]},correctness:"requires_coordinator_review_and_integration_gate".into(),
    continuation:"one fresh correction permitted; provider session resume and automatic replay are intentionally unsupported".into(),
    route_receipt:"not_observed: CLI request configuration is not proof of the selected upstream model/provider; no cross-model fallback list beyond the one selected model is generated".into()}
}

pub fn kill_group(pid: u32) {
    if pid > 1 {
        unsafe {
            libc::kill(-(pid as i32), libc::SIGKILL);
        }
    }
}
fn group_exists(pid: u32) -> bool {
    pid > 1 && unsafe { libc::kill(-(pid as i32), 0) } == 0
}
struct ProcessTree(u32);
impl Drop for ProcessTree {
    fn drop(&mut self) {
        kill_group(self.0);
    }
}
async fn process_birth(pid: u32) -> Option<String> {
    #[cfg(target_os = "macos")]
    {
        let mut info = std::mem::MaybeUninit::<libc::proc_bsdinfo>::zeroed();
        let size = std::mem::size_of::<libc::proc_bsdinfo>();
        // Kernel birth time prevents signalling a reused PID during recovery.
        let read = unsafe {
            libc::proc_pidinfo(
                pid as i32,
                libc::PROC_PIDTBSDINFO,
                0,
                info.as_mut_ptr().cast(),
                size as i32,
            )
        };
        if read != size as i32 {
            return None;
        }
        let info = unsafe { info.assume_init() };
        Some(format!(
            "{}:{}",
            info.pbi_start_tvsec, info.pbi_start_tvusec
        ))
    }
    #[cfg(target_os = "linux")]
    {
        let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
        stat.rsplit_once(')')?
            .1
            .split_whitespace()
            .nth(19)
            .map(String::from)
    }
    #[cfg(not(any(target_os = "macos", target_os = "linux")))]
    {
        let _ = pid;
        None
    }
}
async fn resolve_peers(profile: &Profile) -> Result<Vec<std::net::SocketAddr>> {
    let mut peers = vec![];
    for host in &profile.network_hosts {
        for peer in tokio::time::timeout(
            Duration::from_secs(5),
            tokio::net::lookup_host((host.as_str(), 443)),
        )
        .await??
        {
            let private = match peer.ip() {
                std::net::IpAddr::V4(ip) => {
                    ip.is_private() || ip.is_loopback() || ip.is_link_local()
                }
                std::net::IpAddr::V6(ip) => {
                    ip.is_loopback() || ip.is_unique_local() || ip.is_unicast_link_local()
                }
            };
            ensure!(
                !private,
                "provider DNS resolved to a private/local address; network denied"
            );
            peers.push(peer);
        }
    }
    Ok(peers)
}
