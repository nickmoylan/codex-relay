# Release readiness

The core supports a verified experimental source install on macOS and Linux ARM64. A production release for live providers remains blocked by independent review and provider handoff gates.

| Area | Status | Release requirement |
| --- | --- | --- |
| CLI/lifecycle/stdio MCP | 37 core tests pass on macOS and Linux ARM64 | Both installed-binary six-tool MCP smokes pass. |
| Source package and prefix installer | Locally verified | Fresh public archive built/tested on Linux; both platforms pass install, replace, rollback and uninstall. |
| Native containment | Blocked: `setsid()` succeeds | Stronger process containment and complete denied-access/detachment proof. |
| Docker containment | Seven real synthetic tests pass | macOS host / Linux daemon: denied host access, copy quarantine, success/cancel/timeout/orphan cleanup, interrupted-import reservations, HTTPS allow/deny. |
| Independent security review | Pending | Reconcile review of containment, copy-back, subprocesses and the minimal CONNECT/SNI broker before live use. |
| Muse authentication | Blocked | Safe subscription/session handoff and billing proof. |
| Aider/provider routes/privacy | Untested | Pin versions and run an authorized public-data smoke. |
| Hosted CI | Prepared, unrun | Inspect the first hosted result after approved publication. |
| macOS/Linux CLI | Local macOS and isolated Linux ARM64 builds pass | Native Linux host and x86-64 untested locally; hosted matrix remains unrun. |
| License | MIT selected | Canonical LICENSE and Cargo metadata agree. |
| Public publication | Approval pending | Approve the exact public files and publication scope. |

## Package and release gate

```sh
rtk proxy python3 scripts/package_source.py --output dist/codex-relay-source.tar.gz
```

The allowlisted archive excludes machine config, private patches, local integration, state and build output. It includes source, tests, examples, docs, a portable skill and scripts, with a SHA-256 sidecar and manifest. Review it before publication. No binary release, registry, signing or automatic publication is configured; Cargo has `publish = false`.

The owner selected MIT. Preserve LICENSE and review dependency licenses before distribution. Run format, Clippy, the complete suite, release build, installed smoke and fresh archive installation checks. An ignored native test is not verified containment. Live release also needs authentication/privacy/billing proof and independent review.

## Upgrade, rollback and uninstall

```sh
rtk proxy python3 scripts/install.py --prefix "$HOME/.local"
rtk proxy python3 scripts/install.py --prefix "$HOME/.local" --replace
rtk proxy python3 scripts/install.py --prefix "$HOME/.local" --rollback
rtk proxy python3 scripts/install.py --prefix "$HOME/.local" --recover
rtk proxy python3 scripts/install.py --prefix "$HOME/.local" --uninstall
```

The installer manages its binary, one backup, receipt, private transaction journal and lock under the explicit prefix. Replace requires a valid receipt; rollback swaps the backup. A durable journal records each operation before changing those files, and an interrupted operation completes on the next invocation or with `--recover`. Recovery refuses intervening owner edits before changing any file. Unknown, modified, hardlinked and symlinked destinations fail closed. State/configuration and unrelated files remain untouched; uninstall leaves the lock and directories. Fault-injection tests cover every mutation in install, replace, rollback and uninstall, interrupted recovery, and receipt-write failure. State-schema migration beyond version 1 is unavailable.

Remove manually added MCP/skill entries yourself after reviewing the exact entries. Confirm jobs have stopped before deleting state. No script silently changes global agent instructions or credentials.
