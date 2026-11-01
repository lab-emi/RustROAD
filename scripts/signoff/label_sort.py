# SPDX-License-Identifier: Apache-2.0
"""Exact label multisets with bounded chunk memory, merge fan-in and disk usage."""
from contextlib import ExitStack
import hashlib
import heapq
from pathlib import Path
import tempfile

MAX_LINE_BYTES = 1 << 20
MAX_CHUNKS = 8192
MERGE_INPUTS = 16


def write_sorted(records, destination, *, max_records=5_000_000,
                 max_bytes=4 << 30, max_work=4_000_000_000, buffer_bytes=32 << 20):
    """Publish a sorted UTF-8 multiset atomically, or fail without replacing it."""
    if not 1 <= max_records <= 250_000_000 or not 1 <= max_bytes <= 16 << 30:
        raise ValueError("label count/byte budget outside supported bounds")
    if not 1 <= max_work <= 4_000_000_000 or not 256 <= buffer_bytes <= 64 << 20:
        raise ValueError("label work/memory budget outside supported bounds")
    destination = Path(destination)
    count = total = work = 0

    def charge():
        nonlocal work
        work += 1
        if work > max_work:
            raise ValueError("label sort work budget exceeded")

    with tempfile.TemporaryDirectory(prefix=".labels-", dir=destination.parent) as temporary:
        scratch = Path(temporary)
        paths, chunk = [], []
        retained = 0

        def flush():
            if len(paths) == MAX_CHUNKS:
                raise ValueError("label sort chunk budget exceeded")
            path = scratch / f"chunk{len(paths)}"
            chunk.sort()
            with path.open("wb", buffering=65536) as output:
                for line in chunk:
                    charge()
                    output.write(line)
                    output.write(b"\n")
            paths.append(path)
            chunk.clear()

        for record in records:
            charge()
            if count == max_records:
                raise ValueError("flattened label count budget exceeded")
            if not isinstance(record, str) or "\n" in record:
                raise ValueError("label record must be a single text line")
            if len(record) > MAX_LINE_BYTES:
                raise ValueError("label line byte budget exceeded")
            line = record.encode("utf-8")
            encoded = len(line) + 1
            if encoded > MAX_LINE_BYTES:
                raise ValueError("label line byte budget exceeded")
            if total + encoded > max_bytes:
                raise ValueError("flattened label byte budget exceeded")
            allowance = encoded + 96
            if allowance > buffer_bytes:
                raise ValueError("label record exceeds chunk memory budget")
            if retained + allowance > buffer_bytes:
                flush()
                retained = 0
            chunk.append(line)
            retained += allowance
            total += encoded
            count += 1
        if chunk or not paths:
            flush()

        def merge(inputs, output):
            with ExitStack() as opened:
                files = [opened.enter_context(path.open("rb", buffering=65536)) for path in inputs]
                sink = opened.enter_context(output.open("wb", buffering=65536))

                def read(index):
                    charge()
                    line = files[index].readline(MAX_LINE_BYTES + 1)
                    if len(line) > MAX_LINE_BYTES or (line and not line.endswith(b"\n")):
                        raise ValueError("invalid bounded label chunk")
                    return line[:-1] if line else None

                heap = []
                for index in range(len(files)):
                    line = read(index)
                    if line is not None:
                        heapq.heappush(heap, (line, index))
                written = 0
                while heap:
                    line, index = heapq.heappop(heap)
                    charge()
                    written += len(line) + 1
                    if written > max_bytes:
                        raise ValueError("label merge byte budget exceeded")
                    sink.write(line)
                    sink.write(b"\n")
                    following = read(index)
                    if following is not None:
                        heapq.heappush(heap, (following, index))

        level = 0
        while len(paths) > 1:
            merged = []
            for first in range(0, len(paths), MERGE_INPUTS):
                group = paths[first:first + MERGE_INPUTS]
                target = scratch / f"merge{level}-{first}"
                merge(group, target)
                for path in group:
                    path.unlink()
                merged.append(target)
            paths = merged
            level += 1
        digest = hashlib.sha256()
        with paths[0].open("rb") as stream:
            for block in iter(lambda: stream.read(65536), b""):
                digest.update(block)
        paths[0].replace(destination)
    return dict(path=str(destination), count=count, truncated=False, sha256=digest.hexdigest())
