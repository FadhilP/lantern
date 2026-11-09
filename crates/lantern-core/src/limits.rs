//! Fixed limits. Values without a flag are constants; debug builds let tests lower some of them.

use std::time::{Duration, SystemTime, UNIX_EPOCH};

pub const DEFAULT_MAX_FILE_SIZE: u64 = 1 << 20;
pub const MAX_MAX_FILE_SIZE: u64 = 64 << 20;
pub const DEFAULT_TIMEOUT: Duration = Duration::from_secs(120);
pub const DEFAULT_VERIFY_INTERVAL: Duration = Duration::from_secs(300);
pub const MAX_VERIFY_INTERVAL: Duration = Duration::from_secs(300);

pub const DEFAULT_LIMIT: usize = 30;
pub const DEFAULT_CODE_LIMIT: usize = 10;
pub const MAX_LIMIT: usize = 10_000;
pub const DEFAULT_PER_FILE: usize = 20;
pub const MAX_PER_FILE: usize = 1_000;
pub const MAX_CONTEXT: usize = 20;
pub const DEFAULT_GRAPH_DEPTH: usize = 1;
pub const MAX_GRAPH_DEPTH: usize = 3;
pub const DEFAULT_GRAPH_NODES: usize = 200;
pub const MAX_GRAPH_NODES: usize = 5_000;
pub const MAX_QUERY_CHARS: usize = 4_000;

/// Longest line text, in characters, that any output carries.
pub(crate) const MAX_LINE_CHARS: usize = 500;
pub(crate) const MAX_SIGNATURE_CHARS: usize = 300;
/// A NUL byte in this prefix marks a file as binary.
pub(crate) const TEXT_SNIFF_BYTES: usize = 8 * 1024;
pub(crate) const MAX_RULE_FILE_BYTES: u64 = 4 << 20;
pub(crate) const MAX_RULE_TOTAL_BYTES: u64 = 64 << 20;
pub(crate) const MAX_DEPTH: usize = 1_024;
pub(crate) const REFRESH_ATTEMPTS: u32 = 3;
/// Parses of one blob that may time out before Lantern stops retrying it.
pub(crate) const MAX_PARSE_ATTEMPTS: i64 = 3;
/// Parse budget per MiB of source; small files get the floor.
pub(crate) const PARSE_BUDGET_PER_MIB: Duration = Duration::from_secs(1);
pub(crate) const MIN_PARSE_BUDGET: Duration = Duration::from_millis(100);

pub(crate) fn max_entries() -> u64 {
    test_override("LANTERN_TEST_MAX_ENTRIES").unwrap_or(10_000_000)
}

pub(crate) fn max_nested() -> usize {
    test_override("LANTERN_TEST_MAX_NESTED").map_or(1_000, |value| value as usize)
}

/// Milliseconds since the Unix epoch. Debug builds read `LANTERN_TEST_NOW_MS` first.
pub(crate) fn now_ms() -> i64 {
    if let Some(now) = test_override("LANTERN_TEST_NOW_MS") {
        return now as i64;
    }
    let since_epoch = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default();
    i64::try_from(since_epoch.as_millis()).unwrap_or(i64::MAX)
}

fn test_override(name: &str) -> Option<u64> {
    if !cfg!(debug_assertions) {
        return None;
    }
    std::env::var(name).ok()?.parse().ok()
}
