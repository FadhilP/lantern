//! The entry point of every command: one data directory and its settings.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use serde::Serialize;

use crate::error::{Error, Result};
use crate::home::{Home, default_home};
use crate::limits::MAX_MAX_FILE_SIZE;
use crate::paths;
use crate::query::code::{CodeHit, CodeQuery};
use crate::query::graph::{Graph, GraphQuery};
use crate::query::outline::Outline;
use crate::query::refs::{RefHit, RefsQuery};
use crate::query::symbols::{SymbolHit, SymbolQuery};
use crate::query::{self, Freshness, Opened, Page, PageRequest};
use crate::refresh::{self, Mode, RefreshCounts, Settings};
use crate::source::SourceKind;
use crate::store::remove_database;
use crate::store::shard::{self, Counts};
use crate::workspace::{self, Resolved};

/// Settings that every command shares.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Options {
    /// The data directory; `None` uses [`default_home`].
    pub home: Option<PathBuf>,
    /// The warm refresh timeout; `None` waits without a limit.
    pub timeout: Option<Duration>,
    pub max_file_size: u64,
}

/// What `index` reports.
#[derive(Debug, Clone, Serialize)]
pub struct IndexReport {
    pub root: String,
    pub source: SourceKind,
    #[serde(flatten)]
    pub counts: Counts,
    pub indexed_at: Option<i64>,
    #[serde(flatten)]
    pub refresh: RefreshCounts,
    pub elapsed_ms: u128,
}

/// What `status` reports. It never refreshes.
#[derive(Debug, Clone, Serialize)]
pub struct Status {
    pub root: String,
    pub source: SourceKind,
    pub indexed: bool,
    pub head: Option<String>,
    pub branch: Option<String>,
    pub indexed_at: Option<i64>,
    pub generation: Option<i64>,
    #[serde(flatten)]
    pub counts: Counts,
}

/// What `prune` removed.
#[derive(Debug, Clone, Default, Serialize)]
pub struct PruneReport {
    pub removed_roots: Vec<String>,
    pub removed_files: usize,
}

#[derive(Debug, Clone)]
pub struct Lantern {
    home: Home,
    settings: Settings,
}

impl Lantern {
    /// Validates `options` and opens (or creates) the data directory.
    pub fn open(options: &Options) -> Result<Self> {
        if options.max_file_size == 0 || options.max_file_size > MAX_MAX_FILE_SIZE {
            return Err(Error::usage(format!(
                "--max-file-size must be between 1 and {MAX_MAX_FILE_SIZE} bytes"
            )));
        }
        let dir =
            options.home.clone().or_else(default_home).ok_or_else(|| {
                Error::usage("no data directory; pass --home or set LANTERN_HOME")
            })?;
        Ok(Self {
            home: Home::open(&dir)?,
            settings: Settings {
                max_file_size: options.max_file_size,
                timeout: options.timeout,
            },
        })
    }

    /// Refreshes the root at `path` and reports its totals.
    pub fn index(&self, path: &Path, mode: Mode) -> Result<IndexReport> {
        let started = Instant::now();
        let target = workspace::resolve(path, &self.home)?.target;
        let refreshed = refresh::refresh(&self.home, &target, &self.settings, mode)?;
        // Only `index` reports this; a query must not fail because another query was reading.
        if refreshed.checkpoint_pending {
            return Err(Error::temporary(
                "the index was updated, but a query held it open, so removed content can still be in the write-ahead log; run `lantern index` again",
            ));
        }
        let conn = shard::open_existing(&self.home.shard_path(&target.root_key))?
            .ok_or_else(|| Error::failure("the index was not written"))?;
        Ok(IndexReport {
            root: target.root.to_string_lossy().into_owned(),
            source: target.source,
            counts: shard::counts(&conn)?,
            indexed_at: shard::meta(&conn)?.and_then(|meta| meta.indexed_at),
            refresh: refreshed.counts,
            elapsed_ms: started.elapsed().as_millis(),
        })
    }

    /// Reports the committed state of the root at `path` without refreshing.
    pub fn status(&self, path: &Path) -> Result<Status> {
        let target = workspace::resolve(path, &self.home)?.target;
        let mut status = Status {
            root: target.root.to_string_lossy().into_owned(),
            source: target.source,
            indexed: false,
            head: None,
            branch: None,
            indexed_at: None,
            generation: None,
            counts: Counts::default(),
        };
        let Some(conn) = shard::open_existing(&self.home.shard_path(&target.root_key))? else {
            return Ok(status);
        };
        let Some(meta) = shard::meta(&conn)? else {
            return Ok(status);
        };
        status.indexed = true;
        status.head = meta.head;
        status.branch = meta.branch;
        status.indexed_at = meta.indexed_at;
        status.generation = Some(meta.generation);
        status.counts = shard::counts(&conn)?;
        Ok(status)
    }

    /// Removes shards of roots that no longer exist, and stray files in the shard directory.
    pub fn prune(&self) -> Result<PruneReport> {
        let roots_dir = self.home.roots_dir();
        let entries =
            std::fs::read_dir(&roots_dir).map_err(|error| Error::io(&roots_dir, &error))?;
        let mut files_of: BTreeMap<String, Vec<PathBuf>> = BTreeMap::new();
        for entry in entries {
            let entry = entry.map_err(|error| Error::io(&roots_dir, &error))?;
            let name = entry.file_name().to_string_lossy().into_owned();
            files_of
                .entry(Home::shard_hash(&name).to_owned())
                .or_default()
                .push(entry.path());
        }

        let mut report = PruneReport::default();
        for (hash, files) in files_of {
            // A refresh holds this lock, so its root exists and its files are in use.
            let Ok(_lock) = self.home.lock_hash(&hash, Some(Duration::ZERO)) else {
                continue;
            };
            let shard_path = roots_dir.join(format!("{hash}.sqlite"));
            let root = shard::open_existing(&shard_path)?
                .map(|conn| shard::meta(&conn))
                .transpose()?
                .flatten()
                .map(|meta| meta.root);
            let live = root.as_ref().is_some_and(|root| Path::new(root).is_dir());
            if !live {
                remove_database(&shard_path)?;
                report.removed_roots.extend(root);
            }
            for file in files {
                if is_shard_file(&file, &shard_path) {
                    continue;
                }
                if std::fs::remove_file(&file).is_ok() {
                    report.removed_files += 1;
                }
            }
        }
        Ok(report)
    }

    /// Resolves the root at `cwd`, refreshes it unless `freshness` is stale, and opens it.
    fn open_root(&self, cwd: &Path, freshness: Freshness) -> Result<(Opened, Resolved)> {
        let resolved = workspace::resolve(cwd, &self.home)?;
        if freshness == Freshness::Walk {
            refresh::refresh(
                &self.home,
                &resolved.target,
                &self.settings,
                Mode::Incremental,
            )?;
        }
        let opened = query::open(&self.home, &resolved, freshness)?;
        Ok((opened, resolved))
    }

    pub fn symbols(
        &self,
        cwd: &Path,
        query: &SymbolQuery,
        freshness: Freshness,
        page: &PageRequest,
    ) -> Result<Page<SymbolHit>> {
        let (opened, resolved) = self.open_root(cwd, freshness)?;
        let mut found = query::symbols::run(&opened, query, page)?;
        rebase(
            &resolved.scope,
            found.rows.iter_mut().map(|hit| &mut hit.path),
        );
        Ok(found)
    }

    pub fn code(
        &self,
        cwd: &Path,
        query: &CodeQuery,
        freshness: Freshness,
        page: &PageRequest,
    ) -> Result<Page<CodeHit>> {
        let (opened, resolved) = self.open_root(cwd, freshness)?;
        let mut found = query::code::run(&opened, query, page)?;
        rebase(
            &resolved.scope,
            found.rows.iter_mut().map(|hit| &mut hit.path),
        );
        Ok(found)
    }

    pub fn refs(
        &self,
        cwd: &Path,
        query: &RefsQuery,
        freshness: Freshness,
        page: &PageRequest,
    ) -> Result<Page<RefHit>> {
        let (opened, resolved) = self.open_root(cwd, freshness)?;
        let mut found = query::refs::run(&opened, query, page)?;
        rebase(
            &resolved.scope,
            found.rows.iter_mut().map(|hit| &mut hit.path),
        );
        Ok(found)
    }

    pub fn graph(&self, cwd: &Path, query: &GraphQuery, freshness: Freshness) -> Result<Graph> {
        let (opened, resolved) = self.open_root(cwd, freshness)?;
        let mut graph = query::graph::run(&opened, query)?;
        rebase(
            &resolved.scope,
            graph.nodes.iter_mut().map(|node| &mut node.path),
        );
        Ok(graph)
    }

    /// Lists the symbols of one file, which can be outside the start directory.
    pub fn outline(
        &self,
        cwd: &Path,
        file: &str,
        freshness: Freshness,
        page: &PageRequest,
    ) -> Result<Outline> {
        let (opened, resolved) = self.open_root(cwd, freshness)?;
        let mut outline = if Path::new(file).is_absolute() {
            let absolute = paths::canonical(Path::new(file))
                .map_err(|_| Error::usage(format!("{file} does not exist")))?;
            let relative = paths::relative(&resolved.target.root, &absolute)
                .ok_or_else(|| Error::usage(format!("{file} is outside the workspace")))?;
            query::outline::run(&opened, "", &relative, page)?
        } else {
            query::outline::run(&opened, &resolved.scope, file, page)?
        };
        rebase(&resolved.scope, std::iter::once(&mut outline.path));
        Ok(outline)
    }
}

/// True for the shard itself and its WAL and shared-memory files.
fn is_shard_file(file: &Path, shard_path: &Path) -> bool {
    ["", "-wal", "-shm"].iter().any(|suffix| {
        let mut name = shard_path.as_os_str().to_owned();
        name.push(suffix);
        file.as_os_str() == name
    })
}

/// Rewrites workspace-relative paths relative to the start directory, so a printed path works as
/// a `--path` or `outline` argument from the same directory.
fn rebase<'path>(scope: &str, targets: impl Iterator<Item = &'path mut String>) {
    for path in targets {
        *path = paths::from_dir(scope, path);
    }
}
