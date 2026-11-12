//! Fixed limits. Values without a flag are constants; debug builds let tests lower some of them.

use std::time::{Duration, SystemTime, UNIX_EPOCH};

pub const MAX_MAX_FILE_SIZE: u64 = 64 << 20;

pub const DEFAULT_LIMIT: usize = 30;
pub const DEFAULT_OUTLINE_LIMIT: usize = 100;
pub const DEFAULT_CODE_LIMIT: usize = 10;
pub const MAX_LIMIT: usize = 10_000;
pub const DEFAULT_PER_FILE: usize = 5;
pub const MAX_PER_FILE: usize = 1_000;
pub const MAX_CONTEXT: usize = 50;
pub const DEFAULT_BODY_LIMIT: usize = 5;
pub const DEFAULT_GRAPH_DEPTH: usize = 1;
pub const MAX_GRAPH_DEPTH: usize = 3;
pub const DEFAULT_GRAPH_NODES: usize = 200;
pub const MAX_GRAPH_NODES: usize = 5_000;
pub const MAX_QUERY_CHARS: usize = 4_000;

/// Longest line text, in characters, that JSON output carries.
pub(crate) const MAX_LINE_CHARS: usize = 500;
/// Longest line text in the default text output; agents cut wider lines to about 130.
pub(crate) const TEXT_LINE_CHARS: usize = 200;
pub(crate) const MAX_SIGNATURE_CHARS: usize = 300;
/// Longest body, in lines, that `symbols --body` prints per definition.
pub(crate) const MAX_BODY_LINES: u32 = 100;
/// A NUL byte in this prefix marks a file as binary.
pub(crate) const TEXT_SNIFF_BYTES: usize = 8 * 1024;
pub(crate) const MAX_RULE_FILE_BYTES: u64 = 4 << 20;
pub(crate) const MAX_RULE_TOTAL_BYTES: u64 = 64 << 20;
pub(crate) const MAX_DEPTH: usize = 1_024;
pub(crate) const REFRESH_ATTEMPTS: u32 = 3;
/// Threads that stat the inventory's files.
pub(crate) const STAT_THREADS: usize = 4;
/// Covers coarse file timestamps (2 s on FAT) when a file time is compared with the scan time.
pub(crate) const RACY_SLACK_MS: i64 = 2_000;
/// Parses of one blob that may time out before Lantern stops retrying it.
pub(crate) const MAX_PARSE_ATTEMPTS: i64 = 3;
/// Parse budget per MiB of source; small files get the floor.
pub(crate) const PARSE_BUDGET_PER_MIB: Duration = Duration::from_secs(1);
pub(crate) const MIN_PARSE_BUDGET: Duration = Duration::from_millis(100);

pub(crate) fn max_entries() -> u64 {
    test_override("LANTERN_TEST_MAX_ENTRIES").unwrap_or(10_000_000)
}

pub(crate) fn max_nested() -> usize {
    test_override("LANTERN_TEST_MAX_NESTED")
        .map_or(1_000, |value| usize::try_from(value).unwrap_or(usize::MAX))
}

/// Milliseconds since the Unix epoch.
pub(crate) fn now_ms() -> i64 {
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
