//! Process memory probe (Linux `/proc/self/status`): resident set size now
//! (`VmRSS`) and its high-water mark (`VmHWM`). Used for route telemetry and
//! for the router's runtime guard against `memory_mib`. Elsewhere, or when
//! procfs is unreadable, both are `None` (reported as unmeasured, never 0).
//! One small read per call; callers sample it at most every few thousand
//! search expansions.

/// Resident set size of this process in bytes (`VmRSS`).
pub fn rss_bytes() -> Option<u64> {
    field("VmRSS:")
}

/// Peak resident set size of this process in bytes (`VmHWM`).
pub fn peak_rss_bytes() -> Option<u64> {
    field("VmHWM:")
}

/// Stderr line `[route] <what> rss <MiB> peak <MiB>` when the
/// `RUSTROAD_ROUTE_TRACE` environment variable is set (diagnostics only).
pub fn trace(what: &str) {
    if std::env::var_os("RUSTROAD_ROUTE_TRACE").is_some() {
        let mib = |b: Option<u64>| b.map_or_else(|| "-".into(), |b| format!("{}", b >> 20));
        eprintln!(
            "[route] {what} rss {} MiB peak {} MiB",
            mib(rss_bytes()),
            mib(peak_rss_bytes())
        );
    }
}

fn field(key: &str) -> Option<u64> {
    let text = std::fs::read_to_string("/proc/self/status").ok()?;
    parse(&text, key)
}

fn parse(text: &str, key: &str) -> Option<u64> {
    let line = text.lines().find(|l| l.starts_with(key))?;
    let mut parts = line[key.len()..].split_whitespace();
    let value: u64 = parts.next()?.parse().ok()?;
    match parts.next() {
        Some("kB") => value.checked_mul(1024),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn parses_kib_fields_and_rejects_other_units() {
        let text = "Name:\tx\nVmHWM:\t  2048 kB\nVmRSS:\t1024 kB\nVmSwap:\t3 MB\n";
        assert_eq!(parse(text, "VmRSS:"), Some(1024 * 1024));
        assert_eq!(parse(text, "VmHWM:"), Some(2048 * 1024));
        assert_eq!(parse(text, "VmSwap:"), None);
        assert_eq!(parse(text, "VmData:"), None);
        if cfg!(target_os = "linux") {
            let (now, peak) = (rss_bytes().unwrap(), peak_rss_bytes().unwrap());
            assert!(now > 0 && peak >= now);
        }
    }
}
