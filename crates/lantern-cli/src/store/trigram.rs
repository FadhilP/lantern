use std::cell::RefCell;
use std::collections::{BTreeMap, BTreeSet, HashMap};

use regex_syntax::hir::{ClassUnicode, ClassUnicodeRange};
use rusqlite::{Connection, OptionalExtension, params};

use crate::error::{Error, Result};

/// On vscode, 512 made a one-file edit about 40% slower for 10% less space.
const BLOCK: i64 = 256;

/// About 40 bytes each, so the writer's buffer stays near 80 MB.
const FLUSH_CHANGES: usize = 2_000_000;

/// Folds like the regex engine's simple case folding, so the index never drops a `(?i)` match.
pub(crate) fn fold(c: char) -> char {
    if c.is_ascii() {
        c.to_ascii_lowercase()
    } else {
        fold_wide(c)
    }
}

fn fold_wide(c: char) -> char {
    thread_local! {
        static FOLDED: RefCell<HashMap<char, char>> = RefCell::new(HashMap::new());
    }
    FOLDED.with(|folded| {
        *folded
            .borrow_mut()
            .entry(c)
            .or_insert_with(|| simple_fold(c))
    })
}

fn simple_fold(c: char) -> char {
    let mut class = ClassUnicode::new([ClassUnicodeRange::new(c, c)]);
    class.case_fold_simple();
    let smallest = class.iter().next().map_or(c, ClassUnicodeRange::start);
    smallest.to_ascii_lowercase()
}

pub(crate) fn folded(text: &str) -> String {
    if text.is_ascii() {
        text.to_ascii_lowercase()
    } else {
        text.chars().map(fold).collect()
    }
}

/// Both arguments must be folded.
pub(crate) fn occurrences(haystack: &str, needle: &str) -> u32 {
    let Some(first) = needle.chars().next() else {
        return 0;
    };
    let mut count = 0;
    let mut from = 0;
    while let Some(at) = haystack[from..].find(needle) {
        count += 1;
        from += at + first.len_utf8();
    }
    count
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
struct Trigram(i64);

impl Trigram {
    fn new(chars: [char; 3]) -> Self {
        let [a, b, c] = chars.map(|c| i64::from(u32::from(c)));
        // The sign bit keeps wide keys apart from Latin-1 ones.
        if a < 256 && b < 256 && c < 256 {
            Self(a << 16 | b << 8 | c)
        } else {
            Self(i64::MIN | a << 42 | b << 21 | c)
        }
    }
}

fn next_bit(c: char) -> u8 {
    1 << (u32::from(c).wrapping_mul(0x9E37_79B1) >> 29)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Occurrence {
    count: u32,
    next: u8,
}

#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub(crate) struct Trigrams(Vec<(Trigram, Occurrence)>);

impl Trigrams {
    pub(crate) fn of(text: &str) -> Self {
        let chars: Vec<char> = text.chars().map(fold).collect();
        let mut found: Vec<(Trigram, u8)> = chars
            .windows(3)
            .enumerate()
            .map(|(at, window)| {
                let next = chars.get(at + 3).map_or(0, |&c| next_bit(c));
                (Trigram::new([window[0], window[1], window[2]]), next)
            })
            .collect();
        found.sort_unstable_by_key(|&(trigram, _)| trigram);
        let mut grouped: Vec<(Trigram, Occurrence)> = Vec::new();
        for (trigram, next) in found {
            match grouped.last_mut() {
                Some((last, seen)) if *last == trigram => {
                    seen.count += 1;
                    seen.next |= next;
                }
                _ => grouped.push((trigram, Occurrence { count: 1, next })),
            }
        }
        Self(grouped)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Posting {
    blob: i64,
    count: u32,
    next: u8,
}

fn encode(postings: &[Posting], base: i64) -> Vec<u8> {
    let mut bytes = Vec::with_capacity(postings.len() * 3);
    let mut previous = base;
    for posting in postings {
        let delta = u64::try_from(posting.blob - previous)
            .expect("postings are sorted by blob id, from the block's first id");
        previous = posting.blob;
        if posting.count > 1 {
            put_varint(&mut bytes, delta << 1 | 1);
            put_varint(&mut bytes, u64::from(posting.count - 2));
        } else {
            put_varint(&mut bytes, delta << 1);
        }
        bytes.push(posting.next);
    }
    bytes
}

fn decode(bytes: &[u8], base: i64) -> Result<Vec<Posting>> {
    let corrupt = || Error::failure("corrupt trigram row; run `lantern index --rebuild`");
    let mut postings: Vec<Posting> = Vec::new();
    let mut at = 0;
    let mut blob = base;
    while at < bytes.len() {
        let head = take_varint(bytes, &mut at).ok_or_else(corrupt)?;
        let delta = i64::try_from(head >> 1).map_err(|_| corrupt())?;
        blob = blob.checked_add(delta).ok_or_else(corrupt)?;
        let repeated = postings.last().is_some_and(|last| last.blob == blob);
        if repeated || blob >= base + BLOCK {
            return Err(corrupt());
        }
        let count = if head & 1 == 1 {
            let extra = take_varint(bytes, &mut at).ok_or_else(corrupt)?;
            extra
                .checked_add(2)
                .and_then(|count| u32::try_from(count).ok())
                .ok_or_else(corrupt)?
        } else {
            1
        };
        let next = *bytes.get(at).ok_or_else(corrupt)?;
        at += 1;
        postings.push(Posting { blob, count, next });
    }
    Ok(postings)
}

fn put_varint(bytes: &mut Vec<u8>, mut value: u64) {
    while value >= 0x80 {
        bytes.push((value as u8 & 0x7F) | 0x80);
        value >>= 7;
    }
    bytes.push(value as u8);
}

fn take_varint(bytes: &[u8], at: &mut usize) -> Option<u64> {
    let mut value = 0u64;
    for shift in (0..64).step_by(7) {
        let byte = *bytes.get(*at)?;
        *at += 1;
        value |= u64::from(byte & 0x7F) << shift;
        if byte < 0x80 {
            return Some(value);
        }
    }
    None
}

#[derive(Debug)]
enum Op {
    Add(Posting),
    Remove(i64),
}

#[derive(Debug)]
struct Change {
    block: i64,
    trigram: Trigram,
    op: Op,
}

/// Call [`Pending::finish`] before commit, or the buffered changes are lost.
#[derive(Debug)]
#[must_use = "changes are lost unless `finish` writes them"]
pub(crate) struct Pending {
    changes: Vec<Change>,
    /// When set, only blocks written in this transaction have rows to read.
    started_empty: bool,
    written: BTreeSet<i64>,
}

impl Pending {
    pub(crate) fn new(conn: &Connection) -> Result<Self> {
        let stored = conn
            .prepare_cached("SELECT 1 FROM trigrams LIMIT 1")?
            .exists([])?;
        Ok(Self {
            changes: Vec::new(),
            started_empty: !stored,
            written: BTreeSet::new(),
        })
    }

    pub(crate) fn add(&mut self, conn: &Connection, blob: i64, trigrams: &Trigrams) -> Result<()> {
        let block = blob / BLOCK;
        self.changes
            .extend(trigrams.0.iter().map(|&(trigram, seen)| Change {
                block,
                trigram,
                op: Op::Add(Posting {
                    blob,
                    count: seen.count,
                    next: seen.next,
                }),
            }));
        self.flush_when_full(conn)
    }

    pub(crate) fn remove(
        &mut self,
        conn: &Connection,
        blob: i64,
        trigrams: &Trigrams,
    ) -> Result<()> {
        let block = blob / BLOCK;
        self.changes
            .extend(trigrams.0.iter().map(|&(trigram, _)| Change {
                block,
                trigram,
                op: Op::Remove(blob),
            }));
        self.flush_when_full(conn)
    }

    pub(crate) fn finish(mut self, conn: &Connection) -> Result<()> {
        self.flush(conn)
    }

    fn flush_when_full(&mut self, conn: &Connection) -> Result<()> {
        if self.changes.len() >= FLUSH_CHANGES {
            self.flush(conn)?;
        }
        Ok(())
    }

    fn flush(&mut self, conn: &Connection) -> Result<()> {
        let mut changes = std::mem::take(&mut self.changes);
        // Stable keeps a row's changes in order; rayon here deadlocks with workers sending to us.
        changes.sort_by_key(|change| (change.block, change.trigram));
        let mut read =
            conn.prepare_cached("SELECT postings FROM trigrams WHERE block = ?1 AND trigram = ?2")?;
        let mut write = conn.prepare_cached(
            "INSERT OR REPLACE INTO trigrams (block, trigram, postings) VALUES (?1, ?2, ?3)",
        )?;
        let mut delete =
            conn.prepare_cached("DELETE FROM trigrams WHERE block = ?1 AND trigram = ?2")?;
        for row in changes.chunk_by(|a, b| (a.block, a.trigram) == (b.block, b.trigram)) {
            let (block, trigram) = (row[0].block, row[0].trigram);
            let base = block * BLOCK;
            let may_be_stored = !self.started_empty || self.written.contains(&block);
            let stored: Option<Vec<u8>> = if may_be_stored {
                read.query_row(params![block, trigram.0], |found| found.get(0))
                    .optional()?
            } else {
                None
            };
            let mut postings = match &stored {
                Some(bytes) => decode(bytes, base)?,
                None => Vec::new(),
            };
            for change in row {
                apply(&mut postings, &change.op);
            }
            if !postings.is_empty() {
                write.execute(params![block, trigram.0, encode(&postings, base)])?;
            } else if stored.is_some() {
                delete.execute(params![block, trigram.0])?;
            }
        }
        self.written
            .extend(changes.iter().map(|change| change.block));
        Ok(())
    }
}

fn apply(postings: &mut Vec<Posting>, op: &Op) {
    match *op {
        Op::Add(posting) => match postings.binary_search_by_key(&posting.blob, |p| p.blob) {
            Ok(at) => postings[at] = posting,
            Err(at) => postings.insert(at, posting),
        },
        Op::Remove(blob) => {
            if let Ok(at) = postings.binary_search_by_key(&blob, |p| p.blob) {
                postings.remove(at);
            }
        }
    }
}

/// The counts are upper bounds of how often `literal` occurs.
pub(crate) fn candidates(conn: &Connection, literal: &str) -> Result<HashMap<i64, u32>> {
    let chars: Vec<char> = literal.chars().map(fold).collect();
    assert!(
        chars.len() >= 3,
        "literal {literal:?} is shorter than a trigram"
    );
    let needs = needs(&chars);
    let last: Option<i64> =
        conn.query_row("SELECT max(block) FROM trigrams", [], |row| row.get(0))?;
    let mut read =
        conn.prepare_cached("SELECT postings FROM trigrams WHERE block = ?1 AND trigram = ?2")?;
    let mut found = HashMap::new();
    'blocks: for block in 0..=last.unwrap_or(-1) {
        let base = block * BLOCK;
        let mut alive: Option<Vec<(i64, u32)>> = None;
        for &(trigram, next) in &needs {
            let stored: Option<Vec<u8>> = read
                .query_row(params![block, trigram.0], |row| row.get(0))
                .optional()?;
            let Some(bytes) = stored else {
                continue 'blocks;
            };
            let usable = decode(&bytes, base)?
                .into_iter()
                .filter(|posting| posting.next & next == next);
            let narrowed = match alive {
                None => usable
                    .map(|posting| (posting.blob, posting.count))
                    .collect(),
                Some(previous) => intersect(previous, usable),
            };
            if narrowed.is_empty() {
                continue 'blocks;
            }
            alive = Some(narrowed);
        }
        found.extend(alive.unwrap_or_default());
    }
    Ok(found)
}

fn needs(chars: &[char]) -> Vec<(Trigram, u8)> {
    let mut needs: BTreeMap<Trigram, u8> = BTreeMap::new();
    for (at, window) in chars.windows(3).enumerate() {
        let next = chars.get(at + 3).map_or(0, |&c| next_bit(c));
        *needs
            .entry(Trigram::new([window[0], window[1], window[2]]))
            .or_default() |= next;
    }
    needs.into_iter().collect()
}

fn intersect(
    previous: Vec<(i64, u32)>,
    postings: impl Iterator<Item = Posting>,
) -> Vec<(i64, u32)> {
    let mut postings = postings.peekable();
    let mut both = Vec::new();
    for (blob, count) in previous {
        while postings.next_if(|posting| posting.blob < blob).is_some() {}
        if let Some(posting) = postings.next_if(|posting| posting.blob == blob) {
            both.push((blob, count.min(posting.count)));
        }
    }
    both
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn case_variants_that_regex_treats_as_equal_fold_alike() {
        for group in [
            &['k', 'K', 'K'][..],
            &['s', 'S', 'ſ'],
            &['µ', 'μ', 'Μ'],
            &['σ', 'Σ', 'ς'],
        ] {
            let folded: Vec<char> = group.iter().map(|&c| fold(c)).collect();
            assert!(
                folded.windows(2).all(|pair| pair[0] == pair[1]),
                "{group:?} -> {folded:?}"
            );
        }
    }

    #[test]
    fn fold_table_is_unchanged() {
        let digest = (0..=u32::from(char::MAX))
            .filter_map(char::from_u32)
            .fold(0xcbf2_9ce4_8422_2325_u64, |hash, c| {
                (hash ^ u64::from(u32::from(fold(c)))).wrapping_mul(0x0100_0000_01b3)
            });
        assert_eq!(
            digest, 1_234_273_128_870_392_462,
            "the fold changed, so stored trigrams are stale: bump SCHEMA_VERSION, then this digest"
        );
    }

    #[test]
    fn postings_round_trip() {
        let base = 3 * BLOCK;
        let postings = vec![
            Posting {
                blob: base,
                count: 1,
                next: 0,
            },
            Posting {
                blob: base + 5,
                count: 2,
                next: 0x81,
            },
            Posting {
                blob: base + 255,
                count: 70_000,
                next: 0xFF,
            },
        ];
        assert_eq!(decode(&encode(&postings, base), base).ok(), Some(postings));
    }
}
