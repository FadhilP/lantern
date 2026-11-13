//! Compact text for agents and people by default, JSON documents with `--json`, and errors on
//! stderr.

use std::io::Write;

use crate::changes::{ChangedFile, ChangedLine, Move, SymbolStatus, Unparsed};
use crate::error::{Error, ErrorKind};
use crate::extract::SymbolKind;
use crate::git::FileStatus;
use crate::query::Page;
use crate::query::code::CodeHit;
use crate::query::graph::Graph;
use crate::query::outline::{Outline, OutlineNode};
use crate::query::refs::RefHit;
use crate::query::symbols::SymbolHit;
use serde::Serialize;
use serde_json::{Value, json};

/// Document schema version; every stdout document carries it.
const SCHEMA: u32 = 1;
const MAX_ERROR_BYTES: usize = 4 * 1024;

#[derive(Debug)]
pub struct Printer {
    json: bool,
}

impl Printer {
    pub fn new(json: bool) -> Self {
        Self { json }
    }

    pub fn json(&self) -> bool {
        self.json
    }

    /// Prints one text line, such as the `== QUERY` line above each page of a multi-query command.
    pub fn heading(&self, text: &str) -> Result<(), Error> {
        emit(text)
    }

    /// Prints a report as one document; text output gets it indented.
    pub fn document<T: Serialize>(&self, report: &T) -> Result<(), Error> {
        let mut document = to_value(report);
        document["schema"] = json!(SCHEMA);
        drop_nulls(&mut document);
        if self.json {
            emit(&document.to_string())
        } else {
            emit(&serde_json::to_string_pretty(&document).unwrap_or_default())
        }
    }

    /// Prints a page with the `extra` JSON fields; `max_bytes` drops rows from the end until
    /// the output fits.
    ///
    /// A usage error when not even one row fits, since an empty page would repeat forever.
    pub fn page<T: Serialize>(
        &self,
        page: &Page<T>,
        max_bytes: Option<u64>,
        extra: &[(&str, Value)],
        empty: &str,
        text: impl Fn(&[T]) -> Vec<String>,
    ) -> Result<(), Error> {
        let render = |keep: usize| {
            let rows = &page.rows[..keep];
            let next_cursor = if keep < page.rows.len() {
                Some(page.cursor_at(keep))
            } else {
                page.next_cursor.clone()
            };
            if !self.json {
                let mut lines = text(rows);
                // First, so `| head` cannot cut it off.
                if let Some(cursor) = next_cursor {
                    lines.insert(0, format!("-- more: --cursor {cursor}"));
                }
                return joined(&lines, empty);
            }
            let mut document = json!({
                "schema": SCHEMA,
                "freshness": page.freshness,
                "indexed_at": page.indexed_at,
                "results": rows,
            });
            if let Some(cursor) = next_cursor {
                document["truncated"] = json!(true);
                document["next_cursor"] = json!(cursor);
            }
            if page.rows.is_empty() {
                document["hint"] = json!(hint(empty));
            }
            for (key, value) in extra {
                document[*key] = value.clone();
            }
            drop_nulls(&mut document);
            document.to_string()
        };
        emit(&fit(page.rows.len(), max_bytes, render)?)
    }

    /// Prints a graph; `max_bytes` drops the last nodes found, with their edges, until it fits.
    pub fn graph(&self, graph: &Graph, max_bytes: Option<u64>, empty: &str) -> Result<(), Error> {
        let render = |keep: usize| {
            let mut kept = graph.clone();
            kept.nodes.truncate(keep);
            kept.edges.retain(|edge| edge.from < keep && edge.to < keep);
            kept.roots.retain(|&root| root < keep);
            kept.truncated |= keep < graph.nodes.len();
            if !self.json {
                return joined(&graph_lines(&kept), empty);
            }
            let mut document = to_value(&kept);
            document["schema"] = json!(SCHEMA);
            if graph.nodes.is_empty() {
                document["hint"] = json!(hint(empty));
            }
            drop_nulls(&mut document);
            document.to_string()
        };
        emit(&fit(graph.nodes.len(), max_bytes, render)?)
    }

    /// `unparsed_note` is empty, or the hint for files that ran out of parse time.
    pub fn outline(
        &self,
        outline: &Outline,
        max_bytes: Option<u64>,
        unparsed_note: &str,
    ) -> Result<(), Error> {
        let extra = [
            ("path", json!(outline.path)),
            ("language", json!(outline.language)),
        ];
        let empty = match (outline.language.as_ref(), unparsed_note) {
            (Some(_), "") => "no results".to_owned(),
            (Some(_), note) => format!("no results;{note}"),
            (None, _) => {
                "no results; this file type has no outline. To search its text, try: lantern code \"<text>\" --path <file>".to_owned()
            }
        };
        self.page(&outline.symbols, max_bytes, &extra, &empty, |nodes| {
            let mut lines = Vec::new();
            outline_lines(nodes, 0, &mut lines);
            lines
        })
    }
}

/// The largest of `total` items whose output fits in `max_bytes`, rendered.
///
/// A usage error when items exist but not even one fits.
fn fit(
    total: usize,
    max_bytes: Option<u64>,
    render: impl Fn(usize) -> String,
) -> Result<String, Error> {
    let Some(max_bytes) = max_bytes else {
        return Ok(render(total));
    };
    let fits = |keep: usize| render(keep).len() as u64 <= max_bytes;
    // The output grows with the item count, so search for the largest count that fits.
    let (mut low, mut high) = (0, total);
    while low < high {
        let middle = (low + high).div_ceil(2);
        if fits(middle) {
            low = middle;
        } else {
            high = middle - 1;
        }
    }
    if low == 0 && total > 0 {
        return Err(Error::new(
            ErrorKind::Usage,
            format!(
                "--max-bytes {max_bytes} is smaller than one result, which needs {} bytes",
                render(1).len()
            ),
        ));
    }
    Ok(render(low))
}

/// One line per definition; with `--body`, its `line:text` source lines follow.
pub fn symbol_lines(hits: &[SymbolHit]) -> Vec<String> {
    let mut lines = Vec::new();
    for hit in hits {
        lines.push(format!(
            "{}:{}-{} {}: {}",
            hit.path,
            hit.line,
            hit.end_line,
            kind_in(hit.kind, hit.container.as_deref()),
            hit.signature
        ));
        let Some(body) = &hit.body else {
            continue;
        };
        for (line, text) in (body.line..).zip(&body.text) {
            lines.push(format!("{line}:{text}"));
        }
        let cut = (hit.end_line + 1 - body.line) as usize - body.text.len();
        if cut > 0 {
            lines.push(format!("-- body cut: {cut} more lines"));
        }
    }
    lines
}

/// One heading per file, then rg-style `line:text` matches and `line-text` context, with an
/// indented `in kind name` line right above a match whose enclosing definition changes.
pub fn code_lines(hits: &[CodeHit]) -> Vec<String> {
    let mut lines = Vec::new();
    for hit in hits {
        let shown = hit.matches.len();
        lines.push(if shown == 0 {
            format!("{} ({} matches)", hit.path, hit.match_count)
        } else if hit.match_count > shown {
            format!("{} ({} matches, {shown} shown)", hit.path, hit.match_count)
        } else {
            hit.path.clone()
        });
        let mut site = None;
        for found in &hit.matches {
            let before = found.before.as_deref().unwrap_or_default();
            let first = found.line - before.len();
            for (offset, text) in before.iter().enumerate() {
                lines.push(format!("{}-{text}", first + offset));
            }
            if found.enclosing.as_ref() != site {
                site = found.enclosing.as_ref();
                lines.push(
                    site.map_or("  not in a named definition".to_owned(), |enclosing| {
                        format!(
                            "  in {} {}",
                            enclosing.kind.as_str(),
                            qualified(enclosing.container.as_deref(), &enclosing.name)
                        )
                    }),
                );
            }
            lines.push(format!("{}:{}", found.line, found.text));
            for (offset, text) in found.after.iter().flatten().enumerate() {
                lines.push(format!("{}-{text}", found.line + 1 + offset));
            }
        }
    }
    lines
}

/// One heading per file with its status and line counts, then one line per changed definition
/// and, with `--diff`, its `+line:text` and `-line:text` lines.
pub fn change_lines(files: &[ChangedFile]) -> Vec<String> {
    let mut lines = Vec::new();
    for file in files {
        let mut notes = Vec::new();
        match (file.status, &file.old_path) {
            (FileStatus::Renamed, Some(old_path)) => notes.push(format!("renamed from {old_path}")),
            (FileStatus::Copied, Some(old_path)) => notes.push(format!("copied from {old_path}")),
            (FileStatus::Modified | FileStatus::Renamed | FileStatus::Copied, _) => {}
            (FileStatus::Added, _) => notes.push("added".to_owned()),
            (FileStatus::Deleted, _) => notes.push("deleted".to_owned()),
            (FileStatus::Untracked, _) => notes.push("untracked".to_owned()),
        }
        notes.push(counts(file.added, file.removed));
        notes.extend(file.unparsed.map(|unparsed| {
            match unparsed {
                Unparsed::Binary => "binary",
                Unparsed::TooLarge => "larger than --max-file-size",
                Unparsed::NoParser => "no parser",
                Unparsed::TimedOut => "parsing ran out of time",
                Unparsed::Unreadable => "unreadable",
                Unparsed::OutOfSync => "the file differs from git's diff",
            }
            .to_owned()
        }));
        notes.retain(|note| !note.is_empty());
        lines.push(if notes.is_empty() {
            file.path.clone()
        } else {
            format!("{} ({})", file.path, notes.join(", "))
        });
        let changed = |line: &ChangedLine| format!("{}{}:{}", line.sign, line.line, line.text);
        lines.extend(file.lines.iter().flatten().map(changed));
        for symbol in &file.symbols {
            let status = match symbol.status {
                SymbolStatus::Added => "added",
                SymbolStatus::Removed => "removed",
                SymbolStatus::Modified => "modified",
                SymbolStatus::Moved => "moved",
            };
            let mut row = format!(
                "  {status} {} {} {}-{}",
                symbol.kind.as_str(),
                qualified(symbol.container.as_deref(), &symbol.name),
                symbol.line,
                symbol.end_line,
            );
            match &symbol.moved {
                Some(Move::From(place)) => row += &format!(" from {}:{}", place.path, place.line),
                Some(Move::To(place)) => row += &format!(" to {}:{}", place.path, place.line),
                None => {}
            }
            let counts = counts(symbol.added, symbol.removed);
            if !counts.is_empty() {
                row += &format!(" ({counts})");
            }
            lines.push(row);
            lines.extend(symbol.lines.iter().flatten().map(changed));
        }
    }
    lines
}

/// `+3 -1`, without a zero side.
fn counts(added: usize, removed: usize) -> String {
    match (added, removed) {
        (0, 0) => String::new(),
        (added, 0) => format!("+{added}"),
        (0, removed) => format!("-{removed}"),
        (added, removed) => format!("+{added} -{removed}"),
    }
}

/// One heading per run of references in the same file.
pub fn ref_lines(hits: &[RefHit]) -> Vec<String> {
    let mut lines = Vec::new();
    let mut file = None;
    for hit in hits {
        if file != Some(hit.path.as_str()) {
            lines.push(hit.path.clone());
            file = Some(hit.path.as_str());
        }
        let site = hit.enclosing.as_ref().map_or(String::new(), |enclosing| {
            format!(
                " in {}",
                qualified(enclosing.container.as_deref(), &enclosing.name)
            )
        });
        lines.push(format!(
            "{}:{} {}{site}: {}",
            hit.line,
            hit.column,
            hit.kind.as_str(),
            hit.text
        ));
    }
    lines
}

fn outline_lines(nodes: &[OutlineNode], depth: usize, lines: &mut Vec<String>) {
    for node in nodes {
        lines.push(format!(
            "{:indent$}{}-{} {} {}",
            "",
            node.line,
            node.end_line,
            node.kind.as_str(),
            node.name,
            indent = depth * 2
        ));
        outline_lines(&node.children, depth + 1, lines);
    }
}

/// `-- truncated` first when cut, then nodes as `id kind name path:line`, then edges as
/// `from -> to kind at line N` of the `from` node's file.
fn graph_lines(graph: &Graph) -> Vec<String> {
    let mut lines: Vec<String> = Vec::new();
    if graph.truncated {
        lines.push("-- truncated".to_owned());
    }
    lines.extend(graph.nodes.iter().map(|node| {
        let root = if graph.roots.contains(&node.id) {
            " (root)"
        } else {
            ""
        };
        if node.kind == "file" {
            return format!("{} file {}{root}", node.id, node.path);
        }
        format!(
            "{} {} {} {}:{}{root}",
            node.id,
            node.kind,
            qualified(node.container.as_deref(), &node.name),
            node.path,
            node.line
        )
    }));
    lines.extend(graph.edges.iter().map(|edge| {
        format!(
            "{} -> {} {} at line {}",
            edge.from,
            edge.to,
            edge.kind.as_str(),
            edge.line
        )
    }));
    if !graph.unresolved.is_empty() {
        lines.push(format!("unresolved: {}", graph.unresolved.join(", ")));
    }
    lines
}

fn kind_in(kind: SymbolKind, container: Option<&str>) -> String {
    match container {
        Some(container) => format!("{} in {container}", kind.as_str()),
        None => kind.as_str().to_owned(),
    }
}

fn qualified(container: Option<&str>, name: &str) -> String {
    match container {
        Some(container) => format!("{container}.{name}"),
        None => name.to_owned(),
    }
}

/// The hint of an `empty` text, for JSON; `None` for a bare `no results`.
fn hint(empty: &str) -> Option<&str> {
    empty.strip_prefix("no results;").map(str::trim)
}

/// `empty` is the text for no lines: `no results`, or that with a hint toward a better command.
fn joined(lines: &[String], empty: &str) -> String {
    if lines.is_empty() {
        return empty.to_owned();
    }
    // Each element is one line: a newline in a file name must not print a fake result line, and
    // an escape sequence must not reach the terminal.
    let escape = |line: &String| {
        line.chars()
            .map(|c| match c.is_control() && c != '\t' {
                true => c.escape_debug().to_string(),
                false => c.to_string(),
            })
            .collect::<String>()
    };
    lines.iter().map(escape).collect::<Vec<_>>().join("\n")
}

/// Absent fields cost an agent nothing to read, so null fields are left out.
fn drop_nulls(value: &mut Value) {
    match value {
        Value::Object(map) => {
            map.retain(|_, field| !field.is_null());
            map.values_mut().for_each(drop_nulls);
        }
        Value::Array(items) => items.iter_mut().for_each(drop_nulls),
        _ => {}
    }
}

pub fn print_error(json: bool, code: &str, message: &str) {
    let mut message = message.to_owned();
    if message.len() > MAX_ERROR_BYTES {
        let mut end = MAX_ERROR_BYTES;
        while !message.is_char_boundary(end) {
            end -= 1;
        }
        message.truncate(end);
    }
    let text = if json {
        json!({ "error": { "code": code, "message": message } }).to_string()
    } else {
        format!("lantern: {message}")
    };
    let _ = writeln!(std::io::stderr(), "{text}");
}

fn to_value<T: Serialize>(value: &T) -> Value {
    serde_json::to_value(value).expect("report types serialize to JSON")
}

/// Writes one line to stdout; a closed pipe ends the output quietly.
fn emit(text: &str) -> Result<(), Error> {
    match writeln!(std::io::stdout().lock(), "{text}") {
        Err(error) if error.kind() != std::io::ErrorKind::BrokenPipe => {
            Err(Error::failure(format!("stdout: {error}")))
        }
        _ => Ok(()),
    }
}
