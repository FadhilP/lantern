//! `graph`: what calls X, and what does X call? Edges are resolved by name.

use std::collections::{BTreeSet, HashMap, HashSet, VecDeque};

use clap::ValueEnum;
use rusqlite::{OptionalExtension, params};
use serde::Serialize;

use super::{Opened, PathFilter, validate_text};
use crate::error::{Error, Result};
use crate::extract::{RefKind, SymbolKind};
use crate::language::FileFlags;
use crate::limits::{MAX_GRAPH_DEPTH, MAX_GRAPH_NODES};
use crate::query::Freshness;
use crate::query::symbols::split_qualified;
use crate::store::{refs, shard};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, ValueEnum)]
pub enum Direction {
    Callers,
    Callees,
    Both,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct GraphQuery {
    pub name: String,
    pub direction: Direction,
    pub depth: usize,
    pub max_nodes: usize,
    /// Empty means any kind.
    pub kind: Vec<SymbolKind>,
    pub path: Vec<String>,
    pub include_generated: bool,
}

#[derive(Debug, Clone, Serialize)]
pub struct Graph {
    pub freshness: Freshness,
    pub indexed_at: Option<i64>,
    pub resolution: &'static str,
    pub roots: Vec<usize>,
    pub nodes: Vec<Node>,
    pub edges: Vec<Edge>,
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub truncated: bool,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub unresolved: Vec<String>,
    /// Counted only for an empty graph.
    #[serde(skip)]
    pub incomplete: shard::Incomplete,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Node {
    pub id: usize,
    pub name: String,
    pub container: Option<String>,
    /// A symbol kind, or `file` for references outside any definition.
    pub kind: &'static str,
    pub path: String,
    pub line: u32,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Edge {
    pub from: usize,
    pub to: usize,
    pub kind: RefKind,
    /// The line of the reference in the file of the `from` node.
    pub line: u32,
}

/// A graph vertex.
#[derive(Debug, Clone)]
enum Vertex {
    Definition(Definition),
    /// The calls outside any definition in the file at this path.
    File(String),
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
enum VertexKey {
    Symbol(i64),
    File(String),
}

impl Vertex {
    fn key(&self) -> VertexKey {
        match self {
            Self::Definition(definition) => VertexKey::Symbol(definition.id),
            Self::File(path) => VertexKey::File(path.clone()),
        }
    }
}

#[derive(Debug, Clone)]
struct Definition {
    id: i64,
    name: String,
    container: Option<String>,
    kind: SymbolKind,
    path: String,
    line: u32,
    test: bool,
}

struct Builder<'graph> {
    opened: &'graph Opened,
    filter: &'graph PathFilter,
    include_generated: bool,
    max_nodes: usize,
    ids: HashMap<VertexKey, usize>,
    nodes: Vec<Node>,
    definitions: HashMap<usize, Definition>,
    edges: Vec<Edge>,
    edge_keys: HashSet<(usize, usize, RefKind)>,
    truncated: bool,
    unresolved: BTreeSet<String>,
    by_name: HashMap<String, Vec<Definition>>,
}

pub(crate) fn run(opened: &Opened, query: &GraphQuery) -> Result<Graph> {
    validate_text("the name", &query.name)?;
    if query.depth == 0 || query.depth > MAX_GRAPH_DEPTH {
        return Err(Error::usage(format!(
            "--depth must be between 1 and {MAX_GRAPH_DEPTH}"
        )));
    }
    if query.max_nodes == 0 || query.max_nodes > MAX_GRAPH_NODES {
        return Err(Error::usage(format!(
            "--max-nodes must be between 1 and {MAX_GRAPH_NODES}"
        )));
    }
    let filter = PathFilter::new(&query.path, &opened.scope)?;
    let mut builder = Builder {
        opened,
        filter: &filter,
        include_generated: query.include_generated,
        max_nodes: query.max_nodes,
        ids: HashMap::new(),
        nodes: Vec::new(),
        definitions: HashMap::new(),
        edges: Vec::new(),
        edge_keys: HashSet::new(),
        truncated: false,
        unresolved: BTreeSet::new(),
        by_name: HashMap::new(),
    };
    let (container, name) = split_qualified(&query.name);
    let in_container = |definition: &Definition| {
        container.is_none_or(|container| {
            definition
                .container
                .as_deref()
                .is_some_and(|found| found.eq_ignore_ascii_case(container))
        })
    };
    let mut roots = Vec::new();
    for definition in builder.definitions_named(name)? {
        if !query.kind.is_empty() && !query.kind.contains(&definition.kind) {
            continue;
        }
        if !in_container(&definition) {
            continue;
        }
        if let Some(id) = builder.node(&Vertex::Definition(definition)) {
            roots.push(id);
        }
    }

    let mut queue: VecDeque<(usize, usize)> = roots.iter().map(|&id| (id, 1)).collect();
    let mut expanded = HashSet::new();
    while let Some((id, depth)) = queue.pop_front() {
        if !expanded.insert(id) || builder.truncated {
            continue;
        }
        let Some(definition) = builder.definitions.get(&id).cloned() else {
            continue;
        };
        let mut next = Vec::new();
        if query.direction != Direction::Callees {
            next.extend(builder.callers(id, &definition)?);
        }
        if query.direction != Direction::Callers {
            next.extend(builder.callees(id, &definition)?);
        }
        if depth < query.depth {
            queue.extend(next.into_iter().map(|id| (id, depth + 1)));
        }
    }

    let incomplete = if builder.nodes.is_empty() {
        opened.incomplete(&filter)?
    } else {
        shard::Incomplete::default()
    };
    Ok(Graph {
        freshness: opened.freshness,
        indexed_at: opened.indexed_at,
        resolution: "name",
        roots,
        nodes: builder.nodes,
        edges: builder.edges,
        truncated: builder.truncated,
        unresolved: builder.unresolved.into_iter().collect(),
        incomplete,
    })
}

impl Builder<'_> {
    /// The node id of a vertex, adding it unless the node cap is reached.
    fn node(&mut self, vertex: &Vertex) -> Option<usize> {
        let key = vertex.key();
        if let Some(&id) = self.ids.get(&key) {
            return Some(id);
        }
        if self.nodes.len() >= self.max_nodes {
            self.truncated = true;
            return None;
        }
        let id = self.nodes.len();
        self.nodes.push(match vertex {
            Vertex::Definition(definition) => Node {
                id,
                name: definition.name.clone(),
                container: definition.container.clone(),
                kind: definition.kind.as_str(),
                path: definition.path.clone(),
                line: definition.line,
            },
            Vertex::File(path) => Node {
                id,
                name: path.clone(),
                container: None,
                kind: "file",
                path: path.clone(),
                line: 1,
            },
        });
        self.ids.insert(key, id);
        if let Vertex::Definition(definition) = vertex {
            self.definitions.insert(id, definition.clone());
        }
        Some(id)
    }

    /// Every non-generated definition of `name`, in path order.
    fn definitions_named(&mut self, name: &str) -> Result<Vec<Definition>> {
        if let Some(found) = self.by_name.get(name) {
            return Ok(found.clone());
        }
        // Every path of a shared blob, so the first one in scope names the definition.
        let mut statement = self.opened.conn.prepare_cached(
            "SELECT s.id, s.kind, s.container, s.line, f.path, f.flags
             FROM names n JOIN symbols s ON s.name_id = n.id
             JOIN files f ON f.blob_id = s.blob_id AND (?2 OR f.flags & 1 = 0)
             WHERE n.name = ?1 ORDER BY s.id, f.path",
        )?;
        let rows = statement.query_map(params![name, self.include_generated], |row| {
            Ok(Definition {
                id: row.get(0)?,
                name: name.to_owned(),
                kind: row.get(1)?,
                container: row.get(2)?,
                line: row.get(3)?,
                path: row.get(4)?,
                test: FileFlags(row.get(5)?).is_test(),
            })
        })?;
        let mut found: Vec<Definition> = Vec::new();
        for row in rows {
            let definition = row?;
            let placed = found.last().is_some_and(|last| last.id == definition.id);
            if !placed && self.opened.accept(&definition.path, self.filter) {
                found.push(definition);
            }
        }
        found.sort_by(|a, b| (&a.path, a.line).cmp(&(&b.path, b.line)));
        self.by_name.insert(name.to_owned(), found.clone());
        Ok(found)
    }

    /// Calls to the definition's name, grouped by the definition that contains each one.
    fn callers(&mut self, target: usize, definition: &Definition) -> Result<Vec<usize>> {
        let mut statement = self.opened.conn.prepare_cached(
            "SELECT f.path, f.flags, n.id, r.groups
             FROM names n JOIN ref_names rn ON rn.name_id = n.id
             JOIN refs r ON r.blob_id = rn.blob_id
             JOIN files f ON f.blob_id = rn.blob_id AND (?2 OR f.flags & 1 = 0)
             WHERE n.name = ?1",
        )?;
        let mut enclosing = self.opened.conn.prepare_cached(
            "SELECT n.name, s.container, s.kind, s.line
             FROM symbols s JOIN names n ON n.id = s.name_id WHERE s.id = ?1",
        )?;
        let mut rows = statement.query(params![definition.name, self.include_generated])?;
        let mut sources = Vec::new();
        while let Some(row) = rows.next()? {
            let path: String = row.get(0)?;
            if !self.opened.accept(&path, self.filter) {
                continue;
            }
            let test = FileFlags(row.get(1)?).is_test();
            let groups: Vec<u8> = row.get(3)?;
            for call in refs::of_name(&groups, row.get(2)?)? {
                if call.kind != RefKind::Call {
                    continue;
                }
                let symbol = match call.enclosing {
                    Some(id) => enclosing
                        .query_row([id], |found| {
                            Ok(Definition {
                                id,
                                name: found.get(0)?,
                                container: found.get(1)?,
                                kind: found.get(2)?,
                                path: path.clone(),
                                line: found.get(3)?,
                                test,
                            })
                        })
                        .optional()?,
                    None => None,
                };
                let source = match symbol {
                    Some(definition) => Vertex::Definition(definition),
                    None => Vertex::File(path.clone()),
                };
                sources.push((source, call.kind, path.clone(), call.line));
            }
        }
        drop(rows);
        drop(statement);
        drop(enclosing);
        sources.sort_by(|a, b| (&a.2, a.3).cmp(&(&b.2, b.3)));
        let mut added = Vec::new();
        for (source, kind, _, line) in sources {
            let Some(from) = self.node(&source) else {
                break;
            };
            self.edge(from, target, kind, line);
            added.push(from);
        }
        Ok(added)
    }

    /// Calls whose innermost definition is this one, resolved by name; code outside tests
    /// never resolves to a definition in a test file.
    fn callees(&mut self, source: usize, definition: &Definition) -> Result<Vec<usize>> {
        let conn = &self.opened.conn;
        let blob_id: i64 = conn
            .prepare_cached("SELECT blob_id FROM symbols WHERE id = ?1")?
            .query_row([definition.id], |row| row.get(0))?;
        let mut calls = refs::of_blob(conn, blob_id)?;
        calls.retain(|(_, call)| {
            call.enclosing == Some(definition.id) && call.kind == RefKind::Call
        });
        calls.sort_by_key(|&(_, call)| (call.line, call.col));
        let mut added = Vec::new();
        for (name_id, call) in calls {
            let (kind, line) = (call.kind, call.line);
            let name: String = conn
                .prepare_cached("SELECT name FROM names WHERE id = ?1")?
                .query_row([name_id], |row| row.get(0))?;
            let mut targets = self.definitions_named(&name)?;
            if !definition.test {
                targets.retain(|target| !target.test);
            }
            if targets.is_empty() {
                self.unresolved.insert(name);
                continue;
            }
            for target in targets {
                let Some(to) = self.node(&Vertex::Definition(target)) else {
                    return Ok(added);
                };
                self.edge(source, to, kind, line);
                added.push(to);
            }
        }
        Ok(added)
    }

    /// Keeps one edge per pair and kind, at its first site.
    fn edge(&mut self, from: usize, to: usize, kind: RefKind, line: u32) {
        if self.edge_keys.insert((from, to, kind)) {
            self.edges.push(Edge {
                from,
                to,
                kind,
                line,
            });
        }
    }
}
