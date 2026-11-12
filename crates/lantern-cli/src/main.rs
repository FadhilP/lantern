//! `lantern`: read-only code discovery for large codebases.

mod engine;
mod error;
mod extract;
mod fsmeta;
mod home;
mod language;
mod limits;
mod output;
mod paths;
mod query;
mod refresh;
mod rules;
mod source;
mod store;
mod workspace;

use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::time::Duration;

use clap::{Args, Parser, Subcommand};

use crate::engine::{Lantern, Options};
use crate::error::{Error, ErrorKind};
use crate::extract::{RefKind, SymbolKind};
use crate::limits::{
    DEFAULT_BODY_LIMIT, DEFAULT_CODE_LIMIT, DEFAULT_GRAPH_DEPTH, DEFAULT_GRAPH_NODES,
    DEFAULT_LIMIT, DEFAULT_OUTLINE_LIMIT, DEFAULT_PER_FILE,
};
use crate::output::Printer;
use crate::query::code::{CodeQuery, CodeSort, looks_like_regex};
use crate::query::graph::{Direction, GraphQuery};
use crate::query::refs::RefsQuery;
use crate::query::symbols::{SymbolQuery, split_qualified};
use crate::query::{Freshness, PageRequest};
use crate::refresh::Mode;
use crate::store::shard::Incomplete;

#[derive(Debug, Parser)]
#[command(
    name = "lantern",
    version,
    about = "Read-only code discovery for large codebases"
)]
struct Cli {
    /// Run as if started in this directory.
    #[arg(short = 'C', long = "cwd", global = true, value_name = "DIR")]
    cwd: Option<PathBuf>,
    /// The data directory.
    #[arg(long, env = "LANTERN_HOME", global = true, value_name = "DIR")]
    home: Option<PathBuf>,
    /// Warm refresh timeout in milliseconds; 0 waits without a limit.
    #[arg(
        long,
        env = "LANTERN_TIMEOUT_MS",
        global = true,
        default_value_t = 120_000,
        value_name = "MS"
    )]
    timeout: u64,
    /// Larger files are not indexed as text. Accepts K, M, and G suffixes.
    #[arg(long, env = "LANTERN_MAX_FILE_SIZE", global = true, default_value = "1M", value_parser = parse_size, value_name = "BYTES")]
    max_file_size: u64,
    /// One JSON document instead of compact text.
    #[arg(long, global = true)]
    json: bool,
    #[command(subcommand)]
    command: Command,
}

#[derive(Debug, Subcommand)]
enum Command {
    /// Index or refresh the workspace at PATH.
    Index {
        path: Option<PathBuf>,
        /// Build new shards from the disk and swap them in.
        #[arg(long)]
        rebuild: bool,
        /// Allow the first index of the home folder or a drive root.
        #[arg(long)]
        force: bool,
    },
    /// Show the committed index state without refreshing.
    Status { path: Option<PathBuf> },
    /// Remove shards of roots that no longer exist.
    Prune,
    /// Where is X defined?
    Symbols(SymbolsArgs),
    /// Where does this text or pattern occur?
    Code(CodeArgs),
    /// Who uses X?
    Refs(RefsArgs),
    /// What calls X, and what does X call?
    Graph(GraphArgs),
    /// What is in this file?
    Outline {
        /// One or more files; each gets its own page under a `== FILE` line.
        #[arg(required = true, value_name = "FILE")]
        files: Vec<String>,
        #[command(flatten)]
        page: PageArgs,
    },
}

#[derive(Debug, Args)]
struct SymbolsArgs {
    /// One or more queries; each gets its own page under a `== QUERY` line. With none, lists
    /// every definition that passes the filters, in path and line order.
    #[arg(value_name = "QUERY")]
    queries: Vec<String>,
    /// One kind, or several separated by commas.
    #[arg(long, value_enum, value_delimiter = ',')]
    kind: Vec<SymbolKind>,
    #[arg(long)]
    language: Option<String>,
    /// Repeat to match any of several globs; `!GLOB` excludes.
    #[arg(long, value_name = "GLOB")]
    path: Vec<String>,
    #[arg(long)]
    include_generated: bool,
    /// Print the source lines of each definition; the default --limit drops to 5.
    #[arg(long)]
    body: bool,
    #[command(flatten)]
    page: PageArgs,
}

#[derive(Debug, Args)]
struct CodeArgs {
    #[arg(
        required_unless_present = "dash_pattern",
        conflicts_with = "dash_pattern"
    )]
    pattern: Option<String>,
    /// The pattern, also when it starts with `-`.
    #[arg(short = 'e', value_name = "PATTERN", allow_hyphen_values = true)]
    dash_pattern: Option<String>,
    #[arg(long)]
    regex: bool,
    #[arg(long)]
    case_sensitive: bool,
    /// Accepted from grep habit; matching already ignores case.
    #[arg(short = 'i', long, hide = true, conflicts_with = "case_sensitive")]
    ignore_case: bool,
    /// Repeat to match any of several globs; `!GLOB` excludes.
    #[arg(long, value_name = "GLOB")]
    path: Vec<String>,
    #[arg(long)]
    language: Option<String>,
    /// 0 prints only the files and their match counts, like `--files`.
    #[arg(long, default_value_t = DEFAULT_PER_FILE, value_name = "N")]
    per_file: usize,
    /// Print only the files that match, with their match counts; the same as `--per-file 0`.
    #[arg(short = 'l', long)]
    files: bool,
    #[arg(long, default_value_t = 0, value_name = "N")]
    context: usize,
    #[arg(long, value_enum, default_value_t = CodeSort::Relevance)]
    sort: CodeSort,
    #[arg(long)]
    include_generated: bool,
    #[command(flatten)]
    page: PageArgs,
}

#[derive(Debug, Args)]
struct RefsArgs {
    name: String,
    /// One kind, or several separated by commas.
    #[arg(long, value_enum, value_delimiter = ',')]
    kind: Vec<RefKind>,
    /// Repeat to match any of several globs; `!GLOB` excludes.
    #[arg(long, value_name = "GLOB")]
    path: Vec<String>,
    #[arg(long)]
    include_generated: bool,
    #[command(flatten)]
    page: PageArgs,
}

#[derive(Debug, Args)]
struct GraphArgs {
    name: String,
    #[arg(long, value_enum, default_value_t = Direction::Both)]
    direction: Direction,
    #[arg(long, default_value_t = DEFAULT_GRAPH_DEPTH, value_name = "N")]
    depth: usize,
    #[arg(long, default_value_t = DEFAULT_GRAPH_NODES, value_name = "N")]
    max_nodes: usize,
    /// One kind, or several separated by commas.
    #[arg(long, value_enum, value_delimiter = ',')]
    kind: Vec<SymbolKind>,
    /// Repeat to match any of several globs; `!GLOB` excludes.
    #[arg(long, value_name = "GLOB")]
    path: Vec<String>,
    #[arg(long)]
    include_generated: bool,
    /// Skip the refresh and return the committed state, labeled stale.
    #[arg(long)]
    stale_ok: bool,
    /// Drop the last nodes found, with their edges, until the output fits.
    #[arg(long, value_parser = parse_size, value_name = "BYTES")]
    max_bytes: Option<u64>,
}

#[derive(Debug, Args)]
struct PageArgs {
    #[arg(long, value_name = "N")]
    limit: Option<usize>,
    #[arg(long)]
    cursor: Option<String>,
    /// Skip the refresh and return the committed state, labeled stale.
    #[arg(long)]
    stale_ok: bool,
    /// Drop rows from the end of the page until the output fits; the cursor resumes at the first dropped row.
    #[arg(long, value_parser = parse_size, value_name = "BYTES")]
    max_bytes: Option<u64>,
}

impl PageArgs {
    fn request(&self, default_limit: usize) -> PageRequest {
        PageRequest {
            limit: self.limit.unwrap_or(default_limit),
            cursor: self.cursor.clone(),
        }
    }
}

fn main() -> ExitCode {
    let json_requested = std::env::args_os().any(|arg| arg == "--json");
    let cli = match Cli::try_parse() {
        Ok(cli) => cli,
        Err(error) if !error.use_stderr() => {
            let _ = error.print();
            return ExitCode::SUCCESS;
        }
        Err(error) => {
            let message = error.render().to_string();
            output::print_error(json_requested, "usage", message.trim());
            return ExitCode::from(2);
        }
    };
    let printer = Printer::new(cli.json);
    match run(cli, &printer) {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            output::print_error(printer.json(), error.kind().as_str(), &error.to_string());
            ExitCode::from(error.kind().exit_code())
        }
    }
}

fn run(cli: Cli, printer: &Printer) -> Result<(), Error> {
    let cwd = start_dir(cli.cwd)?;
    let lantern = Lantern::open(&Options {
        home: cli.home,
        timeout: (cli.timeout > 0).then(|| Duration::from_millis(cli.timeout)),
        max_file_size: cli.max_file_size,
    })?;
    match cli.command {
        Command::Index {
            path,
            rebuild,
            force,
        } => {
            let mode = if rebuild {
                Mode::Rebuild
            } else {
                Mode::Incremental
            };
            let path = cwd.join(path.unwrap_or_default());
            printer.document(&lantern.index(&path, mode, force)?)
        }
        Command::Status { path } => {
            printer.document(&lantern.status(&cwd.join(path.unwrap_or_default()))?)
        }
        Command::Prune => printer.document(&lantern.prune()?),
        Command::Symbols(args) => symbols(&lantern, &cwd, printer, args),
        Command::Code(args) => code(&lantern, &cwd, printer, args),
        Command::Refs(args) => refs(&lantern, &cwd, printer, args),
        Command::Graph(args) => graph(&lantern, &cwd, printer, args),
        Command::Outline { files, page } => {
            for_each_query(printer, &page, files, |file, freshness| {
                let request = page.request(DEFAULT_OUTLINE_LIMIT);
                let outline = lantern.outline(&cwd, &file, freshness, &request)?;
                let note = incomplete_note(outline.symbols.incomplete);
                printer.outline(&outline, page.max_bytes, &note)
            })
        }
    }
}

fn start_dir(cwd: Option<PathBuf>) -> Result<PathBuf, Error> {
    match cwd {
        Some(dir) if is_context_count(&dir) => {
            let count = dir.display();
            Err(Error::usage(format!(
                "-C sets the directory, and {count} does not exist; for context lines use --context {count}"
            )))
        }
        Some(dir) => Ok(dir),
        None => std::env::current_dir()
            .map_err(|error| Error::usage(format!("no current directory: {error}"))),
    }
}

fn freshness(stale_ok: bool) -> Freshness {
    if stale_ok {
        Freshness::Stale
    } else {
        Freshness::Walk
    }
}

fn symbols(
    lantern: &Lantern,
    cwd: &Path,
    printer: &Printer,
    args: SymbolsArgs,
) -> Result<(), Error> {
    let SymbolsArgs {
        queries,
        kind,
        language,
        path,
        include_generated,
        body,
        page,
    } = args;
    let run = |query: Option<String>, freshness| {
        let query = SymbolQuery {
            query,
            kind: kind.clone(),
            language: language.clone(),
            path: path.clone(),
            include_generated,
            body,
            // A body is long, so `--body` alone skips the near misses of an exact name.
            exact_only: body && page.limit.is_none(),
        };
        symbols_page(lantern, cwd, printer, &query, freshness, &page)
    };
    if queries.is_empty() {
        return run(None, freshness(page.stale_ok));
    }
    for_each_query(printer, &page, queries, |query, freshness| {
        run(Some(query), freshness)
    })
}

/// Runs `each` once per query; with several, each page follows a `== QUERY` line. The first run
/// refreshes the index, and the rest read the same state.
fn for_each_query(
    printer: &Printer,
    page: &PageArgs,
    queries: Vec<String>,
    mut each: impl FnMut(String, Freshness) -> Result<(), Error>,
) -> Result<(), Error> {
    let several = queries.len() > 1;
    let needs_one_query = printer.json() || page.cursor.is_some();
    if several && needs_one_query {
        return Err(Error::usage(
            "--json and --cursor take one query; run the command once per query",
        ));
    }
    if !several {
        return queries
            .into_iter()
            .try_for_each(|query| each(query, freshness(page.stale_ok)));
    }
    let total = queries.len();
    let mut freshness = freshness(page.stale_ok);
    let mut first_failure: Option<ErrorKind> = None;
    let mut failures = 0;
    for query in queries {
        printer.heading(&format!("== {query}"))?;
        match each(query, freshness) {
            Ok(()) => freshness = Freshness::Stale,
            // The later queries still print; the exit code is the first failure's.
            Err(error) => {
                output::print_error(false, error.kind().as_str(), &error.to_string());
                first_failure.get_or_insert(error.kind());
                failures += 1;
            }
        }
    }
    match first_failure {
        None => Ok(()),
        Some(kind) => Err(Error::new(
            kind,
            format!("{failures} of {total} queries failed"),
        )),
    }
}

fn symbols_page(
    lantern: &Lantern,
    cwd: &Path,
    printer: &Printer,
    query: &SymbolQuery,
    freshness: Freshness,
    page: &PageArgs,
) -> Result<(), Error> {
    let default_limit = if query.body {
        DEFAULT_BODY_LIMIT
    } else {
        DEFAULT_LIMIT
    };
    let found = lantern.symbols(cwd, query, freshness, &page.request(default_limit))?;
    let note = incomplete_note(found.page.incomplete);
    let Some(text) = &query.query else {
        let empty = if note.is_empty() {
            "no results".to_owned()
        } else {
            format!("no results;{note}")
        };
        return printer.page(
            &found.page,
            page.max_bytes,
            &[],
            &empty,
            output::symbol_lines,
        );
    };
    let empty = match (found.other_containers.first(), split_qualified(text)) {
        (Some(first), (Some(container), name)) => format!(
            "no results; \"{name}\" is not defined in {container}. It is defined in: {}. Try: lantern symbols {first}.{name}{note}",
            found.other_containers.join(", ")
        ),
        _ => format!("no results; for a field, a key, or text, try: lantern code \"{text}\"{note}"),
    };
    printer.page(
        &found.page,
        page.max_bytes,
        &[],
        &empty,
        output::symbol_lines,
    )
}

fn code(lantern: &Lantern, cwd: &Path, printer: &Printer, args: CodeArgs) -> Result<(), Error> {
    let CodeArgs {
        pattern,
        dash_pattern,
        regex,
        case_sensitive,
        ignore_case: _,
        path,
        language,
        per_file,
        files,
        context,
        sort,
        include_generated,
        page,
    } = args;
    let pattern = pattern
        .or(dash_pattern)
        .ok_or_else(|| Error::usage("a pattern is required"))?;
    let query = CodeQuery {
        pattern,
        regex,
        case_sensitive,
        path,
        language,
        per_file: if files { 0 } else { per_file },
        context,
        sort,
        include_generated,
    };
    let found = lantern.code(
        cwd,
        &query,
        freshness(page.stale_ok),
        &page.request(DEFAULT_CODE_LIMIT),
    )?;
    let note = incomplete_note(Incomplete {
        unparsed_files: 0,
        ..found.incomplete
    });
    let empty = match code_empty(&query) {
        base if note.is_empty() => base.to_owned(),
        "no results" => format!("no results;{note}"),
        base => format!("{base}.{note}"),
    };
    printer.page(&found, page.max_bytes, &[], &empty, output::code_lines)
}

/// The text for no matches. `a\|b` is grep's alternation; in Rust regex `\|` is a literal `|`.
fn code_empty(query: &CodeQuery) -> &'static str {
    let grep_alternation = query.pattern.contains("\\|");
    if query.regex && grep_alternation {
        return "no results; in --regex, `\\|` matches a literal `|`. For alternation, write `a|b`";
    }
    if query.regex {
        return "no results";
    }
    if grep_alternation {
        return "no results; the pattern was matched as literal text and has regex syntax. For a regex, add --regex and write `a|b` for alternation, not `a\\|b`";
    }
    if looks_like_regex(&query.pattern) {
        return "no results; the pattern was matched as literal text and has regex syntax. For a regex, add --regex";
    }
    "no results"
}

fn refs(lantern: &Lantern, cwd: &Path, printer: &Printer, args: RefsArgs) -> Result<(), Error> {
    let query = RefsQuery {
        name: args.name,
        kind: args.kind,
        path: args.path,
        include_generated: args.include_generated,
    };
    let found = lantern.refs(
        cwd,
        &query,
        freshness(args.page.stale_ok),
        &args.page.request(DEFAULT_LIMIT),
    )?;
    let extra = [("resolution", serde_json::json!("name"))];
    let note = incomplete_note(found.incomplete);
    let empty = format!(
        "no results; refs records calls, types, imports, and implementations by exact, case-sensitive name. For a field, a key, or text, try: lantern code \"{}\"{note}",
        query.name
    );
    printer.page(
        &found,
        args.page.max_bytes,
        &extra,
        &empty,
        output::ref_lines,
    )
}

fn graph(lantern: &Lantern, cwd: &Path, printer: &Printer, args: GraphArgs) -> Result<(), Error> {
    let query = GraphQuery {
        name: args.name,
        direction: args.direction,
        depth: args.depth,
        max_nodes: args.max_nodes,
        kind: args.kind,
        path: args.path,
        include_generated: args.include_generated,
    };
    let graph = lantern.graph(cwd, &query, freshness(args.stale_ok))?;
    let note = incomplete_note(graph.incomplete);
    let empty = format!(
        "no results; no definition named {}. To find it as text, try: lantern code \"{}\"{note}",
        query.name, query.name
    );
    printer.graph(&graph, args.max_bytes, &empty)
}

/// Text for an empty result: files that ran out of parse time hold no symbols to find, and
/// unreadable files hold no content.
fn incomplete_note(incomplete: Incomplete) -> String {
    let mut note = String::new();
    let Incomplete {
        unparsed_files,
        unreadable_files,
        no_file_in_path,
    } = incomplete;
    if no_file_in_path {
        note += " No indexed file matches --path here; ignored files are not indexed.";
    }
    if unparsed_files > 0 {
        note += &format!(
            " Parsing ran out of time for {unparsed_files} file(s), so their symbols are missing. To parse them again, run: lantern index --rebuild"
        );
    }
    if unreadable_files > 0 {
        note += &format!(
            " Lantern could not open {unreadable_files} file(s) (access denied or locked), so their content is missing; the next query reads them again."
        );
    }
    note
}

/// `-C 3` is grep's context flag; here `-C` takes a directory.
fn is_context_count(dir: &Path) -> bool {
    !dir.exists() && dir.to_str().is_some_and(|text| text.parse::<u32>().is_ok())
}

fn parse_size(text: &str) -> Result<u64, String> {
    let text = text.trim();
    let (digits, unit) = match text.char_indices().last() {
        Some((index, suffix)) if suffix.is_ascii_alphabetic() => {
            (&text[..index], suffix.to_ascii_uppercase())
        }
        _ => (text, 'B'),
    };
    let multiplier: u64 = match unit {
        'B' => 1,
        'K' => 1 << 10,
        'M' => 1 << 20,
        'G' => 1 << 30,
        _ => return Err(format!("unknown size suffix in {text:?}; use K, M, or G")),
    };
    let value: u64 = digits
        .parse()
        .map_err(|_| format!("invalid size {text:?}"))?;
    value
        .checked_mul(multiplier)
        .ok_or_else(|| format!("size {text:?} is too large"))
}
