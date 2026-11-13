//! References: one row per blob, with the references to each name in a group of its own.
//!
//! A row is the id of the blob's first symbol, then per name: its id, the byte length of its
//! group, and the group. A reference in a group is its line as a delta, its column, and its kind
//! with the place of its enclosing symbol among the blob's symbols.

use std::collections::HashMap;

use clap::ValueEnum;
use rusqlite::{Connection, OptionalExtension, params};

use crate::error::{Error, Result};
use crate::extract::{RefKind, Reference};

/// Enough for four kinds; a fifth needs a schema version.
const KIND_BITS: u32 = 2;

/// A blob's references, grouped by name and encoded. The writer adds the ids.
#[derive(Debug, Default)]
pub(crate) struct Grouped {
    /// In order of first use.
    groups: Vec<(String, Vec<u8>)>,
    count: i64,
}

impl Grouped {
    pub(crate) fn of(refs: &[Reference]) -> Self {
        let mut index_of: HashMap<&str, usize> = HashMap::new();
        let mut keyed: Vec<(usize, &Reference)> = refs
            .iter()
            .map(|reference| {
                let next = index_of.len();
                let index = *index_of.entry(&reference.name).or_insert(next);
                (index, reference)
            })
            .collect();
        let key = |&(name, reference): &(usize, &Reference)| {
            (name, reference.line, reference.col, reference.kind as u8)
        };
        keyed.sort_by_key(key);
        // Two refs at one position with one name and kind say nothing more than one.
        keyed.dedup_by_key(|entry| key(entry));
        let groups = keyed
            .chunk_by(|a, b| a.0 == b.0)
            .map(|group| (group[0].1.name.clone(), encode(group)))
            .collect();
        Self {
            groups,
            count: i64::try_from(keyed.len()).unwrap_or(i64::MAX),
        }
    }

    pub(crate) fn names(&self) -> impl Iterator<Item = &str> {
        self.groups.iter().map(|(name, _)| name.as_str())
    }
}

fn encode(group: &[(usize, &Reference)]) -> Vec<u8> {
    let mut bytes = Vec::with_capacity(group.len() * 3);
    let mut previous = 0;
    for &(_, reference) in group {
        put_varint(&mut bytes, u64::from(reference.line - previous));
        previous = reference.line;
        put_varint(&mut bytes, u64::from(reference.col));
        let enclosing = reference.enclosing.map_or(0, |index| index as u64 + 1);
        put_varint(&mut bytes, enclosing << KIND_BITS | reference.kind as u64);
    }
    bytes
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Ref {
    pub line: u32,
    pub col: u32,
    pub kind: RefKind,
    /// The id of the innermost symbol around the reference.
    pub enclosing: Option<i64>,
}

/// About 16 bytes each, so the writer's buffer stays near 32 MB.
const FLUSH_NAMES: usize = 2_000_000;

/// Writes the rows of new blobs and buffers their `ref_names` rows: one sorted insert is faster
/// than a row at a time. Call [`Pending::finish`] before commit, or the buffered rows are lost.
#[derive(Debug, Default)]
#[must_use = "rows are lost unless `finish` writes them"]
pub(crate) struct Pending {
    /// Name id, then blob id.
    names: Vec<(i64, i64)>,
}

impl Pending {
    /// `name_ids` are the ids of [`Grouped::names`], in that order. The blob's symbols have
    /// consecutive ids from `first_symbol`.
    pub(crate) fn insert(
        &mut self,
        conn: &Connection,
        blob: i64,
        first_symbol: i64,
        name_ids: &[i64],
        grouped: &Grouped,
    ) -> Result<()> {
        if grouped.groups.is_empty() {
            return Ok(());
        }
        let mut row = Vec::new();
        put_varint(&mut row, first_symbol.cast_unsigned());
        for (&name_id, (_, group)) in name_ids.iter().zip(&grouped.groups) {
            put_varint(&mut row, name_id.cast_unsigned());
            put_varint(&mut row, group.len() as u64);
            row.extend_from_slice(group);
            self.names.push((name_id, blob));
        }
        conn.prepare_cached("INSERT INTO refs (blob_id, count, groups) VALUES (?1, ?2, ?3)")?
            .execute(params![blob, grouped.count, row])?;
        if self.names.len() >= FLUSH_NAMES {
            self.flush(conn)?;
        }
        Ok(())
    }

    pub(crate) fn delete(&mut self, conn: &Connection, blob: i64) -> Result<()> {
        // The blob's rows may still be in the buffer.
        self.flush(conn)?;
        let stored: Option<Vec<u8>> = conn
            .prepare_cached("SELECT groups FROM refs WHERE blob_id = ?1")?
            .query_row([blob], |row| row.get(0))
            .optional()?;
        let Some(stored) = stored else {
            return Ok(());
        };
        let mut name =
            conn.prepare_cached("DELETE FROM ref_names WHERE name_id = ?1 AND blob_id = ?2")?;
        for (name_id, _) in split(&stored)?.groups {
            name.execute(params![name_id, blob])?;
        }
        conn.prepare_cached("DELETE FROM refs WHERE blob_id = ?1")?
            .execute([blob])?;
        Ok(())
    }

    pub(crate) fn finish(mut self, conn: &Connection) -> Result<()> {
        self.flush(conn)
    }

    fn flush(&mut self, conn: &Connection) -> Result<()> {
        self.names.sort_unstable();
        let mut name =
            conn.prepare_cached("INSERT INTO ref_names (name_id, blob_id) VALUES (?1, ?2)")?;
        for (name_id, blob) in self.names.drain(..) {
            name.execute(params![name_id, blob])?;
        }
        Ok(())
    }
}

/// The references to one name in a stored row, in position order.
pub(crate) fn of_name(row: &[u8], name_id: i64) -> Result<Vec<Ref>> {
    let row = split(row)?;
    match row.groups.iter().find(|&&(id, _)| id == name_id) {
        Some(&(_, group)) => decode(group, row.first_symbol),
        None => Ok(Vec::new()),
    }
}

/// Every reference of a blob with its name id, in no order.
pub(crate) fn of_blob(conn: &Connection, blob: i64) -> Result<Vec<(i64, Ref)>> {
    let stored: Option<Vec<u8>> = conn
        .prepare_cached("SELECT groups FROM refs WHERE blob_id = ?1")?
        .query_row([blob], |row| row.get(0))
        .optional()?;
    let mut refs = Vec::new();
    if let Some(stored) = stored {
        let row = split(&stored)?;
        for (name_id, group) in row.groups {
            let decoded = decode(group, row.first_symbol)?;
            refs.extend(decoded.into_iter().map(|reference| (name_id, reference)));
        }
    }
    Ok(refs)
}

fn corrupt() -> Error {
    Error::failure("corrupt refs row; run `lantern index --rebuild`")
}

/// A stored row, split into its parts.
struct Row<'row> {
    first_symbol: i64,
    /// Each name id with its group.
    groups: Vec<(i64, &'row [u8])>,
}

fn split(row: &[u8]) -> Result<Row<'_>> {
    let mut at = 0;
    let first_symbol = take_varint(row, &mut at).ok_or_else(corrupt)?;
    let mut groups = Vec::new();
    while at < row.len() {
        let name_id = take_varint(row, &mut at).ok_or_else(corrupt)?;
        let len = take_varint(row, &mut at).ok_or_else(corrupt)?;
        let end = usize::try_from(len)
            .ok()
            .and_then(|len| at.checked_add(len))
            .ok_or_else(corrupt)?;
        groups.push((name_id.cast_signed(), row.get(at..end).ok_or_else(corrupt)?));
        at = end;
    }
    Ok(Row {
        first_symbol: first_symbol.cast_signed(),
        groups,
    })
}

fn decode(group: &[u8], first_symbol: i64) -> Result<Vec<Ref>> {
    let mut refs = Vec::new();
    let mut at = 0;
    let mut line = 0u32;
    while at < group.len() {
        let delta = take_varint(group, &mut at).ok_or_else(corrupt)?;
        let col = take_varint(group, &mut at).ok_or_else(corrupt)?;
        let tail = take_varint(group, &mut at).ok_or_else(corrupt)?;
        line = u32::try_from(delta)
            .ok()
            .and_then(|delta| line.checked_add(delta))
            .ok_or_else(corrupt)?;
        let kind = RefKind::value_variants()
            .iter()
            .copied()
            .find(|&kind| kind as u64 == tail & ((1 << KIND_BITS) - 1))
            .ok_or_else(corrupt)?;
        let enclosing = match tail >> KIND_BITS {
            0 => None,
            place => Some(
                i64::try_from(place - 1)
                    .ok()
                    .and_then(|index| first_symbol.checked_add(index))
                    .ok_or_else(corrupt)?,
            ),
        };
        refs.push(Ref {
            line,
            col: u32::try_from(col).map_err(|_| corrupt())?,
            kind,
            enclosing,
        });
    }
    Ok(refs)
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

#[cfg(test)]
mod tests {
    use super::*;

    fn reference(name: &str, kind: RefKind, line: u32, enclosing: Option<usize>) -> Reference {
        Reference {
            name: name.to_owned(),
            kind,
            line,
            col: line + 2,
            enclosing,
        }
    }

    #[test]
    fn a_row_round_trips_every_kind_and_drops_a_repeat() {
        let mut refs: Vec<Reference> = RefKind::value_variants()
            .iter()
            .zip(1..)
            .map(|(&kind, line)| reference("used", kind, line * 300, Some(40)))
            .collect();
        refs.push(reference("other", RefKind::Call, 7, None));
        refs.push(reference("used", RefKind::Call, 300, None));
        let grouped = Grouped::of(&refs);
        assert_eq!(grouped.names().collect::<Vec<_>>(), ["used", "other"]);
        assert_eq!(grouped.count, 5);

        let dir = tempfile::tempdir().expect("temp dir");
        let conn = super::super::shard::open(&dir.path().join("shard.sqlite")).expect("open");
        conn.pragma_update(None, "foreign_keys", false)
            .expect("foreign keys off");
        let mut pending = Pending::default();
        pending
            .insert(&conn, 9, 1000, &[5, 6], &grouped)
            .expect("insert");
        let stored: Vec<u8> = conn
            .query_row("SELECT groups FROM refs WHERE blob_id = 9", [], |row| {
                row.get(0)
            })
            .expect("row");
        let expected: Vec<Ref> = refs[..4]
            .iter()
            .map(|reference| Ref {
                line: reference.line,
                col: reference.col,
                kind: reference.kind,
                enclosing: Some(1040),
            })
            .collect();
        assert_eq!(of_name(&stored, 5).expect("decode"), expected);
        let all = of_blob(&conn, 9).expect("decode");
        assert_eq!(all.len(), 5);
        assert_eq!(
            all[4],
            (
                6,
                Ref {
                    line: 7,
                    col: 9,
                    kind: RefKind::Call,
                    enclosing: None
                }
            )
        );

        pending.delete(&conn, 9).expect("delete");
        pending.finish(&conn).expect("finish");
        let left: i64 = conn
            .query_row(
                "SELECT (SELECT count(*) FROM refs) + (SELECT count(*) FROM ref_names)",
                [],
                |row| row.get(0),
            )
            .expect("count");
        assert_eq!(left, 0);
    }
}
