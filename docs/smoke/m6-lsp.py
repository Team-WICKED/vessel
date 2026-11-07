#!/usr/bin/env python3
"""Ask an isolated session's MCP tools about this repository using rust-analyzer.

No real agent account is used: fake-acp receives the session credential, and
an MCP client uses that credential just as an agent would. Rust analysis is
conservative (no build scripts/proc macros/check-on-save). Source is read only.
"""
import os
from pathlib import Path
import shutil
import subprocess
import sys

root = Path(__file__).resolve().parents[2]
server = shutil.which("rust-analyzer")
if not server:
    sys.exit("SKIP: install rust-analyzer yourself and put it on PATH, then retry.")
probe = subprocess.run([server, "--version"], capture_output=True, text=True)
if probe.returncode:
    sys.exit("SKIP: rust-analyzer on PATH is unavailable (for rustup, install its component yourself).")
print(probe.stdout.strip(), flush=True)
env = os.environ.copy()
env.setdefault("CARGO_BUILD_JOBS", "4")
command = ["cargo", "test", "-p", "vessel-mcp", "--test", "mcp",
           "real_rust_analyzer_context_through_agent_mcp", "--", "--ignored", "--nocapture",
           "--test-threads=1"]
raise SystemExit(subprocess.run(command, cwd=root, env=env).returncode)
