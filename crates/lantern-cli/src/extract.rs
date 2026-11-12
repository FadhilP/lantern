//! Symbol and reference extraction with tree-sitter tag queries.

use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::ops::{ControlFlow, Range};
use std::sync::OnceLock;
use std::time::Instant;

use serde::Serialize;
use tree_sitter::{
    Node, ParseOptions, Parser, Query, QueryCursor, QueryCursorOptions, StreamingIterator,
};

use crate::language::Language;
use crate::limits::{MIN_PARSE_BUDGET, PARSE_BUDGET_PER_MIB};

/// The normalized kind of a definition. The shard stores these numbers; a new number needs a
/// schema version.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, clap::ValueEnum)]
#[serde(rename_all = "lowercase")]
pub enum SymbolKind {
    Function = 0,
    Method = 1,
    Class = 2,
    Interface = 3,
    Struct = 4,
    Enum = 5,
    Trait = 6,
    Type = 7,
    Module = 8,
    Constant = 9,
    Variable = 10,
    Field = 11,
}

impl SymbolKind {
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Function => "function",
            Self::Method => "method",
            Self::Class => "class",
            Self::Interface => "interface",
            Self::Struct => "struct",
            Self::Enum => "enum",
            Self::Trait => "trait",
            Self::Type => "type",
            Self::Module => "module",
            Self::Constant => "constant",
            Self::Variable => "variable",
            Self::Field => "field",
        }
    }

    #[must_use]
    pub fn from_name(name: &str) -> Option<Self> {
        <Self as clap::ValueEnum>::from_str(name, false).ok()
    }

    /// Types and functions rank before variables and fields.
    pub(crate) fn rank(self) -> u8 {
        match self {
            Self::Class
            | Self::Interface
            | Self::Struct
            | Self::Enum
            | Self::Trait
            | Self::Type
            | Self::Function
            | Self::Method => 0,
            Self::Module | Self::Constant => 1,
            Self::Variable | Self::Field => 2,
        }
    }

    /// A tag query's `@definition.<suffix>`; the stock queries also tag macros.
    fn from_capture(suffix: &str) -> Option<Self> {
        if suffix == "macro" {
            return Some(Self::Function);
        }
        Self::from_name(suffix)
    }

    pub(crate) fn is_type_like(self) -> bool {
        matches!(
            self,
            Self::Class | Self::Interface | Self::Struct | Self::Enum | Self::Trait | Self::Type
        )
    }
}

/// The kind of a reference. The shard stores these numbers; a new number needs a schema
/// version.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, clap::ValueEnum)]
#[serde(rename_all = "lowercase")]
pub enum RefKind {
    Call = 0,
    Type = 1,
    Import = 2,
    Implementation = 3,
}

impl RefKind {
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Call => "call",
            Self::Type => "type",
            Self::Import => "import",
            Self::Implementation => "implementation",
        }
    }

    fn from_capture(suffix: &str) -> Option<Self> {
        let kind = match suffix {
            "call" | "send" => Self::Call,
            "class" | "type" | "interface" => Self::Type,
            "implementation" => Self::Implementation,
            "import" | "module" => Self::Import,
            _ => return None,
        };
        Some(kind)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Symbol {
    pub name: String,
    pub kind: SymbolKind,
    pub container: Option<String>,
    pub line: u32,
    pub col: u32,
    pub end_line: u32,
    /// The first line with the decorators, attributes, and doc comments of the definition.
    pub start_line: u32,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Reference {
    pub name: String,
    pub kind: RefKind,
    pub line: u32,
    pub col: u32,
    /// Index into [`Extraction::symbols`] of the innermost definition around the reference.
    pub enclosing: Option<usize>,
}

#[derive(Debug, Default)]
pub(crate) struct Extraction {
    pub symbols: Vec<Symbol>,
    pub refs: Vec<Reference>,
    pub timed_out: bool,
}

/// A version string for everything that decides extraction output.
pub(crate) fn extractor_version() -> String {
    let mut hasher = blake3::Hasher::new();
    // A blob out of parse retries parses again only when this version changes.
    hasher.update(format!("{PARSE_BUDGET_PER_MIB:?}{MIN_PARSE_BUDGET:?}").as_bytes());
    for language in Language::ALL {
        hasher.update(query_source(language).as_bytes());
        // Grammar crates rarely set metadata, so the table sizes catch most grammar upgrades.
        let grammar = grammar(language);
        let metadata = grammar
            .metadata()
            .map(|m| (m.major_version, m.minor_version, m.patch_version));
        let shape = (
            grammar.abi_version(),
            grammar.node_kind_count(),
            grammar.field_count(),
            grammar.parse_state_count(),
        );
        hasher.update(format!("{metadata:?}{shape:?}").as_bytes());
    }
    let hash = hasher.finalize().to_hex();
    format!("{}+{}", env!("CARGO_PKG_VERSION"), &hash[..16])
}

/// Parses `source` and returns its definitions and references.
pub(crate) fn extract(language: Language, source: &str) -> Extraction {
    let bytes = f64::from(u32::try_from(source.len()).unwrap_or(u32::MAX));
    let budget = PARSE_BUDGET_PER_MIB
        .mul_f64(bytes / f64::from(1u32 << 20))
        .max(MIN_PARSE_BUDGET);
    let query = query(language);
    let deadline = Instant::now() + budget;
    let timed_out = || Extraction {
        timed_out: true,
        ..Extraction::default()
    };
    let Some(tree) = parse(language, source, deadline) else {
        return timed_out();
    };
    let roles: Vec<Role> = query
        .capture_names()
        .iter()
        .map(|name| Role::of(name))
        .collect();
    let mut tags = Tags::default();
    let mut cursor = QueryCursor::new();
    let cancelled = Cell::new(false);
    let mut progress = |_: &_| over(deadline, &cancelled);
    let options = QueryCursorOptions::new().progress_callback(&mut progress);
    let mut matches =
        cursor.matches_with_options(query, tree.root_node(), source.as_bytes(), options);
    while let Some(found) = matches.next() {
        tags.add(source, &roles, found.captures());
    }
    if cancelled.get() {
        return timed_out();
    }
    tags.finish(source)
}

fn over(deadline: Instant, cancelled: &Cell<bool>) -> ControlFlow<()> {
    if Instant::now() < deadline {
        return ControlFlow::Continue(());
    }
    cancelled.set(true);
    ControlFlow::Break(())
}

fn parse(language: Language, source: &str, deadline: Instant) -> Option<tree_sitter::Tree> {
    thread_local! {
        static PARSERS: RefCell<HashMap<Language, Parser>> = RefCell::new(HashMap::new());
    }
    PARSERS.with_borrow_mut(|parsers| {
        let parser = parsers.entry(language).or_insert_with(|| {
            let mut parser = Parser::new();
            parser
                .set_language(&grammar(language))
                .expect("bundled grammars match the tree-sitter ABI");
            parser
        });
        let bytes = source.as_bytes();
        let cancelled = Cell::new(false);
        let mut progress = |_: &_| over(deadline, &cancelled);
        let options = ParseOptions::new().progress_callback(&mut progress);
        let mut input = |offset: usize, _| &bytes[offset.min(bytes.len())..];
        let tree = parser.parse_with_options(&mut input, None, Some(options));
        if tree.is_none() {
            parser.reset();
        }
        tree
    })
}

fn grammar(language: Language) -> tree_sitter::Language {
    match language {
        Language::C => tree_sitter_c::LANGUAGE.into(),
        Language::Cpp => tree_sitter_cpp::LANGUAGE.into(),
        Language::CSharp => tree_sitter_c_sharp::LANGUAGE.into(),
        Language::Dart => tree_sitter_dart::LANGUAGE.into(),
        Language::Go => tree_sitter_go::LANGUAGE.into(),
        Language::Java => tree_sitter_java::LANGUAGE.into(),
        Language::JavaScript => tree_sitter_javascript::LANGUAGE.into(),
        Language::Php => tree_sitter_php::LANGUAGE_PHP.into(),
        Language::Python => tree_sitter_python::LANGUAGE.into(),
        Language::Ruby => tree_sitter_ruby::LANGUAGE.into(),
        Language::Rust => tree_sitter_rust::LANGUAGE.into(),
        Language::TypeScript => tree_sitter_typescript::LANGUAGE_TYPESCRIPT.into(),
        Language::Tsx => tree_sitter_typescript::LANGUAGE_TSX.into(),
    }
}

fn query_source(language: Language) -> &'static str {
    match language {
        Language::C => include_str!("../queries/c/tags.scm"),
        Language::Cpp => include_str!("../queries/cpp/tags.scm"),
        Language::CSharp => include_str!("../queries/csharp/tags.scm"),
        Language::Dart => include_str!("../queries/dart/tags.scm"),
        Language::Go => include_str!("../queries/go/tags.scm"),
        Language::Java => include_str!("../queries/java/tags.scm"),
        Language::JavaScript => concat!(
            include_str!("../queries/javascript/tags.scm"),
            "\n",
            include_str!("../queries/javascript/heritage.scm"),
            "\n",
            include_str!("../queries/javascript/jsx.scm")
        ),
        Language::Php => include_str!("../queries/php/tags.scm"),
        Language::Python => include_str!("../queries/python/tags.scm"),
        Language::Ruby => include_str!("../queries/ruby/tags.scm"),
        Language::Rust => include_str!("../queries/rust/tags.scm"),
        // TypeScript's published query only adds to the JavaScript one.
        Language::TypeScript => concat!(
            include_str!("../queries/javascript/tags.scm"),
            "\n",
            include_str!("../queries/typescript/tags.scm")
        ),
        Language::Tsx => concat!(
            include_str!("../queries/javascript/tags.scm"),
            "\n",
            include_str!("../queries/typescript/tags.scm"),
            "\n",
            include_str!("../queries/javascript/jsx.scm")
        ),
    }
}

fn query(language: Language) -> &'static Query {
    static QUERIES: [OnceLock<Query>; Language::ALL.len()] =
        [const { OnceLock::new() }; Language::ALL.len()];
    QUERIES[language as usize].get_or_init(|| {
        Query::new(&grammar(language), query_source(language))
            .unwrap_or_else(|error| panic!("bundled {language} tags query is invalid: {error}"))
    })
}

#[derive(Debug, Clone, Copy)]
enum Role {
    Name,
    Definition(SymbolKind),
    Reference(RefKind),
    Scope,
    Container,
    Other,
}

impl Role {
    fn of(capture: &str) -> Self {
        if capture == "name" {
            return Self::Name;
        }
        if capture == "scope" {
            return Self::Scope;
        }
        if capture == "container" {
            return Self::Container;
        }
        if let Some(suffix) = capture.strip_prefix("definition.") {
            return SymbolKind::from_capture(suffix).map_or(Self::Other, Self::Definition);
        }
        if let Some(suffix) = capture.strip_prefix("reference.") {
            return RefKind::from_capture(suffix).map_or(Self::Other, Self::Reference);
        }
        Self::Other
    }
}

#[derive(Debug)]
struct RawDefinition {
    name: Range<usize>,
    node: Range<usize>,
    /// A type named outside the definition's body, like a Go receiver or C++ `Box::`.
    container: Option<Range<usize>>,
    kind: SymbolKind,
    line: u32,
    col: u32,
    end_line: u32,
    start_line: u32,
}

#[derive(Debug)]
struct RawReference {
    name: Range<usize>,
    kind: RefKind,
    line: u32,
    col: u32,
}

#[derive(Debug)]
struct RawScope {
    name: Range<usize>,
    node: Range<usize>,
}

#[derive(Debug, Default)]
struct Tags {
    definitions: Vec<RawDefinition>,
    references: Vec<RawReference>,
    scopes: Vec<RawScope>,
}

const MAX_NAME_BYTES: usize = 256;

impl Tags {
    fn add(&mut self, source: &str, roles: &[Role], captures: &[tree_sitter::QueryCapture<'_>]) {
        let mut name = None;
        let mut container = None;
        let mut tagged = None;
        for capture in captures {
            match roles[capture.index as usize] {
                Role::Name if name.is_none() => name = Some(capture.node),
                Role::Container if container.is_none() => container = Some(capture.node),
                Role::Definition(_) | Role::Reference(_) | Role::Scope => {
                    tagged = Some((roles[capture.index as usize], capture.node));
                }
                _ => {}
            }
        }
        let (Some(name), Some((role, node))) = (name, tagged) else {
            return;
        };
        let text = &source[name.byte_range()];
        if text.trim().is_empty() || text.len() > MAX_NAME_BYTES || text.contains('\n') {
            return;
        }
        let start = name.start_position();
        let (line, col) = (one_based(start.row), one_based(start.column));
        match role {
            Role::Definition(kind) => {
                let node = definition_node(node);
                self.definitions.push(RawDefinition {
                    name: name.byte_range(),
                    node: node.byte_range(),
                    container: container.map(|container| container.byte_range()),
                    kind: refine(kind, node),
                    line,
                    col,
                    end_line: end_line(node),
                    start_line: lead_start_line(node).min(line),
                });
            }
            Role::Reference(kind) => self.references.push(RawReference {
                name: name.byte_range(),
                kind,
                line,
                col,
            }),
            Role::Scope => self.scopes.push(RawScope {
                name: name.byte_range(),
                node: node.byte_range(),
            }),
            Role::Name | Role::Container | Role::Other => {}
        }
    }

    /// Keeps one capture per name, and drops references that are the name of a definition.
    fn dedup(&mut self) {
        // One definition per name; a capture with a container beats the generic capture.
        self.definitions
            .sort_by_key(|definition| (definition.name.start, definition.container.is_none()));
        self.definitions
            .dedup_by_key(|definition| definition.name.clone());
        let definition_names: Vec<Range<usize>> = self
            .definitions
            .iter()
            .map(|definition| definition.name.clone())
            .collect();
        // One reference per name; a specific kind beats the generic type capture.
        self.references
            .sort_by_key(|reference| (reference.name.start, reference.kind == RefKind::Type));
        self.references
            .dedup_by_key(|reference| reference.name.clone());
        self.references.retain(|reference| {
            definition_names
                .binary_search_by_key(&reference.name.start, |name| name.start)
                .is_err()
        });
    }

    fn finish(mut self, source: &str) -> Extraction {
        self.dedup();
        self.definitions.sort_by_key(|definition| {
            (
                definition.node.start,
                std::cmp::Reverse(definition.node.end),
            )
        });
        self.scopes
            .sort_by_key(|scope| (scope.node.start, std::cmp::Reverse(scope.node.end)));
        let mut scopes = self.scopes.iter().peekable();
        let mut open_scopes: Vec<&RawScope> = Vec::new();
        let mut symbols = Vec::with_capacity(self.definitions.len());
        let mut open: Vec<usize> = Vec::new();
        for definition in &self.definitions {
            let at = definition.node.start;
            while let Some(scope) = scopes.next_if(|scope| scope.node.start <= at) {
                open_scopes.push(scope);
            }
            open_scopes.retain(|scope| scope.node.end > at);
            while open
                .last()
                .is_some_and(|&top| self.definitions[top].node.end <= at)
            {
                open.pop();
            }
            let parent = open.last().map(|&top| &self.definitions[top]);
            let scope = open_scopes
                .last()
                .filter(|scope| parent.is_none_or(|parent| scope.node.start > parent.node.start));
            let (container, type_like) = match (&definition.container, scope, parent) {
                (Some(container), _, _) => (Some(container), true),
                (None, Some(scope), _) => (Some(&scope.name), true),
                (None, None, Some(parent)) => (Some(&parent.name), parent.kind.is_type_like()),
                (None, None, None) => (None, false),
            };
            let kind = match definition.kind {
                SymbolKind::Method if !type_like => SymbolKind::Function,
                SymbolKind::Function if type_like => SymbolKind::Method,
                kind => kind,
            };
            symbols.push(Symbol {
                name: source[definition.name.clone()].trim().to_owned(),
                kind,
                container: container.map(|name| source[name.clone()].trim().to_owned()),
                line: definition.line,
                col: definition.col,
                end_line: definition.end_line.max(definition.line),
                start_line: definition.start_line,
            });
            open.push(symbols.len() - 1);
        }

        let mut refs = Vec::with_capacity(self.references.len());
        let mut open: Vec<usize> = Vec::new();
        let mut next = 0;
        for reference in &self.references {
            let at = reference.name.start;
            while next < self.definitions.len() && self.definitions[next].node.start <= at {
                let start = self.definitions[next].node.start;
                while open
                    .last()
                    .is_some_and(|&top| self.definitions[top].node.end <= start)
                {
                    open.pop();
                }
                open.push(next);
                next += 1;
            }
            while open
                .last()
                .is_some_and(|&top| self.definitions[top].node.end <= at)
            {
                open.pop();
            }
            refs.push(Reference {
                name: source[reference.name.clone()].trim().to_owned(),
                kind: reference.kind,
                line: reference.line,
                col: reference.col,
                enclosing: open.last().copied(),
            });
        }
        Extraction {
            symbols,
            refs,
            timed_out: false,
        }
    }
}

/// C and C++ queries tag the declarator; widen it to the whole function definition.
fn definition_node(node: Node<'_>) -> Node<'_> {
    if !node.kind().ends_with("declarator") {
        return node;
    }
    let mut current = node;
    while let Some(parent) = current.parent() {
        if parent.kind() == "function_definition" {
            return parent;
        }
        if !parent.kind().ends_with("declarator") {
            break;
        }
        current = parent;
    }
    node
}

/// The first line of a definition with what belongs to it: a wrapping Python decorator or JS
/// `export`, then the decorators, attributes, annotations, and comments right before it. A blank
/// line, or code that ends on the comment's line, ends the lead.
fn lead_start_line(node: Node<'_>) -> u32 {
    let mut first = node;
    while let Some(parent) = first
        .parent()
        .filter(|parent| matches!(parent.kind(), "decorated_definition" | "export_statement"))
    {
        first = parent;
    }
    while let Some(previous) = first.prev_named_sibling() {
        let adjacent = previous.end_position().row + 1 >= first.start_position().row;
        let trails_code = previous.prev_sibling().is_some_and(|before| {
            !is_lead(before.kind()) && before.end_position().row == previous.start_position().row
        });
        if !adjacent || trails_code || !is_lead(previous.kind()) {
            break;
        }
        first = previous;
    }
    one_based(first.start_position().row)
}

fn is_lead(kind: &str) -> bool {
    // Java's `annotation_type_declaration` is a definition of its own, not a lead.
    !kind.ends_with("_declaration")
        && ["comment", "attribute", "decorator", "annotation"]
            .iter()
            .any(|lead| kind.contains(lead))
}

fn refine(kind: SymbolKind, node: Node<'_>) -> SymbolKind {
    if !kind.is_type_like() {
        return kind;
    }
    let node_kind = node.kind();
    if node_kind == "type_spec" {
        return match node.child_by_field_name("type").map(|child| child.kind()) {
            Some("struct_type") => SymbolKind::Struct,
            Some("interface_type") => SymbolKind::Interface,
            _ => SymbolKind::Type,
        };
    }
    if node_kind.contains("struct") || node_kind.contains("union") {
        SymbolKind::Struct
    } else if node_kind.contains("enum") {
        SymbolKind::Enum
    } else if node_kind.contains("trait") {
        SymbolKind::Trait
    } else if node_kind == "type_item" || node_kind.contains("type_alias") {
        SymbolKind::Type
    } else {
        kind
    }
}

fn end_line(node: Node<'_>) -> u32 {
    let end = node.end_position();
    let row = if end.column == 0 && end.row > node.start_position().row {
        end.row - 1
    } else {
        end.row
    };
    one_based(row)
}

/// Tree-sitter keeps rows and columns as 32-bit values, so this never saturates in practice.
fn one_based(position: usize) -> u32 {
    u32::try_from(position + 1).unwrap_or(u32::MAX)
}

pub(crate) fn truncate_chars(text: &str, max: usize) -> String {
    match text.char_indices().nth(max) {
        Some((index, _)) => text[..index].to_owned(),
        None => text.to_owned(),
    }
}

impl rusqlite::types::FromSql for SymbolKind {
    fn column_result(value: rusqlite::types::ValueRef<'_>) -> rusqlite::types::FromSqlResult<Self> {
        stored_kind(value, |kind: Self| kind as i64)
    }
}

impl rusqlite::types::ToSql for SymbolKind {
    fn to_sql(&self) -> rusqlite::Result<rusqlite::types::ToSqlOutput<'_>> {
        Ok((*self as i64).into())
    }
}

impl rusqlite::types::FromSql for RefKind {
    fn column_result(value: rusqlite::types::ValueRef<'_>) -> rusqlite::types::FromSqlResult<Self> {
        stored_kind(value, |kind: Self| kind as i64)
    }
}

impl rusqlite::types::ToSql for RefKind {
    fn to_sql(&self) -> rusqlite::Result<rusqlite::types::ToSqlOutput<'_>> {
        Ok((*self as i64).into())
    }
}

fn stored_kind<Kind: clap::ValueEnum + Copy>(
    value: rusqlite::types::ValueRef<'_>,
    number: impl Fn(Kind) -> i64,
) -> rusqlite::types::FromSqlResult<Kind> {
    let stored = value.as_i64()?;
    Kind::value_variants()
        .iter()
        .copied()
        .find(|&kind| number(kind) == stored)
        .ok_or(rusqlite::types::FromSqlError::OutOfRange(stored))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn definitions(
        language: Language,
        source: &str,
    ) -> Vec<(String, &'static str, Option<String>, u32, u32)> {
        extract(language, source)
            .symbols
            .into_iter()
            .map(|symbol| {
                (
                    symbol.name,
                    symbol.kind.as_str(),
                    symbol.container,
                    symbol.line,
                    symbol.end_line,
                )
            })
            .collect()
    }

    fn references(language: Language, source: &str) -> Vec<(String, &'static str, Option<String>)> {
        let extraction = extract(language, source);
        extraction
            .refs
            .iter()
            .map(|reference| {
                (
                    reference.name.clone(),
                    reference.kind.as_str(),
                    reference
                        .enclosing
                        .map(|index| extraction.symbols[index].name.clone()),
                )
            })
            .collect()
    }

    fn owned(
        rows: &[(&str, &'static str, Option<&str>, u32, u32)],
    ) -> Vec<(String, &'static str, Option<String>, u32, u32)> {
        rows.iter()
            .map(|(name, kind, container, line, end)| {
                (
                    (*name).to_owned(),
                    *kind,
                    container.map(str::to_owned),
                    *line,
                    *end,
                )
            })
            .collect()
    }

    #[test]
    fn every_bundled_query_compiles() {
        for language in Language::ALL {
            let _ = query(language);
        }
    }

    #[test]
    fn typescript_methods_nest_in_classes() {
        let source = "export class Parser {\n  parse(input: string) {\n    return helper(input);\n  }\n}\nfunction helper(x: string) {\n  return x;\n}\n";
        assert_eq!(
            definitions(Language::TypeScript, source),
            owned(&[
                ("Parser", "class", None, 1, 5),
                ("parse", "method", Some("Parser"), 2, 4),
                ("helper", "function", None, 6, 8),
            ])
        );
        assert_eq!(
            references(Language::TypeScript, source),
            vec![("helper".to_owned(), "call", Some("parse".to_owned()))]
        );
    }

    #[test]
    fn typescript_and_javascript_record_imports_heritage_and_every_type_use() {
        let owned_refs = |rows: &[(&str, &'static str, Option<&str>)]| -> Vec<(String, &'static str, Option<String>)> {
            rows.iter()
                .map(|(name, kind, enclosing)| ((*name).to_owned(), *kind, enclosing.map(str::to_owned)))
                .collect()
        };
        let source = "import { load, type Shape } from \"./x\";\ntype Theme = Color | \"system\";\nclass Box extends Base implements Shape<Color> {\n  open(): void { load(value as Color); }\n}\n";
        assert_eq!(
            references(Language::TypeScript, source),
            owned_refs(&[
                ("load", "import", None),
                ("Shape", "import", None),
                ("Color", "type", Some("Theme")),
                ("Base", "implementation", Some("Box")),
                ("Shape", "implementation", Some("Box")),
                ("Color", "type", Some("Box")),
                ("load", "call", Some("open")),
                ("Color", "type", Some("open")),
            ])
        );
        let source = "import Def, * as ns from \"x\";\nclass A extends Def {}\nclass B extends ns.Base {}\nexport { Def as D } from \"x\";\n";
        assert_eq!(
            references(Language::JavaScript, source),
            owned_refs(&[
                ("Def", "import", None),
                ("ns", "import", None),
                ("Def", "implementation", Some("A")),
                ("Base", "implementation", Some("B")),
                ("Def", "import", None),
            ])
        );
    }

    #[test]
    fn base_types_are_implementations_in_every_class_language() {
        let cases = [
            (
                Language::CSharp,
                "class A : Base, IShow {}\ninterface IShow : IBase {}\n",
                &["Base", "IShow", "IBase"][..],
            ),
            (
                Language::Java,
                "class A extends Base implements IShow {}\n",
                &["Base", "IShow"][..],
            ),
            (
                Language::Php,
                "<?php\nclass A extends Base implements IShow {}\nclass B extends \\App\\Core {}\n",
                &["Base", "IShow", "Core"][..],
            ),
            (
                Language::Cpp,
                "class A : public Base, private ns::Core {};\n",
                &["Base", "Core"][..],
            ),
            (
                Language::Python,
                "class A(Base, mod.Core):\n    pass\n",
                &["Base", "Core"][..],
            ),
            (
                Language::Ruby,
                "class A < Base\nend\nclass B < Mod::Core\nend\n",
                &["Base", "Core"][..],
            ),
        ];
        for (language, source, expected) in cases {
            let found: Vec<String> = references(language, source)
                .into_iter()
                .filter(|row| row.1 == "implementation")
                .map(|row| row.0)
                .collect();
            assert_eq!(found, expected, "{language:?}");
        }
    }

    #[test]
    fn constants_are_definitions_in_every_language() {
        type Case = (
            Language,
            &'static str,
            &'static [(&'static str, &'static str)],
        );
        let cases: [Case; 10] = [
            (
                Language::C,
                "#define GUARD_H\n#define MAX 1\nconst int LIMIT = 2;\nint plain = 3;\nenum E { RED };\n",
                &[
                    ("MAX", "constant"),
                    ("LIMIT", "constant"),
                    ("RED", "constant"),
                ],
            ),
            (
                Language::Cpp,
                "constexpr int N = 1;\nnamespace ns { const int IN = 2; }\nclass B { static const int SIZE = 3; int field; };\n",
                &[("N", "constant"), ("IN", "constant"), ("SIZE", "constant")],
            ),
            (
                Language::CSharp,
                "class C { const int Max = 1; static readonly int Limit = 2; public static readonly int Shared = 3; readonly int late; int field; }\nenum E { Red }\n",
                &[
                    ("Max", "constant"),
                    ("Limit", "constant"),
                    ("Shared", "constant"),
                    ("Red", "constant"),
                ],
            ),
            (
                Language::Dart,
                "const max = 1;\nconst int typed = 2;\nfinal later = 3;\nvar plain = 4;\nclass K { static const size = 5; final field = 6; }\n",
                &[
                    ("max", "constant"),
                    ("typed", "constant"),
                    ("later", "variable"),
                    ("plain", "variable"),
                    ("size", "constant"),
                ],
            ),
            (
                Language::Go,
                "package p\nconst (\n\tA = iota\n\tB\n)\nvar ErrX = 1\nfunc f() { const local = 1 }\n",
                &[("A", "constant"), ("B", "constant"), ("ErrX", "variable")],
            ),
            (
                Language::JavaScript,
                "const MAX = 1;\nexport const LIMIT = {};\nlet plain = 2;\nconst run = () => 1;\nfunction f() { const local = 1; }\n",
                &[("MAX", "constant"), ("LIMIT", "constant")],
            ),
            (
                Language::Php,
                "<?php\nconst MAX = 1;\ndefine('LIMIT', 2);\nclass C { const SIZE = 3; }\nenum E { case Red; }\n",
                &[
                    ("MAX", "constant"),
                    ("LIMIT", "constant"),
                    ("SIZE", "constant"),
                    ("Red", "constant"),
                ],
            ),
            (
                Language::Ruby,
                "MAX = 1\nmodule M\n  INNER = 2\nend\nplain = 3\n",
                &[("MAX", "constant"), ("INNER", "constant")],
            ),
            (
                Language::TypeScript,
                "const MAX = 1;\nexport const LIMIT = 2 as const;\nenum E { Red, Green = 2 }\n",
                &[
                    ("MAX", "constant"),
                    ("LIMIT", "constant"),
                    ("Red", "constant"),
                    ("Green", "constant"),
                ],
            ),
            (
                Language::Tsx,
                "const MAX = 1;\nconst View = () => <div />;\n",
                &[("MAX", "constant")],
            ),
        ];
        for (language, source, expected) in cases {
            let found: Vec<(String, &str)> = definitions(language, source)
                .into_iter()
                .filter(|row| matches!(row.1, "constant" | "variable"))
                .map(|row| (row.0, row.1))
                .collect();
            let expected: Vec<(String, &str)> = expected
                .iter()
                .map(|&(name, kind)| (name.to_owned(), kind))
                .collect();
            assert_eq!(found, expected, "{language:?}");
        }
    }

    #[test]
    fn rust_items_get_precise_kinds() {
        let source = "struct Point { x: i32 }\nenum Shape { A }\ntrait Draw { fn draw(&self); }\nimpl Point {\n    fn new() -> Self { util::make::<i32>(); Self::origin() }\n}\nmod util {\n    fn make() {}\n}\nconst MAX: u32 = 1;\nstatic NAME: &str = \"n\";\nimpl Shape {\n    const ZERO: i32 = 0;\n}\n";
        let rows = definitions(Language::Rust, source);
        let kinds: Vec<(&str, &str)> = rows.iter().map(|row| (row.0.as_str(), row.1)).collect();
        assert_eq!(
            kinds,
            [
                ("Point", "struct"),
                ("Shape", "enum"),
                ("Draw", "trait"),
                ("new", "method"),
                ("util", "module"),
                ("make", "function"),
                ("MAX", "constant"),
                ("NAME", "constant"),
                ("ZERO", "constant"),
            ]
        );
        assert_eq!(rows[8].2.as_deref(), Some("Shape"));
        assert_eq!(rows[3].2.as_deref(), Some("Point"));
        assert_eq!(rows[5].2.as_deref(), Some("util"));
        assert_eq!(
            references(Language::Rust, source),
            vec![
                ("Point".to_owned(), "implementation", None),
                ("make".to_owned(), "call", Some("new".to_owned())),
                ("origin".to_owned(), "call", Some("new".to_owned())),
                ("Shape".to_owned(), "implementation", None),
            ]
        );
    }

    #[test]
    fn python_nested_functions_and_calls() {
        let source = "class A:\n    def run(self):\n        def inner():\n            work()\n        inner()\n";
        assert_eq!(
            definitions(Language::Python, source),
            owned(&[
                ("A", "class", None, 1, 5),
                ("run", "method", Some("A"), 2, 5),
                ("inner", "function", Some("run"), 3, 4),
            ])
        );
        assert_eq!(
            references(Language::Python, source),
            vec![
                ("work".to_owned(), "call", Some("inner".to_owned())),
                ("inner".to_owned(), "call", Some("run".to_owned())),
            ]
        );
    }

    #[test]
    fn c_functions_span_their_bodies() {
        let source = "static int add(int a, int b) {\n  return helper(a) + b;\n}\nint *make(void) {\n  return 0;\n}\n";
        assert_eq!(
            definitions(Language::C, source),
            owned(&[
                ("add", "function", None, 1, 3),
                ("make", "function", None, 4, 6)
            ])
        );
        assert_eq!(
            references(Language::C, source),
            vec![("helper".to_owned(), "call", Some("add".to_owned()))]
        );
    }

    #[test]
    fn go_types_and_methods() {
        let source = "package p\n\ntype Server struct{}\ntype Handler interface{ Serve() }\n\nfunc (s *Server) Start() {\n\trun()\n}\nfunc (Box[T]) Open() {}\n";
        let rows = definitions(Language::Go, source);
        let kinds: Vec<(&str, &str, Option<&str>)> = rows
            .iter()
            .map(|row| (row.0.as_str(), row.1, row.2.as_deref()))
            .collect();
        assert_eq!(
            kinds,
            [
                ("Server", "struct", None),
                ("Handler", "interface", None),
                ("Start", "method", Some("Server")),
                ("Open", "method", Some("Box")),
            ]
        );
        let refs = references(Language::Go, source);
        assert!(refs.contains(&("run".to_owned(), "call", Some("Start".to_owned()))));
    }

    #[test]
    fn java_records_annotations_constructors_and_constants() {
        let source = "\
public @interface AuthCheck {
  boolean permissionCheck() default true;
}
record Point(int x, int y) {
  Point {}
  int sum() { return x + y; }
}
class Svc {
  static final String URL = \"x\", NAME = \"y\";
  private final OrderService orderService;
  Svc() {}
}
interface Limits { int MAX = 3; }
enum Color { RED }
";
        assert_eq!(
            definitions(Language::Java, source),
            owned(&[
                ("AuthCheck", "interface", None, 1, 3),
                ("permissionCheck", "method", Some("AuthCheck"), 2, 2),
                ("Point", "class", None, 4, 7),
                ("Point", "method", Some("Point"), 5, 5),
                ("sum", "method", Some("Point"), 6, 6),
                ("Svc", "class", None, 8, 12),
                ("URL", "constant", Some("Svc"), 9, 9),
                ("NAME", "constant", Some("Svc"), 9, 9),
                ("Svc", "method", Some("Svc"), 11, 11),
                ("Limits", "interface", None, 13, 13),
                ("MAX", "constant", Some("Limits"), 13, 13),
                ("Color", "enum", None, 14, 14),
                ("RED", "constant", Some("Color"), 14, 14),
            ])
        );
    }

    #[test]
    fn methods_name_their_type_inside_or_outside_its_body() {
        let methods =
            |language: Language, source: &str| -> Vec<(String, &'static str, Option<String>)> {
                definitions(language, source)
                    .into_iter()
                    .filter(|row| row.0 != "Box" && row.0 != "Shop")
                    .map(|row| (row.0, row.1, row.2))
                    .collect()
            };
        let cpp = "class Box {\n  void close() {}\n};\nvoid Box::open() { close(); }\n";
        assert_eq!(
            methods(Language::Cpp, cpp),
            vec![
                ("close".to_owned(), "method", Some("Box".to_owned())),
                ("open".to_owned(), "method", Some("Box".to_owned())),
            ]
        );
        let php = "<?php\nclass Shop {\n  function buy() {}\n}\n";
        assert_eq!(
            methods(Language::Php, php),
            vec![("buy".to_owned(), "method", Some("Shop".to_owned()))]
        );
    }

    #[test]
    fn dart_members_span_their_bodies_and_name_their_type() {
        let source = "\
typedef Cb = void Function(int);
var first = 0, second = 1;
int get total => 1;
void helper() {
  void inner() {}
}
mixin Walks {}
class Shop {
  final int size, weight;
  static const String tag = 'x';
  Shop(this.size);
  Shop.named(int s) : size = s {
    init();
  }
  const Shop.c() : size = 1;
  factory Shop.make() => Shop(1);
  factory Shop.redirect() = Shop.named;
  int get count => 1;
  @override
  void buy() {
    pay();
  }
  void later();
  bool operator ==(Object o) => true;
  void operator []=(int i, int v) {}
}
enum Color { red }
extension Str on String { int twice() => 2; }
extension type Id(int v) {}
class Alias = Shop with Walks;
";
        let shop = Some("Shop");
        assert_eq!(
            definitions(Language::Dart, source),
            owned(&[
                ("Cb", "type", None, 1, 1),
                ("first", "variable", None, 2, 2),
                ("second", "variable", None, 2, 2),
                ("total", "function", None, 3, 3),
                ("helper", "function", None, 4, 6),
                ("inner", "function", Some("helper"), 5, 5),
                ("Walks", "class", None, 7, 7),
                ("Shop", "class", None, 8, 26),
                ("size", "field", shop, 9, 9),
                ("weight", "field", shop, 9, 9),
                ("tag", "constant", shop, 10, 10),
                ("Shop", "method", shop, 11, 11),
                ("named", "method", shop, 12, 14),
                ("c", "method", shop, 15, 15),
                ("make", "method", shop, 16, 16),
                ("redirect", "method", shop, 17, 17),
                ("count", "method", shop, 18, 18),
                ("buy", "method", shop, 20, 22),
                ("later", "method", shop, 23, 23),
                ("==", "method", shop, 24, 24),
                ("[]=", "method", shop, 25, 25),
                ("Color", "enum", None, 27, 27),
                ("red", "constant", Some("Color"), 27, 27),
                ("Str", "class", None, 28, 28),
                ("twice", "method", Some("Str"), 28, 28),
                ("Id", "class", None, 29, 29),
                ("Alias", "class", None, 30, 30),
            ])
        );
    }

    #[test]
    fn dart_references_cover_calls_heritage_imports_and_types() {
        let source = "\
import 'a.dart' show Baz hide Zed;
import 'c.dart' as c;
class Shop extends p.Base<Item> with Walks implements Store<Item> {
  void buy(Item a) {
    pay();
    a.copy();
    a?.opt();
    a..ship();
    ctx.read<Foo>();
    Box<Item>();
    const Text();
  }
}
";
        let rows = references(Language::Dart, source);
        let row = |name: &str, kind, enclosing: Option<&str>| {
            (name.to_owned(), kind, enclosing.map(str::to_owned))
        };
        let buy = Some("buy");
        let shop = Some("Shop");
        for expected in [
            row("Baz", "import", None),
            row("c", "import", None),
            row("p", "type", shop),
            row("Base", "implementation", shop),
            row("Walks", "implementation", shop),
            row("Store", "implementation", shop),
            row("Item", "type", shop),
            row("pay", "call", buy),
            row("copy", "call", buy),
            row("opt", "call", buy),
            row("ship", "call", buy),
            row("read", "call", buy),
            row("Foo", "type", buy),
            row("Box", "call", buy),
            row("Text", "type", buy),
        ] {
            assert!(rows.contains(&expected), "{expected:?} not in {rows:?}");
        }
        assert!(!rows.iter().any(|row| row.0 == "Zed"), "{rows:?}");
    }

    #[test]
    fn jsx_components_are_calls_but_html_tags_are_not() {
        let source = "\
function App() {
  return (
    <Card>
      <div />
      <span>{format(1)}</span>
      <Badge />
      <UI.Panel />
    </Card>
  );
}
";
        let call = |name: &str| (name.to_owned(), "call", Some("App".to_owned()));
        for language in [Language::JavaScript, Language::Tsx] {
            assert_eq!(
                references(language, source),
                vec![call("Card"), call("format"), call("Badge"), call("Panel"),],
                "{language}"
            );
        }
    }

    #[test]
    fn a_definition_starts_at_its_decorators_attributes_and_doc_comments() {
        let start_of = |language: Language, source: &str, name: &str| {
            extract(language, source)
                .symbols
                .into_iter()
                .find(|symbol| symbol.name == name)
                .map(|symbol| (symbol.start_line, symbol.line))
        };
        let java = "class A {\n  int x; // trailing\n  /** Doc. */\n  @Override\n  @GetMapping(\n      \"/x\")\n  public void run() {\n  }\n}\n";
        assert_eq!(start_of(Language::Java, java, "run"), Some((3, 7)));
        let annotation_type =
            "@interface Auth {\n  String name();\n  int level();\n}\nclass Bar {\n}\n";
        assert_eq!(
            start_of(Language::Java, annotation_type, "level"),
            Some((3, 3))
        );
        assert_eq!(
            start_of(Language::Java, annotation_type, "Bar"),
            Some((5, 5))
        );
        let rust = "use x;\n\n/// Doc.\n#[test] #[ignore]\nfn run() {\n}\n";
        assert_eq!(start_of(Language::Rust, rust, "run"), Some((3, 5)));
        let python = "# Not attached.\n\n@app.route(\"/x\")\ndef run():\n    pass\n";
        assert_eq!(start_of(Language::Python, python, "run"), Some((3, 4)));
        let typescript =
            "/** Doc. */\n@Component({})\nexport class Run {\n  @Input()\n  name() {}\n}\n";
        assert_eq!(
            start_of(Language::TypeScript, typescript, "Run"),
            Some((1, 3))
        );
        assert_eq!(
            start_of(Language::TypeScript, typescript, "name"),
            Some((4, 5))
        );
        let javascript = "/** Doc. */\nexport function run() {}\n";
        assert_eq!(
            start_of(Language::JavaScript, javascript, "run"),
            Some((1, 2))
        );
        let go = "package p\n\n// Run runs.\nfunc Run() {}\n";
        assert_eq!(start_of(Language::Go, go, "Run"), Some((3, 4)));
        let csharp = "class A {\n  /// Doc.\n  [HttpGet]\n  void Run() {}\n}\n";
        assert_eq!(start_of(Language::CSharp, csharp, "Run"), Some((2, 4)));
        let dart = "class A {\n  @override\n  void run() {}\n}\n";
        assert_eq!(start_of(Language::Dart, dart, "run"), Some((2, 3)));
    }

    #[test]
    fn syntax_errors_keep_what_parses() {
        let source = "def ok():\n    pass\n\ndef broken(:\n    pass\n\ndef later():\n    pass\n";
        let names: Vec<String> = definitions(Language::Python, source)
            .into_iter()
            .map(|row| row.0)
            .collect();
        assert!(names.contains(&"ok".to_owned()), "{names:?}");
        assert!(names.contains(&"later".to_owned()), "{names:?}");
    }

    #[test]
    fn every_language_finds_a_definition() {
        let samples = [
            (
                Language::Cpp,
                "class Box { void open(); };\nvoid Box::open() { close(); }\n",
                "Box",
            ),
            (
                Language::CSharp,
                "class Shop { void Buy() { Pay(); } }\n",
                "Buy",
            ),
            (
                Language::Java,
                "class Shop { void buy() { pay(); } }\n",
                "buy",
            ),
            (Language::JavaScript, "function go() { stop(); }\n", "go"),
            (Language::Php, "<?php\nfunction go() { stop(); }\n", "go"),
            (
                Language::Ruby,
                "class Shop\n  def buy\n    pay(1)\n  end\nend\n",
                "buy",
            ),
            (
                Language::Tsx,
                "export function View() { return <div/>; }\n",
                "View",
            ),
        ];
        for (language, source, name) in samples {
            let names: Vec<String> = definitions(language, source)
                .into_iter()
                .map(|row| row.0)
                .collect();
            assert!(
                names.iter().any(|found| found == name),
                "{language}: {names:?}"
            );
        }
    }
}
