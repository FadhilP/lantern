//! The git CLI, for `changes`: the diff of a range or of the working tree, and blob contents.
//!
//! Git runs with options that keep the user's settings from changing the output or starting an
//! external diff program, and without the index refresh that would rewrite `.git/index`. A clean
//! filter still runs, as for any `git diff`.

use std::collections::HashMap;
use std::io::{self, BufRead, BufReader, Read, Write};
use std::path::Path;
use std::process::{Command, Output, Stdio};

use serde::Serialize;

use crate::error::{Error, Result};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum FileStatus {
    Added,
    Deleted,
    Modified,
    Renamed,
    /// A copy of `old_path`, which the diff also changes.
    Copied,
    /// New in the working tree and not ignored; git does not track it yet.
    Untracked,
}

/// Where the content of one side of a changed file comes from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Side {
    Absent,
    Blob(String),
    WorkingTree,
    /// The blob when git stored it, else the working-tree file: for rename detection, git
    /// hashes a working-tree file without storing it.
    Hashed(String),
}

/// One added or removed line; `line` is 1-based on its side.
#[derive(Debug, Clone)]
pub(crate) struct Line {
    pub added: bool,
    pub line: u32,
    /// Which hunk of its file the line is in.
    pub hunk: u32,
    pub text: String,
}

#[derive(Debug, Clone)]
pub(crate) struct FileDiff {
    pub path: String,
    /// The path before a rename, or the source of a copy.
    pub old_path: Option<String>,
    pub status: FileStatus,
    pub old: Side,
    pub new: Side,
    /// Git found the content binary, so it printed no lines.
    pub binary: bool,
    /// In hunk order; empty for an untracked file until its content is read.
    pub lines: Vec<Line>,
}

#[derive(Debug)]
pub(crate) struct Diff {
    /// Sorted by path.
    pub files: Vec<FileDiff>,
    /// A hash of git's output; a cursor from another state is stale.
    pub state: String,
}

const SUBMODULE_MODE: &str = "160000";
const SYMLINK_MODE: &str = "120000";

/// The changes of `range` as `git diff` gives them. Without a range: the working tree and the
/// index against HEAD, and the untracked files.
pub(crate) fn diff(root: &Path, range: Option<&str>) -> Result<Diff> {
    let working_tree = match range {
        None => true,
        Some(range) => single_revision(root, range)?,
    };
    let output = run(
        root,
        &[
            "-c",
            "diff.autoRefreshIndex=false",
            "-c",
            "core.quotePath=false",
            "diff",
            "--raw",
            "-z",
            "--patch",
            "--unified=0",
            "--no-abbrev",
            // Copies only from files that the diff also changes; the "harder" search reads every file.
            "--find-copies",
            "--no-color",
            "--no-ext-diff",
            "--no-textconv",
            "--no-relative",
            "--inter-hunk-context=0",
            "--submodule=short",
            "--src-prefix=a/",
            "--dst-prefix=b/",
            "--end-of-options",
            range.unwrap_or("HEAD"),
            "--",
        ],
    )?;
    if !output.status.success() {
        return Err(diff_failed(root, range, &output));
    }
    let mut hasher = blake3::Hasher::new();
    hasher.update(&output.stdout);
    let mut files = parse(&output.stdout, working_tree)?;
    if range.is_none() {
        let untracked = run(root, &["ls-files", "--others", "--exclude-standard", "-z"])?;
        if !untracked.status.success() {
            return Err(failed("git ls-files", &untracked));
        }
        hasher.update(&untracked.stdout);
        let paths = untracked.stdout.split(|&byte| byte == 0);
        files.extend(paths.filter_map(|path| {
            let path = std::str::from_utf8(path).ok()?;
            // Also leaves out a nested repository, which git lists as `dir/`.
            if !inside_file(root, path) {
                return None;
            }
            Some(FileDiff {
                path: path.to_owned(),
                old_path: None,
                status: FileStatus::Untracked,
                old: Side::Absent,
                new: Side::WorkingTree,
                binary: false,
                lines: Vec::new(),
            })
        }));
    }
    files.sort_by(|a, b| a.path.cmp(&b.path));
    Ok(Diff {
        files,
        state: hasher.finalize().to_hex()[..16].to_owned(),
    })
}

/// True when `range` names one revision, which `git diff` compares with the working tree. A
/// range that names no revision is a usage error: `git diff` would take a path outside the
/// repository for `--no-index`.
fn single_revision(root: &Path, range: &str) -> Result<bool> {
    let output = run(
        root,
        &["rev-parse", "--revs-only", "--end-of-options", range],
    )?;
    let stdout = String::from_utf8_lossy(&output.stdout);
    let revisions: Vec<&str> = stdout.lines().collect();
    if !output.status.success() || revisions.is_empty() {
        return Err(Error::usage(format!(
            "`{range}` names no revision that git knows; pass a range such as `main...HEAD`"
        )));
    }
    let negative = revisions
        .iter()
        .filter(|revision| revision.starts_with('^'))
        .count();
    if revisions.len() - negative == 1 && negative > 1 {
        return Err(Error::usage(format!(
            "`{range}` makes git compare a merge with all its parents; pass two end points, such as `M^1..M`"
        )));
    }
    Ok(matches!(revisions[..], [revision] if !revision.starts_with('^')))
}

/// A regular file at `path` in `root`, and inside it: a symbolic link or a junction on the way
/// can lead outside.
pub(crate) fn inside_file(root: &Path, path: &str) -> bool {
    let full = root.join(path);
    let regular = full
        .symlink_metadata()
        .is_ok_and(|metadata| metadata.is_file());
    // Both in the same form: a long Windows path keeps its `\\?\` prefix.
    let real = std::fs::canonicalize(&full);
    regular
        && real
            .is_ok_and(|real| std::fs::canonicalize(root).is_ok_and(|root| real.starts_with(root)))
}

/// The contents of `ids`, read by one `git cat-file --batch`. An object that git does not have
/// is left out.
pub(crate) fn read_blobs(root: &Path, ids: &[String]) -> Result<HashMap<String, Vec<u8>>> {
    let mut blobs = HashMap::new();
    if ids.is_empty() {
        return Ok(blobs);
    }
    let mut child = command(root)
        .args(["cat-file", "--batch"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .map_err(spawn_failed)?;
    let (Some(mut stdin), Some(stdout)) = (child.stdin.take(), child.stdout.take()) else {
        return Err(Error::failure("git cat-file has no pipes"));
    };
    let request: String = ids.iter().map(|id| format!("{id}\n")).collect();
    // A separate writer, so a full output pipe cannot block git while it waits for input.
    let writer = std::thread::spawn(move || stdin.write_all(request.as_bytes()));
    let mut reader = BufReader::new(stdout);
    for id in ids {
        let mut header = String::new();
        reader.read_line(&mut header).map_err(read_failed)?;
        if header.trim_end().ends_with(" missing") {
            continue;
        }
        // `<id> <type> <size>`.
        let size = header
            .trim_end()
            .split(' ')
            .nth(2)
            .and_then(|size| size.parse::<usize>().ok())
            .ok_or_else(|| Error::failure(format!("git cannot read object {id}")))?;
        let mut data = vec![0; size + 1];
        reader.read_exact(&mut data).map_err(read_failed)?;
        data.truncate(size);
        blobs.insert(id.clone(), data);
    }
    drop(reader);
    let written = writer
        .join()
        .map_err(|_| Error::failure("git cat-file writer failed"))?;
    written.map_err(read_failed)?;
    child.wait().map_err(read_failed)?;
    Ok(blobs)
}

fn command(root: &Path) -> Command {
    let mut command = Command::new("git");
    // It wins over `--unified=0`.
    command.env_remove("GIT_DIFF_OPTS");
    command.arg("-C").arg(root);
    command
}

fn run(root: &Path, args: &[&str]) -> Result<Output> {
    command(root).args(args).output().map_err(spawn_failed)
}

fn spawn_failed(error: io::Error) -> Error {
    if error.kind() == io::ErrorKind::NotFound {
        return Error::failure("git is not on PATH; `lantern changes` runs it");
    }
    Error::failure(format!("cannot run git: {error}"))
}

fn read_failed(error: io::Error) -> Error {
    Error::failure(format!("git cat-file: {error}"))
}

fn failed(what: &str, output: &Output) -> Error {
    let stderr = String::from_utf8_lossy(&output.stderr);
    Error::failure(format!("{what} failed: {}", stderr.trim()))
}

/// A bad revision is a usage error with git's message.
fn diff_failed(root: &Path, range: Option<&str>, output: &Output) -> Error {
    let has_head = || {
        run(root, &["rev-parse", "--verify", "--quiet", "HEAD"])
            .is_ok_and(|output| output.status.success())
    };
    if range.is_none() && !has_head() {
        return Error::usage(
            "the repository has no commit yet, so there is no HEAD to compare with",
        );
    }
    let stderr = String::from_utf8_lossy(&output.stderr);
    Error::usage(format!("git diff failed: {}", stderr.trim()))
}

fn unexpected() -> Error {
    Error::failure("unexpected `git diff` output")
}

/// One `--raw -z` record: `:mode mode id id status`, then one path, or two for a rename.
struct RawEntry<'a> {
    modes: [&'a str; 2],
    ids: [&'a str; 2],
    status: u8,
    paths: [&'a [u8]; 2],
}

/// The patch part of one file.
struct Section<'a> {
    /// The `diff --git a/OLD b/NEW` line.
    header: &'a [u8],
    binary: bool,
    lines: Vec<Line>,
}

/// Reads the raw records, then the patch. Paths and blob ids come from the raw records. The
/// patch has a section for each record in the same order, except for a file whose content did
/// not change (only its file time did) and an unmerged file; both are left out.
/// With `working_tree`, git compared the new side of each file with the working tree, or with
/// the index for a sparse or assume-unchanged entry.
fn parse(output: &[u8], working_tree: bool) -> Result<Vec<FileDiff>> {
    let (entries, patch) = raw_entries(output)?;
    let mut sections = sections(patch)?.into_iter().peekable();
    let mut files = Vec::new();
    for entry in entries {
        let header = section_header(&entry);
        let Some(section) = sections.next_if(|section| section.header == header) else {
            continue;
        };
        // A type change, such as a symbolic link that became a file, has a second section.
        while sections
            .next_if(|section| section.header == header)
            .is_some()
        {}
        if entry
            .modes
            .iter()
            .any(|&mode| mode == SUBMODULE_MODE || mode == SYMLINK_MODE)
        {
            continue;
        }
        let (Ok(first), Ok(second)) = (
            std::str::from_utf8(entry.paths[0]),
            std::str::from_utf8(entry.paths[1]),
        ) else {
            continue;
        };
        let (status, old_path) = match entry.status {
            b'A' => (FileStatus::Added, None),
            b'D' => (FileStatus::Deleted, None),
            b'R' => (FileStatus::Renamed, Some(first.to_owned())),
            b'C' => (FileStatus::Copied, Some(first.to_owned())),
            _ => (FileStatus::Modified, None),
        };
        let path = if second.is_empty() { first } else { second };
        let old = match entry.status {
            b'A' => Side::Absent,
            _ => Side::Blob(entry.ids[0].to_owned()),
        };
        let new = match status {
            FileStatus::Deleted => Side::Absent,
            // Git names no blob for a working-tree file that differs from the index.
            _ if entry.ids[1].bytes().all(|byte| byte == b'0') => Side::WorkingTree,
            _ if working_tree => Side::Hashed(entry.ids[1].to_owned()),
            _ => Side::Blob(entry.ids[1].to_owned()),
        };
        files.push(FileDiff {
            path: path.to_owned(),
            old_path,
            status,
            old,
            new,
            binary: section.binary,
            lines: section.lines,
        });
    }
    if sections.next().is_some() {
        return Err(unexpected());
    }
    Ok(files)
}

/// The raw records, and the patch after the empty field that ends them.
fn raw_entries(output: &[u8]) -> Result<(Vec<RawEntry<'_>>, &[u8])> {
    let mut rest = output;
    let mut entries = Vec::new();
    while !rest.is_empty() {
        let header = field(&mut rest)?;
        if header.is_empty() {
            break;
        }
        let header = std::str::from_utf8(header).map_err(|_| unexpected())?;
        let parts: Vec<&str> = header.trim_start_matches(':').split(' ').collect();
        let [old_mode, new_mode, old_id, new_id, status] = parts[..] else {
            return Err(unexpected());
        };
        let status = *status.as_bytes().first().ok_or_else(unexpected)?;
        let first = field(&mut rest)?;
        let second = if matches!(status, b'R' | b'C') {
            field(&mut rest)?
        } else {
            &[]
        };
        entries.push(RawEntry {
            modes: [old_mode, new_mode],
            ids: [old_id, new_id],
            status,
            paths: [first, second],
        });
    }
    Ok((entries, rest))
}

/// The bytes before the next NUL, which `rest` then skips.
fn field<'a>(rest: &mut &'a [u8]) -> Result<&'a [u8]> {
    let end = rest
        .iter()
        .position(|&byte| byte == 0)
        .ok_or_else(unexpected)?;
    let field = &rest[..end];
    *rest = &rest[end + 1..];
    Ok(field)
}

/// The `diff --git` line that git prints for `entry`, with its quoting of unusual paths.
fn section_header(entry: &RawEntry) -> Vec<u8> {
    let new = if entry.paths[1].is_empty() {
        entry.paths[0]
    } else {
        entry.paths[1]
    };
    let mut header = b"diff --git ".to_vec();
    header.extend(quoted(b"a/", entry.paths[0]));
    header.push(b' ');
    header.extend(quoted(b"b/", new));
    header
}

/// `prefix` and `path`, in C-style quotes when the path has a control character, `"`, or `\`,
/// as git writes them with `core.quotePath=false`.
fn quoted(prefix: &[u8], path: &[u8]) -> Vec<u8> {
    let plain = |byte: u8| byte >= 0x20 && byte != 0x7f && byte != b'"' && byte != b'\\';
    let mut text = prefix.to_vec();
    if path.iter().all(|&byte| plain(byte)) {
        text.extend(path);
        return text;
    }
    text.insert(0, b'"');
    for &byte in path {
        match byte {
            b'"' | b'\\' => text.extend([b'\\', byte]),
            0x07 => text.extend(b"\\a"),
            0x08 => text.extend(b"\\b"),
            b'\t' => text.extend(b"\\t"),
            b'\n' => text.extend(b"\\n"),
            0x0b => text.extend(b"\\v"),
            0x0c => text.extend(b"\\f"),
            b'\r' => text.extend(b"\\r"),
            byte if plain(byte) => text.push(byte),
            byte => text.extend(format!("\\{byte:03o}").bytes()),
        }
    }
    text.push(b'"');
    text
}

fn sections(patch: &[u8]) -> Result<Vec<Section<'_>>> {
    let mut sections: Vec<Section> = Vec::new();
    let mut lines = patch.split(|&byte| byte == b'\n');
    while let Some(line) = lines.next() {
        if line.starts_with(b"diff --git ") {
            sections.push(Section {
                header: line,
                binary: false,
                lines: Vec::new(),
            });
            continue;
        }
        let Some(section) = sections.last_mut() else {
            continue;
        };
        if line.starts_with(b"Binary files ") {
            section.binary = true;
        } else if line.starts_with(b"@@ ") {
            hunk(line, &mut lines, &mut section.lines)?;
        }
    }
    Ok(sections)
}

/// Reads one hunk by the counts in its `@@ -a,b +c,d @@` header, so a removed line that starts
/// with `-- ` cannot pass for a file header.
fn hunk<'a>(
    header: &[u8],
    lines: &mut impl Iterator<Item = &'a [u8]>,
    out: &mut Vec<Line>,
) -> Result<()> {
    let mut ranges = header
        .strip_prefix(b"@@ ")
        .ok_or_else(unexpected)?
        .split(|&byte| byte == b' ');
    let (Some(old), Some(new)) = (ranges.next(), ranges.next()) else {
        return Err(unexpected());
    };
    let (mut old_line, mut old_left) = range(old, b'-')?;
    let (mut new_line, mut new_left) = range(new, b'+')?;
    // Every hunk has a line, so the last line tells how many hunks came before.
    let index = out.last().map_or(0, |line| line.hunk + 1);
    while old_left > 0 || new_left > 0 {
        let line = lines.next().ok_or_else(unexpected)?;
        let Some((&sign, text)) = line.split_first() else {
            return Err(unexpected());
        };
        let text =
            || String::from_utf8_lossy(text.strip_suffix(b"\r").unwrap_or(text)).into_owned();
        match sign {
            b'-' => {
                old_left = old_left.checked_sub(1).ok_or_else(unexpected)?;
                out.push(Line {
                    added: false,
                    line: old_line,
                    hunk: index,
                    text: text(),
                });
                old_line += 1;
            }
            b'+' => {
                new_left = new_left.checked_sub(1).ok_or_else(unexpected)?;
                out.push(Line {
                    added: true,
                    line: new_line,
                    hunk: index,
                    text: text(),
                });
                new_line += 1;
            }
            b'\\' => {}
            _ => return Err(unexpected()),
        }
    }
    Ok(())
}

/// `-a,b` or `-a` (one line) to the first line and the line count.
fn range(text: &[u8], sign: u8) -> Result<(u32, u32)> {
    let text = text
        .strip_prefix(&[sign])
        .and_then(|text| std::str::from_utf8(text).ok())
        .ok_or_else(unexpected)?;
    let (start, count) = text.split_once(',').unwrap_or((text, "1"));
    let parse = |number: &str| number.parse::<u32>().map_err(|_| unexpected());
    Ok((parse(start)?, parse(count)?))
}
