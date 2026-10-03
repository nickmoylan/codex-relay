//! Validation and OS isolation. Filesystem scopes are authority, not suggestions.
use crate::model::*;
use anyhow::{Context, Result, bail, ensure};
use regex::Regex;
use sha2::{Digest, Sha256};
use std::{
    collections::{BTreeMap, BTreeSet},
    path::{Component, Path, PathBuf},
};
use tokio::process::Command;

pub fn rtk() -> Result<PathBuf> {
    let mut candidates = std::env::var_os("PATH")
        .map(|p| std::env::split_paths(&p).collect::<Vec<_>>())
        .unwrap_or_default()
        .into_iter()
        .filter(|p| {
            p.starts_with("/opt/homebrew/Cellar")
                || p.starts_with("/usr/local")
                || p == Path::new("/usr/bin")
        })
        .map(|p| p.join("rtk"))
        .collect::<Vec<_>>();
    candidates.extend(
        [
            "/opt/homebrew/bin/rtk",
            "/usr/local/bin/rtk",
            "/usr/bin/rtk",
        ]
        .map(PathBuf::from),
    );
    for path in candidates {
        if path.is_file() {
            return Ok(std::fs::canonicalize(path)?);
        }
    }
    bail!("RTK is required; configure it on this host before running workers")
}

pub fn command(program: &Path, args: &[String]) -> Result<Command> {
    let rtk = rtk()?;
    let runtime_path = format!(
        "{}:/usr/bin:/bin:/opt/homebrew/bin:/usr/local/bin",
        rtk.parent()
            .context("RTK installation directory missing")?
            .display()
    );
    let mut cmd = Command::new(rtk);
    cmd.arg("proxy").arg(program).args(args);
    cmd.env_clear()
        .env("PATH", runtime_path)
        .env("LANG", "en_US.UTF-8");
    cmd.env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_TERMINAL_PROMPT", "0")
        .env("GIT_OPTIONAL_LOCKS", "0");
    Ok(cmd)
}

pub async fn git(root: &Path, args: &[&str]) -> Result<Vec<u8>> {
    guard_git_config(root)?;
    let mut argv = vec![
        "-C".into(),
        root.to_string_lossy().into_owned(),
        "-c".into(),
        "core.fsmonitor=false".into(),
        "-c".into(),
        "core.hooksPath=/dev/null".into(),
        "-c".into(),
        "core.excludesFile=/dev/null".into(),
        "-c".into(),
        "core.attributesFile=/dev/null".into(),
        "-c".into(),
        "credential.helper=".into(),
        "-c".into(),
        "submodule.recurse=false".into(),
    ];
    argv.extend(args.iter().map(|s| s.to_string()));
    let out = tokio::time::timeout(
        std::time::Duration::from_secs(10),
        command(Path::new("/usr/bin/git"), &argv)?.output(),
    )
    .await??;
    ensure!(
        out.status.success(),
        "Git inspection failed (diagnostics withheld to avoid leaking configuration)"
    );
    ensure!(
        out.stdout.len() <= 8 * 1024 * 1024,
        "Git inventory exceeds the safety limit"
    );
    Ok(out.stdout)
}

fn guard_git_config(root: &Path) -> Result<()> {
    let dotgit = root.join(".git");
    let (common, worktree) = if dotgit.is_dir() {
        (dotgit, None)
    } else if dotgit.is_file() {
        ensure!(
            dotgit.metadata()?.len() <= 4096,
            "oversized Git worktree link"
        );
        let link = std::fs::read_to_string(&dotgit)?;
        let target = link
            .trim()
            .strip_prefix("gitdir: ")
            .context("invalid Git worktree link")?;
        let gitdir = std::fs::canonicalize(root.join(target))?;
        let parent = gitdir.parent().context("invalid Git directory")?;
        ensure!(
            parent.file_name().is_some_and(|n| n == "worktrees"),
            "Git worktree link is not a standard linked worktree"
        );
        let common = parent
            .parent()
            .context("invalid common Git directory")?
            .to_path_buf();
        ensure!(
            common.file_name().is_some_and(|n| n == ".git"),
            "Git metadata must remain in a conventional repository directory"
        );
        (common, Some(gitdir.join("config.worktree")))
    } else {
        return Ok(());
    };
    let disallowed =
        Regex::new(r"(?mi)^\s*\[\s*(include(?:if)?|filter)(?:\s|\])").expect("static pattern");
    for config in std::iter::once(common.join("config")).chain(worktree) {
        if !config.exists() {
            continue;
        }
        let meta = std::fs::symlink_metadata(&config)?;
        ensure!(
            meta.is_file() && !meta.file_type().is_symlink() && meta.len() <= 128 * 1024,
            "unsafe Git configuration file"
        );
        let content = std::fs::read_to_string(config)?;
        ensure!(
            !disallowed.is_match(&content),
            "Git includes or executable filters are unsupported; external config/code access refused"
        );
    }
    Ok(())
}

pub fn clean_relative(path: &Path) -> Result<()> {
    ensure!(!path.as_os_str().is_empty(), "empty scope path");
    ensure!(
        path.components().all(|c| matches!(c, Component::Normal(_))),
        "scope paths must be relative without traversal"
    );
    ensure!(
        !sensitive(path),
        "sensitive path excluded from worker scope"
    );
    ensure!(path.to_str().is_some(), "paths must be UTF-8");
    Ok(())
}

pub fn sensitive(path: &Path) -> bool {
    path.components().any(|c| {
        let s = c.as_os_str().to_string_lossy().to_ascii_lowercase();
        s == ".git"
            || s == ".muse"
            || s == ".codex"
            || s == ".agents"
            || s == ".claude"
            || s == ".aws"
            || s == ".ssh"
            || s == ".aider"
            || s.starts_with(".aider.")
            || s == "data"
            || s == "config/devlink"
            || s.starts_with(".env")
            || s == "auth.json"
            || s == "credentials"
            || s == "credentials.json"
            || s == "vault.age"
            || s == "keystore.json"
            || s.ends_with(".pem")
            || s.ends_with(".key")
            || s.ends_with(".p12")
            || s.ends_with(".age")
            || s.starts_with("id_rsa")
            || s.starts_with("id_ed25519")
    })
}

pub fn contains_secret(text: &str) -> bool {
    let re = Regex::new(r#"(?im)(-----BEGIN [A-Z ]*PRIVATE KEY-----|(?:sk-(?:or-v1-)?|gh[pousr]_)[A-Za-z0-9_-]{12,}|(?:api[_-]?key|access[_-]?token|password|secret)\s*[:=]\s*["'][^\s"']{16,}|Bearer\s+[A-Za-z0-9._-]{12,})"#).expect("static pattern");
    re.is_match(text)
}

pub fn verify_no_symlink(root: &Path, relative: &Path) -> Result<()> {
    clean_relative(relative)?;
    let mut path = root.to_path_buf();
    for part in relative.components() {
        path.push(part.as_os_str());
        match std::fs::symlink_metadata(&path) {
            Ok(meta) => {
                ensure!(!meta.file_type().is_symlink(), "symlink in worker scope");
                #[cfg(unix)]
                {
                    use std::os::unix::fs::MetadataExt;
                    ensure!(
                        !meta.is_file() || meta.nlink() == 1,
                        "hard-linked source file excluded"
                    );
                }
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => break,
            Err(_) => bail!("cannot inspect worker scope"),
        }
    }
    Ok(())
}

pub fn in_scope(path: &Path, scopes: &[PathBuf]) -> bool {
    scopes.iter().any(|s| path.starts_with(s))
}
pub fn overlaps(a: &[PathBuf], b: &[PathBuf]) -> bool {
    a.iter()
        .any(|x| b.iter().any(|y| x.starts_with(y) || y.starts_with(x)))
}

pub fn validate_config(config: &Config) -> Result<()> {
    ensure!(
        config.schema_version == SCHEMA_VERSION,
        "unsupported config schema"
    );
    ensure!(
        config.state_dir.is_absolute(),
        "state directory must be absolute"
    );
    ensure!(
        config.state_dir.components().count() >= 4,
        "state must be a dedicated nested directory, never filesystem/home root"
    );
    ensure!(
        !config.managed_worktree_roots.is_empty(),
        "managed worktree roots must be configured"
    );
    ensure!(
        (1..=2).contains(&config.max_external_workers),
        "external limit must be one or two"
    );
    ensure!(
        config.max_total_workers >= config.max_external_workers && config.max_total_workers <= 32,
        "invalid combined worker limit"
    );
    ensure!(
        config.reserved_native_slots >= 1
            && config.reserved_native_slots < config.max_total_workers,
        "reserve at least the Sol coordinator slot"
    );
    for root in &config.managed_worktree_roots {
        ensure!(
            root.is_absolute() && !root.components().any(|c| matches!(c, Component::ParentDir)),
            "invalid managed root"
        );
        ensure!(
            root.components().count() >= 4,
            "managed roots cannot grant an entire home or filesystem"
        );
    }
    for (name, p) in &config.profiles {
        ensure!(
            !name.is_empty()
                && name.len() <= 64
                && name
                    .chars()
                    .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_'),
            "invalid profile name"
        );
        validate_profile(p)?;
    }
    Ok(())
}

pub fn validate_profile(p: &Profile) -> Result<()> {
    if let Some(runtime) = &p.container {
        crate::docker::validate_runtime(runtime)?;
        ensure!(
            p.executable.to_str().is_some()
                && !p.executable.starts_with("/relay")
                && !p
                    .executable
                    .components()
                    .any(|c| matches!(c, Component::ParentDir)),
            "container executable must be a fixed in-image path outside scratch mounts"
        );
        ensure!(
            p.api_base.is_none(),
            "container endpoint overrides are unsupported"
        );
        ensure!(
            p.credential_env.is_empty() || !p.network_hosts.is_empty(),
            "container credentials require the restricted HTTPS broker"
        );
        ensure!(
            p.runtime_read_roots.is_empty(),
            "container profiles cannot mount host runtime directories"
        );
        if p.harness == Harness::Simulated {
            ensure!(
                p.executable == Path::new("/usr/local/bin/codex-relay"),
                "container simulation requires the reviewed built-in Relay image executable"
            );
        }
    }
    ensure!(
        p.executable.is_absolute(),
        "harness executable must be an absolute path"
    );
    ensure!(
        !p.model.is_empty()
            && p.model.len() <= 160
            && p.model
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || "/:._-".contains(c)),
        "invalid explicit model id"
    );
    ensure!(
        !p.provider.is_empty()
            && p.provider
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || "_-".contains(c)),
        "invalid provider"
    );
    ensure!(
        p.capabilities.code_editing,
        "profile must declare code editing capability"
    );
    ensure!(
        p.reasoning.is_none() || p.capabilities.reasoning,
        "model reasoning capability is not declared"
    );
    if let Some(r) = &p.reasoning {
        ensure!(
            [
                "none", "minimal", "low", "medium", "high", "xhigh", "max", "ultra"
            ]
            .contains(&r.as_str()),
            "unsupported reasoning level"
        );
    }
    ensure!(
        (1..=3600).contains(&p.budget.timeout_seconds),
        "timeout must be bounded to 1..3600 seconds"
    );
    ensure!(
        (1..=100).contains(&p.budget.max_model_steps),
        "step cap must be bounded to 1..100"
    );
    ensure!(
        (1024..=4 * 1024 * 1024).contains(&p.budget.max_output_bytes),
        "invalid output cap"
    );
    ensure!(
        (1024..=8 * 1024 * 1024).contains(&p.budget.max_change_bytes),
        "invalid change cap"
    );
    ensure!(
        p.budget.max_cost_usd.is_none(),
        "hard monetary caps are not supported by these CLI harnesses; use provider-side limits (no spending cap is implied)"
    );
    ensure!(
        !p.privacy.allow_fallbacks,
        "silent provider/model fallback is forbidden"
    );
    ensure!(
        p.privacy.data_collection == "deny",
        "training/data collection must be denied"
    );
    match p.harness {
        Harness::Muse => {
            ensure!(
                p.provider == "meta" && p.model == "muse-spark-1.3",
                "Muse adapter permits only Standard muse-spark-1.3"
            );
            ensure!(
                p.tools == ["file_read", "file_edit", "shell"] && p.capabilities.tool_use,
                "Muse tool profile must explicitly declare its native file/shell tools"
            );
            ensure!(
                !p.privacy.zero_data_retention,
                "Muse ZDR is not established by these CLI flags"
            );
            ensure!(
                p.api_base.is_none() && p.credential_env.iter().all(|s| s == "META_API_KEY"),
                "unsupported Muse credential or endpoint override"
            );
        }
        Harness::Aider => {
            ensure!(
                p.tools == ["file_read", "file_edit"],
                "Aider adapter exposes file editing only; shell suggestions, auto-lint and auto-test are disabled"
            );
            ensure!(
                !p.capabilities.tool_use,
                "native tool-loop profiles are not implemented by the Aider text-edit adapter"
            );
            if p.provider == "openrouter" {
                ensure!(
                    p.model != "auto" && p.model != "openrouter/auto",
                    "automatic model routing is incompatible with pinned model profiles"
                );
                ensure!(
                    !p.privacy.allowed_providers.is_empty(),
                    "OpenRouter requires an explicit provider allowlist"
                );
                ensure!(
                    p.api_base.is_none(),
                    "OpenRouter endpoint overrides are forbidden"
                );
                ensure!(
                    p.credential_env == ["OPENROUTER_API_KEY"],
                    "OpenRouter requires only OPENROUTER_API_KEY"
                );
            } else {
                ensure!(
                    !p.privacy.confidential_code && !p.privacy.zero_data_retention,
                    "this adapter cannot enforce other providers' confidentiality/ZDR; use a verified adapter for confidential tasks"
                );
                ensure!(
                    p.privacy.allowed_providers.is_empty(),
                    "provider routing applies only to OpenRouter"
                );
            }
            ensure!(
                p.budget.max_model_steps == 1,
                "Aider supports one prompted task, not a verified model-step cap; max_model_steps must be 1 (internal edit retries are bounded by timeout)"
            );
        }
        Harness::Simulated => {
            ensure!(
                p.provider == "simulation"
                    && p.credential_env.is_empty()
                    && p.network_hosts.is_empty(),
                "simulation never uses credentials or network"
            );
        }
    }
    for name in &p.credential_env {
        ensure!(
            [
                "OPENROUTER_API_KEY",
                "META_API_KEY",
                "OPENAI_API_KEY",
                "ANTHROPIC_API_KEY",
                "GEMINI_API_KEY"
            ]
            .contains(&name.as_str()),
            "credential environment name is not supported"
        );
    }
    for root in &p.runtime_read_roots {
        ensure!(root.is_absolute(), "runtime roots must be absolute");
        ensure!(
            [
                "/opt/homebrew/Cellar",
                "/opt/homebrew/lib",
                "/usr/local/lib",
                "/Library/Frameworks"
            ]
            .iter()
            .any(|allowed| root.starts_with(allowed)),
            "runtime reads may cover only approved installation directories, never a home/config directory"
        );
    }
    for host in &p.network_hosts {
        ensure!(
            host.contains('.')
                && host.len() <= 253
                && host
                    .chars()
                    .all(|c| c.is_ascii_alphanumeric() || c == '.' || c == '-'),
            "network hosts must be explicit DNS names"
        );
        ensure!(
            !host.eq_ignore_ascii_case("localhost")
                && !host.ends_with(".local")
                && host.parse::<std::net::IpAddr>().is_err(),
            "local/IP network destinations are forbidden"
        );
    }
    if p.harness == Harness::Aider && p.provider == "openrouter" {
        ensure!(
            p.network_hosts == ["openrouter.ai"],
            "OpenRouter adapter network allowlist must be openrouter.ai only"
        );
    }
    if let Some(base) = &p.api_base {
        ensure!(
            base.starts_with("https://")
                && !base.contains('@')
                && !base.contains('?')
                && !base.contains('#')
                && !contains_secret(base),
            "endpoint must be a credential-free HTTPS URL"
        );
    }
    Ok(())
}

pub async fn validate_workspace(config: &Config, task: &Task) -> Result<(PathBuf, PathBuf)> {
    ensure!(
        task.schema_version == SCHEMA_VERSION,
        "unsupported task schema"
    );
    let path = &task.workspace.path;
    ensure!(path.is_absolute(), "workspace path must be absolute");
    let canonical = std::fs::canonicalize(path).context("workspace does not exist")?;
    ensure!(
        &canonical == path,
        "workspace path must be canonical without symlinks"
    );
    let roots = config
        .managed_worktree_roots
        .iter()
        .filter_map(|r| std::fs::canonicalize(r).ok())
        .collect::<Vec<_>>();
    ensure!(
        roots.iter().any(|r| path.starts_with(r) && path != r),
        "workspace is outside configured Codex-managed roots"
    );
    ensure!(
        task.workspace.artifact_identity == path.to_string_lossy(),
        "artifact identity must equal the Codex attachment's exact worktree root"
    );
    let age = now_ms().saturating_sub(task.workspace.attested_at_ms);
    ensure!(
        age <= 5 * 60 * 1000 && task.workspace.attested_at_ms <= now_ms() + 5000,
        "refresh the Codex worktree attachment attestation"
    );
    ensure!(
        path.join(".git").is_file()
            && !std::fs::symlink_metadata(path.join(".git"))?
                .file_type()
                .is_symlink(),
        "a supplied linked Git worktree is required; no worktrees are created by this utility"
    );
    let base = &task.workspace.base_revision;
    ensure!(
        [40, 64].contains(&base.len()) && base.chars().all(|c| c.is_ascii_hexdigit()),
        "base revision must be a full commit id"
    );
    let head = String::from_utf8(git(path, &["rev-parse", "HEAD"]).await?)?;
    ensure!(
        head.trim() == base,
        "worktree HEAD differs from the supplied base revision"
    );
    let common = String::from_utf8(
        git(
            path,
            &["rev-parse", "--path-format=absolute", "--git-common-dir"],
        )
        .await?,
    )?;
    let common = std::fs::canonicalize(common.trim())?;
    ensure!(
        task.objective.len() <= 16 * 1024
            && !task.objective.is_empty()
            && !contains_secret(&task.objective),
        "objective must be bounded and contain no credentials"
    );
    ensure!(
        !task.writable_paths.is_empty()
            && task.writable_paths.len() <= 64
            && task.read_paths.len() <= 128,
        "invalid scope size"
    );
    for scope in task.writable_paths.iter().chain(&task.read_paths) {
        verify_no_symlink(path, scope)?;
    }
    ensure!(
        !task.acceptance_criteria.is_empty() && task.acceptance_criteria.len() <= 32,
        "acceptance criteria are required and bounded"
    );
    for (id, criterion) in &task.acceptance_criteria {
        ensure!(
            valid_id(id) && criterion.len() <= 2048 && !contains_secret(criterion),
            "invalid acceptance criterion"
        );
    }
    ensure!(task.tests.len() <= 16, "too many verification commands");
    let mut ids = BTreeSet::new();
    for test in &task.tests {
        ensure!(
            valid_id(&test.id) && ids.insert(&test.id),
            "test ids must be unique simple identifiers"
        );
        ensure!(
            test.program.is_absolute()
                && test.timeout_seconds > 0
                && test.timeout_seconds <= 600
                && test.args.len() <= 64,
            "invalid test command"
        );
        ensure!(
            test.args
                .iter()
                .all(|s| s.len() <= 4096 && !contains_secret(s)),
            "test argv must contain no secrets"
        );
        // Shells/interpreters are deliberate code execution and remain inside the same sandbox.
    }
    Ok((canonical, common))
}

pub fn valid_id(s: &str) -> bool {
    !s.is_empty()
        && s.len() <= 64
        && s.chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
}

pub fn executable(meta: &std::fs::Metadata) -> bool {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        meta.permissions().mode() & 0o100 != 0
    }
    #[cfg(not(unix))]
    {
        let _ = meta;
        false
    }
}

pub async fn snapshot(
    root: &Path,
    budget: usize,
    writable_scopes: &[PathBuf],
    read_scopes: &[PathBuf],
) -> Result<BTreeMap<PathBuf, FileStamp>> {
    let tracked: BTreeSet<_> = git(root, &["ls-files", "-z"])
        .await?
        .split(|b| *b == 0)
        .filter(|s| !s.is_empty())
        .map(|s| String::from_utf8(s.to_vec()).map(PathBuf::from))
        .collect::<std::result::Result<_, _>>()?;
    let untracked = git(root, &["ls-files", "--others", "--exclude-standard", "-z"]).await?;
    let mut paths = tracked.clone();
    for s in untracked.split(|b| *b == 0).filter(|s| !s.is_empty()) {
        paths.insert(PathBuf::from(String::from_utf8(s.to_vec())?));
    }
    // Inventory declared ignored read references as well as deliverables, while excluding
    // unrelated build caches elsewhere in the repository.
    let scope_strings: Vec<_> = writable_scopes
        .iter()
        .chain(read_scopes)
        .map(|p| p.to_str().context("non-UTF-8 source scope"))
        .collect::<Result<_>>()?;
    let mut ignored_args = vec![
        "ls-files",
        "--others",
        "--ignored",
        "--exclude-standard",
        "-z",
        "--",
    ];
    ignored_args.extend(scope_strings);
    let ignored = git(root, &ignored_args).await?;
    for s in ignored.split(|b| *b == 0).filter(|s| !s.is_empty()) {
        paths.insert(PathBuf::from(String::from_utf8(s.to_vec())?));
    }
    let mut out = BTreeMap::new();
    let mut total = 0_u64;
    for path in paths {
        // Never read excluded files, even when tracked. A worker cannot write them.
        if sensitive(&path) {
            continue;
        }
        verify_no_symlink(root, &path)?;
        let absolute = root.join(&path);
        if !absolute.exists() {
            continue;
        }
        let meta = absolute.metadata()?;
        let size = meta.len();
        total = total.checked_add(size).context("inventory overflow")?;
        ensure!(
            total <= budget as u64,
            "source inventory exceeds configured bound"
        );
        let bytes = std::fs::read(absolute)?;
        ensure!(
            !contains_secret(&String::from_utf8_lossy(&bytes)),
            "credential-like source content: job/result quarantined without reading it into model context"
        );
        out.insert(
            path.clone(),
            FileStamp {
                sha256: format!("{:x}", Sha256::digest(&bytes)),
                bytes: size,
                tracked: tracked.contains(&path),
                executable: executable(&meta),
            },
        );
    }
    Ok(out)
}

pub fn changes(
    before: &BTreeMap<PathBuf, FileStamp>,
    after: &BTreeMap<PathBuf, FileStamp>,
    scopes: &[PathBuf],
) -> Vec<Change> {
    before
        .keys()
        .chain(after.keys())
        .collect::<BTreeSet<_>>()
        .into_iter()
        .filter_map(|path| {
            let a = before.get(path);
            let b = after.get(path);
            if a == b {
                return None;
            }
            Some(Change {
                path: path.clone(),
                kind: if a.is_none() {
                    "added"
                } else if b.is_none() {
                    "deleted"
                } else if a.map(|f| (&f.sha256, f.bytes)) == b.map(|f| (&f.sha256, f.bytes)) {
                    "mode_changed"
                } else {
                    "modified"
                }
                .into(),
                tracked: a.is_some_and(|f| f.tracked) || b.is_some_and(|f| f.tracked),
                before_sha256: a.map(|f| f.sha256.clone()),
                after_sha256: b.map(|f| f.sha256.clone()),
                before_executable: a.map(|f| f.executable),
                after_executable: b.map(|f| f.executable),
                in_scope: in_scope(path, scopes),
            })
        })
        .collect()
}

fn quoted(path: &Path) -> Result<String> {
    let s = path.to_str().context("sandbox path is not UTF-8")?;
    ensure!(
        !s.contains('\n') && !s.contains('\r') && !s.contains('\0'),
        "invalid sandbox path"
    );
    Ok(serde_json::to_string(s)?)
}

/// Seatbelt denies host reads by default, then allows only source/runtime reads and assigned writes.
/// Linux/Windows intentionally fail closed until an equally tested backend is supplied.
pub fn seatbelt_policy(
    root: &Path,
    scratch: &Path,
    writable: &[PathBuf],
    profile: &Profile,
    binary: &Path,
    peers: &[std::net::SocketAddr],
) -> Result<String> {
    ensure!(
        cfg!(target_os = "macos"),
        "only the macOS Seatbelt backend is implemented; no unsandboxed fallback"
    );
    let mut policy = String::from(
        "(version 1)\n(deny default)\n(allow process-exec process-fork process-info-pidinfo sysctl-read)\n(deny process-info-setcontrol)\n(allow signal (target self))\n(allow mach-lookup (global-name \"com.apple.system.logger\") (global-name \"com.apple.trustd\") (global-name \"com.apple.trustd.agent\"))\n",
    );
    // dyld must stat ancestors to reach explicitly allowed executables/libraries.
    // This permits metadata only at those exact ancestors, not home-directory enumeration or file contents.
    let mut ancestors = BTreeSet::new();
    for path in [root, scratch, binary, profile.executable.as_path()]
        .into_iter()
        .chain(profile.runtime_read_roots.iter().map(PathBuf::as_path))
    {
        ancestors.extend(path.ancestors().map(Path::to_path_buf));
    }
    policy.push_str("(allow file-read-metadata");
    for path in ancestors {
        policy.push_str(&format!(" (literal {})", quoted(&path)?));
    }
    policy.push_str(")\n");
    // Explicitly approved loader permission: literal root-directory entries,
    // with no recursive grant to home or other private directory contents.
    policy.push_str("(allow file-read-data (literal \"/\"))\n");
    policy.push_str("(allow file-read* file-map-executable (subpath \"/System\") (subpath \"/usr\") (subpath \"/bin\") (subpath \"/sbin\") (subpath \"/Library/Apple\") (subpath \"/private/preboot\") (subpath \"/private/var/db/dyld\") (subpath \"/dev\"))\n(allow file-write-data (literal \"/dev/null\"))\n");
    policy.push_str(&format!("(allow file-read* file-map-executable (subpath {}) (subpath {}) (literal {}) (literal {}))\n", quoted(root)?, quoted(scratch)?, quoted(binary)?,quoted(&profile.executable)?));
    for runtime in &profile.runtime_read_roots {
        policy.push_str(&format!(
            "(allow file-read* file-map-executable (subpath {}))\n",
            quoted(runtime)?
        ));
    }
    policy.push_str(&format!(
        "(allow file-write* (subpath {}))\n",
        quoted(scratch)?
    ));
    policy.push_str(&format!(
        "(deny file-write* (literal {}))\n",
        quoted(&scratch.join("ownership.json"))?
    ));
    for path in writable {
        verify_no_symlink(root, path)?;
        policy.push_str(&format!(
            "(allow file-write* (subpath {}))\n",
            quoted(&root.join(path))?
        ));
    }
    // Explicit denies win over source-read/assigned-write allows.
    for name in [
        ".git", ".muse", ".codex", ".agents", ".claude", ".aws", ".ssh", ".aider", "data",
    ] {
        policy.push_str(&format!(
            "(deny file-read* file-write* (subpath {}))\n",
            quoted(&root.join(name))?
        ));
    }
    policy.push_str("(deny file-read* file-write* (regex #\"(^|/)(\\.env[^/]*|\\.aider\\.[^/]*|auth\\.json|credentials(\\.json)?|vault\\.age|keystore\\.json|id_rsa[^/]*|id_ed25519[^/]*|[^/]+\\.(pem|key|p12|age))(/|$)\"))\n");
    // Mach lookup must not turn into a credential/keychain or owner-service route.
    policy.push_str("(deny mach-lookup (global-name \"com.apple.securityd\") (global-name \"com.apple.security.agent\"))\n");
    for peer in peers {
        ensure!(
            !peer.ip().is_loopback() && !peer.ip().is_unspecified(),
            "local endpoint forbidden"
        );
        policy.push_str(&format!(
            "(allow network-outbound (remote tcp {}))\n",
            serde_json::to_string(&peer.to_string())?
        ));
    }
    Ok(policy)
}
