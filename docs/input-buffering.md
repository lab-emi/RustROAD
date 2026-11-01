# Bounded primary-input buffer trees

`import-pdk` can insert noninverting buffer trees on primary inputs whose
functional and passive cell loads exceed an explicit fanout policy. This is
opt-in through `PhysicalConfig.input_buffer`; ordinary imports remain unchanged.
Configured clock ports and nets feeding LEF clock pins are excluded and remain
under CTS. Reset and scan inputs retain their arbitrary logical values.

Use a fixed floorplan with sufficient space for the added cells. The automatic
`floorplan` command explicitly rejects a profile containing `input_buffer`,
because its area estimate does not yet include these trees. Derive the base
floorplan first, then apply the policy during import. Placement, packing and all
physical verification remain mandatory.

The two supplied policies are [SKY130](../fixtures/pdk/sky130-input-buffer.json)
and [GF180](../fixtures/pdk/gf180-input-buffer.json). For example, the SKY130
policy uses `sky130_fd_sc_hd__buf_4`, input `A`, output `X`, and fanout eight.
The cell must implement the declared identity function; the native structural
checks alone do not establish that property.

The policy has independent bounds:

| Field | Meaning | Supported maximum |
| --- | --- | --- |
| `max_fanout` | Loads on each source or generated output | 128; minimum 2 |
| `max_buffers` | New buffers across all selected inputs | 250,000 |
| `max_sinks` | Original moved cell loads | 2,000,000 |
| `max_work` | Work per construction or validation pass | 1,000,000,000 |
| `memory_mib` | Incremental scratch, appended AST and vector relocation | 4,096 MiB |

The parent `max_added_cells` and CLI cell limits also apply. The memory estimate
includes existing vector reallocation peaks, 4,096 bytes per buffer, 96 bytes
per moved sink, 128 bytes per primary bit, copied names and master
classification. It is evaluated before collecting sink lists or creating
buffers. The process must still run under the caller's whole-process guard.

Construction uses bounded pin-table scans and bottom-up groups. Every parent
references an already planned group, with at most 64 levels. There is no path,
cut or cone enumeration. Only identified cell input connections are rewritten;
port declarations, output identities and clock ownership are preserved. The
new cells participate in ordinary physical placement and supply connection.

The revision records source port/bit identities, original load counts, tree
depths, and generated instance names and indices. Reload checks every root,
buffer, fanout and leaf count, and rejects cycles with a bounded linear traversal.
Passive antenna loads count toward fanout but do not become functional
endpoints. A budget or validation failure does not publish a new `HEAD`.

Run the independent functional checks with:

```sh
python3 scripts/verify_input_buffers.py \
  --rustroad target/release/rustroad --out .work/input-buffer-proof
```

They use actual pinned LEF/Liberty files for both PDKs, external Yosys CEC,
ascending/descending bus cases, multilevel trees, wrong-output controls and a
configured inverter control. The complete physical gate can exercise this
policy with `--input-buffer-fanout 2`. It verifies the unchanged base floorplan,
binds the explicit policy and derived physical configuration before execution,
and retains the usual timing, DRC, LVS, antenna, equivalence and determinism gates.
That fixed-floorplan run does not claim automatic sizing for the added trees.
