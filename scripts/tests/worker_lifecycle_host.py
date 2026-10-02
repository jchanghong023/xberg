# -*- coding: utf-8 -*-
"""Simulated host: spawns the worker, publishes its pid, then idles.

Killing THIS process (and only it) orphans the worker the way a crashed or
force-killed host would - the worker must notice and exit on its own (P1).
"""
import json
import os
import subprocess
import sys
import time
from pathlib import Path

REPO = str(Path(__file__).resolve().parents[2])
EXE = os.path.join(REPO, "target", "debug", "xberg.exe")

pid_file = sys.argv[1]
env = dict(os.environ)
env.update({
    "HF_HUB_OFFLINE": "1",
    "HUGGINGFACE_HUB_OFFLINE": "1",
    "TRANSFORMERS_OFFLINE": "1",
    "NO_COLOR": "1",
})
proc = subprocess.Popen(
    [EXE, "worker", "--no-config-discovery", "--config-json", json.dumps({"owner_token": "host-kill-test"})],
    stdin=subprocess.PIPE, stdout=subprocess.PIPE, stderr=subprocess.DEVNULL, cwd=REPO, env=env,
)
with open(pid_file, "w") as fh:
    fh.write(str(proc.pid))
time.sleep(300)
