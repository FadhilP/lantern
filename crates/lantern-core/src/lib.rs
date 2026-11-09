//! Lantern core: a local, read-only code index with inventory sources, tree-sitter extraction,
//! one SQLite shard per physical root, and queries for symbols, code, references, call graphs,
//! and outlines.
//!
//! The command line is the only stable interface; this library API may change.

mod error;
mod extract;
mod fsmeta;
mod home;
mod language;
mod limits;
mod paths;
mod query;
mod refresh;
mod rules;
mod source;
mod store;
mod workspace;

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use serde::Serialize;

pub use error::{Error, ErrorKind, Result};
pub use extract::{RefKind, SymbolKind};
pub use home::default_home;
pub use language::Language;
pub use limits::{
    DEFAULT_CODE_LIMIT, DEFAULT_GRAPH_DEPTH, DEFAULT_GRAPH_NODES, DEFAULT_LIMIT,
    DEFAULT_MAX_FILE_SIZE, DEFAULT_PER_FILE, DEFAULT_TIMEOUT, DEFAULT_VERIFY_INTERVAL,
    MAX_MAX_FILE_SIZE, MAX_VERIFY_INTERVAL,
};
pub use query::code::{CodeHit, CodeQuery, CodeSort, LineMatch};
pub use query::graph::{Direction, Edge, Graph, GraphQuery, Node};
pub use query::outline::{Outline, OutlineNode};
pub use query::refs::{Enclosing, RefHit, RefsQuery};
pub use query::symbols::{SymbolHit, SymbolQuery};
pub use query::{Freshness, Page, PageRequest};
pub use refresh::RefreshCounts;
pub use source::SourceKind;
pub use store::shard::Counts;

use home::Home;
use query::Opened;
use refresh::{Mode, Settings};
use store::catalog::{Catalog, Workspace};
use store::shard;
use workspace::Resolved;

/// Settings that every command shares.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Options {
    /// The data directory; `None` uses [`default_home`].
    pub home: Option<PathBuf>,
    /// The warm refresh timeout; `None` waits without a limit.
    pub timeout: Option<Duration>,
    pub verify_interval: Duration,
    pub max_file_size: u64,
}

impl Default for Options {
    fn default() -> Self {
        Self {
            home: None,
            timeout: Some(DEFAULT_TIMEOUT),
            verify_interval: DEFAULT_VERIFY_INTERVAL,
            max_file_size: DEFAULT_MAX_FILE_SIZE,
        }
    }
}

/// How `index` builds the shards.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum IndexMode {
    /// Reread only what changed.
    #[default]
    Incremental,
    /// Build new shards from the disk and swap them in.
    Rebuild,
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
    pub members: usize,
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
    pub members: Vec<MemberStatus>,
}

#[derive(Debug, Clone, Serialize)]
pub struct MemberStatus {
    pub prefix: String,
    pub root: String,
    pub source: SourceKind,
    pub indexed_at: Option<i64>,
    pub files: u64,
    pub unparsed_files: u64,
}

/// What `prune` removed.
#[derive(Debug, Clone, Default, Serialize)]
pub struct PruneReport {
    pub removed_roots: Vec<String>,
    pub removed_files: usize,
}

/// The entry point: one data directory and its settings.
#[derive(Debug, Clone)]
pub struct Lantern {
    home: Home,
    settings: Settings,
}

impl Lantern {
    /// Validates `options` and opens (or creates) the data directory.
    ///
    /// # Errors
    ///
    /// A usage error for out-of-range options or a missing home; a failure when the data
    /// directory cannot be created.
    pub fn open(options: &Options) -> Result<Self> {
        if options.verify_interval > MAX_VERIFY_INTERVAL {
            return Err(Error::usage(format!(
                "--verify-interval must be at most {} ms",
                MAX_VERIFY_INTERVAL.as_millis()
            )));
        }
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
                verify_interval_ms: options.verify_interval.as_millis() as i64,
                max_file_size: options.max_file_size,
                timeout: options.timeout,
            },
        })
    }

    fn catalog(&self) -> Result<Catalog> {
        Catalog::open(&self.home.catalog_path())
    }

    /// Refreshes the workspace at `path` and reports its totals.
    ///
    /// # Errors
    ///
    /// Any refresh failure; nothing is published when it fails.
    pub fn index(&self, path: &Path, mode: IndexMode) -> Result<IndexReport> {
        let started = Instant::now();
        let mut catalog = self.catalog()?;
        let resolved = workspace::resolve(path, &catalog)?;
        let mode = match mode {
            IndexMode::Incremental => Mode::Incremental,
            IndexMode::Rebuild => Mode::Rebuild,
        };
        let refresh = workspace::refresh(
            &self.home,
            &mut catalog,
            &self.settings,
            &resolved.target,
            mode,
        )?;
        let workspace = catalog
            .workspace(&resolved.target.root_key)?
            .ok_or_else(|| Error::failure("the workspace was not published"))?;
        let mut counts = Counts::default();
        let mut indexed_at = None;
        let mut seen = HashSet::new();
        for member in &workspace.members {
            if !seen.insert(member.root.root_key.clone()) {
                continue;
            }
            if let Some(conn) =
                shard::open_existing(&self.home.roots_dir().join(&member.root.shard))?
            {
                counts += shard::counts(&conn)?;
                indexed_at = indexed_at.max(shard::meta(&conn)?.and_then(|meta| meta.indexed_at));
            }
        }
        Ok(IndexReport {
            root: resolved.target.root.to_string_lossy().into_owned(),
            source: resolved.target.source,
            counts,
            indexed_at,
            refresh,
            members: workspace.members.len(),
            elapsed_ms: started.elapsed().as_millis(),
        })
    }

    /// Reports the committed state of the workspace at `path` without refreshing.
    ///
    /// # Errors
    ///
    /// A usage error for a missing path; a failure when a database cannot be read.
    pub fn status(&self, path: &Path) -> Result<Status> {
        let catalog = self.catalog()?;
        let resolved = workspace::resolve(path, &catalog)?;
        let target = &resolved.target;
        let mut status = Status {
            root: target.root.to_string_lossy().into_owned(),
            source: target.source,
            indexed: false,
            head: None,
            branch: None,
            indexed_at: None,
            generation: None,
            counts: Counts::default(),
            members: Vec::new(),
        };
        let Some(workspace) = catalog.workspace(&target.root_key)? else {
            return Ok(status);
        };
        for member in &workspace.members {
            let Some(conn) = shard::open_existing(&self.home.roots_dir().join(&member.root.shard))?
            else {
                continue;
            };
            let meta = shard::meta(&conn)?;
            let counts = shard::counts(&conn)?;
            if member.prefix.is_empty() {
                status.counts = counts;
                if let Some(meta) = &meta {
                    status.indexed = true;
                    status.head.clone_from(&meta.head);
                    status.branch.clone_from(&meta.branch);
                    status.indexed_at = meta.indexed_at;
                    status.generation = Some(meta.generation);
                }
            }
            status.members.push(MemberStatus {
                prefix: member.prefix.clone(),
                root: member.root.root.clone(),
                source: member.root.source,
                indexed_at: meta.and_then(|meta| meta.indexed_at),
                files: counts.files,
                unparsed_files: counts.unparsed_files,
            });
        }
        Ok(status)
    }

    /// Removes shards of roots that no longer exist, and stray files in the shard directory.
    ///
    /// # Errors
    ///
    /// A failure when a shard or the catalog cannot be changed.
    pub fn prune(&self) -> Result<PruneReport> {
        let catalog = self.catalog()?;
        let mut report = PruneReport::default();
        let mut live = HashSet::new();
        for root in catalog.roots()? {
            if Path::new(&root.root).is_dir() {
                live.insert(root.shard.clone());
                continue;
            }
            let _lock = self.home.lock(&root.root_key, self.settings.timeout)?;
            refresh::remove_database(&self.home.roots_dir().join(&root.shard))?;
            catalog.delete_root(&root.root_key)?;
            report.removed_roots.push(root.root);
        }
        let roots_dir = self.home.roots_dir();
        let entries =
            std::fs::read_dir(&roots_dir).map_err(|error| Error::io(&roots_dir, &error))?;
        for entry in entries {
            let entry = entry.map_err(|error| Error::io(&roots_dir, &error))?;
            let name = entry.file_name().to_string_lossy().into_owned();
            let shard_name = name.trim_end_matches("-wal").trim_end_matches("-shm");
            if live.contains(shard_name) {
                continue;
            }
            // A stray shard may belong to a refresh that is still running or just listed its root.
            let Ok(_lock) = self
                .home
                .lock_hash(Home::shard_hash(shard_name), Some(Duration::ZERO))
            else {
                continue;
            };
            if catalog.roots()?.iter().any(|root| root.shard == shard_name) {
                continue;
            }
            if std::fs::remove_file(entry.path()).is_ok() {
                report.removed_files += 1;
            }
        }
        Ok(report)
    }

    /// Opens the members that intersect the start directory.
    fn open_workspace(&self, cwd: &Path, freshness: Freshness) -> Result<(Opened, Resolved)> {
        let (workspace, resolved) = self.workspace(cwd, freshness)?;
        let opened = query::open(&self.home, &workspace, &resolved.scope, freshness)?;
        Ok((opened, resolved))
    }

    /// Resolves the root at `cwd`, refreshes it unless `freshness` is stale, and reads its members.
    fn workspace(&self, cwd: &Path, freshness: Freshness) -> Result<(Workspace, Resolved)> {
        let not_indexed = |root: &Path| {
            Error::failure(format!(
                "{} is not indexed; run `lantern index` first",
                root.display()
            ))
        };
        let mut catalog = match freshness {
            Freshness::Walk => self.catalog()?,
            Freshness::Stale => Catalog::open_read_only(&self.home.catalog_path())?
                .ok_or_else(|| not_indexed(cwd))?,
        };
        let resolved = workspace::resolve(cwd, &catalog)?;
        if freshness == Freshness::Walk {
            workspace::refresh(
                &self.home,
                &mut catalog,
                &self.settings,
                &resolved.target,
                Mode::Incremental,
            )?;
        }
        let workspace = catalog
            .workspace(&resolved.target.root_key)?
            .ok_or_else(|| not_indexed(&resolved.target.root))?;
        Ok((workspace, resolved))
    }

    /// Finds definitions by fuzzy name.
    ///
    /// # Errors
    ///
    /// A usage error for an invalid query or cursor; any refresh failure.
    pub fn symbols(
        &self,
        cwd: &Path,
        query: &SymbolQuery,
        freshness: Freshness,
        page: &PageRequest,
    ) -> Result<Page<SymbolHit>> {
        let (mut opened, resolved) = self.open_workspace(cwd, freshness)?;
        let mut found = query::symbols::run(&mut opened, &resolved.scope, query, page)?;
        rebase(
            &resolved.scope,
            found.rows.iter_mut().map(|hit| &mut hit.path),
        );
        Ok(found)
    }

    /// Finds lines that match a literal or a regex.
    ///
    /// # Errors
    ///
    /// A usage error for an invalid pattern or cursor; any refresh failure.
    pub fn code(
        &self,
        cwd: &Path,
        query: &CodeQuery,
        freshness: Freshness,
        page: &PageRequest,
    ) -> Result<Page<CodeHit>> {
        let (opened, resolved) = self.open_workspace(cwd, freshness)?;
        let mut found = query::code::run(&opened, &resolved.scope, query, page)?;
        rebase(
            &resolved.scope,
            found.rows.iter_mut().map(|hit| &mut hit.path),
        );
        Ok(found)
    }

    /// Finds references to a name.
    ///
    /// # Errors
    ///
    /// A usage error for an invalid query or cursor; any refresh failure.
    pub fn refs(
        &self,
        cwd: &Path,
        query: &RefsQuery,
        freshness: Freshness,
        page: &PageRequest,
    ) -> Result<Page<RefHit>> {
        let (opened, resolved) = self.open_workspace(cwd, freshness)?;
        let mut found = query::refs::run(&opened, &resolved.scope, query, page)?;
        rebase(
            &resolved.scope,
            found.rows.iter_mut().map(|hit| &mut hit.path),
        );
        Ok(found)
    }

    /// Builds the name-resolved call graph around a name.
    ///
    /// # Errors
    ///
    /// A usage error for an invalid query; any refresh failure.
    pub fn graph(&self, cwd: &Path, query: &GraphQuery, freshness: Freshness) -> Result<Graph> {
        let (opened, resolved) = self.open_workspace(cwd, freshness)?;
        let mut graph = query::graph::run(&opened, &resolved.scope, query)?;
        rebase(
            &resolved.scope,
            graph.nodes.iter_mut().map(|node| &mut node.path),
        );
        Ok(graph)
    }

    /// Lists the symbols of one file, nested by container.
    ///
    /// # Errors
    ///
    /// A usage error for a path outside the workspace; a failure for a file not in the index.
    pub fn outline(
        &self,
        cwd: &Path,
        file: &str,
        freshness: Freshness,
        page: &PageRequest,
    ) -> Result<Outline> {
        // Every member, since the file can be outside the start directory, as `../C/lib.rs` is.
        let (workspace, resolved) = self.workspace(cwd, freshness)?;
        let opened = query::open(&self.home, &workspace, "", freshness)?;
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

/// Rewrites workspace-relative paths relative to the start directory, so a printed path works as
/// a `--path` or `outline` argument from the same directory.
fn rebase<'path>(scope: &str, targets: impl Iterator<Item = &'path mut String>) {
    for path in targets {
        *path = paths::from_dir(scope, path);
    }
}
