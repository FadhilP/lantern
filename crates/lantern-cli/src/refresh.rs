//! The refresh pipeline of one root: inventory, diff, parallel prepare, one writer, one commit.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::io;
use std::path::{Path, PathBuf};
use std::sync::mpsc;
use std::thread;
use std::time::Instant;

use rayon::prelude::*;
use rusqlite::{Connection, TransactionBehavior};

use crate::error::{Error, ErrorKind, Result};
use crate::extract::{self, Extraction};
use crate::fsmeta::{self, Content, Stat};
use crate::home::{FOLDER_DEFAULTS, Home};
use crate::language::{FileFlags, Language};
use crate::limits::{self, MAX_PARSE_ATTEMPTS, RACY_SLACK_MS, REFRESH_ATTEMPTS};
use crate::source::{Entry, Inventory, Scan, SourceKind};
use crate::store::remove_database;
use crate::store::shard::{self, BlobIds, BlobKey, FileRow, Meta, Names, NewBlob};
use crate::store::trigram::{Pending, Trigrams};

/// Settings that decide eligibility and the refresh deadline.
#[derive(Debug, Clone)]
pub(crate) struct Settings {
    pub max_file_size: u64,
    pub timeout: Option<std::time::Duration>,
}

/// One root to refresh.
#[derive(Debug, Clone)]
pub(crate) struct Target {
    pub root: PathBuf,
    pub root_key: String,
    pub source: SourceKind,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Mode {
    /// Reread only what changed.
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
    /// Removed content can still be in the write-ahead log, because a reader held it open.
    pub checkpoint_pending: bool,
}

pub(crate) fn refresh(
    home: &Home,
    target: &Target,
    settings: &Settings,
    mode: Mode,
) -> Result<Refreshed> {
    let deadline = settings.timeout.map(|timeout| Instant::now() + timeout);
    let shard_path = home.shard_path(&target.root_key);
    let warm = match shard::stored_meta(&shard_path) {
        Err(error) if error.kind() == ErrorKind::Damaged => false,
        meta => meta?.is_some_and(|meta| meta.indexed_at.is_some()),
    };
    let deadline = deadline.filter(|_| warm);
    let lock_timeout = deadline.map(|deadline| deadline.saturating_duration_since(Instant::now()));
    let _lock = home.lock(&target.root_key, lock_timeout)?;
    // A killed rebuild leaves its staging copy of the indexed text behind.
    remove_database(&staging_path(&shard_path))?;
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
    let (mut live, generation) = match open_live(shard_path) {
        // The rebuild writes every page again, so it replaces a file SQLite cannot read.
        Err(error) if error.kind() == ErrorKind::Damaged => {
            remove_database(shard_path)?;
            open_live(shard_path)?
        }
        opened => opened?,
    };
    let staging = staging_path(shard_path);
    let staged = refresh_shard(
        home,
        target,
        settings,
        &staging,
        deadline,
        Some(generation + 1),
    )
    .and_then(|refreshed| {
        // The backup API replaces the live pages in one transaction; WAL readers keep their snapshot.
        live.restore(
            rusqlite::MAIN_DB,
            &staging,
            None::<fn(rusqlite::backup::Progress)>,
        )?;
        Ok(refreshed)
    });
    // Also on failure: the staging file holds a copy of the indexed text.
    let removed = remove_database(&staging);
    let mut refreshed = staged?;
    removed?;
    refreshed.checkpoint_pending = !checkpoint(&live)?;
    Ok(refreshed)
}

fn open_live(shard_path: &Path) -> Result<(Connection, i64)> {
    let live = shard::open(shard_path)?;
    let generation = shard::meta(&live)?.map_or(0, |meta| meta.generation);
    Ok((live, generation))
}

fn staging_path(shard_path: &Path) -> PathBuf {
    shard_path.with_extension("rebuild")
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
    if forced_generation.is_some() {
        // A rebuild writes a staging file that nothing reads and a failure deletes, so it needs
        // no WAL and no sync; the memory journal still allows a rollback.
        conn.execute_batch("PRAGMA journal_mode = MEMORY; PRAGMA synchronous = OFF")?;
    }
    let meta = shard::meta(&conn)?;
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
    let mut committed = job.attempt_until_stable(&mut conn)?;
    let was_pending = meta.as_ref().is_some_and(|meta| meta.checkpoint_pending);
    // A staging file has no WAL; its checkpoint would clear the flag the live shard needs.
    let staging = forced_generation.is_some();
    if !staging && (committed.secure || was_pending) {
        committed.refreshed.checkpoint_pending = !checkpoint(&conn)?;
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

impl Committed {
    fn unchanged() -> Self {
        Self {
            refreshed: Refreshed {
                counts: RefreshCounts::default(),
                checkpoint_pending: false,
            },
            secure: false,
            results_changed: false,
        }
    }
}

struct Job<'job> {
    scan: &'job Scan<'job>,
    target: &'job Target,
    settings: &'job Settings,
    meta: Option<&'job Meta>,
    forced_generation: Option<i64>,
}

/// What one attempt must write, decided before anything is read.
struct Work<'work> {
    existing: &'work HashMap<String, FileRow>,
    /// Trusted rows whose blob a moved file takes without a read.
    reuse: HashMap<ReuseKey, Option<i64>>,
    diff: Diff<'work>,
    /// Blobs whose last parse timed out.
    retry: Vec<i64>,
    reextract: bool,
    secure: bool,
}

struct Applied {
    counts: RefreshCounts,
    results_changed: bool,
    secure: bool,
    /// The scan after the last read; `None` when nothing changed.
    rescan: Option<Inventory>,
}

impl Applied {
    fn add(&mut self, more: Self) {
        self.counts += more.counts;
        self.results_changed |= more.results_changed;
        self.secure |= more.secure;
        self.rescan = more.rescan;
    }
}

/// The inventory against the stored rows.
#[derive(Debug)]
struct Diff<'diff> {
    /// New paths, and paths whose row cannot be trusted without a read.
    candidates: Vec<(&'diff String, &'diff Entry)>,
    /// Trusted rows whose flags changed.
    flag_updates: Vec<(&'diff String, FileRow)>,
    removed: Vec<(&'diff String, &'diff FileRow)>,
}

impl Diff<'_> {
    fn is_empty(&self) -> bool {
        self.candidates.is_empty() && self.flag_updates.is_empty() && self.removed.is_empty()
    }
}

fn diff<'diff>(
    inventory: &'diff BTreeMap<String, Entry>,
    existing: &'diff HashMap<String, FileRow>,
    trusted: impl Fn(&str, &FileRow) -> bool,
) -> Diff<'diff> {
    let mut candidates = Vec::new();
    let mut flag_updates = Vec::new();
    for (path, entry) in inventory {
        let unchanged = existing
            .get(path)
            .filter(|row| row.stat == entry.stat && trusted(path, row));
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
    let removed = existing
        .iter()
        .filter(|(path, _)| !inventory.contains_key(*path))
        .collect();
    Diff {
        candidates,
        flag_updates,
        removed,
    }
}

/// An unreadable file is read again at every refresh, so it is never trusted.
fn readable(row: &FileRow) -> bool {
    row.flags & FileFlags::UNREADABLE == 0
}

/// Git's racy rule. A file changed at or after the last scan can change again and keep its
/// stat, so a row is trusted without a read only when its file times are older than that scan.
fn settled(stat: &Stat, last_scan: Option<i64>) -> bool {
    last_scan.is_some_and(|scan| stat.last_change_ms() < scan - RACY_SLACK_MS)
}

/// Re-extraction keeps each blob's language, so files that a new binary detects differently
/// are read again under their new language.
fn relabeled_paths(conn: &Connection) -> Result<HashSet<String>> {
    Ok(shard::file_languages(conn)?
        .into_iter()
        .filter(|(path, stored)| {
            Language::from_file_name(file_name(path)).map(Language::name) != stored.as_deref()
        })
        .map(|(path, _)| path)
        .collect())
}

/// What preparing one candidate produced.
enum Prepared {
    /// Content already stored, or not text (`None`).
    Known {
        blob_id: Option<i64>,
        reused: bool,
    },
    New(Box<NewBlob>),
    Unreadable,
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
        let scanned_at = limits::now_ms();
        let inventory = self.scan.inventory(self.target.source)?;
        let policy = self.policy(&inventory);
        let extractor = extract::extractor_version();
        let policy_changed = self
            .meta
            .is_none_or(|meta| meta.policy != policy || meta.source != self.target.source);
        let reextract = self.meta.is_some_and(|meta| meta.extractor != extractor);
        let relabeled = if reextract {
            relabeled_paths(conn)?
        } else {
            HashSet::new()
        };
        let existing = shard::files(conn)?;
        let last_scan = self.meta.map(|meta| meta.scanned_at);
        let trusted = |path: &str, row: &FileRow| {
            readable(row)
                && !policy_changed
                && !relabeled.contains(path)
                && settled(&row.stat, last_scan)
        };
        let diff = diff(&inventory.files, &existing, trusted);

        let root = self.target.root.to_string_lossy().into_owned();
        let meta_current = self.meta_current(&policy, &extractor, &root, &inventory);
        // A timeout depends on load, so a timed-out blob is parsed again, a few times at most.
        let retry = if reextract {
            Vec::new()
        } else {
            shard::timed_out_blobs(conn, MAX_PARSE_ATTEMPTS)?
        };
        let nothing_to_do =
            diff.is_empty() && retry.is_empty() && meta_current && self.forced_generation.is_none();
        if nothing_to_do {
            return Ok(Committed::unchanged());
        }

        // Removed and re-policied content must not stay readable in the trigram index or the WAL.
        let secure = !diff.removed.is_empty() || (policy_changed && self.meta.is_some());
        let work = Work {
            existing: &existing,
            reuse: existing
                .iter()
                .filter(|(path, row)| trusted(path, row))
                .map(|(path, row)| (reuse_key(path, &row.stat), row.blob_id))
                .collect(),
            diff,
            retry,
            reextract,
            secure,
        };
        // A new shard has only rows that this attempt writes, so it skips their checks.
        conn.pragma_update(None, "foreign_keys", self.meta.is_some())?;
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let mut applied = self.apply(&tx, work)?;
        let inventory = self.catch_up(&tx, inventory, &mut applied)?;
        let Applied {
            counts,
            results_changed,
            secure,
            ..
        } = applied;

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
        let indexed_at = match self.meta.and_then(|meta| meta.indexed_at) {
            Some(at) if !results_changed => at,
            _ => scanned_at,
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
                scanned_at,
                // A commit that removes nothing must not clear a checkpoint that is still owed.
                // A rebuild's meta reaches the live shard with its pages, whose WAL still holds
                // the old pages.
                checkpoint_pending: secure
                    || self.forced_generation.is_some()
                    || self.meta.is_some_and(|meta| meta.checkpoint_pending),
            },
        )?;
        tx.commit()?;
        Ok(Committed {
            refreshed: Refreshed {
                counts,
                checkpoint_pending: false,
            },
            secure,
            results_changed,
        })
    }

    /// Applies the files that changed while the attempt read them, until a scan after the last
    /// read matches what is written. Returns that scan.
    fn catch_up(
        &self,
        tx: &Connection,
        mut inventory: Inventory,
        applied: &mut Applied,
    ) -> Result<Inventory> {
        for round in 0..=REFRESH_ATTEMPTS {
            let Some(again) = applied.rescan.take() else {
                return Ok(inventory);
            };
            if again.same_as(&inventory) {
                return Ok(again);
            }
            if again.rules_digest != inventory.rules_digest {
                return Err(Error::temporary("ignore rules changed during the refresh"));
            }
            if round == REFRESH_ATTEMPTS {
                break;
            }
            let existing = shard::files(tx)?;
            // Every row now holds the stat of the scan before; only a changed stat needs a read.
            let diff = diff(&again.files, &existing, |_, row| readable(row));
            let work = Work {
                existing: &existing,
                reuse: HashMap::new(),
                secure: !diff.removed.is_empty(),
                diff,
                retry: Vec::new(),
                reextract: false,
            };
            applied.add(self.apply(tx, work)?);
            inventory = again;
        }
        Err(Error::temporary("files kept changing during the refresh"))
    }

    fn meta_current(
        &self,
        policy: &str,
        extractor: &str,
        root: &str,
        inventory: &Inventory,
    ) -> bool {
        self.meta.is_some_and(|meta| {
            meta.policy == policy
                && meta.extractor == extractor
                && meta.source == self.target.source
                && meta.root == root
                && meta.head == inventory.head
                && meta.branch == inventory.branch
        })
    }

    /// Writes the blobs and file rows of one attempt into the open transaction.
    fn apply(&self, tx: &Connection, work: Work<'_>) -> Result<Applied> {
        let Work {
            existing,
            reuse,
            diff,
            retry,
            reextract,
            secure,
        } = work;
        let stored_keys = shard::blob_keys(tx)?;
        let mut inserted: HashMap<BlobKey, i64> = HashMap::new();
        let mut names = Names::default();
        let mut ids = BlobIds::load(tx)?;
        let mut trigrams = Pending::new(tx)?;
        let mut counts = RefreshCounts::default();
        let mut results_changed = reextract
            || !retry.is_empty()
            || !diff.removed.is_empty()
            || !diff.flag_updates.is_empty();
        if reextract {
            reextract_all(tx, &mut names, &mut counts)?;
        } else {
            reextract_blobs(tx, &mut names, &mut counts, &retry)?;
        }

        let mut rows: Vec<(&String, FileRow)> = Vec::with_capacity(diff.candidates.len());
        let mut touched: Vec<i64> = Vec::new();
        let prepare =
            |(path, entry): &(&String, &Entry)| self.prepare(path, entry, &reuse, &stored_keys);
        prepare_all(&diff.candidates, prepare, |index, prepared| {
            let (path, entry) = diff.candidates[index];
            let mut flags = entry.flags.0;
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
                        let id =
                            shard::insert_blob(tx, &mut ids, &mut names, &mut trigrams, &blob)?;
                        inserted.insert(blob.key, id);
                        Some(id)
                    }
                }
                Prepared::Unreadable => {
                    flags |= FileFlags::UNREADABLE;
                    None
                }
            };
            let old = existing.get(path);
            if old.is_none_or(|old| old.blob_id != blob_id || old.flags != flags) {
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
                    flags,
                    stat: entry.stat,
                },
            ));
            Ok(())
        })?;
        shard::create_indexes(tx)?;

        thread::scope(|scope| {
            // Every read is done, so the scan that checks them runs beside the writes.
            let rescan =
                results_changed.then(|| scope.spawn(|| self.scan.inventory(self.target.source)));
            for (path, row) in rows.iter().chain(&diff.flag_updates) {
                shard::upsert_file(tx, path, row)?;
            }
            for (path, row) in &diff.removed {
                shard::delete_file(tx, path)?;
                touched.extend(row.blob_id);
                counts.removed += 1;
            }
            let orphans = shard::delete_orphans(tx, &mut trigrams, touched)?;
            trigrams.finish(tx)?;
            if secure && orphans > 0 {
                shard::delete_unused_names(tx)?;
            }
            let rescan = rescan
                .map(|handle| {
                    handle
                        .join()
                        .unwrap_or_else(|panic| std::panic::resume_unwind(panic))
                })
                .transpose()?;
            Ok(Applied {
                counts,
                results_changed,
                secure,
                rescan,
            })
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
        if let Some(&blob_id) = reuse.get(&reuse_key(path, &entry.stat)) {
            return Ok(Prepared::Known {
                blob_id,
                reused: true,
            });
        }
        let full_path = self.target.root.join(path);
        // A directory listing can lag behind a file, so the read is checked against the file.
        let current = match fsmeta::lstat(&full_path) {
            Ok(stat) => stat,
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                return Err(Error::temporary(format!(
                    "{path} vanished during the refresh"
                )));
            }
            Err(error) if fsmeta::is_exhausted(&error) => {
                return Err(Error::io(&full_path, &error));
            }
            Err(_) => return Ok(Prepared::Unreadable),
        };
        let bytes = match fsmeta::read_stable(&full_path, &current, self.settings.max_file_size)? {
            Content::Text(bytes) => bytes,
            Content::Unreadable => return Ok(Prepared::Unreadable),
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
            language, &bytes, &text, key,
        )?)))
    }
}

/// An unchanged file seen at a new path: its stat and file name.
type ReuseKey = (Stat, String);

/// The language depends only on the file name and the content, so a move that keeps the file
/// name keeps the blob; a rename such as `tool` to `tool.py` reads the file again.
fn reuse_key(path: &str, stat: &Stat) -> ReuseKey {
    (*stat, file_name(path).to_owned())
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
/// Returns false when a reader blocked it; `meta.checkpoint_pending` then stays set, and the
/// next refresh tries again.
fn checkpoint(conn: &Connection) -> Result<bool> {
    let busy: i64 = conn.query_row("PRAGMA wal_checkpoint(TRUNCATE)", [], |row| row.get(0))?;
    if busy != 0 {
        return Ok(false);
    }
    conn.execute("UPDATE meta SET checkpoint_pending = 0", [])?;
    Ok(true)
}

fn new_blob(language: Option<Language>, bytes: &[u8], text: &str, key: BlobKey) -> Result<NewBlob> {
    let extraction = language.map_or_else(Extraction::default, |language| {
        extract::extract(language, text)
    });
    let compressed = zstd::encode_all(bytes, 3)
        .map_err(|error| Error::failure(format!("compression failed: {error}")))?;
    Ok(NewBlob {
        key,
        language: language.map(Language::name),
        lines: i64::try_from(text.lines().count()).unwrap_or(i64::MAX),
        compressed,
        trigrams: Trigrams::of(text),
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

const REEXTRACT_BATCH: usize = 256;

/// Re-extracts every stored blob after an extractor change, without reading the disk.
fn reextract_all(conn: &Connection, names: &mut Names, counts: &mut RefreshCounts) -> Result<()> {
    let ids: Vec<i64> = conn
        .prepare("SELECT id FROM blobs ORDER BY id")?
        .query_map([], |row| row.get(0))?
        .collect::<rusqlite::Result<_>>()?;
    reextract_blobs(conn, names, counts, &ids)
}

/// Re-extracts the given blobs from their stored content and language.
fn reextract_blobs(
    conn: &Connection,
    names: &mut Names,
    counts: &mut RefreshCounts,
    ids: &[i64],
) -> Result<()> {
    for batch in ids.chunks(REEXTRACT_BATCH) {
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::language::FileFlags;

    #[test]
    fn a_file_changed_near_the_last_scan_is_read_again() {
        let dir = tempfile::tempdir().expect("temp dir");
        let path = dir.path().join("a.ts");
        std::fs::write(&path, "export const a = 1;\n").expect("write file");
        let stat = fsmeta::lstat(&path).expect("stat file");
        let now = limits::now_ms();
        assert!(
            (stat.last_change_ms() - now).abs() < 60_000,
            "file time {} is not near the clock {now}",
            stat.last_change_ms()
        );

        let entry = Entry {
            stat,
            flags: FileFlags(0),
        };
        let inventory = BTreeMap::from([("a.ts".to_owned(), entry)]);
        let candidates = |stored: Stat, last_scan: i64| {
            let row = FileRow {
                blob_id: Some(1),
                flags: 0,
                stat: stored,
            };
            let existing = HashMap::from([("a.ts".to_owned(), row)]);
            diff(&inventory, &existing, |_, row| {
                settled(&row.stat, Some(last_scan))
            })
            .candidates
            .len()
        };
        assert_eq!(candidates(stat, now), 1, "written at the scan time");
        assert_eq!(candidates(stat, now + 60_000), 0, "written long before");
        let resized = Stat {
            size: stat.size + 1,
            ..stat
        };
        assert_eq!(candidates(resized, now + 60_000), 1, "stat differs");
    }
}
