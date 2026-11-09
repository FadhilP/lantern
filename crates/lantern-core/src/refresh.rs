//! The refresh pipeline of one shard: inventory, diff, parallel prepare, one writer, one commit.

use std::collections::{HashMap, HashSet};
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::mpsc;
use std::thread;
use std::time::Instant;

use rayon::prelude::*;
use rusqlite::{Connection, TransactionBehavior};

use crate::error::{Error, ErrorKind, Result};
use crate::extract::{self, Extraction};
use crate::fsmeta::{self, Content};
use crate::home::{FOLDER_DEFAULTS, Home};
use crate::language::Language;
use crate::limits::{self, MAX_PARSE_ATTEMPTS, REFRESH_ATTEMPTS};
use crate::source::{Entry, Inventory, Scan, SourceKind};
use crate::store::shard::{self, BlobKey, FileRow, Meta, Names, NewBlob};

/// Settings that decide eligibility and freshness.
#[derive(Debug, Clone)]
pub(crate) struct Settings {
    pub verify_interval_ms: i64,
    pub max_file_size: u64,
    pub timeout: Option<std::time::Duration>,
}

/// One physical root to refresh.
#[derive(Debug, Clone)]
pub(crate) struct Target {
    pub root: PathBuf,
    pub root_key: String,
    pub source: SourceKind,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Mode {
    Incremental,
    /// Build a new shard from the disk and swap it in.
    Rebuild,
}

/// Counters of one shard refresh.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, serde::Serialize)]
pub struct RefreshCounts {
    /// Paths whose content or flags changed, including new paths.
    pub changed: u64,
    pub removed: u64,
    /// Candidates whose content was already stored.
    pub reused: u64,
    pub parse_timeouts: u64,
}

impl std::ops::AddAssign for RefreshCounts {
    fn add_assign(&mut self, other: Self) {
        self.changed += other.changed;
        self.removed += other.removed;
        self.reused += other.reused;
        self.parse_timeouts += other.parse_timeouts;
    }
}

#[derive(Debug, Clone)]
pub(crate) struct Refreshed {
    pub counts: RefreshCounts,
    pub gitlinks: Vec<String>,
}

pub(crate) fn refresh(
    home: &Home,
    target: &Target,
    settings: &Settings,
    mode: Mode,
    deadline: Option<Instant>,
) -> Result<Refreshed> {
    let shard_path = home.shard_path(&target.root_key);
    let warm = shard::open_existing(&shard_path)?
        .map(|conn| shard::meta(&conn))
        .transpose()?
        .flatten()
        .is_some_and(|meta| meta.indexed_at.is_some());
    let deadline = deadline.filter(|_| warm);
    let lock_timeout = deadline.map(|deadline| deadline.saturating_duration_since(Instant::now()));
    let _lock = home.lock(&target.root_key, lock_timeout)?;
    // A cold index and a rebuild read the whole tree, so they have no deadline.
    let deadline = deadline.filter(|_| mode == Mode::Incremental);
    match mode {
        Mode::Incremental => refresh_shard(home, target, settings, &shard_path, deadline, None),
        Mode::Rebuild => rebuild(home, target, settings, &shard_path, deadline),
    }
}

fn rebuild(
    home: &Home,
    target: &Target,
    settings: &Settings,
    shard_path: &Path,
    deadline: Option<Instant>,
) -> Result<Refreshed> {
    let mut live = shard::open(shard_path)?;
    let generation = shard::meta(&live)?.map_or(0, |meta| meta.generation);
    let staging = shard_path.with_extension("rebuild");
    remove_database(&staging)?;
    let refreshed = refresh_shard(
        home,
        target,
        settings,
        &staging,
        deadline,
        Some(generation + 1),
    )?;
    // The backup API replaces the live pages in one transaction; WAL readers keep their snapshot.
    live.restore(
        rusqlite::MAIN_DB,
        &staging,
        None::<fn(rusqlite::backup::Progress)>,
    )?;
    remove_database(&staging)?;
    // The live WAL still holds the old pages, removed content included.
    live.execute("UPDATE meta SET checkpoint_pending = 1", [])?;
    checkpoint(&live)?;
    Ok(refreshed)
}

/// Deletes a database file with its WAL and shared-memory files.
pub(crate) fn remove_database(path: &Path) -> Result<()> {
    for suffix in ["", "-wal", "-shm"] {
        let mut name = path.as_os_str().to_owned();
        name.push(suffix);
        let file = PathBuf::from(name);
        match fs::remove_file(&file) {
            Ok(()) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(Error::io(&file, &error)),
        }
    }
    Ok(())
}

fn refresh_shard(
    home: &Home,
    target: &Target,
    settings: &Settings,
    shard_path: &Path,
    deadline: Option<Instant>,
    forced_generation: Option<i64>,
) -> Result<Refreshed> {
    let mut conn = shard::open(shard_path)?;
    let meta = shard::meta(&conn)?;
    if meta.as_ref().is_some_and(|meta| meta.checkpoint_pending) {
        checkpoint(&conn)?;
    }
    let defaults_file = home.defaults_path();
    let scan = Scan {
        root: &target.root,
        data_dir: home.dir(),
        defaults_file: &defaults_file,
        deadline,
    };
    let job = Job {
        scan: &scan,
        target,
        settings,
        meta: meta.as_ref(),
        forced_generation,
    };
    let committed = job.attempt_until_stable(&mut conn)?;
    // After the commit nothing is retried: a second attempt would find no work and hide the counts.
    if committed.secure {
        checkpoint(&conn)?;
    }
    if committed.results_changed {
        conn.execute_batch("PRAGMA optimize")?;
    }
    Ok(committed.refreshed)
}

/// A committed attempt and the follow-up work it needs.
struct Committed {
    refreshed: Refreshed,
    /// Removed content may be in the WAL until a checkpoint empties it.
    secure: bool,
    results_changed: bool,
}

struct Job<'job> {
    scan: &'job Scan<'job>,
    target: &'job Target,
    settings: &'job Settings,
    meta: Option<&'job Meta>,
    forced_generation: Option<i64>,
}

/// What preparing one candidate produced.
enum Prepared {
    /// Content already stored, or not text (`None`).
    Known {
        blob_id: Option<i64>,
        reused: bool,
    },
    New(Box<NewBlob>),
}

impl Job<'_> {
    /// Retries attempts that saw files change, before anything is committed.
    fn attempt_until_stable(&self, conn: &mut Connection) -> Result<Committed> {
        for attempt in 1..=REFRESH_ATTEMPTS {
            match self.attempt(conn) {
                Err(error)
                    if error.kind() == ErrorKind::Temporary && attempt < REFRESH_ATTEMPTS => {}
                Err(error) if error.kind() == ErrorKind::Temporary => {
                    return Err(Error::temporary(format!(
                        "{error} ({REFRESH_ATTEMPTS} attempts); try again"
                    )));
                }
                other => return other,
            }
        }
        unreachable!("the last attempt always returns")
    }

    fn attempt(&self, conn: &mut Connection) -> Result<Committed> {
        let inventory = self.scan.inventory(self.target.source)?;
        let policy = self.policy(&inventory);
        let extractor = extract::extractor_version();
        let policy_changed = self
            .meta
            .is_none_or(|meta| meta.policy != policy || meta.source != self.target.source);
        let reextract = self.meta.is_some_and(|meta| meta.extractor != extractor);
        // Re-extraction keeps each blob's language, so files that a new binary detects
        // differently are read again under their new language.
        let relabeled: HashSet<String> = if reextract {
            shard::file_languages(conn)?
                .into_iter()
                .filter(|(path, stored)| {
                    Language::from_file_name(file_name(path)).map(Language::name)
                        != stored.as_deref()
                })
                .map(|(path, _)| path)
                .collect()
        } else {
            HashSet::new()
        };
        let existing = shard::files(conn)?;
        let now = limits::now_ms();
        let interval = self.settings.verify_interval_ms;
        let fresh = |row: &FileRow| row.verified_at <= now && now - row.verified_at < interval;

        let mut candidates: Vec<(&String, &Entry)> = Vec::new();
        let mut flag_updates: Vec<(&String, FileRow)> = Vec::new();
        for (path, entry) in &inventory.files {
            let unchanged = existing.get(path).filter(|row| {
                !policy_changed
                    && !relabeled.contains(path)
                    && fresh(row)
                    && row.identity == entry.stat.identity()
                    && row.fingerprint == entry.stat.fingerprint()
            });
            match unchanged {
                Some(row) if row.flags != entry.flags.0 => flag_updates.push((
                    path,
                    FileRow {
                        flags: entry.flags.0,
                        ..row.clone()
                    },
                )),
                Some(_) => {}
                None => candidates.push((path, entry)),
            }
        }
        let removed: Vec<(&String, &FileRow)> = existing
            .iter()
            .filter(|(path, _)| !inventory.files.contains_key(*path))
            .collect();

        let root = self.target.root.to_string_lossy().into_owned();
        let meta_current = self.meta.is_some_and(|meta| {
            meta.policy == policy
                && meta.extractor == extractor
                && meta.source == self.target.source
                && meta.root == root
                && meta.head == inventory.head
                && meta.branch == inventory.branch
        });
        // A timeout depends on load, so a timed-out blob is parsed again, a few times at most.
        let retry = if reextract {
            Vec::new()
        } else {
            shard::timed_out_blobs(conn, MAX_PARSE_ATTEMPTS)?
        };
        let nothing_to_do = candidates.is_empty()
            && retry.is_empty()
            && removed.is_empty()
            && flag_updates.is_empty()
            && meta_current
            && self.forced_generation.is_none();
        if nothing_to_do {
            return Ok(Committed {
                refreshed: Refreshed {
                    counts: RefreshCounts::default(),
                    gitlinks: inventory.gitlinks,
                },
                secure: false,
                results_changed: false,
            });
        }

        // Removed and re-policied content must not stay readable in the FTS index or the WAL.
        let secure = !removed.is_empty() || (policy_changed && self.meta.is_some());
        let reuse: HashMap<ReuseKey, Option<i64>> = if policy_changed {
            HashMap::new()
        } else {
            existing
                .iter()
                .filter(|(path, row)| fresh(row) && !relabeled.contains(*path))
                .map(|(path, row)| {
                    (
                        reuse_key(path, &row.identity, &row.fingerprint),
                        row.blob_id,
                    )
                })
                .collect()
        };
        let stored_keys = shard::blob_keys(conn)?;
        let mut inserted: HashMap<BlobKey, i64> = HashMap::new();
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let mut names = Names::default();
        let mut counts = RefreshCounts::default();
        let mut results_changed =
            reextract || !retry.is_empty() || !removed.is_empty() || !flag_updates.is_empty();
        if reextract {
            reextract_all(&tx, &mut names, &mut counts)?;
        } else {
            reextract_blobs(&tx, &mut names, &mut counts, &retry)?;
        }

        let mut rows: Vec<(&String, FileRow)> = Vec::with_capacity(candidates.len());
        let mut touched: Vec<i64> = Vec::new();
        let prepare =
            |(path, entry): &(&String, &Entry)| self.prepare(path, entry, &reuse, &stored_keys);
        let prepared = prepare_all(&candidates, prepare, |index, prepared| {
            let (path, entry) = candidates[index];
            let blob_id = match prepared {
                Prepared::Known { blob_id, reused } => {
                    counts.reused += u64::from(reused);
                    blob_id
                }
                Prepared::New(blob) => {
                    counts.parse_timeouts += u64::from(blob.extraction.timed_out);
                    if let Some(&id) = inserted.get(&blob.key) {
                        Some(id)
                    } else {
                        let id = shard::insert_blob(&tx, &mut names, &blob)?;
                        inserted.insert(blob.key, id);
                        Some(id)
                    }
                }
            };
            let old = existing.get(path);
            if old.is_none_or(|old| old.blob_id != blob_id || old.flags != entry.flags.0) {
                counts.changed += 1;
                results_changed = true;
            }
            if let Some(old_blob) = old
                .and_then(|old| old.blob_id)
                .filter(|&old| Some(old) != blob_id)
            {
                touched.push(old_blob);
            }
            rows.push((
                path,
                FileRow {
                    blob_id,
                    size: entry.stat.size as i64,
                    flags: entry.flags.0,
                    identity: entry.stat.identity(),
                    fingerprint: entry.stat.fingerprint(),
                    verified_at: now,
                },
            ));
            Ok(())
        });
        prepared?;

        if results_changed {
            let again = self.scan.inventory(self.target.source)?;
            if !again.same_as(&inventory) {
                return Err(Error::temporary("files changed during the refresh"));
            }
        }
        for (path, row) in rows.iter().chain(&flag_updates) {
            shard::upsert_file(&tx, path, row)?;
        }
        for (path, row) in &removed {
            shard::delete_file(&tx, path)?;
            touched.extend(row.blob_id);
            counts.removed += 1;
        }
        let orphans = shard::delete_orphans(&tx, touched)?;
        if secure && orphans > 0 {
            // Merge FTS segments so deleted postings are dropped, not only marked.
            tx.execute("INSERT INTO blobs_fts (blobs_fts) VALUES ('optimize')", [])?;
            shard::delete_unused_names(&tx)?;
        }

        let committed_generation = shard::meta(&tx)?.map_or(0, |meta| meta.generation);
        let expected_generation = self.meta.map_or(0, |meta| meta.generation);
        if committed_generation != expected_generation {
            return Err(Error::temporary("another refresh committed first"));
        }
        let generation = match self.forced_generation {
            Some(generation) => generation,
            None if results_changed => expected_generation + 1,
            None => expected_generation,
        };
        let previous_indexed_at = self.meta.and_then(|meta| meta.indexed_at);
        let indexed_at = match previous_indexed_at {
            Some(at) if !results_changed => at,
            _ => now,
        };
        shard::write_meta(
            &tx,
            &Meta {
                root,
                root_key: self.target.root_key.clone(),
                source: self.target.source,
                head: inventory.head.clone(),
                branch: inventory.branch.clone(),
                policy,
                extractor,
                indexed_at: Some(indexed_at),
                generation,
                checkpoint_pending: secure,
            },
        )?;
        tx.commit()?;
        Ok(Committed {
            refreshed: Refreshed {
                counts,
                gitlinks: inventory.gitlinks,
            },
            secure,
            results_changed,
        })
    }

    /// Everything that changes which files are eligible; a change forces a full reconcile.
    fn policy(&self, inventory: &Inventory) -> String {
        let mut hasher = blake3::Hasher::new();
        hasher.update(self.target.source.as_str().as_bytes());
        hasher.update(&self.settings.max_file_size.to_le_bytes());
        hasher.update(FOLDER_DEFAULTS.as_bytes());
        hasher.update(inventory.rules_digest.as_bytes());
        hasher.finalize().to_hex().to_string()
    }

    fn prepare(
        &self,
        path: &str,
        entry: &Entry,
        reuse: &HashMap<ReuseKey, Option<i64>>,
        keys: &HashMap<BlobKey, i64>,
    ) -> Result<Prepared> {
        self.scan.check_deadline()?;
        let key = reuse_key(path, &entry.stat.identity(), &entry.stat.fingerprint());
        if let Some(&blob_id) = reuse.get(&key) {
            return Ok(Prepared::Known {
                blob_id,
                reused: true,
            });
        }
        let full_path = self.target.root.join(path);
        let bytes = match fsmeta::read_stable(&full_path, &entry.stat, self.settings.max_file_size)?
        {
            Content::Text(bytes) => bytes,
            Content::Binary | Content::TooLarge => {
                return Ok(Prepared::Known {
                    blob_id: None,
                    reused: false,
                });
            }
        };
        let text = String::from_utf8_lossy(&bytes).into_owned();
        let language = Language::detect(path, &text);
        let key = blob_key(language, &bytes);
        if let Some(&blob_id) = keys.get(&key) {
            return Ok(Prepared::Known {
                blob_id: Some(blob_id),
                reused: true,
            });
        }
        Ok(Prepared::New(Box::new(new_blob(
            language, &bytes, text, key,
        )?)))
    }
}

/// An unchanged file seen at a new path: identity, fingerprint, and file name.
type ReuseKey = (String, String, String);

/// The language depends only on the file name and the content, so a move that keeps the file
/// name keeps the blob; a rename such as `tool` to `tool.py` reads the file again.
fn reuse_key(path: &str, identity: &str, fingerprint: &str) -> ReuseKey {
    (
        identity.to_owned(),
        fingerprint.to_owned(),
        file_name(path).to_owned(),
    )
}

fn file_name(path: &str) -> &str {
    path.rsplit('/').next().unwrap_or(path)
}

/// Symbols depend on the language, so the same bytes under two languages are two blobs.
fn blob_key(language: Option<Language>, bytes: &[u8]) -> BlobKey {
    let mut hasher = blake3::Hasher::new();
    hasher.update(language.map_or("", Language::name).as_bytes());
    hasher.update(&[0]);
    hasher.update(bytes);
    *hasher.finalize().as_bytes()
}

/// Copies the WAL into the shard and empties it, so removed content does not stay there.
fn checkpoint(conn: &Connection) -> Result<()> {
    let busy: i64 = conn.query_row("PRAGMA wal_checkpoint(TRUNCATE)", [], |row| row.get(0))?;
    if busy != 0 {
        return Err(Error::temporary(
            "the index was updated, but a query held it open, so removed content can still be in the write-ahead log; run `lantern index` again",
        ));
    }
    conn.execute("UPDATE meta SET checkpoint_pending = 0", [])?;
    Ok(())
}

fn new_blob(
    language: Option<Language>,
    bytes: &[u8],
    text: String,
    key: BlobKey,
) -> Result<NewBlob> {
    let extraction = language.map_or_else(Extraction::default, |language| {
        extract::extract(language, &text)
    });
    let compressed = zstd::encode_all(bytes, 3)
        .map_err(|error| Error::failure(format!("compression failed: {error}")))?;
    Ok(NewBlob {
        key,
        language: language.map(Language::name),
        lines: text.lines().count() as i64,
        compressed,
        text,
        extraction,
    })
}

/// Bounded channel between the preparing pool and this thread, which owns the transaction.
const CHANNEL_CAPACITY: usize = 64;

fn prepare_all<Item, P, W>(items: &[Item], prepare: P, mut write: W) -> Result<()>
where
    Item: Sync,
    P: Fn(&Item) -> Result<Prepared> + Sync,
    W: FnMut(usize, Prepared) -> Result<()>,
{
    thread::scope(|scope| {
        let (sender, receiver) = mpsc::sync_channel::<(usize, Result<Prepared>)>(CHANNEL_CAPACITY);
        let prepare = &prepare;
        scope.spawn(move || {
            let _ =
                items
                    .par_iter()
                    .enumerate()
                    .try_for_each_with(sender, |sender, (index, item)| {
                        let prepared = prepare(item);
                        let failed = prepared.is_err();
                        sender.send((index, prepared)).map_err(|_| ())?;
                        if failed { Err(()) } else { Ok(()) }
                    });
        });
        // Returning early drops the receiver, which stops the pool at its next send.
        for (index, prepared) in receiver {
            write(index, prepared?)?;
        }
        Ok(())
    })
}

const REEXTRACT_BATCH: i64 = 256;

/// Re-extracts every stored blob after an extractor change, without reading the disk.
fn reextract_all(conn: &Connection, names: &mut Names, counts: &mut RefreshCounts) -> Result<()> {
    let mut after = 0;
    loop {
        let ids: Vec<i64> = conn
            .prepare_cached("SELECT id FROM blobs WHERE id > ?1 ORDER BY id LIMIT ?2")?
            .query_map([after, REEXTRACT_BATCH], |row| row.get(0))?
            .collect::<rusqlite::Result<_>>()?;
        let Some(&last) = ids.last() else {
            return Ok(());
        };
        after = last;
        reextract_blobs(conn, names, counts, &ids)?;
    }
}

/// Re-extracts the given blobs from their stored content and language.
fn reextract_blobs(
    conn: &Connection,
    names: &mut Names,
    counts: &mut RefreshCounts,
    ids: &[i64],
) -> Result<()> {
    for batch in ids.chunks(REEXTRACT_BATCH as usize) {
        let stored: Vec<(i64, Vec<u8>, Option<String>)> = batch
            .iter()
            .map(|&id| {
                conn.prepare_cached("SELECT content, language FROM blobs WHERE id = ?1")?
                    .query_row([id], |row| Ok((id, row.get(0)?, row.get(1)?)))
            })
            .collect::<rusqlite::Result<_>>()?;
        let extracted: Vec<(i64, Extraction)> = stored
            .into_par_iter()
            .map(|(id, compressed, language)| {
                let Some(language) = language else {
                    return Ok((id, Extraction::default()));
                };
                let language = Language::from_name(&language).ok_or_else(|| {
                    Error::failure(format!("unknown language in shard: {language}"))
                })?;
                let text = shard::decompress(&compressed)?;
                Ok((id, extract::extract(language, &text)))
            })
            .collect::<Result<_>>()?;
        for (id, extraction) in extracted {
            counts.parse_timeouts += u64::from(extraction.timed_out);
            shard::replace_extraction(conn, names, id, &extraction)?;
        }
    }
    Ok(())
}
