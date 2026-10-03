#!/usr/bin/env python3
"""Exercise an installed Relay binary using synthetic files and all six MCP tools."""
import argparse
import json
import os
from pathlib import Path
import select
import shutil
import subprocess
import tempfile
import time


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--binary", type=Path, required=True)
    args = parser.parse_args()
    binary = args.binary.resolve(strict=True)
    rtk = shutil.which("rtk")
    if rtk is None:
        raise RuntimeError("RTK must be installed")
    with tempfile.TemporaryDirectory(prefix="codex-relay-smoke-", dir="/tmp") as temporary:
        root = Path(temporary).resolve()
        home = root / "home"
        home.mkdir()
        env = {"PATH": "/usr/bin:/bin:/opt/homebrew/bin:/usr/local/bin", "HOME": str(home),
               "LANG": "en_US.UTF-8", "NO_COLOR": "1", "GIT_CONFIG_NOSYSTEM": "1",
               "GIT_CONFIG_GLOBAL": "/dev/null", "GIT_TERMINAL_PROMPT": "0"}

        def run(argv, cwd=None):
            return subprocess.check_output([rtk, "proxy"] + list(map(str, argv)), cwd=cwd, env=env, stderr=subprocess.PIPE, timeout=30)

        repo = root / "repo"
        repo.mkdir()
        run(["/usr/bin/git", "init", "-q"], repo)
        (repo / "src").mkdir()
        (repo / "src/example.txt").write_text("before\n")
        run(["/usr/bin/git", "add", "src"], repo)
        run(["/usr/bin/git", "-c", "user.name=Fixture", "-c", "user.email=fixture@example.invalid",
             "-c", "commit.gpgsign=false", "-c", "core.hooksPath=/dev/null", "commit", "-qm", "test fixture"], repo)
        managed = root / "managed"
        managed.mkdir()
        worktree = managed / "worker"
        run(["/usr/bin/git", "worktree", "add", "--detach", worktree, "HEAD"], repo)
        revision = run(["/usr/bin/git", "rev-parse", "HEAD"], worktree).decode().strip()
        config_path = root / "config.json"
        run([binary, "--config", config_path, "init", "--state-dir", root / "state", "--managed-root", managed])
        config = json.loads(config_path.read_text())
        assert config["profiles"] == {} and config["allow_simulated_workers"] is False
        config["allow_simulated_workers"] = True
        config["profiles"] = {"fixture": {"enabled": True, "harness": "simulated", "executable": str(binary),
            "provider": "simulation", "model": "fixture", "reasoning": None, "tools": ["file_read", "file_edit"],
            "capabilities": {"code_editing": True, "reasoning": False, "tool_use": False},
            "privacy": {"confidential_code": False, "data_collection": "deny", "zero_data_retention": False,
                        "allowed_providers": [], "allow_fallbacks": False},
            "budget": {"timeout_seconds": 10, "max_model_steps": 3, "max_output_bytes": 16384,
                       "max_change_bytes": 1048576, "max_cost_usd": None},
            "credential_env": [], "api_base": None, "network_hosts": [], "runtime_read_roots": []}}
        config_path.write_text(json.dumps(config))
        doctor = json.loads(run([binary, "--config", config_path, "doctor", "--json"]))
        assert doctor["rtk_available"] and doctor["isolation_verified"] is False
        client = subprocess.Popen([rtk, "proxy", str(binary), "--config", str(config_path), "mcp"],
                                  stdin=subprocess.PIPE, stdout=subprocess.PIPE, stderr=subprocess.PIPE, env=env)
        counter = 0

        def request(method, params=None):
            nonlocal counter
            counter += 1
            payload = {"jsonrpc": "2.0", "id": counter, "method": method, "params": params or {}}
            client.stdin.write((json.dumps(payload) + "\n").encode())
            client.stdin.flush()
            if not select.select([client.stdout], [], [], 12)[0]:
                raise RuntimeError("MCP response timeout")
            response = json.loads(client.stdout.readline())
            assert response["id"] == counter and "error" not in response, response
            return response["result"]

        def tool(name, arguments):
            result = request("tools/call", {"name": name, "arguments": arguments})
            assert result["isError"] is False, result
            return result["structuredContent"]

        def load():
            return {"active_workers": 1, "observed_at_ms": int(time.time() * 1000), "source": "coordinator_attested"}

        def wait(job):
            deadline = time.monotonic() + 12
            while time.monotonic() < deadline:
                status = tool("workers_status", {"job_id": job})
                if status["status"] not in ["queued", "running"]:
                    return status
                time.sleep(0.05)
            raise RuntimeError("job timeout")

        try:
            initialized = request("initialize", {"protocolVersion": "2025-06-18", "capabilities": {}, "clientInfo": {"name": "relay-smoke", "version": "1"}})
            assert initialized["serverInfo"]["name"] == "codex-relay"
            client.stdin.write(b'{"jsonrpc":"2.0","method":"notifications/initialized"}\n')
            client.stdin.flush()
            tools = request("tools/list")["tools"]
            assert len(tools) == 6
            tool("workers_doctor", {})
            task = {"schema_version": 1, "profile": "fixture", "workspace": {"path": str(worktree),
                "artifact_identity": str(worktree), "base_revision": revision, "attested_at_ms": int(time.time() * 1000)},
                "objective": "Update the synthetic assigned text", "writable_paths": ["src"], "read_paths": [],
                "acceptance_criteria": {"done": "Text changed"},
                "tests": [{"id": "check", "program": "/usr/bin/true", "args": [], "timeout_seconds": 2}],
                "native_load": load(), "simulator": {"writes": {"src/example.txt": "after\n"}, "emit_steps": 1}}
            started = tool("workers_start", {"task": task})
            job = started["job_id"]
            assert wait(job)["status"] == "completed"
            result = tool("workers_result", {"job_id": job})["result"]
            assert result["independently_observed_tests"][0]["outcome"] == "passed"
            assert result["changes"][0]["tracked"] and (worktree / "src/example.txt").read_text() == "after\n"
            tool("workers_correct", {"job_id": job, "feedback": "Confirm the synthetic result once", "native_load": load()})
            assert wait(job)["status"] == "completed"
            tool("workers_cancel", {"job_id": job})
            print(json.dumps({"status": "passed", "binary": str(binary), "mcp_tools_exercised": 6,
                              "simulated_workflow": True, "live_provider_tested": False, "credentials_used": False}))
        finally:
            client.stdin.close()
            try:
                client.wait(timeout=5)
            except subprocess.TimeoutExpired:
                client.kill()
                client.wait()


if __name__ == "__main__":
    main()
