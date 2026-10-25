# Tap placement and verification resource contract

Written before implementing the tap planner or its checker. The planner reserves
site-aligned tap intervals and packs logic into the remaining row intervals; it
does not enumerate paths or search combinations. Default limits remain 250,000
total cells, 100,000 rows, 250,000 tap slots, and a configurable work budget.
Tap/filler/logic cells share the total instance budget. Counts and dimensions are
checked before allocating vectors. Worst-case extra arrays (tap slots, free-row
intervals, assignments and reports) at 128 bytes * 250,000 use about 32 MiB;
sorting/validation references add 32 MiB. No per-corner or per-thread graph copy
is created. The existing 16 MiB snapshot limit remains in force.

The independent checker uses pinned OpenDB for placed-cell/master/PG data and
pinned KLayout to inspect original GDS well, active, contact and metal polygons.
Native placement uses LEF geometry and configured policy; it is not its own GDS
oracle. Each GDS input is <64 MiB, selected cells <=2,000, extracted shapes <=
100,000, region/contact comparisons <=2,000,000. Traverse only bounded selected
cell geometry, never all simple graph paths. Expected checker RSS is <512 MiB;
full library load and region operations receive a 2 GiB process cap. GDS units
are converted exactly to integer DBU and nonintegral conversions fail.

Before builds and suites run `free -h`. Use CARGO_BUILD_JOBS=2,
RUST_TEST_THREADS=2 and RAYON_NUM_THREADS=2; `/usr/bin/time -v`, an 8 GiB VAS cap
and timeout 180s for pure-CPU builds. Native verification children use 2 GiB
VAS/30s. OpenROAD retains the existing 2 GiB physical/no-swap cgroup, 8 GiB VAS,
two CPUs and an inner deadline. Serialize both PDK cases. Record complete
preparation/export/independent-check time and RSS, not only planner time.

The configured maximum tap gap/distance is an engineering acceptance policy.
It is not a blanket declaration of foundry latch-up compliance. Well/contact
existence, electrical PG attachment, placement spacing, and complete DRC/LVS
are separate claims. Missing tools, missing physical data, unknown layers or
unsupported geometry produce unverified/error, never a pass.

## Bounded clock-tree topology milestone

Before implementation: clock sinks <=1,000,000 but total instances <=250,000,
buffer count <=configured max_buffers (initial profiles 128), fanout >=2,
tree depth <=32 and work <=configured max_work. Sorting uses one sink vector;
building/validating uses O(sinks + buffers) endpoint/net arrays and a bounded
queue. No root-to-leaf path collection is stored. At 128 bytes per sink and
256 bytes per buffer, extra state is <160 MiB at global caps; the six-sink case
is <64 KiB. Existing process guards still apply. This is a logical, level-balanced
clock buffer tree, not routed timing-driven CTS or a skew guarantee. Independent
Boolean/clock equivalence, fanout and ideal-wire STA checks are required.
