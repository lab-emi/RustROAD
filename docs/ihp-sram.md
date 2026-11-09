# Hard macros and IHP SG13G2 bring-up

The physical importer accepts `CLASS BLOCK` cells with a fixed placement in the
floorplan's `macro_placements` map. Their dimensions and pins come from the
original LEF, independently of the standard-cell site. Fixed cells retain their
position, orientation and optional halo during placement and CTS, and are
exported as `FIXED` DEF components. Row placement subtracts their halos under
an explicit work limit. Row-rail preparation, taps and fillers use the same segmented rows. Macro-free
vertical channels carry additional supply spines; at least one full-width row
bridge is required to connect them to the primary grid.

A grouped Verilog port needs the original declaration range in
`pin_buses[master][port]`, for example `[31, 0]`. The importer validates that every
bit has a unique scalar LEF pin; it preserves ordering through linking, constant
fanout repair and Verilog export. This does not infer a macro's functionality or
lower its storage into standard cells.

`rail_via_stack` optionally lists original fixed vias from the rail layer to a
higher strap layer. Intermediate landings meet the via enclosure and LEF area
requirements. The final strap can therefore be above standard-cell internal
metal. Empty lists retain the single-via configuration.

`macro_supply` in the physical configuration selects the pin layer, collection
layer, original fixed via and a contiguous via stack from the standard-cell
supply spine. Connections use enclosed access shapes from the macro's real PG
pins. The generator does not replace a macro with a site-height placeholder or
waive missing PG access. `TapConfig.body_view = "gds"` records that the original
GDS contains the contacts omitted from an abstract LEF; independent extraction
and LVS remain required before claiming body-bias connectivity.

The stored LEF obstruction rectangles remain unchanged. Per-layer `OBS SPACING`
values are retained separately. The routing view uses the larger of its explicit
layer policy and the OBS spacing, adding only the difference to its obstacle
view. This is conservative when the layer policy exceeds the OBS override and
avoids counting the spacing twice. The LEF/DEF 5.8 language reference describes
`SPACING` as an override, rather than extra physical metal (page 134).

`ClockConfig.root_net` optionally selects an existing internal clock net for a
buffer tree. The primary `port` still identifies the clock in the CTS constraints.
The internal root must have one driver; its original gate remains connected and
the inserted buffers are driven from that net. The route clock scope includes
both the primary clock and internal root. This is a single selected tree, not
a clock-forest or timing-closure claim. The full design SDC remains necessary for
STA even when a clock-only projection is supplied to the narrow CTS parser.

For the IHP comparison, `build_rc_profile.py --pdk ihp --resistance-model
openrcx-pdk` binds wire resistance to the original constant RESOVER table and
cut resistance to the original technology LEF. The nominal SPEF is shared by
TT and SS Liberty analyses. The default Magic resistance model remains an
independent cross-check, and existing SKY130/GF180 profiles remain unchanged.

Extraction omits `CLASS BLOCK` LEF OBS, which describes routing exclusion
rather than a grounded conductor. Declared macro pin and supply shapes remain
in the extraction view, and routing still respects every original obstruction.
Independent OpenRCX controls compare absent, empty and obstructed macros with
a real grounded routing-wire positive control. The extraction report states
this geometry scope explicitly.

The active Ibex campaign is under the superproject's
`.work/ibex-sg13g2-repair/road-agent/`. Its checks include fixed-macro preservation,
real SRAM bus connectivity, original-vendor-model simulation after CTS, and
independent physical checks. No full routed DRC/LVS/extracted-timing pass is
established by these APIs or by the unit tests.
