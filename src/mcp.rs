//! Small stdio MCP server. Stdout is reserved exclusively for JSON-RPC.
use crate::{
    engine::{Engine, public_status},
    model::*,
};
use anyhow::{Result, ensure};
use serde_json::{Value, json};
use std::path::Path;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

fn object(properties: Value, required: &[&str]) -> Value {
    json!({"type":"object","properties":properties,"required":required,"additionalProperties":false})
}
fn job_schema() -> Value {
    object(
        json!({"job_id":{"type":"string","format":"uuid"}}),
        &["job_id"],
    )
}
fn task_input() -> Value {
    let mut task = serde_json::to_value(schemars::schema_for!(Task)).expect("schema serializes");
    let definitions = task.as_object_mut().and_then(|m| m.remove("$defs"));
    let mut input = object(json!({"task":task}), &["task"]);
    if let Some(definitions) = definitions {
        input["$defs"] = definitions;
    }
    input
}

pub fn tools() -> Value {
    json!([
        {"name":"workers_start","description":"Start one bounded worker in a supplied, freshly attested Codex-managed worktree. Disabled profiles and unavailable OS isolation fail closed. Never creates worktrees or credentials.",
            "inputSchema":task_input(),"annotations":{"readOnlyHint":false,"destructiveHint":false,"idempotentHint":false,"openWorldHint":true}},
        {"name":"workers_status","description":"Read durable progress and combined concurrency. Native Codex load is explicitly coordinator-attested, not automatically discovered.",
            "inputSchema":object(json!({"job_id":{"type":"string","format":"uuid"}}),&[]),"annotations":{"readOnlyHint":false,"destructiveHint":false,"idempotentHint":true,"openWorldHint":false}},
        {"name":"workers_result","description":"Read changed tracked/untracked paths and hashes, worker claims, independently observed tests, blockers and integration requirements. Exit zero is not correctness.",
            "inputSchema":job_schema(),"annotations":{"readOnlyHint":false,"destructiveHint":false,"idempotentHint":true,"openWorldHint":false}},
        {"name":"workers_correct","description":"Request the single fresh correction attempt after review. Requires refreshed native load and an unchanged last-result worktree. No automatic model-session resume.",
            "inputSchema":object(json!({"job_id":{"type":"string","format":"uuid"},"feedback":{"type":"string","maxLength":8192},"native_load":schemars::schema_for!(NativeLoad)}),&["job_id","feedback","native_load"]),"annotations":{"readOnlyHint":false,"destructiveHint":false,"idempotentHint":false,"openWorldHint":true}},
        {"name":"workers_cancel","description":"Record cancellation for the per-job supervisor. Read status/result to confirm descendant cleanup; repeating the request is safe.",
            "inputSchema":job_schema(),"annotations":{"readOnlyHint":false,"destructiveHint":false,"idempotentHint":true,"openWorldHint":false}},
        {"name":"workers_doctor","description":"Inspect non-secret profiles, installed executables, capability limitations and current time. Does not inspect credentials or call providers.",
            "inputSchema":object(json!({}),&[]),"annotations":{"readOnlyHint":true,"openWorldHint":false}}
    ])
}

pub async fn call(engine: &Engine, config_path: &Path, name: &str, args: Value) -> Result<Value> {
    let args = args
        .as_object()
        .ok_or_else(|| anyhow::anyhow!("arguments must be an object"))?;
    let allowed: &[&str] = match name {
        "workers_start" => &["task"],
        "workers_status" => &["job_id"],
        "workers_result" | "workers_cancel" => &["job_id"],
        "workers_correct" => &["job_id", "feedback", "native_load"],
        "workers_doctor" => &[],
        _ => anyhow::bail!("unknown tool"),
    };
    ensure!(
        args.keys().all(|k| allowed.contains(&k.as_str())),
        "unknown tool argument"
    );
    let id = || {
        args.get("job_id")
            .and_then(Value::as_str)
            .ok_or_else(|| anyhow::anyhow!("job_id is required"))
    };
    match name {
        "workers_start" => {
            let task: Task = serde_json::from_value(
                args.get("task")
                    .cloned()
                    .ok_or_else(|| anyhow::anyhow!("task is required"))?,
            )
            .map_err(|_| anyhow::anyhow!("invalid task schema"))?;
            Ok(public_status(&engine.start(task, config_path).await?))
        }
        "workers_status" => {
            if let Some(value) = args.get("job_id") {
                Ok(public_status(
                    &engine
                        .status(
                            value
                                .as_str()
                                .ok_or_else(|| anyhow::anyhow!("invalid job_id"))?,
                        )
                        .await?,
                ))
            } else {
                engine.all_status().await
            }
        }
        "workers_result" => {
            let job = engine.status(id()?).await?;
            Ok(
                json!({"job":public_status(&job),"result":job.result,"ownership_record":engine.store.job_dir(&job.id)?.join("job.json"),
                "integration":"review hashes and files in the supplied worktree; integrate through the attached Codex task; full gates remain the coordinator's responsibility"}),
            )
        }
        "workers_correct" => {
            let feedback = args
                .get("feedback")
                .and_then(Value::as_str)
                .ok_or_else(|| anyhow::anyhow!("feedback is required"))?
                .to_string();
            let load: NativeLoad = serde_json::from_value(
                args.get("native_load")
                    .cloned()
                    .ok_or_else(|| anyhow::anyhow!("native_load is required"))?,
            )
            .map_err(|_| anyhow::anyhow!("invalid native_load"))?;
            Ok(public_status(
                &engine
                    .correct(id()?, feedback, load, Some(config_path))
                    .await?,
            ))
        }
        "workers_cancel" => engine.cancel(id()?).await,
        "workers_doctor" => engine.doctor().await,
        _ => unreachable!(),
    }
}

pub async fn serve(engine: Engine, config_path: &Path) -> Result<()> {
    let mut input = BufReader::new(tokio::io::stdin());
    let mut output = tokio::io::stdout();
    let mut initialized = false;
    let mut ready = false;
    loop {
        let Some(line) = read_frame(&mut input).await? else {
            break;
        };
        let request: Value = match serde_json::from_str(&line) {
            Ok(v) => v,
            Err(_) => {
                send(&mut output,json!({"jsonrpc":"2.0","id":null,"error":{"code":-32700,"message":"Parse error"}})).await?;
                continue;
            }
        };
        let id = request.get("id").cloned();
        let valid_id = id
            .as_ref()
            .is_none_or(|id| id.is_string() || id.is_i64() || id.is_u64());
        let method = request.get("method").and_then(Value::as_str);
        if !request.is_object()
            || request.get("jsonrpc").and_then(Value::as_str) != Some("2.0")
            || method.is_none()
            || !valid_id
        {
            let error_id = if valid_id { id } else { None };
            send(&mut output,json!({"jsonrpc":"2.0","id":error_id,"error":{"code":-32600,"message":"Invalid Request"}})).await?;
            continue;
        }
        let method = method.unwrap_or_default();
        // MCP method parameters are objects. Invalid notifications still have no reply,
        // and must not advance initialization state.
        if request
            .get("params")
            .is_some_and(|params| !params.is_object())
        {
            if let Some(id) = id {
                send(&mut output,json!({"jsonrpc":"2.0","id":id,"error":{"code":-32602,"message":"Invalid params"}})).await?;
            }
            continue;
        }
        if id.is_none() {
            if method == "notifications/initialized" && initialized {
                ready = true;
            }
            // Worker cancellation uses the durable tool; no long-lived request id is retained.
            continue;
        }
        let result = match method {
            "initialize" if !initialized => {
                let requested = request
                    .pointer("/params/protocolVersion")
                    .and_then(Value::as_str)
                    .unwrap_or("2025-06-18");
                let version = if ["2024-11-05", "2025-03-26", "2025-06-18"].contains(&requested) {
                    requested
                } else {
                    "2025-06-18"
                };
                initialized = true;
                Ok(
                    json!({"protocolVersion":version,"capabilities":{"tools":{"listChanged":false}},
                    "serverInfo":{"name":"codex-relay","version":env!("CARGO_PKG_VERSION")},
                    "instructions":"Sol plans, reviews and integrates. Create and attest Codex-managed worktrees before starts. Profiles are non-secret and disabled until authorized. Native load must be freshly attested. No native progress/diff UI integration is implied."}),
                )
            }
            "ping" => Ok(json!({})),
            "tools/list" if ready => Ok(json!({"tools":tools()})),
            "tools/call" if ready => {
                let name = request
                    .pointer("/params/name")
                    .and_then(Value::as_str)
                    .unwrap_or("");
                let args = request
                    .pointer("/params/arguments")
                    .cloned()
                    .unwrap_or_else(|| json!({}));
                match call(&engine, config_path, name, args).await {
                    Ok(value) => Ok(
                        json!({"content":[{"type":"text","text":serde_json::to_string(&value)?}],"structuredContent":value,"isError":false}),
                    ),
                    Err(error) => Ok(
                        json!({"content":[{"type":"text","text":error.to_string()}],"isError":true}),
                    ),
                }
            }
            _ => Err(if !ready { -32002 } else { -32601 }),
        };
        let response = match result {
            Ok(value) => json!({"jsonrpc":"2.0","id":id,"result":value}),
            Err(code) => {
                json!({"jsonrpc":"2.0","id":id,"error":{"code":code,"message":if code==-32002{"Initialize and send notifications/initialized first"}else{"Method not found"}}})
            }
        };
        send(&mut output, response).await?;
    }
    Ok(())
}

async fn send(output: &mut tokio::io::Stdout, value: Value) -> Result<()> {
    output
        .write_all(serde_json::to_string(&value)?.as_bytes())
        .await?;
    output.write_all(b"\n").await?;
    output.flush().await?;
    Ok(())
}

async fn read_frame(input: &mut BufReader<tokio::io::Stdin>) -> Result<Option<String>> {
    let mut frame = Vec::new();
    loop {
        let buffer = input.fill_buf().await?;
        if buffer.is_empty() {
            return if frame.is_empty() {
                Ok(None)
            } else {
                Ok(Some(String::from_utf8(frame)?))
            };
        }
        let end = buffer.iter().position(|b| *b == b'\n');
        let count = end.map_or(buffer.len(), |i| i + 1);
        ensure!(
            frame.len() + count <= 128 * 1024,
            "MCP request exceeds size limit"
        );
        frame.extend_from_slice(&buffer[..count]);
        input.consume(count);
        if end.is_some() {
            return Ok(Some(String::from_utf8(frame)?));
        }
    }
}
