# REA black-box probe resource estimate

Recorded before execution, 2026-10-23. `free -h` reported 59 GiB RAM, 46 GiB available.

These probes load no design, no PDK library, no timing graph, no paths/cuts/clauses. Object/corner/thread design-state product is zero. Expected OpenROAD startup RSS is below 512 MiB, REA/Node/PTY supervisor below 512 MiB, two CPU threads, one probe at a time. Each probe captures at most 256 KiB terminal output, 128 observed processes, and 24,000 terminal cells. Process capture may retain several representations of bounded output.

Outer CPU-only guard: 8 GiB virtual address space and 60 second timeout, `/usr/bin/time -v` receipts. Container guard: MemoryMax=2 GiB equivalent (`--memory=2g --memory-swap=2g`), two CPUs, 128 PIDs, no network, read-only root, bounded 64 MiB tmpfs. Container overhead is included in wall time; outer time max RSS covers the REA launcher tree as reported by GNU time and is not a precise synchronized sum of cgroup peak RSS. No speed comparison or physical-design benchmark is claimed.

## Placement oracle preflight

The hand-authored LEF defines three pinless standard-cell rectangles and one site. The actual positive/negative workload has 24 instances, six rows, one geometry scope, two reference threads, no loaded Liberty/timing graph and no path/cut/clause enumeration. Estimate 24 * 256 B * 1 scope * 2 threads < 16 KiB instance state plus parsed LEF/DEF and a conservative 512 MiB tool/supervisor baseline. Reference internals were not inspected. Conservatively budget all 24 * 23 / 2 possible overlap pairs at 16 B/pair (<5 KiB); this is an estimate, not a claim that the reference materializes pairs.

The reusable harness refuses more than 5000 placed instances or 16 MiB input. At that cap, 5000 * 256 B * 2 threads ~= 2.6 MB state and a conservative 5000 * 4999 / 2 * 16 B ~= 200 MB overlap-pair allowance, plus input parsing and 512 MiB baseline, stay below 1 GiB estimated RSS. Output retention, filesystem hashing and process samples retain the prior explicit budgets. Runs are sequential under the same 2 GiB container/8 GiB host virtual-memory caps and 30s scenario/60s outer deadlines; `/usr/bin/time -v` records observed launcher peak RSS. This bounds a reference-tool runaway even though its implementation is unknown.
