//! `changes`: the definitions that a Git diff touches. Both sides of each changed file are parsed
//! on demand; the index is not used.

use std::borrow::Cow;
use std::collections::{HashMap, HashSet};
use std::path::Path;
use std::sync::OnceLock;

use rayon::prelude::*;
use serde::Serialize;

use crate::error::{Error, Result};
use crate::extract::{self, Symbol, SymbolKind, truncate_chars};
use crate::git::{self, FileDiff, FileStatus, Line, Side};
use crate::language::Language;
use crate::limits::TEXT_SNIFF_BYTES;
use crate::paths;
use crate::query::symbols::split_qualified;
use crate::query::{self, Page, PageRequest, PathFilter};

#[derive(Debug, Clone)]
pub struct ChangesQuery {
    /// As `git diff` takes it; `None` is the working tree and the index against HEAD.
    pub range: Option<String>,
    pub path: Vec<String>,
    /// `Some`: print the changed lines. `Some(Some(name))`: only definitions with this name,
    /// `Container.name` too.
    pub diff: Option<Option<String>>,
}

#[derive(Debug, Clone, Serialize)]
pub struct ChangedFile {
    pub path: String,
    pub old_path: Option<String>,
    pub status: FileStatus,
    pub added: usize,
    pub removed: usize,
    /// Why the file has no definitions.
    pub unparsed: Option<Unparsed>,
    pub symbols: Vec<ChangedSymbol>,
    /// With `--diff` and no name: the changed lines outside the listed definitions.
    pub lines: Option<Vec<ChangedLine>>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Unparsed {
    Binary,
    TooLarge,
    NoParser,
    TimedOut,
    Unreadable,
    /// The file's text does not have the diff's lines where git put them: a clean filter or an
    /// encoding changed it for git, or it changed since git read it.
    OutOfSync,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum SymbolStatus {
    Added,
    Removed,
    Modified,
    /// The same text, without filler lines, is removed in one place and added in another.
    Moved,
}

#[derive(Debug, Clone, Serialize)]
pub struct ChangedSymbol {
    pub status: SymbolStatus,
    pub kind: SymbolKind,
    pub name: String,
    pub container: Option<String>,
    /// On the new side; on the old side for a removed definition.
    pub line: u32,
    pub end_line: u32,
    /// Set exactly when the status is `moved`.
    #[serde(flatten)]
    pub moved: Option<Move>,
    pub added: usize,
    pub removed: usize,
    /// Set only with `--diff`.
    pub lines: Option<Vec<ChangedLine>>,
}

/// The other end of a move: where a moved definition came from, or where it went.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Move {
    From(Place),
    To(Place),
}

/// A path, and the first line there that is not filler.
#[derive(Debug, Clone, Serialize)]
pub struct Place {
    pub path: String,
    pub line: u32,
}

#[derive(Debug, Clone, Serialize)]
pub struct ChangedLine {
    pub sign: char,
    pub line: u32,
    pub text: String,
}

/// The changed files under `scope` in `root`, one page of them.
pub(crate) fn run(
    root: &Path,
    scope: &str,
    query: &ChangesQuery,
    request: &PageRequest,
    max_file_size: u64,
) -> Result<Page<ChangedFile>> {
    if let Some(range) = &query.range
        && (range.is_empty() || range.starts_with('-'))
    {
        return Err(Error::usage(format!(
            "`{range}` is not a revision range; pass one such as `main...HEAD`"
        )));
    }
    query::validate_limit(request)?;
    let filter = PathFilter::new(&query.path, scope)?;
    let mut diff = git::diff(root, query.range.as_deref())?;
    // Every untracked file is read, not only the page's: a definition can move into any of them.
    diff.files
        .par_iter_mut()
        .filter(|file| file.status == FileStatus::Untracked)
        .for_each(|file| {
            let no_blobs = HashMap::new();
            if let Ok(Content::Text(text)) =
                content(root, max_file_size, &file.path, &file.new, &no_blobs)
            {
                file.lines = (1..)
                    .zip(text.lines())
                    .map(|(line, text)| Line {
                        added: true,
                        line,
                        hunk: 0,
                        text: text.to_owned(),
                    })
                    .collect();
            }
        });
    // Git's output names untracked files but has none of their lines.
    let mut state = blake3::Hasher::new();
    state.update(diff.state.as_bytes());
    for file in &diff.files {
        if file.status == FileStatus::Untracked {
            for line in &file.lines {
                state.update(line.text.as_bytes());
                state.update(b"\n");
            }
        }
    }
    let state = state.finalize().to_hex()[..16].to_owned();
    let rows: Vec<usize> = (0..diff.files.len())
        .filter(|&at| {
            let file = &diff.files[at];
            let mut paths = std::iter::once(&file.path).chain(&file.old_path);
            paths.any(|path| paths::is_within(path, scope) && filter.matches(path))
        })
        .collect();
    let key = format!(
        "changes\0{scope}\0{}\0{}\0{:?}",
        query.range.as_deref().unwrap_or_default(),
        query.path.join("\0"),
        query.diff
    );
    let reader = Reader {
        root,
        max_file_size,
        files: &diff.files,
        moves: Moves::new(root, max_file_size, &diff.files),
        lines: query.diff.is_some(),
        name: query
            .diff
            .as_ref()
            .and_then(Option::as_deref)
            .map(split_qualified),
        line_chars: request.line_chars,
    };
    if reader.name.is_none() {
        // Only the files of the page are read and parsed.
        let page = query::page(&state, &key, request, rows)?;
        return page.try_map(|rows| reader.read(&rows));
    }
    let mut changed = reader.read(&rows)?;
    changed.retain(|file| !file.symbols.is_empty());
    query::page(&state, &key, request, changed)
}

/// The changed definitions of a file, and the indexes of its changed lines outside them.
type Definitions = (Vec<ChangedSymbol>, Vec<usize>);

struct Reader<'a> {
    root: &'a Path,
    max_file_size: u64,
    /// Every file of the diff.
    files: &'a [FileDiff],
    moves: Moves<'a>,
    /// Print the changed lines.
    lines: bool,
    name: Option<(Option<&'a str>, &'a str)>,
    line_chars: usize,
}

impl Reader<'_> {
    fn read(&self, rows: &[usize]) -> Result<Vec<ChangedFile>> {
        let ids: Vec<String> = rows
            .iter()
            .map(|&at| &self.files[at])
            .filter(|file| !file.binary && !file.lines.is_empty())
            .flat_map(|file| [&file.old, &file.new])
            .filter_map(|side| match side {
                Side::Blob(id) | Side::Hashed(id) => Some(id.clone()),
                _ => None,
            })
            .collect();
        let blobs = git::read_blobs(self.root, &ids)?;
        rows.par_iter().map(|&at| self.file(at, &blobs)).collect()
    }

    fn file(&self, at: usize, blobs: &HashMap<String, Vec<u8>>) -> Result<ChangedFile> {
        let file = &self.files[at];
        let lines = &file.lines;
        let (unparsed, symbols, outside) = match self.definitions(at, blobs)? {
            Ok((symbols, outside)) => (None, symbols, outside),
            Err(unparsed) => (Some(unparsed), Vec::new(), (0..lines.len()).collect()),
        };
        let added = lines.iter().filter(|line| line.added).count();
        let show_outside = self.lines && self.name.is_none() && !outside.is_empty();
        Ok(ChangedFile {
            path: file.path.clone(),
            old_path: file.old_path.clone(),
            status: file.status,
            added,
            removed: lines.len() - added,
            unparsed,
            symbols,
            lines: show_outside.then(|| self.changed_lines(lines, &outside)),
        })
    }

    /// The changed definitions of the file at `at` and the indexes of the changed lines outside
    /// them, or why the file has no definitions.
    fn definitions(
        &self,
        at: usize,
        blobs: &HashMap<String, Vec<u8>>,
    ) -> Result<std::result::Result<Definitions, Unparsed>> {
        let file = &self.files[at];
        if file.binary {
            return Ok(Err(Unparsed::Binary));
        }
        // A rename or a mode change without an edit: neither side needs to be read.
        // An untracked file without lines is read anyway, for why it has none.
        if file.lines.is_empty() && file.status != FileStatus::Untracked {
            return Ok(Ok((Vec::new(), Vec::new())));
        }
        let old_path = file.old_path.as_deref().unwrap_or(&file.path);
        let old = content(self.root, self.max_file_size, old_path, &file.old, blobs)?;
        let new = content(self.root, self.max_file_size, &file.path, &file.new, blobs)?;
        let (old, new) = match (old, new) {
            (Content::Binary, _) | (_, Content::Binary) => return Ok(Err(Unparsed::Binary)),
            (Content::TooLarge, _) | (_, Content::TooLarge) => {
                return Ok(Err(Unparsed::TooLarge));
            }
            (Content::Unreadable, _) | (_, Content::Unreadable) => {
                return Ok(Err(Unparsed::Unreadable));
            }
            (Content::Text(old), Content::Text(new)) => (Some(old), Some(new)),
            (Content::Text(old), Content::Absent) => (Some(old), None),
            (Content::Absent, Content::Text(new)) => (None, Some(new)),
            (Content::Absent, Content::Absent) => (None, None),
        };
        let detected = match (&new, &old) {
            (Some(text), _) => Language::detect(&file.path, text),
            (None, Some(text)) => Language::detect(old_path, text),
            (None, None) => None,
        };
        let Some(language) = detected else {
            return Ok(Err(Unparsed::NoParser));
        };
        let parse = |text: &Option<String>| {
            text.as_deref()
                .map(|text| extract::extract(language, text))
                .unwrap_or_default()
        };
        let (old_found, new_found) = (parse(&old), parse(&new));
        if old_found.timed_out || new_found.timed_out {
            return Ok(Err(Unparsed::TimedOut));
        }
        let old = View::new(&old_found.symbols, old.as_deref(), &file.lines, false);
        let new = View::new(&new_found.symbols, new.as_deref(), &file.lines, true);
        if !old.agrees(&file.lines, false) || !new.agrees(&file.lines, true) {
            return Ok(Err(Unparsed::OutOfSync));
        }
        Ok(Ok(self.symbols(at, &old, &new)))
    }

    /// Each changed line belongs to one definition around it (see `owner`). A definition is
    /// added when all its lines are new, removed when all are gone, and modified when some
    /// changed. A wholly added or removed definition is moved when the diff has its text on the
    /// other side in another place. Also returns the indexes of the lines outside them all.
    ///
    /// Filler lines, without a letter or digit (blank lines, lone braces), do not count: diffs
    /// often match a `}` of one definition with the `}` of another.
    fn symbols(&self, at: usize, old: &View, new: &View) -> (Vec<ChangedSymbol>, Vec<usize>) {
        let lines = &self.files[at].lines;
        let mut found: Vec<Found> = Vec::new();
        let mut whole: HashMap<(bool, usize), usize> = HashMap::new();
        let mut modified: HashMap<Key, usize> = HashMap::new();
        for filler_pass in [false, true] {
            for (index, line) in lines.iter().enumerate() {
                let view = if line.added { new } else { old };
                if view.is_filler(line.line) != filler_pass {
                    continue;
                }
                let Some(innermost) = view.innermost[index] else {
                    continue;
                };
                let symbol = self.owner(view, innermost);
                if view.whole[symbol] {
                    let at = *whole.entry((line.added, symbol)).or_insert_with(|| {
                        found.push(Found::whole(line.added, symbol));
                        found.len() - 1
                    });
                    found[at].lines.push(index);
                    continue;
                }
                let key = view.key(symbol);
                let at = match modified.get(&key) {
                    Some(&at) => at,
                    // A filler line joins a definition only when other lines changed it.
                    None if filler_pass => continue,
                    None => {
                        found.push(Found::modified());
                        modified.insert(key, found.len() - 1);
                        found.len() - 1
                    }
                };
                let side = if line.added {
                    &mut found[at].new
                } else {
                    &mut found[at].old
                };
                *side = Some(symbol);
                found[at].lines.push(index);
            }
        }
        let mut outside = vec![true; lines.len()];
        for &index in found.iter().flat_map(|entry| &entry.lines) {
            outside[index] = false;
        }
        let mut inner = Vec::new();
        for entry in &mut found {
            inner.extend(self.find_moves(entry, at, old, new));
        }
        found.extend(inner);
        let merged = pair_rewrites(&mut found, old, new);
        // A definition only shortened on the old side still shows its new lines.
        for entry in &mut found {
            if entry.status == SymbolStatus::Modified && entry.new.is_none() {
                entry.new = entry.old.and_then(|symbol| new.find(&old.key(symbol)));
            }
        }
        let mut symbols: Vec<ChangedSymbol> = found
            .into_iter()
            .enumerate()
            .filter(|(at, _)| !merged.contains(at))
            .filter_map(|(_, entry)| self.changed_symbol(entry, old, new, lines))
            .collect();
        symbols.sort_by_key(|symbol| (symbol.line, symbol.end_line));
        let outside = (0..lines.len())
            .filter(|&index| outside[index] && !filler(&lines[index].text))
            .collect();
        (symbols, outside)
    }

    /// Marks a wholly added or removed definition of file `at` moved when the diff has its text
    /// on the other side. Otherwise returns the moved definitions inside it, such as a method
    /// moved into a new class, and takes their lines from it.
    fn find_moves(&self, entry: &mut Found, at: usize, old: &View, new: &View) -> Vec<Found> {
        let (added, view, symbol) = match (entry.status, entry.old, entry.new) {
            (SymbolStatus::Added, _, Some(symbol)) => (true, new, symbol),
            // A copy's removed lines are still in its source.
            (SymbolStatus::Removed, Some(symbol), _)
                if self.files[at].status != FileStatus::Copied =>
            {
                (false, old, symbol)
            }
            _ => return Vec::new(),
        };
        if !self.matches(&view.symbols[symbol]) {
            return Vec::new();
        }
        let lines = &self.files[at].lines;
        let hunks: HashSet<u32> = entry.lines.iter().map(|&line| lines[line].hunk).collect();
        let place = |inner: usize| {
            let place = self.moves.find(!added, &view.body(inner), at, &hunks)?;
            Some(if added {
                Move::From(place)
            } else {
                Move::To(place)
            })
        };
        if let Some(moved) = place(symbol) {
            entry.status = SymbolStatus::Moved;
            entry.moved = Some(moved);
            entry.lines.clear();
            return Vec::new();
        }
        // With a name, a matching container keeps the lines of everything inside it.
        if self.name.is_some() {
            return Vec::new();
        }
        // Everything inside a wholly changed definition is wholly changed too.
        let mut found = Vec::new();
        let mut inner: Vec<usize> = view.children[symbol].iter().rev().copied().collect();
        while let Some(index) = inner.pop() {
            let Some(moved) = place(index) else {
                inner.extend(view.children[index].iter().rev());
                continue;
            };
            let symbol = &view.symbols[index];
            let span = symbol.start_line..=symbol.end_line;
            entry
                .lines
                .retain(|&line| !span.contains(&lines[line].line));
            let mut moved_entry = Found::whole(added, index);
            moved_entry.status = SymbolStatus::Moved;
            moved_entry.moved = Some(moved);
            found.push(moved_entry);
        }
        found
    }

    /// The definition that a line inside `innermost` counts for. With a name filter: the nearest
    /// one around it with that name. Without: the outermost of the wholly changed ones around it.
    fn owner(&self, view: &View, innermost: usize) -> usize {
        let mut around = std::iter::successors(Some(innermost), |&symbol| view.parent[symbol]);
        if self.name.is_some() {
            return around
                .find(|&symbol| self.matches(&view.symbols[symbol]))
                .unwrap_or(innermost);
        }
        around
            .take_while(|&symbol| view.whole[symbol])
            .last()
            .unwrap_or(innermost)
    }

    /// True without a name filter.
    fn matches(&self, symbol: &Symbol) -> bool {
        let Some((container, name)) = self.name else {
            return true;
        };
        let container_matches = match (container, symbol.container.as_deref()) {
            (None, _) => true,
            (Some(wanted), Some(found)) => wanted.eq_ignore_ascii_case(found),
            (Some(_), None) => false,
        };
        symbol.name == name && container_matches
    }

    fn changed_symbol(
        &self,
        mut entry: Found,
        old: &View,
        new: &View,
        lines: &[Line],
    ) -> Option<ChangedSymbol> {
        let symbol = match (entry.new, entry.old) {
            (Some(index), _) => &new.symbols[index],
            (None, Some(index)) => &old.symbols[index],
            (None, None) => return None,
        };
        if !self.matches(symbol) {
            return None;
        }
        entry.lines.sort_unstable();
        let added = entry.lines.iter().filter(|&&at| lines[at].added).count();
        Some(ChangedSymbol {
            status: entry.status,
            kind: symbol.kind,
            name: symbol.name.clone(),
            container: symbol.container.clone(),
            line: symbol.line,
            end_line: symbol.end_line,
            moved: entry.moved,
            added,
            removed: entry.lines.len() - added,
            lines: self.lines.then(|| self.changed_lines(lines, &entry.lines)),
        })
    }

    fn changed_lines(&self, lines: &[Line], indexes: &[usize]) -> Vec<ChangedLine> {
        indexes
            .iter()
            .map(|&at| ChangedLine {
                sign: if lines[at].added { '+' } else { '-' },
                line: lines[at].line,
                text: truncate_chars(&lines[at].text, self.line_chars),
            })
            .collect()
    }
}

/// A definition changed on every line, such as a one-line one, is modified, not removed and added
/// again: each wholly added definition takes the first wholly removed one of the same name.
/// Returns the removed entries merged away.
fn pair_rewrites(found: &mut [Found], old: &View, new: &View) -> HashSet<usize> {
    let mut removed: HashMap<NameKey, Vec<usize>> = HashMap::new();
    for (at, entry) in found.iter().enumerate().rev() {
        if let (SymbolStatus::Removed, Some(symbol)) = (entry.status, entry.old) {
            removed
                .entry(name_key(&old.symbols[symbol]))
                .or_default()
                .push(at);
        }
    }
    let mut merged = HashSet::new();
    for at in 0..found.len() {
        let (SymbolStatus::Added, Some(symbol)) = (found[at].status, found[at].new) else {
            continue;
        };
        let Some(gone) = removed
            .get_mut(&name_key(&new.symbols[symbol]))
            .and_then(Vec::pop)
        else {
            continue;
        };
        let lines = std::mem::take(&mut found[gone].lines);
        found[at].status = SymbolStatus::Modified;
        found[at].old = found[gone].old;
        found[at].lines.extend(lines);
        merged.insert(gone);
    }
    merged
}

type NameKey = (Option<String>, String, SymbolKind);

fn name_key(symbol: &Symbol) -> NameKey {
    (symbol.container.clone(), symbol.name.clone(), symbol.kind)
}

/// A listed definition: its symbol on each side, and its lines by index.
struct Found {
    status: SymbolStatus,
    old: Option<usize>,
    new: Option<usize>,
    moved: Option<Move>,
    lines: Vec<usize>,
}

impl Found {
    fn whole(added: bool, symbol: usize) -> Self {
        Self {
            status: if added {
                SymbolStatus::Added
            } else {
                SymbolStatus::Removed
            },
            old: (!added).then_some(symbol),
            new: added.then_some(symbol),
            moved: None,
            lines: Vec::new(),
        }
    }

    fn modified() -> Self {
        Self {
            status: SymbolStatus::Modified,
            old: None,
            new: None,
            moved: None,
            lines: Vec::new(),
        }
    }
}

/// Pairs a definition on the two sides: container, name, kind, and which one it is of the
/// definitions with those that are not wholly added or removed.
type Key = (Option<String>, String, SymbolKind, usize);

/// The definitions of one side and its changed lines.
struct View<'a> {
    symbols: &'a [Symbol],
    text: Vec<&'a str>,
    /// For each line of the diff on this side, the innermost definition around it.
    innermost: Vec<Option<usize>>,
    parent: Vec<Option<usize>>,
    /// In start order.
    children: Vec<Vec<usize>>,
    /// Every line but filler is changed.
    whole: Vec<bool>,
    occurrence: Vec<usize>,
}

impl<'a> View<'a> {
    fn new(symbols: &'a [Symbol], text: Option<&'a str>, lines: &[Line], added: bool) -> Self {
        let text: Vec<&str> = text.map(|text| text.lines().collect()).unwrap_or_default();
        let mut order: Vec<usize> = (0..symbols.len()).collect();
        order.sort_by_key(|&index| {
            let symbol = &symbols[index];
            (symbol.start_line, std::cmp::Reverse(symbol.end_line))
        });
        // Definitions nest, so a stack of the open ones in start order finds the parents.
        let mut parent = vec![None; symbols.len()];
        let mut children = vec![Vec::new(); symbols.len()];
        let mut open: Vec<usize> = Vec::new();
        for &index in &order {
            while open
                .last()
                .is_some_and(|&top| symbols[top].end_line < symbols[index].start_line)
            {
                open.pop();
            }
            parent[index] = open.last().copied();
            if let Some(&top) = open.last() {
                children[top].push(index);
            }
            open.push(index);
        }
        let mut innermost = vec![None; lines.len()];
        let mut open: Vec<usize> = Vec::new();
        let mut next = 0;
        // The lines of one side come in ascending order.
        for (at, line) in lines.iter().enumerate().filter(|(_, l)| l.added == added) {
            while next < order.len() && symbols[order[next]].start_line <= line.line {
                open.push(order[next]);
                next += 1;
            }
            while open
                .last()
                .is_some_and(|&top| symbols[top].end_line < line.line)
            {
                open.pop();
            }
            innermost[at] = open.last().copied();
        }
        let changed: HashSet<u32> = lines
            .iter()
            .filter(|line| line.added == added)
            .map(|line| line.line)
            .collect();
        let whole: Vec<bool> = symbols
            .iter()
            .map(|symbol| {
                let mut lines =
                    (symbol.start_line..=symbol.end_line).filter(|&line| !is_filler(&text, line));
                let mut any = false;
                let all = lines.all(|line| {
                    any = true;
                    changed.contains(&line)
                });
                any && all
            })
            .collect();
        let mut seen: HashMap<(Option<&str>, &str, SymbolKind), usize> = HashMap::new();
        let mut occurrence = vec![0; symbols.len()];
        for &index in order.iter().filter(|&&index| !whole[index]) {
            let symbol = &symbols[index];
            let count = seen
                .entry((symbol.container.as_deref(), &symbol.name, symbol.kind))
                .or_default();
            occurrence[index] = *count;
            *count += 1;
        }
        Self {
            symbols,
            text,
            innermost,
            parent,
            children,
            whole,
            occurrence,
        }
    }

    fn is_filler(&self, line: u32) -> bool {
        is_filler(&self.text, line)
    }

    /// The text has the diff's lines of this side at their line numbers.
    fn agrees(&self, lines: &[Line], added: bool) -> bool {
        lines.iter().filter(|line| line.added == added).all(|line| {
            let at = (line.line as usize).checked_sub(1);
            let text = at.and_then(|at| self.text.get(at));
            // Git drops one `\r` before each line end, `str::lines` only one before `\n`.
            let strip = |text: &str| text.strip_suffix('\r').unwrap_or(text).to_owned();
            text.is_some_and(|text| strip(text) == strip(&line.text))
        })
    }

    /// The lines of a definition, without filler.
    fn body(&self, index: usize) -> Vec<&'a str> {
        let symbol = &self.symbols[index];
        (symbol.start_line..=symbol.end_line)
            .filter(|&line| !self.is_filler(line))
            .filter_map(|line| self.text.get(line as usize - 1).copied())
            .collect()
    }

    fn key(&self, index: usize) -> Key {
        let symbol = &self.symbols[index];
        (
            symbol.container.clone(),
            symbol.name.clone(),
            symbol.kind,
            self.occurrence[index],
        )
    }

    fn find(&self, key: &Key) -> Option<usize> {
        (0..self.symbols.len()).find(|&index| !self.whole[index] && self.key(index) == *key)
    }
}

fn is_filler(text: &[&str], line: u32) -> bool {
    (line as usize)
        .checked_sub(1)
        .and_then(|index| text.get(index))
        .is_none_or(|text| filler(text))
}

fn indent(text: &str) -> &str {
    &text[..text.len() - text.trim_start().len()]
}

/// No letter or digit: a blank line or a lone brace.
fn filler(text: &str) -> bool {
    !text.chars().any(char::is_alphanumeric)
}

/// One side of a file.
enum Content {
    Absent,
    Text(String),
    Binary,
    TooLarge,
    /// A working-tree file that vanished or cannot be read since git listed it, or that a link
    /// on its path leads outside the repository.
    Unreadable,
}

fn content(
    root: &Path,
    max_file_size: u64,
    path: &str,
    side: &Side,
    blobs: &HashMap<String, Vec<u8>>,
) -> Result<Content> {
    let bytes: Cow<[u8]> = match side {
        Side::Absent => return Ok(Content::Absent),
        Side::Blob(id) => Cow::Borrowed(
            blobs
                .get(id)
                .ok_or_else(|| Error::failure(format!("git did not return object {id}")))?,
        ),
        Side::Hashed(id) if blobs.contains_key(id) => Cow::Borrowed(&blobs[id]),
        Side::WorkingTree | Side::Hashed(_) => {
            if !git::inside_file(root, path) {
                return Ok(Content::Unreadable);
            }
            let full = root.join(path);
            let Ok(metadata) = full.symlink_metadata() else {
                return Ok(Content::Unreadable);
            };
            if metadata.len() > max_file_size {
                return Ok(Content::TooLarge);
            }
            let Ok(bytes) = std::fs::read(&full) else {
                return Ok(Content::Unreadable);
            };
            Cow::Owned(bytes)
        }
    };
    if bytes.len() as u64 > max_file_size {
        return Ok(Content::TooLarge);
    }
    if bytes[..bytes.len().min(TEXT_SNIFF_BYTES)].contains(&0) {
        return Ok(Content::Binary);
    }
    Ok(Content::Text(String::from_utf8_lossy(&bytes).into_owned()))
}

/// The changed lines of the whole diff without filler, to find a definition's text elsewhere.
struct Moves<'a> {
    root: &'a Path,
    max_file_size: u64,
    files: &'a [FileDiff],
    /// For each file: its removed lines, then its added lines.
    sides: Vec<[Vec<&'a Line>; 2]>,
    /// For each file and side, parsed on first use: the definitions' first and last lines
    /// without filler, and how many such lines they have.
    spans: Vec<[OnceLock<HashSet<Span>>; 2]>,
    /// For each side, trimmed text, and trimmed text of the next line: the file, and the
    /// position in its side. Two lines, since many definitions start with the same one
    /// (`@Override`).
    starts: HashMap<Start<'a>, Vec<(usize, usize)>>,
}

type Start<'a> = (bool, &'a str, Option<&'a str>);

type Span = (u32, u32, usize);

impl<'a> Moves<'a> {
    fn new(root: &'a Path, max_file_size: u64, files: &'a [FileDiff]) -> Self {
        let mut sides = Vec::with_capacity(files.len());
        let mut starts: HashMap<Start, Vec<(usize, usize)>> = HashMap::new();
        for (at, file) in files.iter().enumerate() {
            let mut side: [Vec<&Line>; 2] = [Vec::new(), Vec::new()];
            // A copy's removed lines are still in its source.
            let copy = file.status == FileStatus::Copied;
            for line in file.lines.iter().filter(|line| !filler(&line.text)) {
                if line.added || !copy {
                    side[usize::from(line.added)].push(line);
                }
            }
            for (index, lines) in side.iter().enumerate() {
                for (position, line) in lines.iter().enumerate() {
                    let next = lines.get(position + 1).map(|next| next.text.trim());
                    for key in [None, next] {
                        let key = (index == 1, line.text.trim(), key);
                        starts.entry(key).or_default().push((at, position));
                    }
                }
            }
            sides.push(side);
        }
        Self {
            root,
            max_file_size,
            files,
            sides,
            spans: files.iter().map(|_| Default::default()).collect(),
            starts,
        }
    }

    /// The spans of the definitions on one side of file `at`; none when it cannot be parsed.
    fn spans(&self, at: usize, added: bool) -> &HashSet<Span> {
        self.spans[at][usize::from(added)].get_or_init(|| {
            let file = &self.files[at];
            let (path, side) = match added {
                true => (file.path.as_str(), &file.new),
                false => (file.old_path.as_deref().unwrap_or(&file.path), &file.old),
            };
            let ids = match side {
                Side::Blob(id) | Side::Hashed(id) => vec![id.clone()],
                Side::Absent | Side::WorkingTree => Vec::new(),
            };
            let Ok(blobs) = git::read_blobs(self.root, &ids) else {
                return HashSet::new();
            };
            let Ok(Content::Text(text)) =
                content(self.root, self.max_file_size, path, side, &blobs)
            else {
                return HashSet::new();
            };
            let Some(language) = Language::detect(path, &text) else {
                return HashSet::new();
            };
            let lines: Vec<&str> = text.lines().collect();
            let found = extract::extract(language, &text);
            let span = |symbol: &Symbol| {
                let body: Vec<u32> = (symbol.start_line..=symbol.end_line)
                    .filter(|&line| !is_filler(&lines, line))
                    .collect();
                Some((*body.first()?, *body.last()?, body.len()))
            };
            found.symbols.iter().filter_map(span).collect()
        })
    }

    /// Another place where the diff has `body` on the `added` side, with the same indentation
    /// relative to the first line. There, a definition must have exactly these lines, so a match
    /// inside a longer one (with a decorator, a doc comment, or more lines) is no move. Inside
    /// file `at`, the place must be in none of `hunks`, so a definition reindented or changed in
    /// line endings in place is not a move.
    fn find(&self, added: bool, body: &[&str], at: usize, hunks: &HashSet<u32>) -> Option<Place> {
        let first = body.first()?;
        // As git's `--color-moved`: shorter text, such as `FAILED,` or `Long getId();`, also
        // occurs by chance.
        let letters = body.iter().flat_map(|line| line.chars());
        if letters.filter(|c| c.is_alphanumeric()).count() < 20 {
            return None;
        }
        let key = (added, first.trim(), body.get(1).map(|next| next.trim()));
        let starts = self.starts.get(&key)?;
        starts.iter().find_map(|&(other, position)| {
            let side = &self.sides[other][usize::from(added)];
            let run = side.get(position..position + body.len())?;
            if other == at && hunks.contains(&run[0].hunk) {
                return None;
            }
            let (ours, theirs) = (indent(first), indent(&run[0].text));
            let same = |line: &Line, text: &str| {
                let text = text.strip_prefix(ours).unwrap_or(text);
                line.text.strip_prefix(theirs).unwrap_or(&line.text) == text
            };
            if !run.iter().zip(body).all(|(line, text)| same(line, text)) {
                return None;
            }
            let span = (run[0].line, run[run.len() - 1].line, run.len());
            if !self.spans(other, added).contains(&span) {
                return None;
            }
            let file = &self.files[other];
            let path = match (added, &file.old_path) {
                (false, Some(old_path)) => old_path,
                _ => &file.path,
            };
            Some(Place {
                path: path.clone(),
                line: run[0].line,
            })
        })
    }
}
