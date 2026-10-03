# Security boundaries

The coordinator controls admission, executable profiles and integration. Workers receive assigned paths, criteria and an immutable ownership snapshot. External processes must pass the OS containment probe before startup.

Relay rejects traversal, source symlinks/hardlinks, suspicious Git includes/filters and credential-like source content. It inventories tracked/untracked source and ignored source within assigned scopes, then quarantines unexpected or oversized changes. Conservative checks may reject legitimate tasks.

Time/output limits and inherited-group cleanup bound each attempt. An unverifiable surviving group reserves its worktree for manual recovery. Jobs survive CLI/MCP disconnect. Corrections start a fresh attempt over unchanged prior edits; provider-session resume is unavailable.

Raw worker stdout/stderr is discarded. Results keep known criterion/test IDs, separating worker claims from independently observed outcomes. Task/profile records remain private state files; keep secrets out of their text.

## Credentials and privacy

Configuration accepts environment **names**, not credential values. Doctor does not read them. Future authorized adapters receive only named keys; verification commands receive a cleared environment without those keys. Live Muse is blocked because a safe subscription/session handoff is missing and inherited keys would reach its model-controlled shell tools.

The prepared Aider/OpenRouter request pins one model, an explicit provider allowlist, disabled fallback, denied data collection and explicit ZDR. Those settings request routing; Relay cannot verify upstream receipts. ZDR concerns inference retention, not arbitrary plugins. Other providers cannot claim confidentiality controls this adapter cannot enforce.

No hard dollar cap, independent native-agent discovery, automatic replay or arbitrary executable adapters are implemented. Aider's internal inference/edit retries have time/output bounds, not a hard model-step guarantee.

The [Docker boundary](runtime.md) uses sanitized source copies on macOS and Linux. Workers have no host mounts beyond those copies and scratch, and no external network interface. A trusted broker accepts only declared public HTTPS destinations through a private IPC socket. Its TLS ClientHello checks fail closed for fragmented hellos or encrypted names; some clients may be incompatible. The local Docker daemon and reviewed immutable image remain trusted. Relay does not install either.

The host and installed runtime remain trusted. Treat failed containment, incompatible capabilities and missing credentials/privacy as blockers. Review fixes, prove the synthetic boundary and use separate project databases, ports, sockets and messaging resources before live work.


The built-in simulator has a narrower verification contract: only fixed `/usr/bin/true` or `/usr/bin/false` with empty arguments. It cannot use task test arguments to launch Relay's hidden commands or another host executable. Native fixture launch also checks the exact built-in fixture invocation.
