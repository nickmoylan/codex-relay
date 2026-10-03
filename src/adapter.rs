//! Each adapter maps a named profile to an argument vector, never a shell string.
use crate::model::*;
use crate::store::atomic_json;
use anyhow::{Result, ensure};
use serde_json::json;
use std::path::{Path, PathBuf};

#[derive(Debug)]
pub struct Invocation {
    pub executable: PathBuf,
    pub args: Vec<String>,
}

pub fn build(job: &Job, scratch: &Path) -> Result<Invocation> {
    let prompt = scratch.join("prompt.txt");
    let mut instructions = format!(
        "You are a bounded coding worker, not the coordinator.\nObjective: {}\nWritable paths: {:?}\nRead references: {:?}\nAcceptance criteria: {:?}\nNo subagents, commits, staging, pushing, owner services, production databases, credentials, purchases or deployments. Every shell command must use RTK. Stop and report out-of-scope dependencies. Exit zero does not establish correctness.\n",
        job.task.objective,
        job.task.writable_paths,
        job.task.read_paths,
        job.task.acceptance_criteria
    );
    instructions.push_str("Do not run tests yourself. The runner runs the coordinator's explicit test argv independently after edits. The coordinator owns full integration gates.\n");
    instructions.push_str(&format!(
        "Read the shared ownership snapshot before writing: {}. Do not edit that record.\n",
        scratch.join("ownership.json").display()
    ));
    if let Some(correction) = &job.correction {
        instructions.push_str(&format!(
            "One correction attempt, starting from your previous edits: {correction}\n"
        ));
    }
    std::fs::write(&prompt, instructions)?;
    let profile = &job.profile;
    let mut args = vec![];
    match profile.harness {
        Harness::Muse => {
            std::fs::create_dir_all(scratch.join("home/config/muse"))?;
            atomic_json(
                &scratch.join("home/config/muse/settings.json"),
                &json!({"agents":{"execution_capacity":1},"mcp_servers":{}}),
            )?;
            args.extend(["exec", "--json", "--no-session-log", "--model"].map(String::from));
            args.push(profile.model.clone());
            args.extend([
                "--workspace".into(),
                job.task.workspace.path.to_string_lossy().into_owned(),
                "--prompt-file".into(),
                prompt.to_string_lossy().into_owned(),
                "--max-model-steps".into(),
                profile.budget.max_model_steps.to_string(),
            ]);
            // Keep Muse's own sandbox/automatic approval review as a second layer.
            args.extend(
                [
                    "--trust-workspace",
                    "--approval-mode",
                    "on-request",
                    "--approval-judge",
                    "on",
                    "--sandbox-network",
                    "proxy-only",
                ]
                .map(String::from),
            );
        }
        Harness::Aider => {
            let model = format!("{}/{}", profile.provider, profile.model);
            args.extend([
                "--model".into(),
                model.clone(),
                "--weak-model".into(),
                model.clone(),
                "--editor-model".into(),
                model.clone(),
            ]);
            args.extend([
                "--message-file".into(),
                prompt.to_string_lossy().into_owned(),
            ]);
            args.extend(
                [
                    "--no-git",
                    "--no-auto-commits",
                    "--no-dirty-commits",
                    "--no-auto-lint",
                    "--no-auto-test",
                    "--no-suggest-shell-commands",
                    "--no-detect-urls",
                    "--no-check-update",
                    "--no-show-release-notes",
                    "--analytics-disable",
                    "--no-restore-chat-history",
                    "--no-pretty",
                    "--no-stream",
                    "--yes-always",
                    "--map-tokens",
                    "0",
                ]
                .map(String::from),
            );
            let empty = scratch.join("empty.yml");
            std::fs::write(&empty, "{}\n")?;
            let environment = scratch.join("empty.env");
            std::fs::write(&environment, "")?;
            args.extend([
                "--config".into(),
                empty.to_string_lossy().into_owned(),
                "--env-file".into(),
                environment.to_string_lossy().into_owned(),
            ]);
            args.extend([
                "--input-history-file".into(),
                scratch.join("input-history").to_string_lossy().into_owned(),
                "--chat-history-file".into(),
                scratch.join("chat-history").to_string_lossy().into_owned(),
            ]);
            let settings = scratch.join("model-settings.json");
            let mut row = json!({"name":model, "weak_model_name":model, "editor_model_name":model, "extra_params":{}});
            if profile.provider == "openrouter" {
                row["extra_params"]["extra_body"] = json!({"models":[profile.model],"provider": {
                    "only": profile.privacy.allowed_providers,
                    "order": profile.privacy.allowed_providers,
                    "allow_fallbacks": false,
                    "data_collection":"deny",
                    "require_parameters":true,
                    "zdr":profile.privacy.zero_data_retention
                }});
            }
            // Override any aider/extra_params row even if a future harness unexpectedly discovers project defaults.
            let enforced = json!({"name":"aider/extra_params","extra_params":row["extra_params"]});
            atomic_json(&settings, &vec![row, enforced])?;
            args.extend([
                "--model-settings-file".into(),
                settings.to_string_lossy().into_owned(),
            ]);
            if let Some(base) = &profile.api_base {
                args.extend(["--openai-api-base".into(), base.clone()]);
            }
            for path in &job.task.writable_paths {
                ensure!(
                    !job.task.workspace.path.join(path).is_dir(),
                    "Aider assignments require explicit files, not directories"
                );
                args.extend(["--file".into(), path.to_string_lossy().into_owned()]);
            }
            for path in &job.task.read_paths {
                args.extend(["--read".into(), path.to_string_lossy().into_owned()]);
            }
        }
        Harness::Simulated => {
            let spec = scratch.join("simulation.json");
            atomic_json(&spec, &job.task.simulator.clone().unwrap_or_default())?;
            args.extend(["__fixture".into(), spec.to_string_lossy().into_owned()]);
        }
    }
    if profile.harness != Harness::Simulated
        && let Some(reasoning) = &profile.reasoning
    {
        args.extend(["--reasoning-effort".into(), reasoning.clone()]);
    }
    Ok(Invocation {
        executable: profile.executable.clone(),
        args,
    })
}
