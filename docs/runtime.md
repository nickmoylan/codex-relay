# macOS and Linux runtime

Relay's CLI, state store and stdio MCP are portable Unix components. The worker boundary uses a local Linux Docker daemon on either macOS (Docker Desktop) or Linux. Installing or starting that daemon is an operator action; Relay never changes host security settings or starts a service.

Build the public synthetic image from a reviewed source archive:

```sh
rtk proxy docker build --file containers/Dockerfile --tag codex-relay-synthetic:local .
rtk proxy docker image inspect --format '{{.Id}}' codex-relay-synthetic:local
```

Use the returned immutable `sha256:…` image ID in a profile's `container` field:

```json
{
  "image": "sha256:REPLACE_WITH_64_LOWERCASE_HEX_DIGITS",
  "memory_mb": 512,
  "pids_limit": 64,
  "cpus": 1
}
```

This image contains Relay and RTK for **synthetic verification only**. It contains no Muse or Aider installation. A live harness needs a separately reviewed image with its executable and dependencies installed at declared in-image paths. Relay never pulls an image on job admission; tags are rejected. Keep all live profiles disabled until privacy, credentials and billing have separate verification.

The worker receives a sanitized copy of only declared readable/writable source and scratch directories. The supplied worktree, its Git metadata, host home, Docker socket and owner credentials are not mounted. Worker containers use a read-only image, dropped capabilities, no new privileges, the host's non-root numeric user, resource limits, no restart policy and no retained Docker logs.

The runtime records durable ownership before launch. Success, cancellation, timeout and recovery remove the entire owned container and verify its absence. Detached descendants stay within its PID namespace. Container names, IDs and labels must match before cleanup; unrelated containers are untouched. A cleanup uncertainty reserves the worktree. Only validated assigned changes are imported after container removal and a fresh check that the worktree has not changed.

Workers have `--network none`. The HTTPS route uses a loopback forwarder and a dedicated Unix IPC socket to a trusted CONNECT broker. The broker receives only a host allowlist, with no source or credential values; it allows port 443 to public resolved addresses only. The worker receives no bridge route to owner services. Synthetic verification passed for a public example.com HTTPS request and rejected unlisted, private and direct connections. That evidence does not verify provider authentication, billing or route/privacy guarantees. Profile credential fields contain names only; provider calls remain outside the current verification scope.

The native macOS Seatbelt path remains blocked by the observed session-detachment failure. Native Linux external execution fails closed. Choose the reviewed Docker boundary explicitly; Relay never falls back to unrestricted host execution.


Relay validates and records the canonical local Unix daemon endpoint before detaching a job. Supervisors, status recovery and cleanup use that durable endpoint with an empty private Docker configuration; cleared `HOME` does not change daemon selection.

Source inventory includes ignored files within declared read and write scopes. Ignored read references are exported without granting import permission. Snapshots and results record the owner-executable bit, including mode-only changes; copy-back normalizes files to 0644 or 0755. A concurrent host executable-bit change stops copy-back. Other permission bits, extended attributes and ACLs are outside the file-result contract.
