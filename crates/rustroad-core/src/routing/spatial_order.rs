//! Order-preserving edits of a spatial index.
//!
//! Bins chain their entries in insertion order, and a probe stops at the
//! first blocking entry, so the work it charges depends on that order. A
//! persistent index (the antenna jumper planner keeps one across attempts)
//! therefore inserts and unlinks shapes such that every bin chain equals the
//! one a fresh build in `key` order would have: obstacles first, then each
//! added shape by ascending key. `entries` and the live-shape count follow
//! the fresh build exactly, so every budget check sees the same numbers.
use super::{
    BIN_PAGE_CELLS, BIN_PAGE_SIDE, BinGrid, BinMode, Blockage, NIL, Result, Spatial, budget,
    invalid,
};

impl BinGrid {
    fn chained(&self, x: i64, y: i64) -> bool {
        self.mode == BinMode::Paged || (self.mode == BinMode::Dense && self.slot(x, y).is_some())
    }
    fn page_key(layer: u32, x: i64, y: i64) -> ((u32, i64, i64), usize) {
        (
            (
                layer,
                x.div_euclid(BIN_PAGE_SIDE),
                y.div_euclid(BIN_PAGE_SIDE),
            ),
            (y.rem_euclid(BIN_PAGE_SIDE) * BIN_PAGE_SIDE + x.rem_euclid(BIN_PAGE_SIDE)) as usize,
        )
    }
    /// `[head, tail]` of a chained bin (empty when never allocated).
    fn cell(&self, layer: u32, x: i64, y: i64) -> [u32; 2] {
        if self.mode == BinMode::Paged {
            let (key, slot) = Self::page_key(layer, x, y);
            return self.pages.get(&key).map_or([NIL, NIL], |p| p[slot]);
        }
        let k = self.slot(x, y).expect("dense bin is inside the grid");
        self.cells[layer as usize]
            .get(k)
            .copied()
            .unwrap_or([NIL, NIL])
    }
    fn set_cell(&mut self, layer: u32, x: i64, y: i64, value: [u32; 2]) -> Result<()> {
        if self.mode == BinMode::Paged {
            let (key, slot) = Self::page_key(layer, x, y);
            self.pages
                .try_reserve(1)
                .map_err(|_| budget("routing bin-page allocation"))?;
            self.pages
                .entry(key)
                .or_insert_with(|| Box::new([[NIL, NIL]; BIN_PAGE_CELLS]))[slot] = value;
            return Ok(());
        }
        let k = self.slot(x, y).expect("dense bin is inside the grid");
        let grid = &mut self.cells[layer as usize];
        if grid.is_empty() {
            grid.resize((self.nx * self.ny) as usize, [NIL, NIL]);
        }
        grid[k] = value;
        Ok(())
    }
    /// Link `id` before the first entry whose key exceeds `key`.
    fn insert_sorted(
        &mut self,
        layer: u32,
        x: i64,
        y: i64,
        id: usize,
        key: u64,
        key_of: &dyn Fn(usize) -> u64,
    ) -> Result<()> {
        if !self.chained(x, y) {
            self.overflow
                .try_reserve(1)
                .map_err(|_| budget("sparse routing bin allocation"))?;
            let bin = self.overflow.entry((layer, x, y)).or_default();
            bin.try_reserve(1)
                .map_err(|_| budget("sparse routing bin entries"))?;
            let at = bin.partition_point(|&other| key_of(other) <= key);
            bin.insert(at, id);
            return Ok(());
        }
        let e = u32::try_from(self.entries.len())
            .ok()
            .filter(|&e| e != NIL)
            .ok_or_else(|| budget("routing spatial-bin count"))?;
        let id = u32::try_from(id).map_err(|_| budget("routing obstacle count"))?;
        let [head, tail] = self.cell(layer, x, y);
        let (mut previous, mut next) = (NIL, head);
        if tail != NIL && key_of(self.entries[tail as usize][0] as usize) <= key {
            (previous, next) = (tail, NIL);
        } else {
            while next != NIL && key_of(self.entries[next as usize][0] as usize) <= key {
                previous = next;
                next = self.entries[next as usize][1];
            }
        }
        self.entries.push([id, next]);
        let head = if previous == NIL {
            e
        } else {
            self.entries[previous as usize][1] = e;
            head
        };
        self.set_cell(layer, x, y, [head, if next == NIL { e } else { tail }])
    }
    /// Unlink the entry of `id` from one bin.
    fn unlink(&mut self, layer: u32, x: i64, y: i64, id: usize) -> Result<()> {
        if !self.chained(x, y) {
            let bin = self
                .overflow
                .get_mut(&(layer, x, y))
                .ok_or_else(|| invalid("spatial unlink: missing bin"))?;
            let at = bin
                .iter()
                .position(|&other| other == id)
                .ok_or_else(|| invalid("spatial unlink: missing entry"))?;
            bin.remove(at);
            return Ok(());
        }
        let [head, tail] = self.cell(layer, x, y);
        let (mut previous, mut current) = (NIL, head);
        while current != NIL && self.entries[current as usize][0] as usize != id {
            previous = current;
            current = self.entries[current as usize][1];
        }
        if current == NIL {
            return Err(invalid("spatial unlink: missing entry"));
        }
        let next = self.entries[current as usize][1];
        let head = if previous == NIL {
            next
        } else {
            self.entries[previous as usize][1] = next;
            head
        };
        self.set_cell(
            layer,
            x,
            y,
            [head, if tail == current { previous } else { tail }],
        )
    }
}

impl Spatial<'_> {
    fn bin_range(&self, b: &Blockage) -> Result<(i64, i64, i64, i64)> {
        if b.layer as usize >= self.input.layers.len() || !b.rect.valid() {
            return Err(invalid("routing obstacle"));
        }
        Ok((
            b.rect.lx.div_euclid(self.bin),
            b.rect.ux.div_euclid(self.bin),
            b.rect.ly.div_euclid(self.bin),
            b.rect.uy.div_euclid(self.bin),
        ))
    }
    /// Spatial work charged so far (probes and construction).
    pub(crate) fn charged(&self) -> usize {
        self.work
    }
    /// Reset the charged work (a persistent index restarting a fresh budget).
    pub(crate) fn reset_work(&mut self, work: usize) {
        self.work = work;
    }
    /// Add a fixed-net shape as `add` does, linked in every bin before the
    /// first entry whose key (`key_of`, obstacles 0) exceeds `key`. `dead`
    /// counts shapes unlinked earlier: the obstacle budget sees live shapes.
    pub(crate) fn insert_ordered(
        &mut self,
        b: Blockage,
        key: u64,
        key_of: &dyn Fn(usize) -> u64,
        dead: usize,
    ) -> Result<usize> {
        let instances = self
            .input
            .instances
            .as_ref()
            .map_or(0, super::InstanceObstacles::len);
        let live = self.len().saturating_sub(dead);
        if live.saturating_add(instances) >= self.limits.max_obstacles {
            return Err(budget(&format!(
                "routing obstacle count: {live} stored plus {instances} instance shapes, limit {}; 0 removed stored shapes",
                self.limits.max_obstacles,
            )));
        }
        let (x0, x1, y0, y1) = self.bin_range(&b)?;
        let n = (i128::from(x1) - i128::from(x0) + 1) * (i128::from(y1) - i128::from(y0) + 1);
        if n < 0 || n > self.limits.max_bin_entries.saturating_sub(self.entries) as i128 {
            return Err(budget("routing spatial-bin count"));
        }
        let id = self.len();
        for x in x0..=x1 {
            for y in y0..=y1 {
                self.bins.insert_sorted(b.layer, x, y, id, key, key_of)?;
                self.entries += 1;
            }
        }
        self.shapes.push(b);
        self.marks.push(0);
        self.alive.push(true);
        self.routed.push(false);
        Ok(id)
    }
    /// Remove an added shape from every bin; its id is never reused.
    pub(crate) fn unlink(&mut self, id: usize) -> Result<()> {
        let offset = id
            .checked_sub(self.input.obstacles.len())
            .filter(|&o| o < self.shapes.len() && self.alive[o])
            .ok_or_else(|| invalid("spatial unlink: not a live added shape"))?;
        let b = self.shapes[offset].clone();
        let (x0, x1, y0, y1) = self.bin_range(&b)?;
        for x in x0..=x1 {
            for y in y0..=y1 {
                self.bins.unlink(b.layer, x, y, id)?;
                self.entries -= 1;
            }
        }
        self.alive[offset] = false;
        Ok(())
    }
}

/// One bin's entries as (layer, rect, net) in chain order.
pub(crate) type BinDump = Vec<(u32, [i64; 4], Option<u32>)>;

impl Spatial<'_> {
    /// Every non-empty bin with its entries in chain order, plus the entry
    /// count: equal for two indexes iff every probe visits the same shapes in
    /// the same order (diagnostic; ids are not compared).
    pub(crate) fn dump_bins(
        &self,
    ) -> (std::collections::BTreeMap<(u32, i64, i64), BinDump>, usize) {
        let mut out = std::collections::BTreeMap::new();
        let shape = |id: usize| {
            let b = self.shape(id);
            (b.layer, [b.rect.lx, b.rect.ly, b.rect.ux, b.rect.uy], b.net)
        };
        let walk = |mut at: u32| {
            let mut v = Vec::new();
            while at != NIL {
                let [id, next] = self.bins.entries[at as usize];
                v.push(shape(id as usize));
                at = next;
            }
            v
        };
        for (layer, grid) in self.bins.cells.iter().enumerate() {
            for (k, cell) in grid.iter().enumerate() {
                if cell[0] != NIL {
                    let (x, y) = (
                        self.bins.bx0 + k as i64 % self.bins.nx,
                        self.bins.by0 + k as i64 / self.bins.nx,
                    );
                    out.insert((layer as u32, x, y), walk(cell[0]));
                }
            }
        }
        for (&(layer, px, py), page) in &self.bins.pages {
            for (slot, cell) in page.iter().enumerate() {
                if cell[0] != NIL {
                    let (x, y) = (
                        px * BIN_PAGE_SIDE + slot as i64 % BIN_PAGE_SIDE,
                        py * BIN_PAGE_SIDE + slot as i64 / BIN_PAGE_SIDE,
                    );
                    out.insert((layer, x, y), walk(cell[0]));
                }
            }
        }
        for (&key, list) in &self.bins.overflow {
            if !list.is_empty() {
                out.insert(key, list.iter().map(|&id| shape(id)).collect());
            }
        }
        (out, self.entries)
    }
}
