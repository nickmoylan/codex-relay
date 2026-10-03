# CLI conventions

Use `codex-relay --help` or `-h`, subcommand `--help`, and `--version` or `-V`. Data commands return JSON to stdout by default; `--json` makes that preference explicit. Help/version remain text. Errors go to stderr. Piping produces no color, progress messages or interactive prompts; `NO_COLOR` is honored by this color-free presentation. MCP stdout contains JSON-RPC only.

| Exit | Meaning |
| --- | --- |
| 0 | Command completed; inspect job status/result for worker success. |
| 1 | Operational, contract, configuration or safety failure. |
| 2 | Invalid command-line usage. |
| 130 | Client interrupted by SIGINT. |
| 143 | Client terminated by SIGTERM. |

Client signals stop the CLI/MCP client. Accepted jobs have independent supervisors and remain durable; use `cancel` and then `status` to confirm worker cleanup. A signaled supervisor drops its process guard; status resolves its interrupted record. Closing stdin ends MCP without discarding accepted jobs.

## Paths and precedence

Configuration precedence is `--config` > `CODEX_RELAY_CONFIG` > platform default. Relay reads only that configuration; it does not search other agent settings or credential files. An explicit `init --state-dir` overrides the state default; after init the config's `state_dir` controls it.

| Platform | Config | Default state |
| --- | --- | --- |
| macOS | `~/Library/Application Support/codex-relay/config.json` | `~/Library/Application Support/codex-relay/state` |
| Other Unix, unvalidated | `$XDG_CONFIG_HOME/codex-relay/config.json`, falling back to `~/.config/codex-relay/config.json` | `$XDG_STATE_HOME/codex-relay/state`, falling back to `~/.local/state/codex-relay/state` |

Absolute XDG directories apply on Linux. macOS and Linux use the explicit Docker boundary for external workers; see runtime.md for actual verification scope. Other operating systems have no supported execution backend.

Quote paths containing spaces. Use `--config=--leading-hyphen.json` or `--task=--leading-hyphen.json` for a flag-like filename. `--` follows clap's end-of-options convention; it does not turn an unknown option into a known subcommand.

```sh
rtk proxy codex-relay start --task task.json --json
rtk proxy codex-relay status --job UUID --json
rtk proxy codex-relay result --job UUID --json
rtk proxy codex-relay correct --job UUID --feedback-file correction.txt --native-load native-load.json
rtk proxy codex-relay cancel --job UUID
rtk proxy codex-relay schemas
```

`start` returns admission/progress, not a correctness verdict. Poll status/result. A successful `doctor` is an inspection result, not live-setup approval.
