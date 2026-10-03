#!/usr/bin/env python3
"""Install, replace, roll back or remove only receipt-owned Relay binaries."""
import argparse
import base64
import fcntl
import hashlib
import json
import os
from pathlib import Path
import stat
import tempfile


def digest(data):
    return hashlib.sha256(data).hexdigest()


def sha(path):
    return digest(path.read_bytes())


def safe(path):
    for part in [path] + list(path.parents):
        if part.is_symlink():
            raise RuntimeError("symlinked installation path refused")
    if path.exists() and (not path.is_file() or path.stat().st_nlink != 1):
        raise RuntimeError("non-regular or hardlinked destination refused")


def sync_directory(path):
    fd = os.open(path, os.O_RDONLY)
    try:
        os.fsync(fd)
    finally:
        os.close(fd)


def atomic(path, data, mode):
    safe(path)
    path.parent.mkdir(parents=True, exist_ok=True)
    fd, name = tempfile.mkstemp(prefix=".relay-", dir=path.parent)
    try:
        os.fchmod(fd, mode)
        with os.fdopen(fd, "wb") as output:
            output.write(data)
            output.flush()
            os.fsync(output.fileno())
        os.replace(name, path)
        sync_directory(path.parent)
    finally:
        if os.path.exists(name):
            os.unlink(name)


def checkpoint(name):
    # Synthetic fault injection: no executable or external command is accepted.
    if os.environ.get("CODEX_RELAY_INSTALL_TEST_FAIL_AFTER") == name:
        raise RuntimeError("synthetic interrupted transaction after " + name)


def image(path):
    safe(path)
    if not path.exists():
        return None
    if path.stat().st_size > 64 * 1024 * 1024:
        raise RuntimeError("installation file exceeds transaction limit")
    return {"data": base64.b64encode(path.read_bytes()).decode("ascii"),
            "mode": stat.S_IMODE(path.stat().st_mode)}


def decode(value):
    if value is None:
        return None
    if not isinstance(value, dict) or set(value) != {"data", "mode"}:
        raise RuntimeError("invalid transaction file record")
    if type(value["mode"]) is not int or value["mode"] not in range(0o1000) or not isinstance(value["data"], str):
        raise RuntimeError("invalid transaction mode or data")
    try:
        data = base64.b64decode(value["data"], validate=True)
    except ValueError as error:
        raise RuntimeError("invalid transaction encoding") from error
    if len(data) > 64 * 1024 * 1024:
        raise RuntimeError("transaction file exceeds limit")
    return data, value["mode"]


def encoded(data, mode):
    return None if data is None else {"data": base64.b64encode(data).decode("ascii"), "mode": mode}


def complete(journal, paths):
    safe(journal)
    if journal.stat().st_size > 256 * 1024 * 1024:
        raise RuntimeError("transaction journal exceeds limit")
    transaction = json.loads(journal.read_text())
    if (not isinstance(transaction, dict) or set(transaction) != {"schema_version", "files"}
            or transaction["schema_version"] != 1
            or not isinstance(transaction["files"], dict)
            or set(transaction["files"]) != set(paths)):
        raise RuntimeError("invalid installation transaction")
    # Validate every file before changing any: an owner's intervening edit is never
    # overwritten, including an edit after an interrupted install or uninstall.
    for name, path in paths.items():
        entry = transaction["files"][name]
        if not isinstance(entry, dict) or set(entry) != {"before", "after"}:
            raise RuntimeError("invalid installation transaction entry")
        decode(entry["before"])
        decode(entry["after"])
        if image(path) not in [entry["before"], entry["after"]]:
            raise RuntimeError("interrupted install contains an owner-modified file; recovery refused")
    for name, path in paths.items():
        desired = decode(transaction["files"][name]["after"])
        if desired is None:
            if path.exists():
                path.unlink()
                sync_directory(path.parent)
        else:
            atomic(path, *desired)
        checkpoint(name)
    journal.unlink()
    sync_directory(journal.parent)
    checkpoint("commit")


def transact(journal, paths, targets):
    transaction = {"schema_version": 1, "files": {
        name: {"before": image(path), "after": targets[name]}
        for name, path in paths.items()}}
    atomic(journal, (json.dumps(transaction) + "\n").encode(), 0o600)
    checkpoint("journal")
    complete(journal, paths)


def operate(args, prefix, paths, journal):
    recovered = journal.exists()
    if recovered:
        complete(journal, paths)
    if args.recover:
        print(json.dumps({"action": "recovered" if recovered else "nothing_to_recover",
                          "prefix": str(prefix), "global_configuration_changed": False}))
        return
    binary, previous, receipt = paths["binary"], paths["previous"], paths["receipt"]
    for path in paths.values():
        safe(path)
    record = json.loads(receipt.read_text()) if receipt.exists() else None
    if record is not None:
        if not isinstance(record, dict) or record.get("schema_version") != 1 or not binary.is_file() or sha(binary) != record.get("sha256"):
            raise RuntimeError("managed install differs from its receipt; refusing to replace or delete it")
        old_hash = record.get("previous_sha256")
        if (old_hash is None and previous.exists()) or (old_hash is not None and (not previous.exists() or sha(previous) != old_hash)):
            raise RuntimeError("backup differs from its receipt")
    elif binary.exists() or previous.exists():
        raise RuntimeError("destination contains an unowned binary; refusing overwrite")
    if args.rollback:
        if record is None or record.get("previous_sha256") is None:
            raise RuntimeError("no managed previous binary to roll back")
        current_bytes, previous_bytes = binary.read_bytes(), previous.read_bytes()
        record["sha256"], record["previous_sha256"] = record["previous_sha256"], record["sha256"]
        targets = {"binary": encoded(previous_bytes, 0o755), "previous": encoded(current_bytes, 0o755)}
        action = "rolled_back"
    elif args.uninstall:
        if record is None:
            raise RuntimeError("no managed installation to remove")
        transact(journal, paths, {name: None for name in paths})
        print(json.dumps({"action": "uninstalled", "prefix": str(prefix), "state_removed": False}))
        return
    else:
        if record is not None and not args.replace:
            raise RuntimeError("already installed; use --replace to retain a managed backup")
        source = args.binary.resolve(strict=True)
        if not source.is_file() or not os.access(source, os.X_OK) or source.stat().st_size > 64 * 1024 * 1024:
            raise RuntimeError("build an executable release binary within the transaction limit first")
        source_bytes = source.read_bytes()
        backup = binary.read_bytes() if record is not None else None
        targets = {"binary": encoded(source_bytes, 0o755), "previous": encoded(backup, 0o755)}
        record = {"schema_version": 1, "sha256": digest(source_bytes),
                  "previous_sha256": digest(backup) if backup is not None else None}
        action = "installed"
    targets["receipt"] = encoded((json.dumps(record, indent=2) + "\n").encode(), 0o600)
    transact(journal, paths, targets)
    print(json.dumps({"action": action, "binary": str(binary), "receipt": str(receipt),
                      "global_configuration_changed": False}))


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--prefix", type=Path, required=True)
    parser.add_argument("--binary", type=Path, default=Path(__file__).resolve().parents[1] / "target/release/codex-relay")
    modes = parser.add_mutually_exclusive_group()
    for mode in ["replace", "rollback", "uninstall", "recover"]:
        modes.add_argument("--" + mode, action="store_true")
    args = parser.parse_args()
    prefix = args.prefix.absolute()
    if len(prefix.parts) < 3 or ".." in prefix.parts:
        raise RuntimeError("use a dedicated explicit installation prefix")
    metadata = prefix / "share/codex-relay"
    lock = metadata / "install.lock"
    journal = metadata / "transaction.json"
    paths = {"previous": prefix / "bin/codex-relay.previous", "binary": prefix / "bin/codex-relay",
             "receipt": metadata / "install.json"}
    for path in [*paths.values(), journal, lock]:
        safe(path)
    metadata.mkdir(parents=True, exist_ok=True)
    fd = os.open(lock, os.O_CREAT | os.O_RDWR | os.O_NOFOLLOW, 0o600)
    try:
        info = os.fstat(fd)
        if not stat.S_ISREG(info.st_mode) or info.st_nlink != 1:
            raise RuntimeError("non-regular or hardlinked installation lock refused")
        fcntl.flock(fd, fcntl.LOCK_EX)
        operate(args, prefix, paths, journal)
    finally:
        os.close(fd)


if __name__ == "__main__":
    try:
        main()
    except (OSError, ValueError, RuntimeError, TypeError) as error:
        import sys
        print("codex-relay install: " + str(error), file=sys.stderr)
        raise SystemExit(1)
