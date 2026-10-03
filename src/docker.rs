//! Optional offline Docker boundary. Only sanitized copies cross the container boundary.
use crate::{model::*, safety, store};
use anyhow::{Context, Result, ensure};
use sha2::{Digest, Sha256};
use std::{
    collections::BTreeMap,
    path::{Path, PathBuf},
    process::Stdio,
    time::Duration,
};
use tokio::{io::AsyncReadExt, process::Command};

const OWNER_LABEL: &str = "org.codex-relay.owner";
const JOB_LABEL: &str = "org.codex-relay.job";
const NAME_LABEL: &str = "org.codex-relay.name";
pub const WORKSPACE: &str = "/relay/workspace";
pub const CONTROL: &str = "/relay/control";
pub const SCRATCH: &str = "/relay/scratch";

pub fn validate_runtime(runtime: &DockerRuntime) -> Result<()> {
    let (repository, digest) = runtime
        .image
        .rsplit_once("sha256:")
        .context("container image must have an immutable sha256 digest")?;
    ensure!(
        digest.len() == 64
            && digest
                .chars()
                .all(|c| c.is_ascii_hexdigit() && !c.is_ascii_uppercase()),
        "invalid image digest"
    );
    ensure!(
        repository.is_empty()
            || (repository.ends_with('@')
                && repository[..repository.len() - 1]
                    .chars()
                    .all(|c| c.is_ascii_alphanumeric() || "/._-:".contains(c))
                && !repository.starts_with('-')),
        "invalid immutable image reference"
    );
    ensure!(
        (64..=4096).contains(&runtime.memory_mb),
        "container memory limit must be 64..4096 MiB"
    );
    ensure!(
        (16..=256).contains(&runtime.pids_limit),
        "container PID limit must be 16..256"
    );
    ensure!(
        (1..=4).contains(&runtime.cpus),
        "container CPU limit must be 1..4"
    );
    Ok(())
}

pub fn cli_installed() -> bool {
    [
        "/opt/homebrew/bin/docker",
        "/usr/local/bin/docker",
        "/usr/bin/docker",
        "/Applications/Docker.app/Contents/Resources/bin/docker",
    ]
    .into_iter()
    .any(|p| Path::new(p).is_file())
}

#[derive(Clone)]
pub struct Client {
    cli: PathBuf,
    config: PathBuf,
    pub endpoint: String,
}
impl Client {
    pub fn local(state: &Path) -> Result<Self> {
        let mut sockets = vec![PathBuf::from("/var/run/docker.sock")];
        if let Some(home) = std::env::var_os("HOME") {
            sockets.push(PathBuf::from(home).join(".docker/run/docker.sock"));
        }
        let socket = sockets
            .into_iter()
            .find_map(|p| std::fs::canonicalize(p).ok())
            .context("local Docker Unix socket is unavailable; no service was started")?;
        Self::at(
            state,
            &format!(
                "unix://{}",
                socket.to_str().context("non-UTF-8 Docker socket")?
            ),
        )
    }

    /// Explicit durable handoff. Never reads HOME, Docker contexts, auth or owner config.
    pub fn at(state: &Path, endpoint: &str) -> Result<Self> {
        let raw = endpoint
            .strip_prefix("unix://")
            .context("Docker endpoint must use a local Unix socket")?;
        ensure!(
            !raw.contains(['\0', '\n', '\r', '?', '#']),
            "invalid local Docker endpoint"
        );
        let socket = Path::new(raw);
        ensure!(
            socket.is_absolute() && std::fs::canonicalize(socket)? == socket,
            "Docker endpoint must be a canonical local Unix socket"
        );
        #[cfg(unix)]
        {
            use std::os::unix::fs::FileTypeExt;
            ensure!(
                socket.metadata()?.file_type().is_socket(),
                "Docker endpoint must be a local Unix socket"
            );
        }
        let config = state.join("docker-empty-config");
        store::private_dir(&config)?;
        ensure!(
            std::fs::read_dir(&config)?.next().is_none(),
            "Docker runtime config must remain empty"
        );
        let cli = [
            "/opt/homebrew/bin/docker",
            "/usr/local/bin/docker",
            "/usr/bin/docker",
            "/Applications/Docker.app/Contents/Resources/bin/docker",
        ]
        .into_iter()
        .map(PathBuf::from)
        .find(|p| p.is_file())
        .context("Docker CLI is not installed at a supported path")?;
        Ok(Self {
            cli,
            config,
            endpoint: endpoint.into(),
        })
    }

    pub fn command(&self, args: &[String]) -> Result<Command> {
        let mut vector = vec!["--host".into(), self.endpoint.clone()];
        vector.extend_from_slice(args);
        let mut cmd = safety::command(&self.cli, &vector)?;
        // Do not consult owner Docker contexts/configuration or inherit credential helpers.
        cmd.env("DOCKER_CONFIG", &self.config)
            .stdin(Stdio::null())
            .kill_on_drop(true);
        Ok(cmd)
    }
    async fn output(&self, args: &[String]) -> Result<Vec<u8>> {
        self.output_with_keys(args, &[]).await
    }
    async fn output_with_keys(&self, args: &[String], keys: &[String]) -> Result<Vec<u8>> {
        let mut cmd = self.command(args)?;
        for name in keys {
            let value = std::env::var(name)
                .map_err(|_| anyhow::anyhow!("named worker credential unavailable"))?;
            cmd.env(name, value);
        }
        cmd.stdout(Stdio::piped()).stderr(Stdio::null());
        let mut child = cmd
            .spawn()
            .context("Docker management command could not launch")?;
        let mut stdout = child.stdout.take().context("Docker stdout missing")?;
        tokio::time::timeout(Duration::from_secs(10), async {
            let mut bytes = Vec::new();
            (&mut stdout)
                .take(64 * 1024 + 1)
                .read_to_end(&mut bytes)
                .await?;
            ensure!(
                bytes.len() <= 64 * 1024,
                "Docker management output exceeded its bound"
            );
            ensure!(
                child.wait().await?.success(),
                "Docker management command failed; raw diagnostics withheld"
            );
            Ok::<_, anyhow::Error>(bytes)
        })
        .await
        .context("Docker management command timed out")?
    }
    pub async fn available(&self, image: &str) -> Result<()> {
        ensure!(
            self.output(&["info".into(), "--format".into(), "{{.OSType}}".into()])
                .await?
                == b"linux\n",
            "Docker requires a local Linux container daemon"
        );
        let id = self
            .output(&[
                "image".into(),
                "inspect".into(),
                "--format".into(),
                "{{.Id}}".into(),
                image.into(),
            ])
            .await?;
        ensure!(
            String::from_utf8_lossy(&id).trim().starts_with("sha256:"),
            "immutable image is not installed; Relay never pulls an image at launch"
        );
        Ok(())
    }
    async fn owned_id(&self, identity: &ContainerIdentity) -> Result<Option<String>> {
        ensure!(
            identity.endpoint == self.endpoint,
            "container daemon identity changed; manual recovery required"
        );
        let list = self
            .output(&[
                "container".into(),
                "ls".into(),
                "--all".into(),
                "--no-trunc".into(),
                "--filter".into(),
                format!("name=^/{}$", identity.name),
                "--format".into(),
                "{{.ID}}".into(),
            ])
            .await?;
        let list = String::from_utf8(list)?;
        let ids = list.lines().filter(|s| !s.is_empty()).collect::<Vec<_>>();
        if ids.is_empty() {
            return Ok(None);
        }
        ensure!(
            ids.len() == 1 && ids[0].len() == 64 && ids[0].chars().all(|c| c.is_ascii_hexdigit()),
            "ambiguous container identity"
        );
        if let Some(id) = &identity.id {
            ensure!(
                id == ids[0],
                "container name was reused; manual recovery required"
            );
        }
        let format = format!(
            "{{{{ index .Config.Labels \"{OWNER_LABEL}\" }}}}|{{{{ index .Config.Labels \"{JOB_LABEL}\" }}}}|{{{{ index .Config.Labels \"{NAME_LABEL}\" }}}}"
        );
        let labels = self
            .output(&[
                "container".into(),
                "inspect".into(),
                "--format".into(),
                format,
                ids[0].into(),
            ])
            .await?;
        ensure!(
            String::from_utf8_lossy(&labels).trim()
                == format!("{}|{}|{}", identity.owner, identity.job_id, identity.name),
            "container ownership labels mismatch; cleanup refused"
        );
        Ok(Some(ids[0].into()))
    }
    pub async fn cleanup(&self, identity: &ContainerIdentity) -> Result<()> {
        if let Some(id) = self.owned_id(identity).await? {
            self.output(&["container".into(), "rm".into(), "--force".into(), id])
                .await?;
        }
        ensure!(
            self.owned_id(identity).await?.is_none(),
            "container cleanup could not be verified"
        );
        Ok(())
    }
    #[allow(clippy::too_many_arguments)]
    pub async fn create(
        &self,
        identity: &mut ContainerIdentity,
        runtime: &DockerRuntime,
        work: &Path,
        control: &Path,
        scratch: &Path,
        executable: &Path,
        args: &[String],
        ipc: Option<&Path>,
        broker: bool,
        keys: &[String],
    ) -> Result<()> {
        ensure!(
            self.owned_id(identity).await?.is_none(),
            "container name already exists; no implicit reuse"
        );
        let mut entry_args = Vec::new();
        let entrypoint = if broker {
            executable
        } else {
            entry_args.push("__container-worker".into());
            for name in keys {
                entry_args.extend(["--key".into(), name.clone()]);
            }
            entry_args.extend(["--".into(), executable.to_string_lossy().into_owned()]);
            entry_args.extend_from_slice(args);
            Path::new("/usr/local/bin/codex-relay")
        };
        let mut argv = create_args(
            identity,
            runtime,
            work,
            control,
            scratch,
            entrypoint,
            if broker { args } else { &entry_args },
        )?;
        let image_index = argv
            .iter()
            .position(|s| s == &runtime.image)
            .context("image argv missing")?;
        let mut extra = Vec::new();
        if let Some(ipc) = ipc {
            ensure!(
                !ipc.to_string_lossy().contains([',', '\n', '\r']),
                "invalid broker IPC mount"
            );
            extra.extend([
                "--mount".into(),
                format!(
                    "type=bind,src={},dst=/relay/ipc{}",
                    ipc.display(),
                    if broker { "" } else { ",readonly" }
                ),
            ]);
            if !broker {
                extra.extend(["--env".into(), "RELAY_REQUIRE_PROXY=1".into()]);
            }
        }
        for name in keys {
            extra.extend(["--env".into(), name.clone()]);
        }
        argv.splice(image_index..image_index, extra);
        if broker {
            let network = argv
                .iter_mut()
                .find(|s| s.as_str() == "--network=none")
                .context("network option missing")?;
            *network = "--network=bridge".into();
        }
        let out = self.output_with_keys(&argv, keys).await?;
        let id = String::from_utf8(out)?.trim().to_string();
        ensure!(
            id.len() == 64 && id.chars().all(|c| c.is_ascii_hexdigit()),
            "invalid created container identity"
        );
        identity.id = Some(id);
        ensure!(
            self.owned_id(identity).await?.is_some(),
            "created container ownership is unverified"
        );
        Ok(())
    }
    pub fn attach(&self, identity: &ContainerIdentity) -> Result<Command> {
        self.command(&[
            "container".into(),
            "start".into(),
            "--attach".into(),
            identity
                .id
                .clone()
                .context("container has not been created")?,
        ])
    }
}

pub fn identity(
    state: &Path,
    job: &str,
    attempt: u8,
    stage: &str,
    endpoint: &str,
) -> Result<ContainerIdentity> {
    ensure!(
        uuid::Uuid::parse_str(job).is_ok() && safety::valid_id(stage),
        "invalid durable container identity"
    );
    let owner = format!("{:x}", Sha256::digest(state.to_string_lossy().as_bytes()));
    Ok(ContainerIdentity {
        name: format!("relay-{}-{job}-{attempt}-{stage}", &owner[..12]),
        owner,
        job_id: job.into(),
        endpoint: endpoint.into(),
        id: None,
    })
}

#[allow(clippy::too_many_arguments)]
pub fn create_args(
    identity: &ContainerIdentity,
    runtime: &DockerRuntime,
    work: &Path,
    control: &Path,
    scratch: &Path,
    executable: &Path,
    args: &[String],
) -> Result<Vec<String>> {
    validate_runtime(runtime)?;
    ensure!(
        executable.is_absolute()
            && !executable
                .components()
                .any(|c| matches!(c, std::path::Component::ParentDir)),
        "container executable must be an absolute in-image path"
    );
    let uid = unsafe { libc::getuid() };
    let gid = unsafe { libc::getgid() };
    ensure!(
        uid != 0,
        "container execution requires a non-root host user"
    );
    let mut out = vec![
        "container".into(),
        "create".into(),
        "--name".into(),
        identity.name.clone(),
        "--pull=never".into(),
        "--network=none".into(),
        "--read-only".into(),
        "--cap-drop=ALL".into(),
        "--security-opt=no-new-privileges:true".into(),
        "--pids-limit".into(),
        runtime.pids_limit.to_string(),
        "--memory".into(),
        format!("{}m", runtime.memory_mb),
        "--memory-swap".into(),
        format!("{}m", runtime.memory_mb),
        "--cpus".into(),
        runtime.cpus.to_string(),
        "--user".into(),
        format!("{uid}:{gid}"),
        "--init".into(),
        "--restart=no".into(),
        "--log-driver=none".into(),
        "--workdir".into(),
        WORKSPACE.into(),
        "--entrypoint".into(),
        executable.to_string_lossy().into_owned(),
    ];
    for (name, value) in [
        (OWNER_LABEL, &identity.owner),
        (JOB_LABEL, &identity.job_id),
        (NAME_LABEL, &identity.name),
    ] {
        out.extend(["--label".into(), format!("{name}={value}")]);
    }
    for (host, dest, readonly) in [
        (work, WORKSPACE, false),
        (control, CONTROL, true),
        (scratch, SCRATCH, false),
    ] {
        ensure!(
            host.is_absolute() && !host.to_string_lossy().contains([',', '\n', '\r']),
            "invalid container scratch mount"
        );
        out.extend([
            "--mount".into(),
            format!(
                "type=bind,src={},dst={dest}{}",
                host.display(),
                if readonly { ",readonly" } else { "" }
            ),
        ]);
    }
    for env in [
        "HOME=/relay/scratch/home",
        "XDG_CONFIG_HOME=/relay/scratch/home/config",
        "TMPDIR=/relay/scratch/tmp",
        "TMP=/relay/scratch/tmp",
        "TEMP=/relay/scratch/tmp",
        "CI=true",
        "NO_COLOR=1",
        "GIT_CONFIG_NOSYSTEM=1",
        "GIT_CONFIG_GLOBAL=/dev/null",
        "GIT_TERMINAL_PROMPT=0",
    ] {
        out.extend(["--env".into(), env.into()]);
    }
    out.push(runtime.image.clone());
    out.extend_from_slice(args);
    Ok(out)
}

/// A drop initiates removal by the immutable, previously ownership-checked ID. Durable state
/// remains recovery-blocked until async cleanup verifies absence (including after process death).
pub struct Guard {
    client: Client,
    identity: ContainerIdentity,
    armed: bool,
}
impl Guard {
    pub fn new(client: Client, identity: ContainerIdentity) -> Self {
        Self {
            client,
            identity,
            armed: true,
        }
    }
    pub async fn cleanup(&mut self) -> Result<()> {
        self.client.cleanup(&self.identity).await?;
        self.armed = false;
        Ok(())
    }
}
impl Drop for Guard {
    fn drop(&mut self) {
        if self.armed
            && let Some(id) = &self.identity.id
            && let Ok(cmd) = self.client.command(&[
                "container".into(),
                "rm".into(),
                "--force".into(),
                id.clone(),
            ])
        {
            let source = cmd.as_std();
            let mut cmd = std::process::Command::new(source.get_program());
            cmd.args(source.get_args()).env_clear();
            for (key, value) in source.get_envs() {
                if let Some(value) = value {
                    cmd.env(key, value);
                }
            }
            let _ = cmd
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .spawn();
        }
    }
}

pub fn rewrite_args(args: &[String], host_work: &Path, host_control: &Path) -> Result<Vec<String>> {
    let work = host_work.to_str().context("non-UTF-8 workspace")?;
    let control = host_control.to_str().context("non-UTF-8 control path")?;
    Ok(args
        .iter()
        .map(|s| s.replace(control, CONTROL).replace(work, WORKSPACE))
        .collect())
}

pub fn export(job: &Job, work: &Path) -> Result<BTreeMap<PathBuf, FileStamp>> {
    store::private_dir(work)?;
    let before = job
        .last_snapshot
        .iter()
        .filter(|(p, _)| {
            safety::in_scope(p, &job.task.writable_paths)
                || safety::in_scope(p, &job.task.read_paths)
        })
        .map(|(p, s)| (p.clone(), s.clone()))
        .collect::<BTreeMap<_, _>>();
    for (path, stamp) in &before {
        safety::verify_no_symlink(&job.task.workspace.path, path)?;
        let source = job.task.workspace.path.join(path);
        let meta = source.metadata()?;
        let bytes = std::fs::read(&source)?;
        ensure!(
            format!("{:x}", Sha256::digest(&bytes)) == stamp.sha256
                && safety::executable(&meta) == stamp.executable,
            "source changed during sanitized export"
        );
        let dest = work.join(path);
        if let Some(parent) = dest.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::write(&dest, bytes)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(
                dest,
                std::fs::Permissions::from_mode(if stamp.executable { 0o755 } else { 0o644 }),
            )?;
        }
    }
    Ok(before)
}

pub fn inventory(
    work: &Path,
    before: &BTreeMap<PathBuf, FileStamp>,
) -> Result<BTreeMap<PathBuf, FileStamp>> {
    let mut out = BTreeMap::new();
    let mut todo = vec![PathBuf::new()];
    let mut bytes = 0u64;
    let mut entries = 0usize;
    while let Some(relative) = todo.pop() {
        for entry in std::fs::read_dir(work.join(&relative))? {
            let entry = entry?;
            let path = relative.join(entry.file_name());
            safety::clean_relative(&path)?;
            entries += 1;
            ensure!(entries <= 100_000, "container copy has too many entries");
            safety::verify_no_symlink(work, &path)?;
            let meta = entry.metadata()?;
            if meta.is_dir() {
                todo.push(path);
                continue;
            }
            ensure!(meta.is_file(), "container copy contains special files");
            bytes = bytes
                .checked_add(meta.len())
                .context("container inventory overflow")?;
            ensure!(
                bytes <= 512 * 1024 * 1024,
                "container copy exceeds inventory bound"
            );
            let content = std::fs::read(entry.path())?;
            ensure!(
                !safety::contains_secret(&String::from_utf8_lossy(&content)),
                "container changes contain sensitive content"
            );
            out.insert(
                path.clone(),
                FileStamp {
                    sha256: format!("{:x}", Sha256::digest(&content)),
                    bytes: meta.len(),
                    tracked: before.get(&path).is_some_and(|s| s.tracked),
                    executable: safety::executable(&meta),
                },
            );
        }
    }
    Ok(out)
}

pub fn validate_changes(
    job: &Job,
    before: &BTreeMap<PathBuf, FileStamp>,
    after: &BTreeMap<PathBuf, FileStamp>,
) -> Result<Vec<Change>> {
    let changes = safety::changes(before, after, &job.task.writable_paths);
    ensure!(
        changes.iter().all(|c| c.in_scope),
        "container changed files outside assigned scope"
    );
    let bytes: u64 = changes
        .iter()
        .filter_map(|c| after.get(&c.path))
        .map(|s| s.bytes)
        .sum();
    ensure!(
        bytes <= job.profile.budget.max_change_bytes as u64,
        "container changes exceed the byte budget"
    );
    Ok(changes)
}

pub fn import(job: &Job, work: &Path, changes: &[Change]) -> Result<()> {
    // All content has been checked after the container was removed. Preflight every destination.
    for change in changes {
        safety::verify_no_symlink(&job.task.workspace.path, &change.path)?;
        ensure!(
            !job.task.workspace.path.join(&change.path).is_dir(),
            "container import would replace a directory"
        );
    }
    for change in changes {
        let dest = job.task.workspace.path.join(&change.path);
        if change.kind == "deleted" {
            std::fs::remove_file(dest)?;
        } else {
            if let Some(parent) = dest.parent() {
                std::fs::create_dir_all(parent)?;
            }
            safety::verify_no_symlink(&job.task.workspace.path, &change.path)?;
            let source = work.join(&change.path);
            let meta = source.metadata()?;
            let content = std::fs::read(&source)?;
            ensure!(
                change.after_sha256.as_deref()
                    == Some(format!("{:x}", Sha256::digest(&content)).as_str())
                    && change.after_executable == Some(safety::executable(&meta)),
                "container copy changed during import"
            );
            // Atomic file replacement avoids following a destination exchanged for a symlink.
            let temp = dest.with_file_name(format!(".relay-import-{}", uuid::Uuid::new_v4()));
            let mut options = std::fs::OpenOptions::new();
            options.write(true).create_new(true);
            #[cfg(unix)]
            {
                use std::os::unix::fs::OpenOptionsExt;
                options.mode(0o600).custom_flags(libc::O_NOFOLLOW);
            }
            {
                use std::io::Write;
                let mut file = options.open(&temp)?;
                file.write_all(&content)?;
                file.sync_all()?;
            }
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                std::fs::set_permissions(
                    &temp,
                    std::fs::Permissions::from_mode(if change.after_executable == Some(true) {
                        0o755
                    } else {
                        0o644
                    }),
                )?;
            }
            std::fs::rename(temp, dest)?;
        }
    }
    Ok(())
}

/// Reject reserved, private, transition and local destinations, including IPv4-mapped IPv6.
pub fn public_ip(ip: std::net::IpAddr) -> bool {
    match ip {
        std::net::IpAddr::V4(ip) => {
            let [a, b, c, _] = ip.octets();
            !(a == 0
                || a == 10
                || a == 127
                || a >= 224
                || (a == 100 && (64..=127).contains(&b))
                || (a == 169 && b == 254)
                || (a == 172 && (16..=31).contains(&b))
                || (a == 192 && (b == 168 || b == 0 || (b == 88 && c == 99)))
                || (a == 198 && (b == 18 || b == 19 || (b == 51 && c == 100)))
                || (a == 203 && b == 0 && c == 113))
        }
        std::net::IpAddr::V6(ip) => {
            let s = ip.segments();
            s[0] & 0xe000 == 0x2000
                && s[0] != 0x2002
                && !(s[0] == 0x2001 && (s[1] < 0x0200 || s[1] == 0x0db8))
        }
    }
}

#[derive(Clone, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProxyConfig {
    pub hosts: BTreeMap<String, Vec<std::net::SocketAddr>>,
    pub timeout_seconds: u64,
}
impl ProxyConfig {
    pub fn validate(&self) -> Result<()> {
        ensure!(
            !self.hosts.is_empty()
                && self.hosts.len() <= 16
                && (1..=3600).contains(&self.timeout_seconds),
            "invalid HTTPS proxy bounds"
        );
        for (host, peers) in &self.hosts {
            ensure!(
                host.contains('.')
                    && host.len() <= 253
                    && host == &host.to_ascii_lowercase()
                    && host
                        .chars()
                        .all(|c| c.is_ascii_alphanumeric() || ".-".contains(c)),
                "invalid HTTPS proxy host"
            );
            ensure!(
                !peers.is_empty()
                    && peers.len() <= 32
                    && peers.iter().all(|p| p.port() == 443 && public_ip(p.ip())),
                "HTTPS proxy destinations must be public IP:443 only"
            );
        }
        Ok(())
    }
}

pub async fn proxy_config(profile: &Profile) -> Result<ProxyConfig> {
    let mut hosts = BTreeMap::new();
    ensure!(
        profile.network_hosts.len() <= 16,
        "too many container HTTPS destinations"
    );
    for host in &profile.network_hosts {
        let peers = tokio::time::timeout(
            Duration::from_secs(5),
            tokio::net::lookup_host((host.as_str(), 443)),
        )
        .await??
        .collect::<Vec<_>>();
        hosts.insert(host.to_ascii_lowercase(), peers);
    }
    let config = ProxyConfig {
        hosts,
        timeout_seconds: profile.budget.timeout_seconds,
    };
    config.validate()?;
    Ok(config)
}

/// Trusted broker: no HTTP forwarding, DNS, secrets, source mounts or host-facing TCP listener.
/// It accepts CONNECT for an exact configured host:443, then checks TLS SNI before connecting
/// to a pre-resolved public address. Unrecognized/fragmented/ECH hellos fail closed.
pub async fn serve_https_proxy(config_path: &Path) -> Result<()> {
    let config: ProxyConfig = store::read_json(config_path, 32 * 1024)?;
    config.validate()?;
    let listener = tokio::net::UnixListener::bind("/relay/ipc/proxy.sock")?;
    let config = std::sync::Arc::new(config);
    let slots = std::sync::Arc::new(tokio::sync::Semaphore::new(16));
    let lifetime = tokio::time::sleep(Duration::from_secs(config.timeout_seconds));
    tokio::pin!(lifetime);
    loop {
        tokio::select! {
            _=&mut lifetime=>return Ok(()),
            accepted=listener.accept()=>{
                let (stream,_)=accepted?;
                if let Ok(permit)=slots.clone().try_acquire_owned() {
                    let config=config.clone();
                    tokio::spawn(async move {let _permit=permit;let _=tokio::time::timeout(Duration::from_secs(config.timeout_seconds),proxy_connection(stream,&config)).await;});
                }
            }
        }
    }
}

pub fn connect_host(header: &[u8], config: &ProxyConfig) -> Result<String> {
    ensure!(
        header.len() <= 8192 && header.ends_with(b"\r\n\r\n"),
        "invalid CONNECT framing"
    );
    let text = std::str::from_utf8(header)?;
    let line = text.split("\r\n").next().context("CONNECT line absent")?;
    let parts = line.split(' ').collect::<Vec<_>>();
    ensure!(
        parts.len() == 3 && parts[0] == "CONNECT" && parts[2] == "HTTP/1.1",
        "proxy permits CONNECT HTTP/1.1 only"
    );
    let host = parts[1]
        .strip_suffix(":443")
        .context("proxy requires HTTPS port 443")?;
    ensure!(
        config.hosts.contains_key(host),
        "CONNECT destination is outside the explicit allowlist"
    );
    for line in text.split("\r\n").skip(1).filter(|s| !s.is_empty()) {
        let (key, value) = line.split_once(':').context("invalid CONNECT header")?;
        ensure!(
            ["host", "user-agent", "proxy-connection", "connection"]
                .contains(&key.to_ascii_lowercase().as_str()),
            "unsupported CONNECT header"
        );
        if key.eq_ignore_ascii_case("host") {
            ensure!(value.trim() == parts[1], "CONNECT host header mismatch");
        }
    }
    Ok(host.into())
}

pub fn tls_server_name(record: &[u8]) -> Result<String> {
    ensure!(
        record.len() >= 9 && record[0] == 22 && record[1] == 3 && record[5] == 1,
        "proxy requires a TLS ClientHello"
    );
    let size = u16::from_be_bytes([record[3], record[4]]) as usize;
    ensure!(
        size <= 16 * 1024 && record.len() == size + 5,
        "invalid TLS record bound"
    );
    let handshake =
        ((record[6] as usize) << 16) | ((record[7] as usize) << 8) | (record[8] as usize);
    ensure!(
        handshake + 4 == size,
        "fragmented or combined TLS hello unsupported"
    );
    let payload = &record[9..];
    let mut at = 34;
    fn take<'a>(bytes: &'a [u8], at: &mut usize, n: usize) -> Result<&'a [u8]> {
        let end = at.checked_add(n).context("TLS length overflow")?;
        let value = bytes.get(*at..end).context("truncated TLS hello")?;
        *at = end;
        Ok(value)
    }
    fn word(bytes: &[u8], at: &mut usize) -> Result<usize> {
        let s = take(bytes, at, 2)?;
        Ok(u16::from_be_bytes([s[0], s[1]]) as usize)
    }
    let session = take(payload, &mut at, 1)?[0] as usize;
    take(payload, &mut at, session)?;
    let ciphers = word(payload, &mut at)?;
    take(payload, &mut at, ciphers)?;
    let compression = take(payload, &mut at, 1)?[0] as usize;
    take(payload, &mut at, compression)?;
    let extensions = word(payload, &mut at)?;
    ensure!(
        at + extensions == payload.len(),
        "invalid TLS extension framing"
    );
    let mut name = None;
    while at < payload.len() {
        let kind = word(payload, &mut at)?;
        let size = word(payload, &mut at)?;
        let extension = take(payload, &mut at, size)?;
        ensure!(
            kind != 0xfe0d,
            "encrypted ClientHello cannot establish an allowed hostname"
        );
        if kind == 0 {
            ensure!(name.is_none(), "duplicate TLS server-name extension");
            let mut i = 0;
            let list = word(extension, &mut i)?;
            ensure!(list + 2 == extension.len(), "invalid TLS name list");
            ensure!(
                take(extension, &mut i, 1)?[0] == 0,
                "unsupported TLS name type"
            );
            let size = word(extension, &mut i)?;
            let host = std::str::from_utf8(take(extension, &mut i, size)?)?;
            ensure!(
                i == extension.len() && host.len() <= 253,
                "ambiguous TLS server name"
            );
            name = Some(host.to_string());
        }
    }
    name.context("TLS hostname attestation missing")
}

async fn proxy_connection(mut stream: tokio::net::UnixStream, config: &ProxyConfig) -> Result<()> {
    use tokio::io::AsyncWriteExt;
    let mut header = Vec::new();
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            let byte = stream.read_u8().await?;
            header.push(byte);
            ensure!(header.len() <= 8192, "CONNECT header bound exceeded");
            if header.ends_with(b"\r\n\r\n") {
                break;
            }
        }
        Ok::<_, anyhow::Error>(())
    })
    .await??;
    let host = connect_host(&header, config)?;
    stream
        .write_all(b"HTTP/1.1 200 Connection Established\r\n\r\n")
        .await?;
    let mut record = vec![0u8; 5];
    tokio::time::timeout(Duration::from_secs(5), async {
        stream.read_exact(&mut record).await?;
        let size = u16::from_be_bytes([record[3], record[4]]) as usize;
        ensure!(size <= 16 * 1024, "TLS hello bound exceeded");
        record.resize(size + 5, 0);
        stream.read_exact(&mut record[5..]).await?;
        Ok::<_, anyhow::Error>(())
    })
    .await??;
    ensure!(
        tls_server_name(&record)? == host,
        "TLS hostname differs from CONNECT destination"
    );
    let mut connected = None;
    for peer in config.hosts.get(&host).context("unknown CONNECT host")? {
        ensure!(
            public_ip(peer.ip()) && peer.port() == 443,
            "non-public proxy destination rejected"
        );
        if let Ok(Ok(server)) =
            tokio::time::timeout(Duration::from_secs(3), tokio::net::TcpStream::connect(peer)).await
        {
            connected = Some(server);
            break;
        }
    }
    let mut server = connected.context("public HTTPS endpoint unavailable")?;
    server.write_all(&record).await?;
    let (client_read, mut client_write) = stream.split();
    let (server_read, mut server_write) = server.split();
    let outgoing = async {
        let n = tokio::io::copy(&mut client_read.take(16 * 1024 * 1024), &mut server_write).await?;
        server_write.shutdown().await?;
        Ok::<_, std::io::Error>(n)
    };
    let incoming = async {
        let n = tokio::io::copy(&mut server_read.take(16 * 1024 * 1024), &mut client_write).await?;
        client_write.shutdown().await?;
        Ok::<_, std::io::Error>(n)
    };
    tokio::try_join!(outgoing, incoming)?;
    Ok(())
}

/// Inside the isolated worker: preserve only explicit credential names, and forward the local
/// HTTP proxy port to the broker's private Unix socket. The worker has no external network.
pub async fn run_container_worker(argv: &[String]) -> Result<i32> {
    let mut at = 0;
    let mut keys = Vec::new();
    while argv.get(at).map(String::as_str) == Some("--key") {
        let name = argv.get(at + 1).context("credential name missing")?;
        ensure!(
            [
                "OPENROUTER_API_KEY",
                "META_API_KEY",
                "OPENAI_API_KEY",
                "ANTHROPIC_API_KEY",
                "GEMINI_API_KEY"
            ]
            .contains(&name.as_str()),
            "unsupported credential name"
        );
        keys.push(name.clone());
        at += 2;
    }
    // Clap removes a leading end-of-options delimiter for a trailing argv field.
    // Accept its normalized form; the next token must still be an absolute program.
    if argv.get(at).map(String::as_str) == Some("--") {
        at += 1;
    }
    let program = Path::new(argv.get(at).context("in-image worker executable missing")?);
    ensure!(
        program.is_absolute(),
        "in-image worker executable must be absolute"
    );
    at += 1;
    let mut command = safety::command(program, &argv[at..])?;
    for key in keys {
        let value = std::env::var(&key)
            .map_err(|_| anyhow::anyhow!("named worker credential unavailable"))?;
        command.env(key, value);
    }
    for name in [
        "HOME",
        "XDG_CONFIG_HOME",
        "TMPDIR",
        "TMP",
        "TEMP",
        "CI",
        "NO_COLOR",
    ] {
        if let Some(value) = std::env::var_os(name) {
            command.env(name, value);
        }
    }
    let forwarding = if Path::new("/relay/ipc/proxy.sock").exists() {
        let listener = tokio::net::TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 3128)).await?;
        for name in ["HTTP_PROXY", "HTTPS_PROXY", "http_proxy", "https_proxy"] {
            command.env(name, "http://127.0.0.1:3128");
        }
        command.env("NO_PROXY", "").env("no_proxy", "");
        Some(tokio::spawn(async move {
            let slots = std::sync::Arc::new(tokio::sync::Semaphore::new(16));
            while let Ok((mut tcp, _)) = listener.accept().await {
                if let Ok(permit) = slots.clone().try_acquire_owned() {
                    tokio::spawn(async move {
                        let _permit = permit;
                        if let Ok(mut unix) =
                            tokio::net::UnixStream::connect("/relay/ipc/proxy.sock").await
                        {
                            let _ = tokio::time::timeout(
                                Duration::from_secs(3600),
                                tokio::io::copy_bidirectional(&mut tcp, &mut unix),
                            )
                            .await;
                        }
                    });
                }
            }
        }))
    } else {
        ensure!(
            std::env::var_os("RELAY_REQUIRE_PROXY").is_none(),
            "required HTTPS broker socket missing"
        );
        None
    };
    command
        .stdin(Stdio::null())
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit())
        .kill_on_drop(true);
    let result = command.status().await?;
    if let Some(forwarding) = forwarding {
        forwarding.abort();
    }
    Ok(result.code().unwrap_or(1))
}
