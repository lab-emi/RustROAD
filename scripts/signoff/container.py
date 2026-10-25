#!/usr/bin/env python3
# SPDX-License-Identifier: Apache-2.0
"""Guarded podman runner for black-box signoff tools.

Every run uses ``--pull=never --network=none --read-only`` with a small /tmp
tmpfs, drops all capabilities, sets ``no-new-privileges``, caps memory (swap
equal to memory, i.e. no extra swap), CPUs and pids, and bind-mounts only the
explicitly listed host paths read-only plus exactly one writable output
directory at ``/out``. The command runs under coreutils ``timeout`` and GNU
``/usr/bin/time -v`` inside the container, so every receipt carries the
tool's peak RSS; podman's own ``--timeout`` and a host-side subprocess timeout
are backstops. The environment is fixed (no host variables are forwarded).

Exit codes of the wrapped tools are recorded but never interpreted as a
verdict: several signoff tools exit 0 on failure. Callers parse reports.
"""
from __future__ import annotations

import argparse
import json
import os
from pathlib import Path
import re
import subprocess
import sys
import time

SIGNOFF_IMAGE = "4c48415d8bd08c04033f4777b65a96299e22c1e94d12a8ea1d9a062697c006ee"
CONTAINER_PATH = "/foss/tools/bin:/foss/tools/klayout:/usr/local/bin:/usr/bin:/bin"
# Host directories that are too broad to expose to a tool container.
FORBIDDEN_HOST_DIRS = {"/", "/home", "/mnt", "/mnt/data2", "/usr", "/etc", "/var", "/tmp",
                       str(Path.home())}
# Container paths that must not be shadowed by a bind mount.
RESERVED_CONTAINER_PREFIXES = ("/foss", "/usr", "/bin", "/sbin", "/lib", "/etc", "/proc",
                               "/sys", "/dev", "/tmp", "/out")
MAX_CAPTURE_BYTES = 64 * 1024 * 1024
TIME_FIELDS = {
    "peak_rss_kib": r"Maximum resident set size \(kbytes\):\s*(\d+)",
    "user_seconds": r"User time \(seconds\):\s*([0-9.]+)",
    "system_seconds": r"System time \(seconds\):\s*([0-9.]+)",
    "inner_exit_status": r"Exit status:\s*(-?\d+)",
}


class ContainerError(RuntimeError):
    """The container could not be started or the request was unsafe."""


def deterministic_env(threads: int, extra: dict[str, str] | None = None) -> dict[str, str]:
    env = {
        "LC_ALL": "C", "LANG": "C", "TZ": "UTC", "HOME": "/tmp", "PDK_ROOT": "/foss/pdks",
        "PATH": CONTAINER_PATH, "PYTHONHASHSEED": "0", "PYTHONDONTWRITEBYTECODE": "1",
        "QT_QPA_PLATFORM": "offscreen", "OMP_NUM_THREADS": str(threads),
        "SOURCE_DATE_EPOCH": "0", "MAGTYPE": "mag",
    }
    for key, value in (extra or {}).items():
        if not re.fullmatch(r"[A-Z_][A-Z0-9_]*", key):
            raise ContainerError(f"unsafe environment variable name {key!r}")
        env[key] = str(value)
    return env


def check_mount(host: str, container: str) -> tuple[Path, str]:
    path = Path(host)
    if not path.is_absolute():
        raise ContainerError(f"mount source must be absolute: {host}")
    resolved = path.resolve()
    if not resolved.exists():
        raise ContainerError(f"mount source does not exist: {host}")
    if resolved.is_dir() and str(resolved) in FORBIDDEN_HOST_DIRS:
        raise ContainerError(f"refusing to expose broad host directory {resolved}")
    if not container.startswith("/") or ".." in Path(container).parts or ":" in container:
        raise ContainerError(f"bad container path {container!r}")
    if any(container == p or container.startswith(p + "/") for p in RESERVED_CONTAINER_PREFIXES):
        raise ContainerError(f"container path {container} shadows a reserved image path")
    if "," in str(resolved) or ":" in str(resolved):
        raise ContainerError(f"mount source contains a separator character: {resolved}")
    return resolved, container


def parse_time_file(text: str) -> dict:
    out = {}
    for key, pattern in TIME_FIELDS.items():
        match = re.search(pattern, text)
        if match:
            out[key] = float(match[1]) if "." in match[1] else int(match[1])
    return out


def image_identity(image: str, timeout: int = 60) -> dict:
    """Return podman's identity record for an image id, or an error record."""
    try:
        result = subprocess.run(["podman", "image", "inspect", image, "--format",
                                 "{{json .}}"], capture_output=True, text=True,
                                timeout=timeout, check=False, stdin=subprocess.DEVNULL)
    except (OSError, subprocess.TimeoutExpired) as error:
        return dict(present=False, error=str(error))
    if result.returncode != 0:
        return dict(present=False, error=result.stderr.strip()[:2000])
    data = json.loads(result.stdout)
    if isinstance(data, list):
        data = data[0]
    env = (data.get("Config") or {}).get("Env") or []
    return dict(present=True, id=data.get("Id"), repo_digests=sorted(data.get("RepoDigests") or []),
                repo_tags=sorted(data.get("RepoTags") or []), created=data.get("Created"),
                env=[e for e in env if e.startswith("IIC_OSIC_TOOLS_VERSION=")])


def run(command: list[str], *, out_dir: Path, label: str, image: str = SIGNOFF_IMAGE,
        ro: list[tuple[str, str]] | None = None, memory: str = "4g", cpus: int = 4,
        pids: int = 256, timeout: int = 900, tmpfs_mib: int = 512, threads: int = 4,
        env: dict[str, str] | None = None, workdir: str = "/out") -> dict:
    """Run ``command`` in ``image``; return a receipt (never raises on tool failure)."""
    if not re.fullmatch(r"[A-Za-z0-9_.-]+", label):
        raise ContainerError(f"label must be shell-safe: {label!r}")
    if not re.fullmatch(r"[0-9]+[mg]", memory):
        raise ContainerError(f"memory must look like 4g/512m: {memory!r}")
    out_dir = out_dir.resolve()
    if str(out_dir) == "/tmp" or str(out_dir).startswith("/tmp/"):
        raise ContainerError("output directory under RAM-backed /tmp is not allowed")
    out_dir.mkdir(parents=True, exist_ok=True)
    receipt_dir = out_dir / "receipts" / label
    receipt_dir.mkdir(parents=True, exist_ok=False)
    time_rel = receipt_dir.relative_to(out_dir) / "time.txt"
    mounts = [check_mount(h, c) for h, c in (ro or [])]
    containers = [c for _, c in mounts]
    if len(set(containers)) != len(containers):
        raise ContainerError("duplicate container mount path")
    name = f"rustroad-signoff-{os.getpid()}-{label}"[:200]
    argv = ["podman", "run", "--rm", "--pull=never", "--network=none", "--read-only",
            f"--tmpfs=/tmp:rw,size={tmpfs_mib}m,mode=1777", "--user=0:0", "--cap-drop=all",
            "--security-opt=no-new-privileges", f"--memory={memory}", f"--memory-swap={memory}",
            f"--cpus={cpus}", f"--pids-limit={pids}", f"--timeout={timeout + 30}",
            "--hostname=signoff", f"--name={name}", "--log-driver=none",
            f"--volume={out_dir}:/out:rw", f"--workdir={workdir}"]
    for host, container in mounts:
        argv.append(f"--volume={host}:{container}:ro")
    for key, value in sorted(deterministic_env(threads, env).items()):
        argv.append(f"--env={key}={value}")
    argv += ["--entrypoint=/usr/bin/timeout", image, "--signal=TERM", "--kill-after=5s",
             f"{timeout}s", "/usr/bin/time", "-v", "-o", f"/out/{time_rel}", *map(str, command)]
    started = time.monotonic()
    stdout_path, stderr_path = receipt_dir / "stdout.txt", receipt_dir / "stderr.txt"
    host_error = None
    with stdout_path.open("wb") as stdout, stderr_path.open("wb") as stderr:
        try:
            proc = subprocess.run(argv, stdout=stdout, stderr=stderr, stdin=subprocess.DEVNULL,
                                  timeout=timeout + 60, check=False)
            exit_code = proc.returncode
        except subprocess.TimeoutExpired:
            subprocess.run(["podman", "rm", "-f", name], capture_output=True, check=False,
                           stdin=subprocess.DEVNULL, timeout=60)
            exit_code, host_error = None, "host-side timeout"
        except OSError as error:
            exit_code, host_error = None, f"podman could not start: {error}"
    wall = time.monotonic() - started
    for path in (stdout_path, stderr_path):
        if path.stat().st_size > MAX_CAPTURE_BYTES:
            host_error = (host_error or "") + f" {path.name} exceeded capture budget"
    time_file = out_dir / time_rel
    measured = parse_time_file(time_file.read_text(errors="replace")) if time_file.is_file() else {}
    receipt = dict(label=label, image=image, command=[str(c) for c in command], podman=argv,
                   mounts=[dict(host=str(h), container=c, mode="ro") for h, c in mounts] +
                   [dict(host=str(out_dir), container="/out", mode="rw")],
                   limits=dict(memory=memory, memory_swap=memory, cpus=cpus, pids=pids,
                               timeout_seconds=timeout, tmpfs_mib=tmpfs_mib, network="none",
                               capabilities="none", threads=threads),
                   exit_code=exit_code, timed_out=exit_code == 124 or host_error == "host-side timeout",
                   host_error=host_error, wall_seconds=round(wall, 3),
                   peak_rss_kib=measured.get("peak_rss_kib"), time=measured,
                   stdout=str(stdout_path), stderr=str(stderr_path), time_file=str(time_file))
    (receipt_dir / "receipt.json").write_text(json.dumps(receipt, indent=2) + "\n")
    return receipt


def next_label(out_dir: Path, prefix: str) -> str:
    """First ``prefix_N`` whose receipt directory does not exist yet (labels never collide)."""
    n = 0
    while (Path(out_dir) / "receipts" / f"{prefix}_{n}").exists():
        n += 1
    return f"{prefix}_{n}"


def read_capture(path: str | Path, limit: int = MAX_CAPTURE_BYTES) -> str:
    path = Path(path)
    if not path.is_file():
        return ""
    with path.open("rb") as handle:
        return handle.read(limit).decode("utf-8", errors="replace")


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    sub = parser.add_subparsers(dest="action", required=True)
    r = sub.add_parser("run", help="run one guarded command; prints the receipt JSON")
    r.add_argument("--image", default=SIGNOFF_IMAGE)
    r.add_argument("--out", type=Path, required=True, help="host dir mounted rw at /out")
    r.add_argument("--label", required=True)
    r.add_argument("--ro", action="append", default=[], metavar="HOST:CONTAINER")
    r.add_argument("--memory", default="4g")
    r.add_argument("--cpus", type=int, default=4)
    r.add_argument("--threads", type=int, default=4)
    r.add_argument("--timeout", type=int, default=900)
    r.add_argument("--env", action="append", default=[], metavar="KEY=VALUE")
    r.add_argument("cmd", nargs=argparse.REMAINDER)
    i = sub.add_parser("inspect", help="print podman identity of an image")
    i.add_argument("--image", default=SIGNOFF_IMAGE)
    pr = sub.add_parser("probe", help="re-verify image/tool/PDK identities against the manifest")
    pr.add_argument("--manifest", type=Path, default=Path(__file__).resolve().parents[2] / "fixtures/flow/signoff-pinned.json")
    pr.add_argument("--out", type=Path, required=True)
    pr.add_argument("--pdk", action="append", choices=("sky130", "gf180"))
    args = parser.parse_args(argv)
    if args.action == "inspect":
        print(json.dumps(image_identity(args.image), indent=2))
        return 0
    if args.action == "probe":
        sys.path.insert(0, str(Path(__file__).resolve().parent))
        import manifest  # noqa: PLC0415 (lazy: manifest imports this module)
        result = manifest.verify(args.manifest.resolve(), args.out.resolve(), args.pdk or ["sky130", "gf180"],
                                 sorted(manifest.TOOL_SPEC) + ["openroad", "opensta"], label=next_label(args.out, "probe"))
        print(json.dumps(dict(status=result["status"], mismatches=result["mismatches"], checked=result.get("checked"))))
        return 0 if result["status"] == "verified" else 3
    cmd = args.cmd[1:] if args.cmd[:1] == ["--"] else args.cmd
    if not cmd:
        parser.error("missing command after --")
    ro = []
    for spec in args.ro:
        host, sep, container = spec.rpartition(":")
        if not sep:
            parser.error(f"--ro needs HOST:CONTAINER, got {spec!r}")
        ro.append((host, container))
    env = dict(e.split("=", 1) for e in args.env)
    receipt = run(cmd, out_dir=args.out, label=args.label, image=args.image, ro=ro,
                  memory=args.memory, cpus=args.cpus, timeout=args.timeout,
                  threads=args.threads, env=env)
    print(json.dumps(receipt, indent=2))
    return 0 if receipt["exit_code"] is not None else 2


if __name__ == "__main__":
    sys.exit(main())
