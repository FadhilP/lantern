//! One shard per root: blobs, trigram index, names, symbols, references, files.

use std::cell::RefCell;
use std::collections::{BTreeSet, HashMap};
use std::io::Read;
use std::path::Path;

use rayon::prelude::*;
use rusqlite::{Connection, OptionalExtension, params};

use crate::error::{Error, Result};
use crate::extract::{Extraction, Symbol};
use crate::fsmeta::{EntryKind, Stat};
use crate::language::FileFlags;
use crate::source::SourceKind;
use crate::store::refs::{self, Grouped};
use crate::store::trigram::{Pending, Trigrams};

const SCHEMA: &str = r"
CREATE TABLE meta (
  root TEXT NOT NULL, root_key TEXT NOT NULL, source TEXT NOT NULL,
  head TEXT, branch TEXT,
  policy TEXT NOT NULL,
  extractor TEXT NOT NULL,
  indexed_at INTEGER, generation INTEGER NOT NULL DEFAULT 0,
  -- When the last committed refresh started its inventory; see `refresh::settled`.
  scanned_at INTEGER NOT NULL,
  checkpoint_pending INTEGER NOT NULL DEFAULT 0
);
CREATE TABLE blobs (
  id INTEGER PRIMARY KEY,
  key BLOB NOT NULL UNIQUE,
  language TEXT,
  lines INTEGER NOT NULL,
  content BLOB NOT NULL,
  -- Parse timeouts in a row; a timed-out blob is parsed again on later refreshes.
  timeouts INTEGER NOT NULL DEFAULT 0
);
-- The postings of one trigram for a block of blob ids; see `store::trigram`.
CREATE TABLE trigrams (
  block INTEGER NOT NULL,
  trigram INTEGER NOT NULL,
  postings BLOB NOT NULL,
  PRIMARY KEY (block, trigram)
) WITHOUT ROWID;
CREATE TABLE names (
  id INTEGER PRIMARY KEY,
  name TEXT NOT NULL UNIQUE,
  initials TEXT NOT NULL
);
CREATE VIRTUAL TABLE names_fts USING fts5(
  name, content='names', content_rowid='id', tokenize='trigram'
);
CREATE TABLE symbols (
  id INTEGER PRIMARY KEY,
  blob_id INTEGER NOT NULL REFERENCES blobs ON DELETE CASCADE,
  name_id INTEGER NOT NULL REFERENCES names,
  kind INTEGER NOT NULL,
  container TEXT,
  line INTEGER NOT NULL, col INTEGER NOT NULL, end_line INTEGER NOT NULL,
  -- The first line of the decorators, attributes, and doc comments before the definition.
  start_line INTEGER NOT NULL
);
-- The references of one blob, grouped by name; see `store::refs`. No cascade: with foreign keys
-- on, a blob deleted before `refs::Pending::delete` ran fails, instead of leaving `ref_names` rows.
CREATE TABLE refs (
  blob_id INTEGER PRIMARY KEY REFERENCES blobs,
  count INTEGER NOT NULL,
  groups BLOB NOT NULL
);
-- The blobs with references to a name. `refs::Pending::delete` removes a blob's rows; a foreign
-- key would need an index of its own.
CREATE TABLE ref_names (
  name_id INTEGER NOT NULL REFERENCES names,
  blob_id INTEGER NOT NULL,
  PRIMARY KEY (name_id, blob_id)
) WITHOUT ROWID;
-- blob_id is NULL for binary and oversized files, so they are not reread on every refresh.
CREATE TABLE files (
  path TEXT PRIMARY KEY,
  blob_id INTEGER REFERENCES blobs,
  flags INTEGER NOT NULL,
  device INTEGER NOT NULL, inode INTEGER NOT NULL, size INTEGER NOT NULL,
  -- In the platform's unit; see `fsmeta::Stat`.
  modified INTEGER NOT NULL, changed INTEGER NOT NULL
) WITHOUT ROWID;
";

/// A new shard builds these after its first load: one sorted build is faster than a row at a time.
const INDEXES: &str = r"
CREATE INDEX IF NOT EXISTS blobs_timeouts ON blobs(timeouts) WHERE timeouts > 0;
-- A blobs row shares its page with the content; `code --language` reads this instead.
CREATE INDEX IF NOT EXISTS blobs_language ON blobs(language);
CREATE INDEX IF NOT EXISTS names_nocase ON names(name COLLATE NOCASE);
CREATE INDEX IF NOT EXISTS names_initials ON names(initials);
CREATE INDEX IF NOT EXISTS symbols_name ON symbols(name_id);
CREATE INDEX IF NOT EXISTS symbols_blob ON symbols(blob_id, line);
CREATE INDEX IF NOT EXISTS files_blob ON files(blob_id);
";

pub(crate) fn open(path: &Path) -> Result<Connection> {
    let conn = super::open(path, SCHEMA)?;
    // Freed cells and pages are zeroed; stale copies in page gaps stay until a rebuild.
    // The writer touches index pages at random; with the 2 MB default cache it rereads them.
    conn.execute_batch(
        "PRAGMA secure_delete = ON; PRAGMA cache_size = -65536; PRAGMA threads = 4",
    )?;
    Ok(conn)
}

pub(crate) fn create_indexes(conn: &Connection) -> Result<()> {
    conn.execute_batch(INDEXES)?;
    Ok(())
}

/// Opens an existing shard read-only; `None` when this Lantern never indexed the root.
pub(crate) fn open_existing(path: &Path) -> Result<Option<Connection>> {
    if !path.exists() {
        return Ok(None);
    }
    super::open_read_only(path)
}

/// The meta of an existing shard; `None` when it has no committed schema or meta yet.
pub(crate) fn stored_meta(path: &Path) -> Result<Option<Meta>> {
    let Some(conn) = open_existing(path)? else {
        return Ok(None);
    };
    meta(&conn)
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Meta {
    pub root: String,
    pub root_key: String,
    pub source: SourceKind,
    pub head: Option<String>,
    pub branch: Option<String>,
    pub policy: String,
    pub extractor: String,
    pub indexed_at: Option<i64>,
    pub generation: i64,
    pub scanned_at: i64,
    /// A secure commit whose WAL was not yet emptied.
    pub checkpoint_pending: bool,
}

pub(crate) fn meta(conn: &Connection) -> Result<Option<Meta>> {
    let row = conn
        .query_row(
            "SELECT root, root_key, source, head, branch, policy, extractor, indexed_at, generation,
                    scanned_at, checkpoint_pending
             FROM meta",
            [],
            |row| {
                Ok(Meta {
                    root: row.get(0)?,
                    root_key: row.get(1)?,
                    source: row.get(2)?,
                    head: row.get(3)?,
                    branch: row.get(4)?,
                    policy: row.get(5)?,
                    extractor: row.get(6)?,
                    indexed_at: row.get(7)?,
                    generation: row.get(8)?,
                    scanned_at: row.get(9)?,
                    checkpoint_pending: row.get(10)?,
                })
            },
        )
        .optional()?;
    Ok(row)
}

pub(crate) fn write_meta(conn: &Connection, meta: &Meta) -> Result<()> {
    conn.execute("DELETE FROM meta", [])?;
    conn.execute(
        "INSERT INTO meta (root, root_key, source, head, branch, policy, extractor, indexed_at, generation,
           scanned_at, checkpoint_pending)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11)",
        params![
            meta.root,
            meta.root_key,
            meta.source.as_str(),
            meta.head,
            meta.branch,
            meta.policy,
            meta.extractor,
            meta.indexed_at,
            meta.generation,
            meta.scanned_at,
            meta.checkpoint_pending
        ],
    )?;
    Ok(())
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct FileRow {
    pub blob_id: Option<i64>,
    pub flags: u8,
    pub stat: Stat,
}

pub(crate) fn files(conn: &Connection) -> Result<HashMap<String, FileRow>> {
    let mut statement = conn.prepare(
        "SELECT path, blob_id, flags, device, inode, size, modified, changed FROM files",
    )?;
    let rows = statement.query_map([], |row| {
        Ok((
            row.get::<_, String>(0)?,
            FileRow {
                blob_id: row.get(1)?,
                flags: row.get(2)?,
                stat: Stat {
                    kind: EntryKind::File,
                    device: row.get::<_, i64>(3)?.cast_unsigned(),
                    inode: row.get::<_, i64>(4)?.cast_unsigned(),
                    size: row.get::<_, i64>(5)?.cast_unsigned(),
                    modified: row.get(6)?,
                    changed: row.get(7)?,
                },
            },
        ))
    })?;
    Ok(rows.collect::<rusqlite::Result<_>>()?)
}

/// Each text file with the language stored on its blob.
pub(crate) fn file_languages(conn: &Connection) -> Result<Vec<(String, Option<String>)>> {
    let mut statement =
        conn.prepare("SELECT f.path, b.language FROM files f JOIN blobs b ON b.id = f.blob_id")?;
    let rows = statement.query_map([], |row| Ok((row.get(0)?, row.get(1)?)))?;
    Ok(rows.collect::<rusqlite::Result<_>>()?)
}

pub(crate) type BlobKey = [u8; 32];

pub(crate) fn blob_keys(conn: &Connection) -> Result<HashMap<BlobKey, i64>> {
    let mut statement = conn.prepare("SELECT key, id FROM blobs")?;
    let rows = statement.query_map([], |row| {
        Ok((row.get::<_, Vec<u8>>(0)?, row.get::<_, i64>(1)?))
    })?;
    let mut keys = HashMap::new();
    for row in rows {
        let (key, id) = row?;
        if let Ok(key) = BlobKey::try_from(key.as_slice()) {
            keys.insert(key, id);
        }
    }
    Ok(keys)
}

pub(crate) fn upsert_file(conn: &Connection, path: &str, row: &FileRow) -> Result<()> {
    conn.prepare_cached(
        "INSERT INTO files (path, blob_id, flags, device, inode, size, modified, changed)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)
         ON CONFLICT (path) DO UPDATE SET blob_id = excluded.blob_id, flags = excluded.flags,
           device = excluded.device, inode = excluded.inode, size = excluded.size,
           modified = excluded.modified, changed = excluded.changed",
    )?
    .execute(params![
        path,
        row.blob_id,
        row.flags,
        row.stat.device.cast_signed(),
        row.stat.inode.cast_signed(),
        row.stat.size.cast_signed(),
        row.stat.modified,
        row.stat.changed
    ])?;
    Ok(())
}

pub(crate) fn delete_file(conn: &Connection, path: &str) -> Result<()> {
    conn.prepare_cached("DELETE FROM files WHERE path = ?1")?
        .execute([path])?;
    Ok(())
}

pub(crate) fn delete_orphans(
    conn: &Connection,
    trigrams: &mut Pending,
    refs: &mut refs::Pending,
    candidates: impl IntoIterator<Item = i64>,
) -> Result<usize> {
    let mut used = conn.prepare_cached("SELECT 1 FROM files WHERE blob_id = ?1 LIMIT 1")?;
    let mut orphans = BTreeSet::new();
    for id in candidates {
        if !used.exists([id])? {
            orphans.insert(id);
        }
    }
    let orphans: Vec<i64> = orphans.into_iter().collect();
    let mut content = conn.prepare_cached("SELECT content FROM blobs WHERE id = ?1")?;
    let mut blob = conn.prepare_cached("DELETE FROM blobs WHERE id = ?1")?;
    for batch in orphans.chunks(ORPHAN_BATCH) {
        let compressed: Vec<Vec<u8>> = batch
            .iter()
            .map(|&id| content.query_row([id], |row| row.get(0)))
            .collect::<rusqlite::Result<_>>()?;
        let counted: Vec<Trigrams> = compressed
            .par_iter()
            .map(|bytes| Ok(Trigrams::of(&decompress(bytes)?)))
            .collect::<Result<_>>()?;
        for (&id, counted) in batch.iter().zip(&counted) {
            trigrams.remove(conn, id, counted)?;
            // Not left to the cascade: a new shard writes with foreign keys off.
            delete_extraction(conn, refs, id)?;
            blob.execute([id])?;
        }
    }
    Ok(orphans.len())
}

/// Bounds the memory of a large removal.
const ORPHAN_BATCH: usize = 256;

/// Deletes names that no symbol or reference uses, so removed identifiers do not stay readable.
pub(crate) fn delete_unused_names(conn: &Connection) -> Result<()> {
    const UNUSED: &str = "NOT EXISTS (SELECT 1 FROM symbols s WHERE s.name_id = n.id)
        AND NOT EXISTS (SELECT 1 FROM ref_names r WHERE r.name_id = n.id)";
    conn.execute(
        &format!(
            "INSERT INTO names_fts (names_fts, rowid, name)
             SELECT 'delete', n.id, n.name FROM names n WHERE {UNUSED}"
        ),
        [],
    )?;
    conn.execute(&format!("DELETE FROM names AS n WHERE {UNUSED}"), [])?;
    conn.execute("INSERT INTO names_fts (names_fts) VALUES ('optimize')", [])?;
    Ok(())
}

#[derive(Debug)]
pub(crate) struct NewBlob {
    pub key: BlobKey,
    pub language: Option<&'static str>,
    pub lines: i64,
    pub compressed: Vec<u8>,
    pub trigrams: Trigrams,
    pub extraction: Extracted,
}

/// An extraction in the form the writer stores. The workers build it.
#[derive(Debug, Default)]
pub(crate) struct Extracted {
    symbols: Vec<Symbol>,
    refs: Grouped,
    pub timed_out: bool,
}

impl From<Extraction> for Extracted {
    fn from(extraction: Extraction) -> Self {
        Self {
            refs: Grouped::of(&extraction.refs),
            symbols: extraction.symbols,
            timed_out: extraction.timed_out,
        }
    }
}

/// Gaps first, so trigram blocks stay full.
#[derive(Debug)]
pub(crate) struct BlobIds {
    /// Descending, so the smallest gap is last.
    gaps: Vec<i64>,
    next: i64,
}

impl BlobIds {
    pub(crate) fn load(conn: &Connection) -> Result<Self> {
        let mut ids: Vec<i64> = conn
            .prepare("SELECT id FROM blobs")?
            .query_map([], |row| row.get(0))?
            .collect::<rusqlite::Result<_>>()?;
        ids.sort_unstable();
        let mut gaps = Vec::new();
        let mut next = 1;
        for id in ids {
            gaps.extend(next..id);
            next = id + 1;
        }
        gaps.reverse();
        Ok(Self { gaps, next })
    }

    fn take(&mut self) -> i64 {
        self.gaps.pop().unwrap_or_else(|| {
            self.next += 1;
            self.next - 1
        })
    }
}

pub(crate) fn insert_blob(
    conn: &Connection,
    ids: &mut BlobIds,
    names: &mut Names,
    trigrams: &mut Pending,
    refs: &mut refs::Pending,
    blob: &NewBlob,
) -> Result<i64> {
    let id = ids.take();
    conn.prepare_cached(
        "INSERT INTO blobs (id, key, language, lines, content, timeouts)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
    )?
    .execute(params![
        id,
        blob.key.as_slice(),
        blob.language,
        blob.lines,
        blob.compressed,
        blob.extraction.timed_out
    ])?;
    trigrams.add(conn, id, &blob.trigrams)?;
    insert_extraction(conn, names, refs, id, &blob.extraction)?;
    Ok(id)
}

pub(crate) fn insert_extraction(
    conn: &Connection,
    names: &mut Names,
    refs: &mut refs::Pending,
    blob_id: i64,
    extraction: &Extracted,
) -> Result<()> {
    // Explicit ids, so a reference can store its enclosing symbol as a place in this list.
    let first_symbol: i64 = conn
        .prepare_cached("SELECT coalesce(max(id), 0) + 1 FROM symbols")?
        .query_row([], |row| row.get(0))?;
    for (id, symbol) in (first_symbol..).zip(&extraction.symbols) {
        let name_id = names.id(conn, &symbol.name)?;
        conn.prepare_cached(
            "INSERT INTO symbols
               (id, blob_id, name_id, kind, container, line, col, end_line, start_line)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
        )?
        .execute(params![
            id,
            blob_id,
            name_id,
            symbol.kind,
            symbol.container,
            symbol.line,
            symbol.col,
            symbol.end_line,
            symbol.start_line
        ])?;
    }
    let name_ids: Vec<i64> = extraction
        .refs
        .names()
        .map(|name| names.id(conn, name))
        .collect::<Result<_>>()?;
    refs.insert(conn, blob_id, first_symbol, &name_ids, &extraction.refs)?;
    Ok(())
}

/// Blobs whose last parse timed out, fewer than `max` times in a row.
pub(crate) fn timed_out_blobs(conn: &Connection, max: i64) -> Result<Vec<i64>> {
    let mut statement =
        conn.prepare("SELECT id FROM blobs WHERE timeouts > 0 AND timeouts < ?1 ORDER BY id")?;
    let rows = statement.query_map([max], |row| row.get(0))?;
    Ok(rows.collect::<rusqlite::Result<_>>()?)
}

/// Replaces a blob's symbols and references, and records whether its parse timed out.
pub(crate) fn replace_extraction(
    conn: &Connection,
    names: &mut Names,
    refs: &mut refs::Pending,
    blob_id: i64,
    extraction: &Extracted,
) -> Result<()> {
    delete_extraction(conn, refs, blob_id)?;
    insert_extraction(conn, names, refs, blob_id, extraction)?;
    conn.prepare_cached(
        "UPDATE blobs SET timeouts = CASE WHEN ?2 THEN timeouts + 1 ELSE 0 END WHERE id = ?1",
    )?
    .execute(params![blob_id, extraction.timed_out])?;
    Ok(())
}

fn delete_extraction(conn: &Connection, refs: &mut refs::Pending, blob_id: i64) -> Result<()> {
    refs.delete(conn, blob_id)?;
    conn.prepare_cached("DELETE FROM symbols WHERE blob_id = ?1")?
        .execute([blob_id])?;
    Ok(())
}

/// Decompresses one blob.
pub(crate) fn blob_text(conn: &Connection, blob_id: i64) -> Result<String> {
    let compressed: Vec<u8> = conn
        .prepare_cached("SELECT content FROM blobs WHERE id = ?1")?
        .query_row([blob_id], |row| row.get(0))?;
    decompress(&compressed)
}

/// Blobs are about 8% smaller than at level 3, for about 3 times the CPU time. The threads that
/// compress wait for the one writer most of the time.
const COMPRESSION_LEVEL: i32 = 7;

pub(crate) fn compress(bytes: &[u8]) -> Result<Vec<u8>> {
    thread_local! {
        // Creating a context costs more than encoding a typical file.
        static CONTEXT: RefCell<zstd::zstd_safe::CCtx<'static>> =
            RefCell::new(zstd::zstd_safe::CCtx::create());
    }
    let mut compressed = Vec::with_capacity(zstd::zstd_safe::compress_bound(bytes.len()));
    CONTEXT
        .with_borrow_mut(|context| context.compress(&mut compressed, bytes, COMPRESSION_LEVEL))
        .map_err(|code| {
            let name = zstd::zstd_safe::get_error_name(code);
            Error::failure(format!("compression failed: {name}"))
        })?;
    compressed.shrink_to_fit();
    Ok(compressed)
}

pub(crate) fn decompress(compressed: &[u8]) -> Result<String> {
    thread_local! {
        // Creating a context costs more than decoding a typical file.
        static CONTEXT: RefCell<zstd::zstd_safe::DCtx<'static>> =
            RefCell::new(zstd::zstd_safe::DCtx::create());
    }
    let mut bytes = Vec::new();
    CONTEXT
        .with_borrow_mut(|context| {
            zstd::stream::read::Decoder::with_context(compressed, context).read_to_end(&mut bytes)
        })
        .map_err(|error| {
            Error::failure(format!(
                "corrupt blob: {error}; run `lantern index --rebuild`"
            ))
        })?;
    Ok(String::from_utf8(bytes)
        .unwrap_or_else(|error| String::from_utf8_lossy(error.as_bytes()).into_owned()))
}

/// Row counts for `index` and `status` output.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, serde::Serialize)]
pub struct Counts {
    pub files: u64,
    pub text_files: u64,
    pub code_files: u64,
    pub symbols: u64,
    pub refs: u64,
    /// Files with no symbols because their last parse ran out of time.
    pub unparsed_files: u64,
    /// Files with no content because the last refresh could not open or read them.
    pub unreadable_files: u64,
}

impl std::ops::AddAssign for Counts {
    fn add_assign(&mut self, other: Self) {
        self.files += other.files;
        self.text_files += other.text_files;
        self.code_files += other.code_files;
        self.symbols += other.symbols;
        self.refs += other.refs;
        self.unparsed_files += other.unparsed_files;
        self.unreadable_files += other.unreadable_files;
    }
}

pub(crate) fn counts(conn: &Connection) -> Result<Counts> {
    Ok(conn.query_row(
        "SELECT (SELECT count(*) FROM files),
                (SELECT count(*) FROM files WHERE blob_id IS NOT NULL),
                (SELECT count(*) FROM files f JOIN blobs b ON b.id = f.blob_id WHERE b.language IS NOT NULL),
                (SELECT count(*) FROM symbols),
                (SELECT coalesce(sum(count), 0) FROM refs),
                (SELECT count(*) FROM files f JOIN blobs b ON b.id = f.blob_id WHERE b.timeouts > 0),
                (SELECT count(*) FROM files WHERE flags & ?1)",
        [FileFlags::UNREADABLE],
        |row| {
            Ok(Counts {
                files: row.get::<_, i64>(0)?.unsigned_abs(),
                text_files: row.get::<_, i64>(1)?.unsigned_abs(),
                code_files: row.get::<_, i64>(2)?.unsigned_abs(),
                symbols: row.get::<_, i64>(3)?.unsigned_abs(),
                refs: row.get::<_, i64>(4)?.unsigned_abs(),
                unparsed_files: row.get::<_, i64>(5)?.unsigned_abs(),
                unreadable_files: row.get::<_, i64>(6)?.unsigned_abs(),
            })
        },
    )?)
}

/// Why an empty result may be empty.
#[derive(Debug, Clone, Copy, Default)]
pub struct Incomplete {
    /// Files with no symbols because their last parse ran out of time.
    pub unparsed_files: u64,
    /// Files with no content because the last refresh could not open or read them.
    pub unreadable_files: u64,
    /// `--path` was given and no indexed file in scope passes it.
    pub no_file_in_path: bool,
}

pub(crate) fn incomplete(conn: &Connection) -> Result<Incomplete> {
    Ok(conn.query_row(
        "SELECT (SELECT count(*) FROM files f JOIN blobs b ON b.id = f.blob_id WHERE b.timeouts > 0),
                (SELECT count(*) FROM files WHERE flags & ?1)",
        [FileFlags::UNREADABLE],
        |row| {
            Ok(Incomplete {
                unparsed_files: row.get::<_, i64>(0)?.unsigned_abs(),
                unreadable_files: row.get::<_, i64>(1)?.unsigned_abs(),
                no_file_in_path: false,
            })
        },
    )?)
}

/// Interned identifiers. A secure refresh deletes unused ones.
#[derive(Debug, Default)]
pub(crate) struct Names {
    ids: HashMap<String, i64>,
}

impl Names {
    pub fn id(&mut self, conn: &Connection, name: &str) -> Result<i64> {
        if let Some(&id) = self.ids.get(name) {
            return Ok(id);
        }
        let existing: Option<i64> = conn
            .prepare_cached("SELECT id FROM names WHERE name = ?1")?
            .query_row([name], |row| row.get(0))
            .optional()?;
        let id = if let Some(id) = existing {
            id
        } else {
            conn.prepare_cached("INSERT INTO names (name, initials) VALUES (?1, ?2)")?
                .execute(params![name, initials(name)])?;
            let id = conn.last_insert_rowid();
            conn.prepare_cached("INSERT INTO names_fts (rowid, name) VALUES (?1, ?2)")?
                .execute(params![id, name])?;
            id
        };
        self.ids.insert(name.to_owned(), id);
        Ok(id)
    }
}

/// Camel-case and snake-case initials, lowercased: `getUserById` and `get_user_by_id` give `gubi`.
pub(crate) fn initials(name: &str) -> String {
    let chars: Vec<char> = name.chars().collect();
    let mut out = String::new();
    for (index, &current) in chars.iter().enumerate() {
        if !current.is_alphanumeric() {
            continue;
        }
        let previous = index.checked_sub(1).map(|at| chars[at]);
        let next = chars.get(index + 1);
        let starts_word = match previous {
            None => true,
            Some(previous) if !previous.is_alphanumeric() => true,
            Some(previous) if current.is_uppercase() && !previous.is_uppercase() => true,
            Some(previous) => {
                current.is_uppercase()
                    && previous.is_uppercase()
                    && next.is_some_and(|next| next.is_lowercase())
            }
        };
        if starts_word {
            out.extend(current.to_lowercase());
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_orphan_loses_its_extraction_with_foreign_keys_off() {
        let dir = tempfile::tempdir().expect("temp dir");
        let conn = open(&dir.path().join("shard.sqlite")).expect("open shard");
        conn.pragma_update(None, "foreign_keys", false)
            .expect("foreign keys off");
        let content = zstd::encode_all(
            "x
"
            .as_bytes(),
            3,
        )
        .expect("compress");
        conn.execute(
            "INSERT INTO blobs (id, key, lines, content) VALUES (1, x'01', 1, ?1)",
            [content],
        )
        .expect("insert blob");
        conn.execute_batch(
            "INSERT INTO names VALUES (1, 'f', 'f');
             INSERT INTO symbols (blob_id, name_id, kind, line, col, end_line, start_line)
               VALUES (1, 1, 0, 1, 0, 1, 1);
             INSERT INTO refs VALUES (1, 1, x'010103010000');
             INSERT INTO ref_names VALUES (1, 1);",
        )
        .expect("insert extraction");
        let mut trigrams = Pending::new(&conn).expect("pending");
        let mut refs = refs::Pending::default();
        assert_eq!(
            delete_orphans(&conn, &mut trigrams, &mut refs, [1]).expect("delete"),
            1
        );
        trigrams.finish(&conn).expect("finish");
        refs.finish(&conn).expect("finish");
        let left: i64 = conn
            .query_row(
                "SELECT (SELECT count(*) FROM symbols) + (SELECT count(*) FROM refs)
                   + (SELECT count(*) FROM ref_names)",
                [],
                |row| row.get(0),
            )
            .expect("count");
        assert_eq!(left, 0);
    }

    #[test]
    fn a_blob_with_refs_cannot_be_deleted_before_them() {
        let dir = tempfile::tempdir().expect("temp dir");
        let conn = open(&dir.path().join("shard.sqlite")).expect("open shard");
        conn.execute_batch(
            "INSERT INTO blobs (id, key, lines, content) VALUES (1, x'01', 1, x'');
             INSERT INTO names VALUES (1, 'f', 'f');
             INSERT INTO refs VALUES (1, 1, x'0001020000');
             INSERT INTO ref_names VALUES (1, 1);",
        )
        .expect("insert");
        assert!(conn.execute("DELETE FROM blobs WHERE id = 1", []).is_err());
    }

    #[test]
    fn initials_follow_word_boundaries() {
        assert_eq!(initials("getUserById"), "gubi");
        assert_eq!(initials("get_user_by_id"), "gubi");
        assert_eq!(initials("HTTPServer"), "hs");
        assert_eq!(initials("DEFAULT_EXPLORER_STATE"), "des");
        assert_eq!(initials("parseHTML"), "ph");
    }
}
