#!/usr/bin/env python3
"""Create an allowlisted public source archive, without local integration/state."""
import argparse
import hashlib
import gzip
import json
from pathlib import Path
import re
import tarfile

FILES = ["Cargo.toml", "Cargo.lock", "README.md", ".gitignore", ".dockerignore", "AGENTS.md",
         "scripts/install.py", "scripts/smoke.py", "scripts/package_source.py"]
DIRECTORIES = ["src", "tests", "docs", "examples", "skills", "containers", ".github/workflows"]


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--output", type=Path, required=True)
    args = parser.parse_args()
    root = Path(__file__).resolve().parents[1]
    paths = [root / p for p in FILES]
    for folder in DIRECTORIES:
        paths.extend(p for p in (root / folder).rglob("*") if p.is_file())
    if (root / "LICENSE").is_file():
        paths.append(root / "LICENSE")
    manifest = []
    for path in sorted(paths):
        if path.is_symlink() or any(p.is_symlink() for p in path.parents if p != root.parent):
            raise RuntimeError("symlinked source refused")
        text = path.read_text()
        owner_path = re.compile(r"(?:^|[\"'=:\s])/" + r"(?:Users|home)/", re.MULTILINE)
        if owner_path.search(text) or ("Documents" + "/" + "Codex") in text:
            raise RuntimeError("machine-specific path in public source: " + str(path.relative_to(root)))
        manifest.append(str(path.relative_to(root)))
    args.output.parent.mkdir(parents=True, exist_ok=True)
    def portable_metadata(info):
        info.uid = info.gid = 0
        info.uname = info.gname = ""
        info.mtime = 0
        return info

    with args.output.open("wb") as raw:
        with gzip.GzipFile(filename="", fileobj=raw, mode="wb", mtime=0) as compressed:
            with tarfile.open(fileobj=compressed, mode="w") as archive:
                for relative in manifest:
                    archive.add(root / relative, arcname="codex-relay/" + relative,
                                recursive=False, filter=portable_metadata)
    digest = hashlib.sha256(args.output.read_bytes()).hexdigest()
    args.output.with_suffix(args.output.suffix + ".sha256").write_text(digest + "  " + args.output.name + "\n")
    args.output.with_suffix(args.output.suffix + ".manifest.json").write_text(json.dumps(manifest, indent=2) + "\n")
    print(json.dumps({"archive": str(args.output), "sha256": digest, "files": len(manifest), "license_present": (root / "LICENSE").is_file()}))


if __name__ == "__main__":
    main()
