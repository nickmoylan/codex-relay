# Native containment diagnosis

The initial synthetic probe failed before Relay reached `main`: launcher exit 134/SIGABRT, with libignition namespace `0x23` (35), code 2, and `ignition_halt`, `boot_boot`, `ignite` and dyld's `CacheFinder` on the faulting thread. The matching kernel log recorded `deny(1) file-read-data /` immediately before termination.

Apple's [reason definitions](https://raw.githubusercontent.com/apple-oss-distributions/xnu/main/bsd/sys/reason.h) identify namespace 35 as libignition; its [dyld source](https://raw.githubusercontent.com/apple-oss-distributions/dyld/main/dyld/DyldProcessConfig.cpp) calls ignition while locating the shared cache. The policy allowed root metadata but denied reading the root directory itself.

The user approved a specific runtime grant: `file-read-data` for the **literal directory `/`**, permitting top-level entries without recursive home reads. Retesting clears loader startup and passes the file-boundary stages, but the child then succeeds at `setsid()`: **sandbox permits session detachment**. The current Seatbelt `process-info-setcontrol` denial does not prevent this operation. A detached worker could escape process-group cleanup, so Relay refuses external startup. A process-containment backend that prevents or contains detached descendants remains necessary. No further permission grant or unrestricted fallback fixes that contract. Additional runtime permissions need separate review; do not grant home subtrees, private credentials, unrestricted Mach lookup or a fallback outside containment.

```sh
rtk proxy cargo test --locked --test worker_lifecycle real_os_isolation_probe -- --ignored --exact
```

Require denied reads, denied writes, permitted scratch writes and denied session/process-group detachment. A working loader alone is insufficient. Test the exact installed harness/runtime before enabling a live profile; authentication, privacy and billing remain separate gates. Other macOS versions and alternative containment backends are unvalidated. Do not change OS security settings or inject loader overrides.
