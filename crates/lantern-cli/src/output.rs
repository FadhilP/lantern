//! Compact text for agents and people by default, JSON documents with `--json`, and errors on
//! stderr.

use std::io::Write;

use crate::error::{Error, ErrorKind};
use crate::extract::SymbolKind;
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

    /// Prints a report as one document; text output gets it indented.
    pub fn document<T: Serialize>(&self, report: &T) {
        let mut document = to_value(report);
        document["schema"] = json!(SCHEMA);
        drop_nulls(&mut document);
        if self.json {
            emit(&document.to_string());
        } else {
            emit(&serde_json::to_string_pretty(&document).unwrap_or_default());
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
                if let Some(cursor) = next_cursor {
                    lines.push(format!("-- more: --cursor {cursor}"));
                }
                return joined(lines, empty);
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
            for (key, value) in extra {
                document[*key] = value.clone();
            }
            drop_nulls(&mut document);
            document.to_string()
        };
        emit(&fit(page.rows.len(), max_bytes, render)?);
        Ok(())
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
                return joined(graph_lines(&kept), empty);
            }
            let mut document = to_value(&kept);
            document["schema"] = json!(SCHEMA);
            drop_nulls(&mut document);
            document.to_string()
        };
        emit(&fit(graph.nodes.len(), max_bytes, render)?);
        Ok(())
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
            format!("--max-bytes {max_bytes} is smaller than one result"),
        ));
    }
    Ok(render(low))
}

pub fn symbol_lines(hits: &[SymbolHit]) -> Vec<String> {
    hits.iter()
        .map(|hit| {
            format!(
                "{}:{}-{} {}: {}",
                hit.path,
                hit.line,
                hit.end_line,
                kind_in(hit.kind, hit.container.as_deref()),
                hit.signature
            )
        })
        .collect()
}

/// One heading per file, then rg-style `line:text` matches and `line-text` context.
pub fn code_lines(hits: &[CodeHit]) -> Vec<String> {
    let mut lines = Vec::new();
    for hit in hits {
        let shown = hit.matches.len();
        lines.push(if hit.match_count > shown {
            format!("{} ({} matches, {shown} shown)", hit.path, hit.match_count)
        } else {
            hit.path.clone()
        });
        for found in &hit.matches {
            let before = found.before.as_deref().unwrap_or_default();
            let first = found.line - before.len();
            for (offset, text) in before.iter().enumerate() {
                lines.push(format!("{}-{text}", first + offset));
            }
            lines.push(format!("{}:{}", found.line, found.text));
            for (offset, text) in found.after.iter().flatten().enumerate() {
                lines.push(format!("{}-{text}", found.line + 1 + offset));
            }
        }
    }
    lines
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
            "{} {}{site}: {}",
            hit.line,
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

/// Nodes as `id kind name path:line`, then edges as `from -> to kind at line N` of the `from`
/// node's file.
fn graph_lines(graph: &Graph) -> Vec<String> {
    let mut lines: Vec<String> = graph
        .nodes
        .iter()
        .map(|node| {
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
        })
        .collect();
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
    if graph.truncated {
        lines.push("-- truncated".to_owned());
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

/// `empty` is the text for no lines: `no results`, or that with a hint toward a better command.
fn joined(lines: Vec<String>, empty: &str) -> String {
    if lines.is_empty() {
        return empty.to_owned();
    }
    lines.join("\n")
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
fn emit(text: &str) {
    let _ = writeln!(std::io::stdout().lock(), "{text}");
}
