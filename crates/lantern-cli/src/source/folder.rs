//! The folder source: a parallel walk with scoped `.gitignore`, `.lanternignore`, and default excludes.

use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Mutex, mpsc};

use ignore::{WalkBuilder, WalkState};

use super::{DirRules, GITATTRIBUTES, Inventory, LANTERNIGNORE, Scan, classify, read_rule_file};
use crate::error::{Error, Result};
use crate::fsmeta::{self, EntryKind, Stat};
use crate::limits::{self, MAX_DEPTH};
use crate::paths;

enum Found {
    File(String, Stat),
    Rules {
        dir: String,
        file: &'static str,
        content: String,
    },
}

pub(super) fn inventory(scan: &Scan<'_>) -> Result<Inventory> {
    let mut builder = WalkBuilder::new(scan.root);
    builder
        .standard_filters(false)
        .hidden(false)
        .follow_links(false)
        .max_depth(Some(MAX_DEPTH))
        .git_ignore(true)
        .require_git(false)
        .parents(false)
        .ignore(false)
        .git_global(false)
        .git_exclude(false)
        .add_custom_ignore_filename(LANTERNIGNORE);
    if let Some(error) = builder.add_ignore(scan.defaults_file) {
        return Err(Error::failure(format!("default excludes: {error}")));
    }
    let data_dir = scan.data_dir.to_path_buf();
    builder.filter_entry(move |entry| {
        entry.file_name() != ".git" && !entry.path().starts_with(&data_dir)
    });

    let (sender, receiver) = mpsc::channel();
    let failure: Mutex<Option<Error>> = Mutex::new(None);
    let entries = AtomicU64::new(0);
    let rule_bytes = AtomicU64::new(0);
    let max_entries = limits::max_entries();
    builder.build_parallel().run(|| {
        let sender = sender.clone();
        let (failure, entries, rule_bytes) = (&failure, &entries, &rule_bytes);
        Box::new(move |result| {
            let visited = visit(scan, result, &sender, rule_bytes).and_then(|()| {
                if entries.fetch_add(1, Ordering::Relaxed) >= max_entries {
                    return Err(Error::failure(format!(
                        "folder has more than {max_entries} entries"
                    )));
                }
                scan.check_deadline()
            });
            match visited {
                Ok(()) => WalkState::Continue,
                Err(error) => {
                    failure
                        .lock()
                        .expect("failure lock is never poisoned")
                        .get_or_insert(error);
                    WalkState::Quit
                }
            }
        })
    });
    drop(sender);
    if let Some(error) = failure
        .into_inner()
        .expect("failure lock is never poisoned")
    {
        return Err(error);
    }

    let mut stats = Vec::new();
    let mut rules = DirRules::default();
    for found in receiver {
        match found {
            Found::File(path, stat) => stats.push((path, stat)),
            Found::Rules { dir, file, content } if file == LANTERNIGNORE => {
                rules.lanternignore.push((dir, content));
            }
            Found::Rules { dir, content, .. } => rules.attributes.push((dir, content)),
        }
    }
    Ok(Inventory {
        files: classify(stats, &rules)?,
        rules_digest: rules.digest(),
        head: None,
        branch: None,
    })
}

fn visit(
    scan: &Scan<'_>,
    result: Result<ignore::DirEntry, ignore::Error>,
    sender: &mpsc::Sender<Found>,
    rule_bytes: &AtomicU64,
) -> Result<()> {
    let entry = result.map_err(|error| Error::failure(format!("walk failed: {error}")))?;
    if let Some(error) = entry.error() {
        return Err(Error::failure(format!("ignore rules: {error}")));
    }
    let Some(file_type) = entry.file_type() else {
        return Ok(());
    };
    // Paths are stored as UTF-8; the Git source skips such paths too.
    let Some(relative) = paths::relative(scan.root, entry.path()) else {
        return Ok(());
    };
    if file_type.is_dir() {
        if entry.depth() >= MAX_DEPTH {
            return Err(Error::failure(format!(
                "directory depth exceeds {MAX_DEPTH}: {relative}"
            )));
        }
        return read_dir_rules(entry.path(), &relative, sender, rule_bytes);
    }
    if !file_type.is_file() {
        return Ok(());
    }
    let stat = match fsmeta::lstat(entry.path()) {
        Ok(stat) => stat,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return Err(Error::temporary(format!(
                "{relative} vanished during the walk"
            )));
        }
        Err(error) => return Err(Error::io(entry.path(), &error)),
    };
    if stat.kind == EntryKind::File {
        let _ = sender.send(Found::File(relative, stat));
    }
    Ok(())
}

fn read_dir_rules(
    dir: &Path,
    relative: &str,
    sender: &mpsc::Sender<Found>,
    rule_bytes: &AtomicU64,
) -> Result<()> {
    // The walker reads `.gitignore` itself; reading it here enforces the size limits.
    for file in [LANTERNIGNORE, GITATTRIBUTES, ".gitignore"] {
        if let Some(content) = read_rule_file(&dir.join(file), rule_bytes)?
            && file != ".gitignore"
        {
            let _ = sender.send(Found::Rules {
                dir: relative.to_owned(),
                file,
                content,
            });
        }
    }
    Ok(())
}
