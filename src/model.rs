use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use std::{collections::BTreeMap, path::PathBuf};

pub const SCHEMA_VERSION: u32 = 1;
pub fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}

#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Config {
    pub schema_version: u32,
    pub state_dir: PathBuf,
    pub managed_worktree_roots: Vec<PathBuf>,
    pub max_external_workers: usize,
    pub max_total_workers: usize,
    pub reserved_native_slots: usize,
    #[serde(default)]
    pub allow_simulated_workers: bool,
    pub profiles: BTreeMap<String, Profile>,
}

#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum Harness {
    Muse,
    Aider,
    Simulated,
}

#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Capabilities {
    pub code_editing: bool,
    pub reasoning: bool,
    pub tool_use: bool,
}

#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Privacy {
    pub confidential_code: bool,
    pub data_collection: String,
    pub zero_data_retention: bool,
    pub allowed_providers: Vec<String>,
    pub allow_fallbacks: bool,
}

#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Budget {
    pub timeout_seconds: u64,
    /// Native hard limit for Muse/simulation. Aider requires 1 for one prompted task;
    /// it cannot expose a hard inference-call count, and incompatible profiles are rejected.
    pub max_model_steps: u32,
    pub max_output_bytes: usize,
    pub max_change_bytes: usize,
    pub max_cost_usd: Option<f64>,
}

#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Profile {
    /// None selects the native backend. Container images must be immutable and locally present.
    #[serde(default)]
    pub container: Option<DockerRuntime>,
    pub enabled: bool,
    pub harness: Harness,
    pub executable: PathBuf,
    pub provider: String,
    pub model: String,
    pub reasoning: Option<String>,
    pub tools: Vec<String>,
    pub capabilities: Capabilities,
    pub privacy: Privacy,
    pub budget: Budget,
    /// Names only. Values are read at execution time and never persisted.
    pub credential_env: Vec<String>,
    pub api_base: Option<String>,
    /// HTTPS hosts, resolved to explicit addresses at launch. Empty means no network.
    pub network_hosts: Vec<String>,
    /// Explicit runtime directories; home directories and configuration roots are forbidden.
    pub runtime_read_roots: Vec<PathBuf>,
}

#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct DockerRuntime {
    /// Exact repository@sha256:digest or sha256:image-id. No pull occurs at launch.
    pub image: String,
    pub memory_mb: u32,
    pub pids_limit: u32,
    pub cpus: u32,
}

#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ContainerIdentity {
    pub name: String,
    pub owner: String,
    pub job_id: String,
    pub endpoint: String,
    #[serde(default)]
    pub id: Option<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Workspace {
    pub path: PathBuf,
    pub artifact_identity: String,
    pub base_revision: String,
    pub attested_at_ms: u64,
}

#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct NativeLoad {
    pub active_workers: usize,
    pub observed_at_ms: u64,
    pub source: String,
}

#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct TestCommand {
    pub id: String,
    pub program: PathBuf,
    pub args: Vec<String>,
    pub timeout_seconds: u64,
}

#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Task {
    pub schema_version: u32,
    pub profile: String,
    pub workspace: Workspace,
    pub objective: String,
    pub writable_paths: Vec<PathBuf>,
    pub read_paths: Vec<PathBuf>,
    pub acceptance_criteria: BTreeMap<String, String>,
    pub tests: Vec<TestCommand>,
    pub native_load: NativeLoad,
    #[serde(default)]
    pub simulator: Option<Simulation>,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Simulation {
    #[serde(default)]
    pub delay_ms: u64,
    #[serde(default)]
    pub descendant: bool,
    #[serde(default)]
    pub exit_code: i32,
    #[serde(default)]
    pub writes: BTreeMap<PathBuf, String>,
    #[serde(default)]
    pub emit_steps: u32,
    #[serde(default)]
    pub output_bytes: usize,
    #[serde(default)]
    pub malformed_event: bool,
    #[serde(default)]
    pub claimed_test_ids: Vec<String>,
    #[serde(default)]
    pub echo: String,
}

#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum JobStatus {
    Queued,
    Running,
    Completed,
    Failed,
    Cancelled,
    TimedOut,
    OutputLimit,
    StepLimit,
    ScopeViolation,
    Interrupted,
    IsolationFailed,
}
impl JobStatus {
    pub fn active(&self) -> bool {
        matches!(self, Self::Queued | Self::Running)
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
pub struct ProcessIdentity {
    pub pid: u32,
    pub birth: String,
}

#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
pub struct Job {
    pub schema_version: u32,
    pub id: String,
    pub task: Task,
    pub profile: Profile,
    pub repository_identity: PathBuf,
    pub status: JobStatus,
    pub created_at_ms: u64,
    pub updated_at_ms: u64,
    pub attempt: u8,
    pub supervisor_pid: Option<u32>,
    pub worker: Option<ProcessIdentity>,
    /// Validated local daemon endpoint handed to detached supervisors without owner environment.
    #[serde(default)]
    pub docker_endpoint: Option<String>,
    #[serde(default)]
    pub container: Option<ContainerIdentity>,
    #[serde(default)]
    pub broker: Option<ContainerIdentity>,
    pub recovery_required: bool,
    /// Copy-back started but final host inventory was not durably confirmed.
    #[serde(default)]
    pub import_pending: bool,
    pub baseline: BTreeMap<PathBuf, FileStamp>,
    pub last_snapshot: BTreeMap<PathBuf, FileStamp>,
    pub result: Option<WorkerResult>,
    pub correction: Option<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema, PartialEq, Eq)]
pub struct FileStamp {
    pub sha256: String,
    pub bytes: u64,
    pub tracked: bool,
    /// Git-style owner-executable bit; other permissions are intentionally normalized.
    #[serde(default)]
    pub executable: bool,
}

#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
pub struct Change {
    pub path: PathBuf,
    pub kind: String,
    pub tracked: bool,
    pub before_sha256: Option<String>,
    pub after_sha256: Option<String>,
    #[serde(default)]
    pub before_executable: Option<bool>,
    #[serde(default)]
    pub after_executable: Option<bool>,
    pub in_scope: bool,
}

#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
pub struct ObservedTest {
    pub id: String,
    pub exit_code: Option<i32>,
    pub outcome: String,
    pub elapsed_ms: u64,
}

#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
pub struct WorkerResult {
    pub status: JobStatus,
    pub process_exit_code: Option<i32>,
    pub changes: Vec<Change>,
    pub worker_claimed_test_ids: Vec<String>,
    pub independently_observed_tests: Vec<ObservedTest>,
    pub completed_criteria_claims: Vec<String>,
    pub output_bytes: usize,
    pub model_steps_observed: u32,
    pub malformed_events: usize,
    pub blockers: Vec<String>,
    pub correctness: String,
    pub continuation: String,
    pub route_receipt: String,
}
