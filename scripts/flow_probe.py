#!/usr/bin/env python3
"""Read-only bounded version probes for a pinned RustROAD flow manifest.

The workspace must exist. This command takes a shared workspace.lock and writes
only one JSON result to stdout. It never turns availability into validation.
"""
import argparse
import fcntl
import hashlib
import json
import os
from pathlib import Path
import resource
import selectors
import signal
import stat
import subprocess
import sys
import time

MAX_INPUT = 1024 * 1024
MAX_FILE = 256 * 1024 * 1024
MAX_OUTPUT = 65536
ARGS = {"rustsys": ["--version"], "rusteda": ["--version"], "ruststa": ["--version"],
        "rustroad": ["--version"], "yosys": ["-V"], "opensta": ["-version"],
        "openroad": ["-version"], "verilator": ["--version"], "iverilog": ["-V"],
        "vvp": ["-V"], "ngspice": ["--version"], "klayout": ["-v"],
        "magic": ["--version"], "netgen": ["-batch"]}


def digest(path):
    if not path.is_file():
        raise ValueError("identity input is not a regular file")
    descriptor = os.open(path, os.O_RDONLY | os.O_NONBLOCK)
    with os.fdopen(descriptor, "rb") as source:
        metadata = os.fstat(source.fileno())
        if not stat.S_ISREG(metadata.st_mode) or metadata.st_size > MAX_FILE:
            raise ValueError("identity input must be a regular file at most 256 MiB")
        value, size = hashlib.sha256(), 0
        while chunk := source.read(65536):
            size += len(chunk)
            if size > MAX_FILE:
                raise ValueError("identity input grew beyond 256 MiB")
            value.update(chunk)
        after = os.fstat(source.fileno())
        if (after.st_size, after.st_mtime_ns) != (metadata.st_size, metadata.st_mtime_ns):
            raise ValueError("identity input changed while hashing")
    return value.hexdigest()


def probe(command, seconds):
    def limits():
        for key, value in ((resource.RLIMIT_AS, 1024**3), (resource.RLIMIT_CPU, seconds + 1),
                           (resource.RLIMIT_FSIZE, MAX_OUTPUT), (resource.RLIMIT_CORE, 0)):
            _, hard = resource.getrlimit(key)
            value = value if hard == resource.RLIM_INFINITY else min(value, hard)
            resource.setrlimit(key, (value, value))
    started, data, status = time.monotonic(), bytearray(), "completed"
    process = subprocess.Popen(command, stdin=subprocess.DEVNULL, stdout=subprocess.PIPE,
        stderr=subprocess.STDOUT, start_new_session=True, preexec_fn=limits,
        env={**os.environ, "QT_QPA_PLATFORM": "offscreen", "LC_ALL": "C", "OMP_NUM_THREADS": "1"})
    selector = selectors.DefaultSelector()
    selector.register(process.stdout, selectors.EVENT_READ)
    try:
        while selector.get_map():
            if time.monotonic() - started > seconds:
                status = "timeout"
                break
            for key, _ in selector.select(timeout=0.05):
                chunk = os.read(key.fd, 8192)
                if not chunk:
                    selector.unregister(key.fileobj)
                else:
                    data.extend(chunk)
                    if len(data) > MAX_OUTPUT:
                        status = "output_limit"
                        break
            if status != "completed":
                break
        while status == "completed" and os.waitid(os.P_PID, process.pid, os.WEXITED | os.WNOHANG | os.WNOWAIT) is None:
            if time.monotonic() - started > seconds:
                status = "timeout"
                break
            time.sleep(0.005)
        # Version probes must terminate their complete process group, including
        # wrapper descendants. Keep leader unreaped until cleanup to avoid reuse.
        try:
            os.killpg(process.pid, signal.SIGKILL)
        except ProcessLookupError:
            pass
        code = process.wait(timeout=2)
    finally:
        selector.close()
        process.stdout.close()
        if process.returncode is None:
            os.killpg(process.pid, signal.SIGKILL)
            process.wait(timeout=2)
    return {"status": status, "exit_code": code, "wall_seconds": time.monotonic() - started,
        "output": data[:MAX_OUTPUT].decode(errors="replace"), "max_output_bytes": MAX_OUTPUT,
        "memory_limit_mib": 1024, "timeout_seconds": seconds}


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--workspace", type=Path, required=True)
    parser.add_argument("--manifest", type=Path, required=True)
    parser.add_argument("--timeout-seconds", type=int, choices=range(1, 11), default=3)
    args = parser.parse_args()
    try:
        with (args.workspace / "workspace.lock").open("rb") as lock:
            fcntl.flock(lock.fileno(), fcntl.LOCK_SH | fcntl.LOCK_NB)
            if not args.manifest.is_file():
                raise ValueError("manifest must be a regular file")
            descriptor = os.open(args.manifest, os.O_RDONLY | os.O_NONBLOCK)
            with os.fdopen(descriptor, "rb") as source:
                if not stat.S_ISREG(os.fstat(source.fileno()).st_mode):
                    raise ValueError("manifest changed to a non-regular file")
                raw = source.read(MAX_INPUT + 1)
            if len(raw) > MAX_INPUT:
                raise ValueError("manifest exceeds 1 MiB")
            manifest = json.loads(raw)
            if manifest.get("schema") != "rustroad.flow.v1" or len(manifest["tools"]) > 16:
                raise ValueError("unsupported manifest or tool budget exceeded")
            result = {"schema": "rustroad.tool-probe.v1", "status": "assessed", "tools": [],
                      "full_rtl_to_gds_status": "unverified", "analysis_scope": {
                          "corner": None, "wire_model": None, "unconstrained_endpoints": None},
                      "limitations": ["Version probe only; no EDA verification is performed.",
                          "Hashes pin entrypoints, not shared libraries or wrapper descendants."]}
            for tool in manifest["tools"]:
                record = {"id": tool["id"], "verification_status": "not_run"}
                try:
                    binary = Path(tool["executable"]["path"])
                    actual = digest(binary)
                    record.update(path=str(binary), sha256=actual)
                    if actual != tool["executable"]["sha256"]:
                        raise ValueError("binary hash mismatch")
                    flags = ARGS.get(tool["id"])
                    if flags is None:
                        raise ValueError("no allowlisted version probe for tool id")
                    record.update(probe([str(binary), *flags], args.timeout_seconds))
                    record["identity_stable"] = actual == digest(binary)
                    # A killed leader after EOF may be a successfully completed
                    # wrapper with children. Preserve code; never infer a pass.
                    record["availability"] = "available" if record["exit_code"] == 0 and record["identity_stable"] else "unverified"
                except (OSError, ValueError, subprocess.SubprocessError) as error:
                    record.update(availability="unverified", reason=str(error))
                result["tools"].append(record)
            print(json.dumps(result, indent=2))
        return 0
    except (OSError, ValueError, KeyError, TypeError) as error:
        print(json.dumps({"status": "unverified", "diagnostic": str(error)}))
        return 1


if __name__ == "__main__":
    sys.exit(main())
