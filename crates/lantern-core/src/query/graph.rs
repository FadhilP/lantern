//! `graph`: what calls X, and what does X call? Edges are resolved by name across every member.

use std::collections::{BTreeSet, HashMap, HashSet, VecDeque};

use rusqlite::params;
use serde::Serialize;

use super::{Opened, PathFilter, validate_text};
use crate::error::{Error, Result};
use crate::extract::{RefKind, SymbolKind};
use crate::language::FileFlags;
use crate::limits::{MAX_GRAPH_DEPTH, MAX_GRAPH_NODES};
use crate::query::Freshness;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub enum Direction {
    Callers,
    Callees,
    #[default]
    Both,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct GraphQuery {
    pub name: String,
    pub direction: Direction,
    pub depth: usize,
    pub max_nodes: usize,
    pub kind: Option<SymbolKind>,
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

/// A graph vertex: a definition, or a file for top-level references.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
enum Key {
    Symbol { member: usize, id: i64 },
    File { member: usize, path: String },
}

#[derive(Debug, Clone)]
struct Definition {
    key: Key,
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
    ids: HashMap<Key, usize>,
    nodes: Vec<Node>,
    definitions: HashMap<usize, Definition>,
    edges: Vec<Edge>,
    edge_keys: HashSet<(usize, usize, RefKind)>,
    truncated: bool,
    unresolved: BTreeSet<String>,
    by_name: HashMap<String, Vec<Definition>>,
}

pub(crate) fn run(opened: &Opened, scope: &str, query: &GraphQuery) -> Result<Graph> {
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
    let filter = PathFilter::new(&query.path, scope)?;
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
    let name = query.name.trim();
    let mut roots = Vec::new();
    for definition in builder.definitions_named(name)? {
        if query.kind.is_some_and(|kind| kind != definition.kind) {
            continue;
        }
        if let Some(id) = builder.node(&definition) {
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

    Ok(Graph {
        freshness: opened.freshness,
        indexed_at: opened.indexed_at,
        resolution: "name",
        roots,
        nodes: builder.nodes,
        edges: builder.edges,
        truncated: builder.truncated,
        unresolved: builder.unresolved.into_iter().collect(),
    })
}

impl Builder<'_> {
    /// The node id of a vertex, adding it unless the node cap is reached.
    fn node(&mut self, definition: &Definition) -> Option<usize> {
        if let Some(&id) = self.ids.get(&definition.key) {
            return Some(id);
        }
        if self.nodes.len() >= self.max_nodes {
            self.truncated = true;
            return None;
        }
        let id = self.nodes.len();
        let kind = match definition.key {
            Key::Symbol { .. } => definition.kind.as_str(),
            Key::File { .. } => "file",
        };
        self.nodes.push(Node {
            id,
            name: definition.name.clone(),
            container: definition.container.clone(),
            kind,
            path: definition.path.clone(),
            line: definition.line,
        });
        self.ids.insert(definition.key.clone(), id);
        if matches!(definition.key, Key::Symbol { .. }) {
            self.definitions.insert(id, definition.clone());
        }
        Some(id)
    }

    /// Every non-generated definition of `name` in every member, in path order.
    fn definitions_named(&mut self, name: &str) -> Result<Vec<Definition>> {
        if let Some(found) = self.by_name.get(name) {
            return Ok(found.clone());
        }
        let mut found = Vec::new();
        for (index, member) in self.opened.members.iter().enumerate() {
            // Every path of a shared blob, so the first one in scope names the definition.
            let mut statement = member.conn.prepare_cached(
                "SELECT s.id, s.kind, s.container, s.line, f.path, f.flags
                 FROM names n JOIN symbols s ON s.name_id = n.id
                 JOIN files f ON f.blob_id = s.blob_id AND (?2 OR f.flags & 1 = 0)
                 WHERE n.name = ?1 ORDER BY s.id, f.path",
            )?;
            let rows = statement.query_map(params![name, self.include_generated], |row| {
                Ok((
                    row.get::<_, i64>(0)?,
                    row.get::<_, SymbolKind>(1)?,
                    row.get::<_, Option<String>>(2)?,
                    row.get::<_, u32>(3)?,
                    row.get::<_, String>(4)?,
                    FileFlags(row.get(5)?),
                ))
            })?;
            let mut placed = HashSet::new();
            for row in rows {
                let (id, kind, container, line, path, flags) = row?;
                if placed.contains(&id) {
                    continue;
                }
                let Some(path) = member.accept(&path, self.filter) else {
                    continue;
                };
                placed.insert(id);
                found.push(Definition {
                    key: Key::Symbol { member: index, id },
                    name: name.to_owned(),
                    container,
                    kind,
                    path,
                    line,
                    test: flags.is_test(),
                });
            }
        }
        found.sort_by(|a, b| (&a.path, a.line).cmp(&(&b.path, b.line)));
        self.by_name.insert(name.to_owned(), found.clone());
        Ok(found)
    }

    /// Calls to the definition's name, grouped by the definition that contains each one.
    fn callers(&mut self, target: usize, definition: &Definition) -> Result<Vec<usize>> {
        let mut sources = Vec::new();
        for (index, member) in self.opened.members.iter().enumerate() {
            let mut statement = member.conn.prepare_cached(
                "SELECT r.kind, r.line, f.path, es.id, en.name, es.kind, es.container, es.line, f.flags
                 FROM names n JOIN refs r ON r.name_id = n.id
                 JOIN files f ON f.blob_id = r.blob_id AND (?2 OR f.flags & 1 = 0)
                 LEFT JOIN symbols es ON es.id = r.enclosing
                 LEFT JOIN names en ON en.id = es.name_id
                 WHERE n.name = ?1 AND r.kind = 'call'",
            )?;
            let rows =
                statement.query_map(params![definition.name, self.include_generated], |row| {
                    Ok((
                        row.get::<_, RefKind>(0)?,
                        row.get::<_, u32>(1)?,
                        row.get::<_, String>(2)?,
                        row.get::<_, Option<i64>>(3)?,
                        row.get::<_, Option<String>>(4)?,
                        row.get::<_, Option<SymbolKind>>(5)?,
                        row.get::<_, Option<String>>(6)?,
                        row.get::<_, Option<u32>>(7)?,
                        FileFlags(row.get(8)?),
                    ))
                })?;
            for row in rows {
                let (kind, line, path, symbol, name, symbol_kind, container, symbol_line, flags) =
                    row?;
                let Some(path) = member.accept(&path, self.filter) else {
                    continue;
                };
                let source = match (symbol, name, symbol_kind) {
                    (Some(id), Some(name), Some(kind)) => Definition {
                        key: Key::Symbol { member: index, id },
                        name,
                        container,
                        kind,
                        path: path.clone(),
                        line: symbol_line.unwrap_or(line),
                        test: flags.is_test(),
                    },
                    _ => Definition {
                        key: Key::File {
                            member: index,
                            path: path.clone(),
                        },
                        name: path.clone(),
                        container: None,
                        kind: SymbolKind::Module,
                        path: path.clone(),
                        line: 1,
                        test: flags.is_test(),
                    },
                };
                sources.push((source, kind, path, line));
            }
        }
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
        let Key::Symbol { member, id } = definition.key else {
            return Ok(Vec::new());
        };
        let conn = &self.opened.members[member].conn;
        let mut statement = conn.prepare_cached(
            "SELECT n.name, r.kind, r.line FROM refs r JOIN names n ON n.id = r.name_id
             WHERE r.enclosing = ?1 AND r.kind = 'call' ORDER BY r.line, r.col",
        )?;
        let calls: Vec<(String, RefKind, u32)> = statement
            .query_map(params![id], |row| {
                Ok((row.get(0)?, row.get(1)?, row.get(2)?))
            })?
            .collect::<rusqlite::Result<_>>()?;
        let mut added = Vec::new();
        for (name, kind, line) in calls {
            let mut targets = self.definitions_named(&name)?;
            if !definition.test {
                targets.retain(|target| !target.test);
            }
            if targets.is_empty() {
                self.unresolved.insert(name);
                continue;
            }
            for target in targets {
                let Some(to) = self.node(&target) else {
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
