# SPDX-License-Identifier: Apache-2.0
"""Artifact identity binding and determinism comparison of the RTL-to-GDS gate.

``bind`` records the SHA-256 of every artifact (RTL, mapped / ECO / final /
powered netlists, DEF, GDS, SPEF per corner and source, SDC, Liberty, RC
profiles, tool binaries, ...) together with the RustROAD revision (git HEAD,
dirty-tree digest, rustroad binary SHA-256) and a digest over the whole
record. ``verify`` recomputes everything at the end of the run: any changed
file, recorded hash or revision fails the binding.

``compare_runs`` compares two independent full runs byte for byte. The only
normalisation is OpenRCX's wall-clock ``*DATE`` header line; it is listed in
the result, never applied silently.
"""
from __future__ import annotations

import hashlib
import json
from pathlib import Path
import subprocess

from rtl2gds_design import ROOT, item, sha256_file

BINDING_SCHEMA = "rustroad.rtl2gds-binding.v1"


def git_identity(root: Path = ROOT) -> dict:
    def git(*args):
        return subprocess.run(["git", "-C", str(root), *args], capture_output=True, text=True, check=True).stdout
    status = git("status", "--porcelain=v1", "--untracked-files=all")
    diff = git("diff", "HEAD", "--binary")
    return dict(head=git("rev-parse", "HEAD").strip(), branch=git("rev-parse", "--abbrev-ref", "HEAD").strip(),
                dirty=bool(status.strip()), dirty_paths=len(status.splitlines()),
                dirty_digest=hashlib.sha256((status + "\0" + diff).encode()).hexdigest())


def _digest(artifacts: dict, revision: dict) -> str:
    canon = json.dumps(dict(artifacts={k: v["sha256"] for k, v in sorted(artifacts.items())}, revision=revision),
                       sort_keys=True, separators=(",", ":"))
    return hashlib.sha256(canon.encode()).hexdigest()


def bind(artifacts: dict[str, Path], revision: dict) -> dict:
    rows = {}
    for name, path in sorted(artifacts.items()):
        p = Path(path)
        rows[name] = dict(path=str(p.resolve()), sha256=sha256_file(p), bytes=p.stat().st_size)
    return dict(schema=BINDING_SCHEMA, revision=revision, artifacts=rows, digest=_digest(rows, revision))


def verify(record: dict, revision_now: dict) -> dict:
    """Re-hash every bound file and re-derive the revision; any change fails."""
    changed = []
    for name, row in record["artifacts"].items():
        p = Path(row["path"])
        actual = sha256_file(p) if p.is_file() else None
        if actual != row["sha256"]:
            changed.append(dict(artifact=name, recorded=row["sha256"], actual=actual))
    problems = []
    if _digest(record["artifacts"], record["revision"]) != record["digest"]:
        problems.append("binding record digest does not match its contents (recorded hash tampered)")
    for key in ("head", "dirty_digest", "rustroad_sha256"):
        if record["revision"].get(key) != revision_now.get(key):
            problems.append(f"RustROAD revision {key} changed: {record['revision'].get(key)} -> {revision_now.get(key)}")
    if changed:
        problems.append(f"{len(changed)} artifact(s) differ from their bound sha256: {[c['artifact'] for c in changed][:8]}")
    return item("passed" if not problems else "failed", "; ".join(problems) or None, artifacts=len(record["artifacts"]),
                changed=changed[:20], digest=record["digest"])


def normalized(path: Path, kind: str) -> tuple[bytes, list[str]]:
    data = Path(path).read_bytes()
    if kind != "openrcx_spef":
        return data, []
    lines = data.split(b"\n")
    kept = [x for x in lines if not x.startswith(b"*DATE ")]
    return b"\n".join(kept), ["*DATE header line removed (OpenRCX wall clock)"] if len(kept) != len(lines) else []


def compare_runs(first: dict[str, Path], second: dict[str, Path], kinds: dict[str, str]) -> dict:
    rows, bad = {}, []
    for name in sorted(set(first) | set(second)):
        if name not in first or name not in second:
            rows[name] = dict(status="failed", reason="artifact produced by one run only")
            bad.append(name)
            continue
        a, notes = normalized(first[name], kinds.get(name, "bytes"))
        b, _ = normalized(second[name], kinds.get(name, "bytes"))
        same = a == b
        rows[name] = dict(status="passed" if same else "failed", sha256=hashlib.sha256(a).hexdigest(),
                          other_sha256=hashlib.sha256(b).hexdigest(), normalization=notes)
        if not same:
            bad.append(name)
    return item("passed" if not bad and rows else "failed", None if not bad else f"runs differ: {bad}",
                compared=len(rows), artifacts=rows)
