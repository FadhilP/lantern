//! One shard per physical root: blobs, trigram index, names, symbols, references, files.

use std::collections::HashMap;
use std::io::Read;
use std::path::Path;

use rusqlite::{Connection, OptionalExtension, params};

use crate::error::{Error, Result};
use crate::extract::Extraction;
use crate::source::SourceKind;

const SCHEMA: &str = r"
CREATE TABLE meta (
  root TEXT NOT NULL, root_key TEXT NOT NULL, source TEXT NOT NULL,
  head TEXT, branch TEXT,
  policy TEXT NOT NULL,
  extractor TEXT NOT NULL,
  indexed_at INTEGER, generation INTEGER NOT NULL DEFAULT 0,
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
CREATE INDEX blobs_timeouts ON blobs(timeouts) WHERE timeouts > 0;
CREATE VIRTUAL TABLE blobs_fts USING fts5(
  content, content='', contentless_delete=1, tokenize='trigram'
);
CREATE TABLE names (
  id INTEGER PRIMARY KEY,
  name TEXT NOT NULL UNIQUE,
  initials TEXT NOT NULL
);
CREATE INDEX names_nocase ON names(name COLLATE NOCASE);
CREATE INDEX names_initials ON names(initials);
CREATE VIRTUAL TABLE names_fts USING fts5(
  name, content='names', content_rowid='id', tokenize='trigram'
);
CREATE TABLE symbols (
  id INTEGER PRIMARY KEY,
  blob_id INTEGER NOT NULL REFERENCES blobs ON DELETE CASCADE,
  name_id INTEGER NOT NULL REFERENCES names,
  kind TEXT NOT NULL,
  container TEXT,
  line INTEGER NOT NULL, col INTEGER NOT NULL, end_line INTEGER NOT NULL,
  signature TEXT NOT NULL
);
CREATE INDEX symbols_name ON symbols(name_id);
CREATE INDEX symbols_blob ON symbols(blob_id, line);
CREATE TABLE refs (
  blob_id INTEGER NOT NULL REFERENCES blobs ON DELETE CASCADE,
  name_id INTEGER NOT NULL REFERENCES names,
  kind TEXT NOT NULL,
  line INTEGER NOT NULL, col INTEGER NOT NULL,
  enclosing INTEGER REFERENCES symbols(id) ON DELETE CASCADE
);
CREATE INDEX refs_name ON refs(name_id);
CREATE INDEX refs_blob ON refs(blob_id, line);
CREATE INDEX refs_enclosing ON refs(enclosing);
-- blob_id is NULL for binary and oversized files, so they are not reread on every refresh.
CREATE TABLE files (
  path TEXT PRIMARY KEY,
  blob_id INTEGER REFERENCES blobs,
  size INTEGER NOT NULL,
  flags INTEGER NOT NULL,
  identity TEXT NOT NULL,
  fingerprint TEXT NOT NULL,
  verified_at INTEGER NOT NULL
) WITHOUT ROWID;
CREATE INDEX files_blob ON files(blob_id);
CREATE INDEX files_identity ON files(identity, fingerprint);
";

pub(crate) fn open(path: &Path) -> Result<Connection> {
    let conn = super::open(path, SCHEMA)?;
    // Freed pages are zeroed, so edited and removed content does not stay readable in them.
    conn.execute_batch("PRAGMA secure_delete = ON")?;
    Ok(conn)
}

/// Opens an existing shard read-only; `None` when the root was never indexed.
pub(crate) fn open_existing(path: &Path) -> Result<Option<Connection>> {
    if !path.exists() {
        return Ok(None);
    }
    super::open_read_only(path)
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
    /// A secure commit whose WAL was not yet emptied.
    pub checkpoint_pending: bool,
}

pub(crate) fn meta(conn: &Connection) -> Result<Option<Meta>> {
    let row = conn
        .query_row(
            "SELECT root, root_key, source, head, branch, policy, extractor, indexed_at, generation,
                    checkpoint_pending
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
                    checkpoint_pending: row.get(9)?,
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
           checkpoint_pending)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)",
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
            meta.checkpoint_pending
        ],
    )?;
    Ok(())
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct FileRow {
    pub blob_id: Option<i64>,
    pub size: i64,
    pub flags: u8,
    pub identity: String,
    pub fingerprint: String,
    pub verified_at: i64,
}

pub(crate) fn files(conn: &Connection) -> Result<HashMap<String, FileRow>> {
    let mut statement = conn.prepare(
        "SELECT path, blob_id, size, flags, identity, fingerprint, verified_at FROM files",
    )?;
    let rows = statement.query_map([], |row| {
        Ok((
            row.get::<_, String>(0)?,
            FileRow {
                blob_id: row.get(1)?,
                size: row.get(2)?,
                flags: row.get(3)?,
                identity: row.get(4)?,
                fingerprint: row.get(5)?,
                verified_at: row.get(6)?,
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
        "INSERT INTO files (path, blob_id, size, flags, identity, fingerprint, verified_at)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)
         ON CONFLICT (path) DO UPDATE SET blob_id = excluded.blob_id, size = excluded.size,
           flags = excluded.flags, identity = excluded.identity,
           fingerprint = excluded.fingerprint, verified_at = excluded.verified_at",
    )?
    .execute(params![
        path,
        row.blob_id,
        row.size,
        row.flags,
        row.identity,
        row.fingerprint,
        row.verified_at
    ])?;
    Ok(())
}

pub(crate) fn delete_file(conn: &Connection, path: &str) -> Result<()> {
    conn.prepare_cached("DELETE FROM files WHERE path = ?1")?
        .execute([path])?;
    Ok(())
}

/// Deletes every blob in `candidates` that no file references. Returns how many it deleted.
pub(crate) fn delete_orphans(
    conn: &Connection,
    candidates: impl IntoIterator<Item = i64>,
) -> Result<usize> {
    let mut used = conn.prepare_cached("SELECT 1 FROM files WHERE blob_id = ?1 LIMIT 1")?;
    let mut fts = conn.prepare_cached("DELETE FROM blobs_fts WHERE rowid = ?1")?;
    let mut blob = conn.prepare_cached("DELETE FROM blobs WHERE id = ?1")?;
    let mut deleted = 0;
    for id in candidates {
        if used.exists([id])? {
            continue;
        }
        fts.execute([id])?;
        deleted += blob.execute([id])?;
    }
    Ok(deleted)
}

/// Deletes names that no symbol or reference uses, so removed identifiers do not stay readable.
pub(crate) fn delete_unused_names(conn: &Connection) -> Result<()> {
    const UNUSED: &str = "NOT EXISTS (SELECT 1 FROM symbols s WHERE s.name_id = n.id)
        AND NOT EXISTS (SELECT 1 FROM refs r WHERE r.name_id = n.id)";
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

/// A blob ready to insert: compressed content plus its extraction.
#[derive(Debug)]
pub(crate) struct NewBlob {
    pub key: BlobKey,
    pub language: Option<&'static str>,
    pub lines: i64,
    pub compressed: Vec<u8>,
    pub text: String,
    pub extraction: Extraction,
}

pub(crate) fn insert_blob(conn: &Connection, names: &mut Names, blob: &NewBlob) -> Result<i64> {
    conn.prepare_cached(
        "INSERT INTO blobs (key, language, lines, content, timeouts) VALUES (?1, ?2, ?3, ?4, ?5)",
    )?
    .execute(params![
        blob.key.as_slice(),
        blob.language,
        blob.lines,
        blob.compressed,
        blob.extraction.timed_out
    ])?;
    let id = conn.last_insert_rowid();
    conn.prepare_cached("INSERT INTO blobs_fts (rowid, content) VALUES (?1, ?2)")?
        .execute(params![id, blob.text])?;
    insert_extraction(conn, names, id, &blob.extraction)?;
    Ok(id)
}

pub(crate) fn insert_extraction(
    conn: &Connection,
    names: &mut Names,
    blob_id: i64,
    extraction: &Extraction,
) -> Result<()> {
    let mut symbol_ids = Vec::with_capacity(extraction.symbols.len());
    for symbol in &extraction.symbols {
        let name_id = names.id(conn, &symbol.name)?;
        conn.prepare_cached(
            "INSERT INTO symbols (blob_id, name_id, kind, container, line, col, end_line, signature)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
        )?
        .execute(params![
            blob_id,
            name_id,
            symbol.kind.as_str(),
            symbol.container,
            symbol.line,
            symbol.col,
            symbol.end_line,
            symbol.signature
        ])?;
        symbol_ids.push(conn.last_insert_rowid());
    }
    for reference in &extraction.refs {
        let name_id = names.id(conn, &reference.name)?;
        let enclosing = reference.enclosing.map(|index| symbol_ids[index]);
        conn.prepare_cached(
            "INSERT INTO refs (blob_id, name_id, kind, line, col, enclosing) VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
        )?
        .execute(params![
            blob_id,
            name_id,
            reference.kind.as_str(),
            reference.line,
            reference.col,
            enclosing
        ])?;
    }
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
    blob_id: i64,
    extraction: &Extraction,
) -> Result<()> {
    delete_extraction(conn, blob_id)?;
    insert_extraction(conn, names, blob_id, extraction)?;
    conn.prepare_cached(
        "UPDATE blobs SET timeouts = CASE WHEN ?2 THEN timeouts + 1 ELSE 0 END WHERE id = ?1",
    )?
    .execute(params![blob_id, extraction.timed_out])?;
    Ok(())
}

fn delete_extraction(conn: &Connection, blob_id: i64) -> Result<()> {
    conn.prepare_cached("DELETE FROM refs WHERE blob_id = ?1")?
        .execute([blob_id])?;
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

pub(crate) fn decompress(compressed: &[u8]) -> Result<String> {
    let mut bytes = Vec::new();
    zstd::stream::read::Decoder::new(compressed)
        .and_then(|mut decoder| decoder.read_to_end(&mut bytes))
        .map_err(|error| Error::failure(format!("corrupt blob: {error}")))?;
    Ok(String::from_utf8_lossy(&bytes).into_owned())
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
}

impl std::ops::AddAssign for Counts {
    fn add_assign(&mut self, other: Self) {
        self.files += other.files;
        self.text_files += other.text_files;
        self.code_files += other.code_files;
        self.symbols += other.symbols;
        self.refs += other.refs;
        self.unparsed_files += other.unparsed_files;
    }
}

pub(crate) fn counts(conn: &Connection) -> Result<Counts> {
    Ok(conn.query_row(
        "SELECT (SELECT count(*) FROM files),
                (SELECT count(*) FROM files WHERE blob_id IS NOT NULL),
                (SELECT count(*) FROM files f JOIN blobs b ON b.id = f.blob_id WHERE b.language IS NOT NULL),
                (SELECT count(*) FROM symbols),
                (SELECT count(*) FROM refs),
                (SELECT count(*) FROM files f JOIN blobs b ON b.id = f.blob_id WHERE b.timeouts > 0)",
        [],
        |row| {
            Ok(Counts {
                files: row.get::<_, i64>(0)?.unsigned_abs(),
                text_files: row.get::<_, i64>(1)?.unsigned_abs(),
                code_files: row.get::<_, i64>(2)?.unsigned_abs(),
                symbols: row.get::<_, i64>(3)?.unsigned_abs(),
                refs: row.get::<_, i64>(4)?.unsigned_abs(),
                unparsed_files: row.get::<_, i64>(5)?.unsigned_abs(),
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
        let id = match existing {
            Some(id) => id,
            None => {
                conn.prepare_cached("INSERT INTO names (name, initials) VALUES (?1, ?2)")?
                    .execute(params![name, initials(name)])?;
                let id = conn.last_insert_rowid();
                conn.prepare_cached("INSERT INTO names_fts (rowid, name) VALUES (?1, ?2)")?
                    .execute(params![id, name])?;
                id
            }
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
    use super::initials;

    #[test]
    fn initials_follow_word_boundaries() {
        assert_eq!(initials("getUserById"), "gubi");
        assert_eq!(initials("get_user_by_id"), "gubi");
        assert_eq!(initials("HTTPServer"), "hs");
        assert_eq!(initials("DEFAULT_EXPLORER_STATE"), "des");
        assert_eq!(initials("parseHTML"), "ph");
    }
}
