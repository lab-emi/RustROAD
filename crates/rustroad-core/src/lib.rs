//! Bounded, deterministic physical-design primitives. All geometry is integer DBU.
use rayon::prelude::*;
use serde::{Deserialize, Serialize};
use slotmap::{Key, SlotMap, new_key_type};
use std::collections::{HashMap, HashSet};

pub mod connectivity;
pub mod extract;
pub mod io;
pub mod jumper;
pub mod memory;
pub mod place;
pub mod power;
pub(crate) mod route_rules;
pub mod routing;
pub mod supply;
pub mod tap;
pub(crate) mod troute;
pub use troute::MemoryEstimate;
pub mod wire_runs;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum Orientation {
    #[default]
    N,
    FS,
}

pub fn transform_rect(
    rect: Rect,
    x: i64,
    y: i64,
    height: i64,
    orientation: Orientation,
) -> Result<Rect> {
    if !rect.valid() || height <= 0 {
        return Err(Error::Invalid("invalid rectangle transform input".into()));
    }
    let (ly, uy) = match orientation {
        Orientation::N => (
            i128::from(y) + i128::from(rect.ly),
            i128::from(y) + i128::from(rect.uy),
        ),
        Orientation::FS => (
            i128::from(y) + i128::from(height) - i128::from(rect.uy),
            i128::from(y) + i128::from(height) - i128::from(rect.ly),
        ),
    };
    let convert = |v: i128| {
        i64::try_from(v).map_err(|_| Error::Invalid("rectangle transform overflow".into()))
    };
    Ok(Rect {
        lx: convert(i128::from(x) + i128::from(rect.lx))?,
        ly: convert(ly)?,
        ux: convert(i128::from(x) + i128::from(rect.ux))?,
        uy: convert(uy)?,
    })
}

new_key_type! { pub struct InstId; }

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("INVALID_DESIGN: {0}")]
    Invalid(String),
    #[error("BUDGET_EXCEEDED: {0}")]
    Budget(String),
    #[error("PLACEMENT_CAPACITY: no legal row remains for instance {0}")]
    Capacity(String),
}
pub type Result<T> = std::result::Result<T, Error>;

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Limits {
    pub max_cells: usize,
    pub max_rows: usize,
    pub max_expansions: usize,
}
impl Default for Limits {
    fn default() -> Self {
        Self {
            max_cells: 250_000,
            max_rows: 100_000,
            max_expansions: 350_000,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Rect {
    pub lx: i64,
    pub ly: i64,
    pub ux: i64,
    pub uy: i64,
}
impl Rect {
    fn valid(self) -> bool {
        self.lx < self.ux && self.ly < self.uy
    }
    fn contains(self, other: Self) -> bool {
        self.lx <= other.lx && self.ly <= other.ly && self.ux >= other.ux && self.uy >= other.uy
    }
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Site {
    #[serde(default = "default_site_name")]
    pub name: String,
    pub width: i64,
    pub height: i64,
}
fn default_site_name() -> String {
    "rustroad_site".into()
}
#[derive(Clone, Copy, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Row {
    pub x_start: i64,
    pub x_end: i64,
    pub y: i64,
    #[serde(default)]
    pub orientation: Orientation,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Cell {
    pub name: String,
    pub master: String,
    pub width: i64,
    pub height: i64,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Design {
    pub name: String,
    pub units_per_micron: u32,
    pub die_area: Rect,
    pub core_area: Rect,
    pub site: Site,
    pub rows: Vec<Row>,
    pub cells: Vec<Cell>,
}

/// Accepted names are also safe DEF tokens and shell-safe CLI handles.
pub fn valid_handle(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 128
        && value
            .bytes()
            .all(|c| c.is_ascii_alphanumeric() || c == b'_' || c == b'-')
}

/// Logical/physical object names are not CLI handles. Preserve escaped Verilog
/// identifiers while forbidding controls, whitespace, and unbounded names.
pub fn valid_object_name(value: &str) -> bool {
    !value.is_empty() && value.len() <= 1024 && value.bytes().all(|b| b.is_ascii_graphic())
}

/// DEF escaping is independent of shell-safe revision/object handles.
pub fn def_name(value: &str) -> String {
    let mut result = String::with_capacity(value.len());
    for c in value.chars() {
        if !c.is_ascii_alphanumeric() && !matches!(c, '_' | '.' | '/' | '[' | ']' | '-') {
            result.push('\\');
        }
        result.push(c);
    }
    result
}

impl Design {
    pub fn validate(&self, limits: &Limits) -> Result<()> {
        if self.cells.len() > limits.max_cells || self.rows.len() > limits.max_rows {
            return Err(Error::Budget(
                "cell/row count exceeds configured limit".into(),
            ));
        }
        if self.cells.is_empty() || self.rows.is_empty() {
            return Err(Error::Invalid("at least one cell and row required".into()));
        }
        if !valid_object_name(&self.name) || self.units_per_micron == 0 {
            return Err(Error::Invalid("invalid design name or DBU scale".into()));
        }
        if !self.die_area.valid()
            || !self.core_area.valid()
            || !self.die_area.contains(self.core_area)
        {
            return Err(Error::Invalid(
                "core must be a nonempty rectangle inside die".into(),
            ));
        }
        if !valid_object_name(&self.site.name) || self.site.width <= 0 || self.site.height <= 0 {
            return Err(Error::Invalid("site dimensions must be positive".into()));
        }
        let mut previous_top = None;
        for row in &self.rows {
            let top = row
                .y
                .checked_add(self.site.height)
                .ok_or_else(|| Error::Invalid("row coordinate overflow".into()))?;
            let rect = Rect {
                lx: row.x_start,
                ly: row.y,
                ux: row.x_end,
                uy: top,
            };
            if !rect.valid()
                || !self.core_area.contains(rect)
                || (i128::from(row.x_start) - i128::from(self.core_area.lx))
                    % i128::from(self.site.width)
                    != 0
                || (i128::from(row.x_end) - i128::from(row.x_start)) % i128::from(self.site.width)
                    != 0
                || (i128::from(row.y) - i128::from(self.core_area.ly))
                    % i128::from(self.site.height)
                    != 0
                || previous_top.is_some_and(|y| y > row.y)
            {
                return Err(Error::Invalid(
                    "rows must be sorted, disjoint, site-aligned and inside core".into(),
                ));
            }
            previous_top = Some(top);
        }
        let mut names = HashSet::with_capacity(self.cells.len());
        let mut masters = HashMap::new();
        for cell in &self.cells {
            if !valid_object_name(&cell.name)
                || !valid_object_name(&cell.master)
                || !names.insert(cell.name.as_str())
            {
                return Err(Error::Invalid("invalid or duplicate instance name".into()));
            }
            if cell.width <= 0
                || cell.width % self.site.width != 0
                || cell.height != self.site.height
            {
                return Err(Error::Invalid(format!(
                    "{} is not a single-height, site-aligned cell",
                    cell.name
                )));
            }
            if let Some(dimensions) =
                masters.insert(cell.master.as_str(), (cell.width, cell.height))
                && dimensions != (cell.width, cell.height)
            {
                return Err(Error::Invalid(format!(
                    "master {} has inconsistent dimensions",
                    cell.master
                )));
            }
        }
        Ok(())
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct Symbol(u32);
#[derive(Default)]
pub struct SymbolInterner {
    values: Vec<String>,
    index: HashMap<String, Symbol>,
}
impl SymbolInterner {
    fn intern(&mut self, value: &str) -> Symbol {
        if let Some(symbol) = self.index.get(value) {
            return *symbol;
        }
        // Input and unique symbols have already been bounded below u32 capacity.
        let symbol = Symbol(self.values.len() as u32);
        self.values.push(value.to_owned());
        self.index.insert(value.to_owned(), symbol);
        symbol
    }
    pub fn resolve(&self, symbol: Symbol) -> &str {
        &self.values[symbol.0 as usize]
    }
}
pub struct Instance {
    pub name: Symbol,
    pub master: Symbol,
    pub width: i64,
    pub height: i64,
}
pub struct DesignDb {
    symbols: SymbolInterner,
    instances: SlotMap<InstId, Instance>,
    order: Vec<InstId>,
    rows: Vec<Row>,
}
impl DesignDb {
    pub fn new(design: &Design, limits: &Limits) -> Result<Self> {
        design.validate(limits)?;
        if design.cells.len() > (u32::MAX as usize - 1) / 2 {
            return Err(Error::Budget("interner index capacity".into()));
        }
        let mut db = Self {
            symbols: SymbolInterner::default(),
            instances: SlotMap::with_key(),
            order: Vec::with_capacity(design.cells.len()),
            rows: design.rows.clone(),
        };
        for cell in &design.cells {
            let name = db.symbols.intern(&cell.name);
            let master = db.symbols.intern(&cell.master);
            db.order.push(db.instances.insert(Instance {
                name,
                master,
                width: cell.width,
                height: cell.height,
            }));
        }
        Ok(db)
    }
    /// Ephemeral contiguous CPU view; the mutable arena remains separate.
    pub fn lower_cpu_ref(&self) -> ComputeView {
        let widths = self
            .order
            .par_iter()
            .map(|id| self.instances[*id].width)
            .collect();
        ComputeView {
            ids: self.order.clone(),
            widths,
            rows: self.rows.clone(),
        }
    }
    pub fn unplaced_records(&self) -> Vec<Placed> {
        self.order
            .iter()
            .map(|id| {
                let inst = &self.instances[*id];
                Placed {
                    id: id.data().as_ffi(),
                    x: 0,
                    y: 0,
                    width: inst.width,
                    height: inst.height,
                }
            })
            .collect()
    }
}
pub struct ComputeView {
    ids: Vec<InstId>,
    widths: Vec<i64>,
    rows: Vec<Row>,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Placed {
    pub id: u64,
    pub x: i64,
    pub y: i64,
    pub width: i64,
    pub height: i64,
}
#[derive(Debug)]
pub struct Placement {
    pub cells: Vec<Placed>,
    pub expansions: usize,
}

/// Fixed-order row packing in O(cells + rows), not global placement optimization.
/// Each candidate consumes one expansion; exhausted budgets produce no partial result.
pub fn place_cpu_ref(db: &DesignDb, limits: &Limits) -> Result<Placement> {
    if db.order.len() > limits.max_cells || db.rows.len() > limits.max_rows {
        return Err(Error::Budget(
            "compute view exceeds configured limit".into(),
        ));
    }
    let view = db.lower_cpu_ref();
    let mut cells = Vec::with_capacity(view.ids.len());
    let mut row_index = 0;
    let mut x = view.rows[0].x_start;
    let mut expansions = 0usize;
    for (id, width) in view.ids.iter().zip(&view.widths) {
        loop {
            if expansions >= limits.max_expansions {
                return Err(Error::Budget(
                    "max_expansions reached during placement".into(),
                ));
            }
            expansions += 1;
            let row = view.rows[row_index];
            if let Some(right) = x.checked_add(*width).filter(|right| *right <= row.x_end) {
                cells.push(Placed {
                    id: id.data().as_ffi(),
                    x,
                    y: row.y,
                    width: *width,
                    height: db.instances[*id].height,
                });
                x = right;
                break;
            }
            row_index += 1;
            if row_index == view.rows.len() {
                return Err(Error::Capacity(
                    db.symbols.resolve(db.instances[*id].name).into(),
                ));
            }
            x = view.rows[row_index].x_start;
        }
    }
    Ok(Placement { cells, expansions })
}

impl Orientation {
    pub fn def(self) -> &'static str {
        match self {
            Self::N => "N",
            Self::FS => "FS",
        }
    }
}

pub fn placed_orientation(design: &Design, placed: &Placed) -> Result<Orientation> {
    let index = design
        .rows
        .binary_search_by_key(&placed.y, |r| r.y)
        .map_err(|_| Error::Invalid("instance outside rows".into()))?;
    Ok(design.rows[index].orientation)
}

/// Bounded order-independent validation permits physical-only filler instances
/// to be appended after logical cells without changing their IDs/order.
pub fn validate_placement(design: &Design, cells: &[Placed], limits: &Limits) -> Result<()> {
    design.validate(limits)?;
    if cells.len() != design.cells.len() {
        return Err(Error::Invalid("placement length mismatch".into()));
    }
    let mut intervals = Vec::with_capacity(cells.len());
    for (cell, spec) in cells.iter().zip(&design.cells) {
        let index = design
            .rows
            .binary_search_by_key(&cell.y, |r| r.y)
            .map_err(|_| Error::Invalid("illegal placement: instance outside rows".into()))?;
        let row = design.rows[index];
        let right = cell
            .x
            .checked_add(cell.width)
            .ok_or_else(|| Error::Invalid("coordinate overflow".into()))?;
        if cell.width != spec.width
            || cell.height != spec.height
            || cell.x < row.x_start
            || right > row.x_end
            || (i128::from(cell.x) - i128::from(row.x_start)) % i128::from(design.site.width) != 0
        {
            return Err(Error::Invalid(
                "illegal placement: dimensions/bounds/grid".into(),
            ));
        }
        intervals.push((index, cell.x, right));
    }
    intervals.sort_unstable();
    if intervals
        .windows(2)
        .any(|w| w[0].0 == w[1].0 && w[0].2 > w[1].1)
    {
        return Err(Error::Invalid(
            "illegal placement: overlapping cells".into(),
        ));
    }
    Ok(())
}

/// Serialize fixed-order placement only. Nets, pin geometry, routing and signoff
/// are absent. Validate even when called outside the workspace API.
pub fn write_def(
    design: &Design,
    cells: &[Placed],
    output: &mut impl std::io::Write,
) -> std::io::Result<()> {
    write_def_components(design, cells, output)?;
    writeln!(output, "END DESIGN")
}

pub fn write_def_components(
    design: &Design,
    cells: &[Placed],
    output: &mut impl std::io::Write,
) -> std::io::Result<()> {
    validate_placement(
        design,
        cells,
        &Limits {
            max_cells: cells.len(),
            max_rows: design.rows.len(),
            max_expansions: 0,
        },
    )
    .map_err(std::io::Error::other)?;
    writeln!(
        output,
        "VERSION 5.8 ;\nDIVIDERCHAR \"/\" ;\nBUSBITCHARS \"[]\" ;\nDESIGN {} ;\nUNITS DISTANCE MICRONS {} ;",
        def_name(&design.name),
        design.units_per_micron
    )?;
    let die = design.die_area;
    writeln!(
        output,
        "DIEAREA ( {} {} ) ( {} {} ) ;",
        die.lx, die.ly, die.ux, die.uy
    )?;
    for (i, row) in design.rows.iter().enumerate() {
        let count =
            (i128::from(row.x_end) - i128::from(row.x_start)) / i128::from(design.site.width);
        writeln!(
            output,
            "ROW row{i} {} {} {} {} DO {count} BY 1 STEP {} 0 ;",
            def_name(&design.site.name),
            row.x_start,
            row.y,
            row.orientation.def(),
            design.site.width
        )?;
    }
    writeln!(output, "COMPONENTS {} ;", cells.len())?;
    for (cell, placed) in design.cells.iter().zip(cells) {
        writeln!(
            output,
            "- {} {} + PLACED ( {} {} ) {} ;",
            def_name(&cell.name),
            def_name(&cell.master),
            placed.x,
            placed.y,
            placed_orientation(design, placed)
                .map_err(std::io::Error::other)?
                .def()
        )?;
    }
    writeln!(output, "END COMPONENTS")
}

#[cfg(test)]
mod tests {
    use super::*;
    fn design() -> Design {
        Design {
            name: "tiny".into(),
            units_per_micron: 1000,
            die_area: Rect {
                lx: 0,
                ly: 0,
                ux: 100,
                uy: 40,
            },
            core_area: Rect {
                lx: 0,
                ly: 0,
                ux: 100,
                uy: 40,
            },
            site: Site {
                name: default_site_name(),
                width: 10,
                height: 20,
            },
            rows: vec![
                Row {
                    x_start: 0,
                    x_end: 100,
                    y: 0,
                    orientation: Orientation::N,
                },
                Row {
                    x_start: 0,
                    x_end: 100,
                    y: 20,
                    orientation: Orientation::N,
                },
            ],
            cells: (0..3)
                .map(|i| Cell {
                    name: format!("u{i}"),
                    master: "BUF".into(),
                    width: 40,
                    height: 20,
                })
                .collect(),
        }
    }
    #[test]
    fn packs_in_fixed_order_and_is_repeatable() {
        let db = DesignDb::new(&design(), &Limits::default()).unwrap();
        let first = place_cpu_ref(&db, &Limits::default()).unwrap();
        assert_eq!(
            first.cells.iter().map(|c| (c.x, c.y)).collect::<Vec<_>>(),
            [(0, 0), (40, 0), (0, 20)]
        );
        assert_eq!(first.expansions, 4);
        assert_eq!(
            first.cells,
            place_cpu_ref(&db, &Limits::default()).unwrap().cells
        );
    }
    #[test]
    fn work_budget_and_capacity_fail_explicitly() {
        let d = design();
        let db = DesignDb::new(&d, &Limits::default()).unwrap();
        let limits = Limits {
            max_expansions: 3,
            ..Limits::default()
        };
        assert!(matches!(place_cpu_ref(&db, &limits), Err(Error::Budget(_))));
        let mut d = d;
        d.cells.extend((3..5).map(|i| Cell {
            name: format!("u{i}"),
            master: "BUF".into(),
            width: 40,
            height: 20,
        }));
        assert!(matches!(
            place_cpu_ref(
                &DesignDb::new(&d, &Limits::default()).unwrap(),
                &Limits::default()
            ),
            Err(Error::Capacity(_))
        ));
    }
    #[test]
    fn rejects_overlap_overflow_and_unknown_shapes() {
        let mut d = design();
        d.rows[1].y = 10;
        assert!(d.validate(&Limits::default()).is_err());
        d.rows[1].y = i64::MAX;
        assert!(d.validate(&Limits::default()).is_err());
        d = design();
        d.cells[0].height = 40;
        assert!(d.validate(&Limits::default()).is_err());
        d = design();
        d.cells[0].width = 30;
        assert!(d.validate(&Limits::default()).is_err()); // Inconsistent master dimensions.
    }
    #[test]
    fn symbols_and_generation_belong_to_the_database() {
        let mut db = DesignDb::new(&design(), &Limits::default()).unwrap();
        let old = db.order[0];
        let instance = db.instances.remove(old).unwrap();
        let new = db.instances.insert(instance);
        assert_ne!(old, new);
        assert!(db.instances.get(old).is_none());
        assert!(!valid_handle("rev#1"));
        assert!(!valid_handle("rev 1"));
    }
    #[test]
    fn export_rejects_invalid_public_inputs_without_panicking() {
        let mut d = design();
        let db = DesignDb::new(&d, &Limits::default()).unwrap();
        let mut cells = place_cpu_ref(&db, &Limits::default()).unwrap().cells;
        d.site.width = 0;
        assert!(write_def(&d, &cells, &mut Vec::new()).is_err());
        d = design();
        cells[1].x = 0;
        assert!(write_def(&d, &cells, &mut Vec::new()).is_err());
    }
}
