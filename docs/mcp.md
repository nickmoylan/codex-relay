# MCP and task contracts

Relay uses newline-delimited JSON-RPC over stdio, supporting MCP `2024-11-05`, `2025-03-26` and `2025-06-18`. Initialize and send `notifications/initialized` before calling tools.

Review this registration, substitute absolute paths and merge it without replacing other entries:

```toml
[mcp_servers.codex-relay]
command = "rtk"
args = ["proxy", "/absolute/path/to/codex-relay", "--config", "/absolute/path/to/config.json", "mcp"]
enabled = true
startup_timeout_sec = 10
tool_timeout_sec = 30
```

Registration enables local tool calls; it supplies no credentials and enables no profile. Reload your client to discover the tools. The prefix installer does not edit Codex instructions, models, memory, skills or MCP configuration.

| Tool | Contract |
| --- | --- |
| `workers_doctor` | Inspect non-secret declarations and executable availability. |
| `workers_start` | Admit a named profile in a supplied, freshly attested worktree. |
| `workers_status` | Read durable jobs/ownership and recover positively identified orphan groups. |
| `workers_result` | Read changed paths/hashes, claims, observed tests and blockers. |
| `workers_correct` | Request one fresh correction with bounded feedback and native-load attestation. |
| `workers_cancel` | Record cancellation; confirm cleanup through status/result. |

The coordinator obtains a worktree through supported Codex workspace controls and supplies its root, exact attachment identity, full HEAD and recent timestamp. Relay verifies the linked-worktree structure and clean initial source; it trusts the coordinator's claim that Codex created it. Terminal-created worktrees serve only as test fixtures.

Tasks contain relative writable/read paths, an objective, criteria and test argument vectors. The coordinator attests the native worker count, including itself, within 60 seconds of admission. Other native agents or installations using another state directory are not automatically visible.

Review actual files, integrate accepted edits into the attached task and run the project's full gate. Hashes and worker claims alone prove neither correctness nor fidelity. Native Codex progress cards/diff panes are outside this implementation.

`codex-relay schemas` emits generated config/task/result schemas. The [skill source](../skills/codex-relay/SKILL.md) preserves these boundaries; its installation requires a separate user decision.


Stdio uses one JSON-RPC object per line. Request IDs must be non-null strings or integers; null and fractional IDs are rejected. Malformed request objects, scalar values and invalid ID types receive `-32600` with a null ID when the ID cannot be established. Method parameters must be objects; invalid request parameters receive `-32602`. Valid notifications receive no reply, and invalid notification parameters do not advance initialization. JSON parse failures receive `-32700`. Batch arrays are unsupported.
