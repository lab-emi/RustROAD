//! The `be_postroute` step: screen a routed, antenna-repaired and extracted
//! layout for electrical limit violations and repair them by equal-width
//! upsizing, then publish the final workspace and SPEF.
//!
//! This is the screen order (`rusteda.native-postroute-inputs.v3`) of the
//! Ibex SG13G2 recipe's post-route runner; the earlier order, in which the
//! runner extracted the pre-antenna trial itself, stays with the Python
//! helper. See `docs/postroute-repair.md` for the protocol, the inputs and
//! the result record.
//!
//! The step is a controller: it never routes, extracts or times anything in
//! this process. It runs the pinned `rustroad`, `rusteda` and `ruststa`
//! executables named in its inputs, one child at a time (the first netlist
//! export overlaps input hashing, and the four read-only timing report
//! queries of a round run together), under one deadline, and reads the
//! report files they write. RustSTA is not linked.
//!
//! Memory: the flat netlist (about 16k instances), two Liberty cell tables
//! and at most 100,000 DRV rows per corner; well under 256 MiB.
pub mod lef;
pub mod liberty;
pub mod netlist;
pub mod policy;
mod runner;

pub use runner::run;
