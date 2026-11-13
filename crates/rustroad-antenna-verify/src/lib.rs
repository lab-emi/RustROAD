//! Independent, bounded verifier of a grouped antenna-repair session's
//! lineage (`rusteda.native-antenna-lineage.v3`/`.v4`); it never executes an
//! EDA command.
//!
//! Independence is structural: this crate depends on neither the engine
//! crate that ran the session nor the flow crate that wrote the lineage, so
//! it cannot read their data structures. Everything is decoded again from
//! the files the lineage names, as generic JSON, and every file identity is
//! re-hashed. The checks are those of the recipe's `antenna_session/verify.py`
//! (same order, same rejection codes): the controller inputs and the single
//! native process receipt (exact argv, captured stdout equal to the session
//! summary byte for byte), every typed in-process operation and its report,
//! the canonical selection and coverage of every check report with its rows
//! streamed and counted, the attempt chain with its logical revisions and
//! physical-source fingerprints, the work and report-byte ledgers recomputed
//! from the reports, the routed inventory before and after, and the final
//! groups' exact partition of the inventory.
//!
//! A passing review is `metadata_lineage_verified_requires_full_signoff`:
//! it says the records are consistent with each other and with the files,
//! never that the layout is antenna clean (foundry antenna, DRC, LVS and
//! timing remain separate gates).
//!
//! Lineage v4 (native summary v2): only the initial and the committed final
//! revision are materialized; attempt children are logical revisions, chained
//! by name and bound to distinct physical-source fingerprints by their check
//! reports. Check reports may hold only their violated rows
//! (`check_rows: "violated"`); the coverage row counts, worst-net summary and
//! verdict counts are then checked against the complete evaluated inventory.
//!
//! Peak estimate below 1.5 GiB: one 64 MiB connectivity model decoded at a
//! time, 16 MiB lineage and summary, bounded name/ID sets; raw check rows
//! are decoded one at a time. A 2 GiB address-space limit is applied and
//! recorded; the review stops itself after 170 s.
mod review;

pub use review::{Rejected, run};
