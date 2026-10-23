# SPDX-License-Identifier: Apache-2.0
"""In-container identity probe (stdlib only; runs under the image's python3).

usage: python3 identity_probe.py REQUEST.json OUTPUT.json

REQUEST = {"files": [abs paths], "trees": [abs dirs], "versions": {name: argv}}
Hashes regular files (symlinks are resolved and the target recorded), digests
directory trees over every regular file, and captures the first 4 KiB of each
version command's combined output. Missing paths are reported, never skipped.
"""
import hashlib
import json
import os
import subprocess
import sys

MAX_TREE_FILES = 20000


def digest(path):
    h = hashlib.sha256()
    size = 0
    with open(path, "rb") as handle:
        while True:
            block = handle.read(1 << 20)
            if not block:
                break
            size += len(block)
            h.update(block)
    return h.hexdigest(), size


def main():
    request = json.load(open(sys.argv[1]))
    out = {"files": {}, "trees": {}, "versions": {}}
    for path in request.get("files", []):
        if not os.path.exists(path):
            out["files"][path] = {"missing": True}
            continue
        real = os.path.realpath(path)
        sha, size = digest(real)
        out["files"][path] = {"sha256": sha, "bytes": size, "realpath": real}
    for root in request.get("trees", []):
        if not os.path.isdir(root):
            out["trees"][root] = {"missing": True}
            continue
        entries = []
        for base, dirs, files in os.walk(root):
            dirs.sort()
            for name in sorted(files):
                full = os.path.join(base, name)
                if os.path.islink(full) or not os.path.isfile(full):
                    continue
                entries.append((os.path.relpath(full, root), digest(full)[0]))
                if len(entries) > MAX_TREE_FILES:
                    raise SystemExit("tree file budget exceeded: " + root)
        h = hashlib.sha256()
        for rel, sha in sorted(entries):
            h.update(f"{rel}\0{sha}\n".encode())
        out["trees"][root] = {"sha256": h.hexdigest(), "files": len(entries)}
    for name, argv in request.get("versions", {}).items():
        try:
            proc = subprocess.run(argv, stdout=subprocess.PIPE, stderr=subprocess.STDOUT,
                                  stdin=subprocess.DEVNULL, timeout=60, check=False)
            out["versions"][name] = {"exit": proc.returncode,
                                     "output": proc.stdout[:4096].decode("utf-8", "replace")}
        except (OSError, subprocess.TimeoutExpired) as error:
            out["versions"][name] = {"exit": None, "output": str(error)}
    with open(sys.argv[2], "w") as handle:
        json.dump(out, handle, indent=1, sort_keys=True)


if __name__ == "__main__":
    main()
