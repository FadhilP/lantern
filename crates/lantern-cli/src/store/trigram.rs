use std::cell::RefCell;
use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::thread;

use regex_syntax::hir::{ClassUnicode, ClassUnicodeRange};
use rusqlite::{Connection, OptionalExtension, params};

use super::{put_varint, take_varint};
use crate::error::{Error, Result};

/// On vscode, 512 made a one-file edit about 40% slower for 10% less space.
const BLOCK: i64 = 256;

/// About 32 bytes each, so the writer's buffer stays near 64 MB.
const FLUSH_CHANGES: usize = 2_000_000;

/// A new shard flushes between two blocks from this size on. Blob ids come in ascending order,
/// so each row is then written once, and the sorts are small. A flush inside a block makes the
/// next one read and replace that block's rows.
const FLUSH_BETWEEN_BLOCKS: usize = FLUSH_CHANGES / 4;

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

/// From this many occurrences in a blob, a posting also stores `pair`: the one-character bloom
/// of a frequent trigram is too full to reject much.
const PAIR_FROM: u32 = 4;

fn hash(c: u32, bits: u32) -> u8 {
    (c.wrapping_mul(0x9E37_79B1) >> (32 - bits)) as u8
}

/// What the characters after one occurrence of a trigram tell.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
struct Follow {
    /// One of 8 bits, from the next character.
    next: u8,
    /// One of 8 bits, from the next two characters.
    pair: u8,
    /// Four hash bits of each of the next two characters.
    exact: u8,
    /// The bits of `exact` that come from a character.
    known: u8,
}

fn follow(chars: &[char], at: usize) -> Follow {
    let Some(&first) = chars.get(at + 3) else {
        return Follow::default();
    };
    let first = u32::from(first);
    let next = 1 << hash(first, 3);
    let Some(&second) = chars.get(at + 4) else {
        return Follow {
            next,
            pair: 0,
            exact: hash(first, 4) << 4,
            known: 0xF0,
        };
    };
    let second = u32::from(second);
    Follow {
        next,
        pair: 1 << hash(first.wrapping_mul(31).wrapping_add(second), 3),
        exact: hash(first, 4) << 4 | hash(second, 4),
        known: 0xFF,
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Occurrence {
    count: u32,
    next: u8,
    pair: u8,
    /// Of the first occurrence.
    exact: u8,
}

#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub(crate) struct Trigrams(Vec<(Trigram, Occurrence)>);

impl Trigrams {
    pub(crate) fn of(text: &str) -> Self {
        let chars: Vec<char> = text.chars().map(fold).collect();
        let mut found: Vec<(Trigram, Follow)> = chars
            .windows(3)
            .enumerate()
            .map(|(at, window)| {
                let trigram = Trigram::new([window[0], window[1], window[2]]);
                (trigram, follow(&chars, at))
            })
            .collect();
        found.sort_unstable_by_key(|&(trigram, _)| trigram);
        let mut grouped: Vec<(Trigram, Occurrence)> = Vec::new();
        for (trigram, follow) in found {
            match grouped.last_mut() {
                Some((last, seen)) if *last == trigram => {
                    seen.count += 1;
                    seen.next |= follow.next;
                    seen.pair |= follow.pair;
                }
                _ => grouped.push((
                    trigram,
                    Occurrence {
                        count: 1,
                        next: follow.next,
                        pair: follow.pair,
                        exact: follow.exact,
                    },
                )),
            }
        }
        Self(grouped)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Posting {
    blob: i64,
    count: u32,
    /// `Follow::exact` when `count` is 1, else the bloom of `Follow::next`.
    next: u8,
    /// The bloom of `Follow::pair`; 0 below `PAIR_FROM`.
    pair: u8,
}

impl Posting {
    fn new(blob: i64, seen: Occurrence) -> Self {
        Self {
            blob,
            count: seen.count,
            next: if seen.count == 1 {
                seen.exact
            } else {
                seen.next
            },
            pair: if seen.count < PAIR_FROM { 0 } else { seen.pair },
        }
    }

    /// False when the blob cannot hold the trigram as often as the literal does, or followed
    /// by the literal's characters.
    fn allows(&self, need: &Need) -> bool {
        if self.count < need.times {
            return false;
        }
        if self.count == 1 {
            return self.next & need.follow.known == need.follow.exact;
        }
        self.next & need.follow.next == need.follow.next
            && (self.count < PAIR_FROM || self.pair & need.follow.pair == need.follow.pair)
    }
}

/// Per posting: a varint of the blob id delta and two bits for a count of 1 to 3; from
/// `PAIR_FROM`, a varint of the rest of the count; `next`; from `PAIR_FROM`, `pair`.
fn encode(postings: &[Posting], base: i64) -> Vec<u8> {
    let mut bytes = Vec::with_capacity(postings.len() * 3);
    let mut previous = base;
    for posting in postings {
        let delta = u64::try_from(posting.blob - previous)
            .expect("postings are sorted by blob id, from the block's first id");
        previous = posting.blob;
        let small = u64::from(posting.count.min(PAIR_FROM) - 1);
        put_varint(&mut bytes, delta << 2 | small);
        if posting.count >= PAIR_FROM {
            put_varint(&mut bytes, u64::from(posting.count - PAIR_FROM));
        }
        bytes.push(posting.next);
        if posting.count >= PAIR_FROM {
            bytes.push(posting.pair);
        }
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
        let delta = i64::try_from(head >> 2).map_err(|_| corrupt())?;
        blob = blob.checked_add(delta).ok_or_else(corrupt)?;
        let repeated = postings.last().is_some_and(|last| last.blob == blob);
        if repeated || blob >= base + BLOCK {
            return Err(corrupt());
        }
        let small = (head & 3) as u32 + 1;
        let count = if small == PAIR_FROM {
            let rest = take_varint(bytes, &mut at).ok_or_else(corrupt)?;
            rest.checked_add(u64::from(PAIR_FROM))
                .and_then(|count| u32::try_from(count).ok())
                .ok_or_else(corrupt)?
        } else {
            small
        };
        let next = *bytes.get(at).ok_or_else(corrupt)?;
        at += 1;
        let pair = if count >= PAIR_FROM {
            at += 1;
            *bytes.get(at - 1).ok_or_else(corrupt)?
        } else {
            0
        };
        postings.push(Posting {
            blob,
            count,
            next,
            pair,
        });
    }
    Ok(postings)
}

#[derive(Debug)]
enum Op {
    Add(Posting),
    Remove(i64),
}

#[derive(Debug)]
struct Change {
    trigram: Trigram,
    op: Op,
}

impl Change {
    fn block(&self) -> i64 {
        match self.op {
            Op::Add(Posting { blob, .. }) | Op::Remove(blob) => blob / BLOCK,
        }
    }
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
        self.flush_between_blocks(conn, blob)?;
        self.changes
            .extend(trigrams.0.iter().map(|&(trigram, seen)| Change {
                trigram,
                op: Op::Add(Posting::new(blob, seen)),
            }));
        self.flush_when_full(conn)
    }

    pub(crate) fn remove(
        &mut self,
        conn: &Connection,
        blob: i64,
        trigrams: &Trigrams,
    ) -> Result<()> {
        self.changes
            .extend(trigrams.0.iter().map(|&(trigram, _)| Change {
                trigram,
                op: Op::Remove(blob),
            }));
        self.flush_when_full(conn)
    }

    pub(crate) fn finish(mut self, conn: &Connection) -> Result<()> {
        self.flush(conn)
    }

    /// Not in a shard with rows: there the last flush runs beside the scan that follows the
    /// reads, and an early flush would take work out of that time.
    fn flush_between_blocks(&mut self, conn: &Connection, blob: i64) -> Result<()> {
        let block_ended = self
            .changes
            .last()
            .is_some_and(|last| last.block() != blob / BLOCK);
        let large = self.changes.len() >= FLUSH_BETWEEN_BLOCKS;
        if self.started_empty && block_ended && large {
            self.flush(conn)?;
        }
        Ok(())
    }

    fn flush_when_full(&mut self, conn: &Connection) -> Result<()> {
        if self.changes.len() >= FLUSH_CHANGES {
            self.flush(conn)?;
        }
        Ok(())
    }

    fn flush(&mut self, conn: &Connection) -> Result<()> {
        let mut changes = std::mem::take(&mut self.changes);
        sort(&mut changes);
        let mut read =
            conn.prepare_cached("SELECT postings FROM trigrams WHERE block = ?1 AND trigram = ?2")?;
        let mut write = conn.prepare_cached(
            "INSERT OR REPLACE INTO trigrams (block, trigram, postings) VALUES (?1, ?2, ?3)",
        )?;
        let mut delete =
            conn.prepare_cached("DELETE FROM trigrams WHERE block = ?1 AND trigram = ?2")?;
        for row in changes.chunk_by(|a, b| (a.block(), a.trigram) == (b.block(), b.trigram)) {
            let (block, trigram) = (row[0].block(), row[0].trigram);
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
        self.written.extend(changes.iter().map(Change::block));
        Ok(())
    }
}

const SORT_THREADS: usize = 4;

/// By row. Stable, so a row's changes stay in order. Rayon would deadlock here: its workers
/// block while they send to this thread.
fn sort(changes: &mut [Change]) {
    let row = |change: &Change| (change.block(), change.trigram);
    if changes.len() >= FLUSH_BETWEEN_BLOCKS {
        let part = changes.len().div_ceil(SORT_THREADS);
        thread::scope(|scope| {
            for chunk in changes.chunks_mut(part) {
                scope.spawn(move || chunk.sort_by_key(row));
            }
        });
    }
    // Merges the sorted parts of a large list.
    changes.sort_by_key(row);
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
        for need in &needs {
            let stored: Option<Vec<u8>> = read
                .query_row(params![block, need.trigram.0], |row| row.get(0))
                .optional()?;
            let Some(bytes) = stored else {
                continue 'blocks;
            };
            let usable = decode(&bytes, base)?
                .into_iter()
                .filter(|posting| posting.allows(need));
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

/// What a blob with the literal has for one of its trigrams.
#[derive(Debug)]
struct Need {
    trigram: Trigram,
    /// How often the literal holds the trigram.
    times: u32,
    /// The union over those places; `exact` and `known` only count when `times` is 1.
    follow: Follow,
}

fn needs(chars: &[char]) -> Vec<Need> {
    let mut needs: BTreeMap<Trigram, Need> = BTreeMap::new();
    for (at, window) in chars.windows(3).enumerate() {
        let trigram = Trigram::new([window[0], window[1], window[2]]);
        let here = follow(chars, at);
        let need = needs.entry(trigram).or_insert(Need {
            trigram,
            times: 0,
            follow: here,
        });
        need.times += 1;
        need.follow.next |= here.next;
        need.follow.pair |= here.pair;
    }
    needs.into_values().collect()
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
                pair: 0,
            },
            Posting {
                blob: base + 5,
                count: 3,
                next: 0x81,
                pair: 0,
            },
            Posting {
                blob: base + 6,
                count: 4,
                next: 0x81,
                pair: 0x42,
            },
            Posting {
                blob: base + 255,
                count: 70_000,
                next: 0xFF,
                pair: 0xFF,
            },
        ];
        assert_eq!(decode(&encode(&postings, base), base).ok(), Some(postings));
    }

    fn allowed(text: &str, literal: &str) -> bool {
        let chars: Vec<char> = literal.chars().collect();
        let stored = Trigrams::of(text).0;
        needs(&chars).iter().all(|need| {
            stored
                .iter()
                .find(|(trigram, _)| *trigram == need.trigram)
                .is_some_and(|&(_, seen)| Posting::new(1, seen).allows(need))
        })
    }

    #[test]
    fn a_blob_with_the_literal_is_never_filtered_out() {
        let text = "abcabcabcabd abcd xyz xyzw aaaaaa abcab";
        let chars: Vec<char> = text.chars().collect();
        for length in 3..=8 {
            for literal in chars.windows(length) {
                let literal: String = literal.iter().collect();
                assert!(allowed(text, &literal), "{literal:?}");
            }
        }
    }

    #[test]
    fn the_characters_after_a_trigram_filter() {
        assert!(!allowed("abcd abce", "abcx"));
        assert!(!allowed("abcde", "abcdx"));
        assert!(!allowed(
            "abcd abcd abcd abcde bcdw bcdw bcdw bcdw",
            "abcdw"
        ));
        assert!(!allowed("aaa", "aaaa"));
    }
}
