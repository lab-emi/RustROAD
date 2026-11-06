//! Validator build identity for commit-time validation receipts.
//!
//! A receipt written by one build is trusted only by a build whose validator
//! sources are byte-identical: the digest covers every Rust source of the
//! crates that define read-time validation semantics (core geometry/rules,
//! workspace loading/storage, format parsers), the dependency lock file, the
//! compiler version and the build profile (debug assertions and overflow
//! checks change what a validator does on the same sources). Uses only
//! `std`, so the lock file and dependency graph are unchanged.
use std::fs;
use std::path::{Path, PathBuf};

const ROOTS: [&str; 4] = [
    "src",
    "../rustroad-core/src",
    "../rustroad-formats/src",
    "../../Cargo.lock",
];

/// FNV-1a (64-bit) folded into two lanes; an identity, not a security hash.
struct Digest([u64; 2]);
impl Digest {
    fn update(&mut self, bytes: &[u8]) {
        for &b in bytes {
            self.0[0] = (self.0[0] ^ u64::from(b)).wrapping_mul(0x0000_0100_0000_01b3);
            self.0[1] =
                (self.0[1] ^ u64::from(b.rotate_left(3))).wrapping_mul(0x0000_0100_0000_01b3);
        }
    }
}

fn collect(path: &Path, out: &mut Vec<PathBuf>) {
    let Ok(meta) = fs::metadata(path) else {
        return;
    };
    if meta.is_file() {
        out.push(path.to_path_buf());
        return;
    }
    let Ok(entries) = fs::read_dir(path) else {
        return;
    };
    for entry in entries.flatten() {
        collect(&entry.path(), out);
    }
}

fn main() {
    let manifest = PathBuf::from(std::env::var("CARGO_MANIFEST_DIR").expect("cargo manifest dir"));
    let mut files = Vec::new();
    for root in ROOTS {
        let path = manifest.join(root);
        println!("cargo:rerun-if-changed={}", path.display());
        collect(&path, &mut files);
    }
    let mut named: Vec<(String, PathBuf)> = files
        .into_iter()
        .filter_map(|p| {
            let rel = p
                .strip_prefix(&manifest)
                .ok()?
                .to_string_lossy()
                .replace('\\', "/");
            (rel.ends_with(".rs") || rel.ends_with("Cargo.lock")).then_some((rel, p))
        })
        .collect();
    named.sort();
    let mut digest = Digest([0xcbf2_9ce4_8422_2325, 0x8422_2325_cbf2_9ce4]);
    for (rel, path) in &named {
        let bytes = fs::read(path).expect("validator source readable");
        digest.update(rel.as_bytes());
        digest.update(&(bytes.len() as u64).to_le_bytes());
        digest.update(&bytes);
    }
    // Compiler and profile: a receipt from a debug build is not trusted by a
    // release build of the same sources (and vice versa).
    let rustc = std::env::var_os("RUSTC").unwrap_or_else(|| "rustc".into());
    let version = std::process::Command::new(rustc)
        .arg("-vV")
        .output()
        .ok()
        .filter(|o| o.status.success())
        .map_or_else(|| b"unknown".to_vec(), |o| o.stdout);
    digest.update(b"rustc");
    digest.update(&(version.len() as u64).to_le_bytes());
    digest.update(&version);
    for key in [
        "TARGET",
        "PROFILE",
        "OPT_LEVEL",
        "DEBUG",
        "CARGO_CFG_DEBUG_ASSERTIONS",
        "CARGO_CFG_OVERFLOW_CHECKS",
        "CARGO_CFG_PANIC",
    ] {
        let value = std::env::var(key).unwrap_or_else(|_| "<unset>".into());
        digest.update(key.as_bytes());
        digest.update(&(value.len() as u64).to_le_bytes());
        digest.update(value.as_bytes());
    }
    println!(
        "cargo:rustc-env=RUSTROAD_VALIDATOR_DIGEST={:016x}{:016x}",
        digest.0[0], digest.0[1]
    );
}
