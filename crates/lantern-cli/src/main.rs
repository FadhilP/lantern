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

use std::path::PathBuf;
use std::process::ExitCode;
use std::time::Duration;

use clap::{Args, Parser, Subcommand};

use crate::engine::{Lantern, Options};
use crate::error::{Error, ErrorKind};
use crate::extract::{RefKind, SymbolKind};
use crate::limits::{
    DEFAULT_CODE_LIMIT, DEFAULT_GRAPH_DEPTH, DEFAULT_GRAPH_NODES, DEFAULT_LIMIT, DEFAULT_PER_FILE,
};
use crate::output::Printer;
use crate::query::code::{CodeQuery, CodeSort};
use crate::query::graph::{Direction, GraphQuery};
use crate::query::refs::RefsQuery;
use crate::query::symbols::SymbolQuery;
use crate::query::{Freshness, PageRequest};
use crate::refresh::Mode;

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
    },
    /// Show the committed index state without refreshing.
    Status { path: Option<PathBuf> },
    /// Remove shards of roots that no longer exist.
    Prune,
    /// Where is X defined?
    Symbols {
        query: String,
        #[arg(long, value_parser = parse_symbol_kind)]
        kind: Option<SymbolKind>,
        #[arg(long)]
        language: Option<String>,
        /// Repeat to match any of several globs.
        #[arg(long, value_name = "GLOB")]
        path: Vec<String>,
        #[arg(long)]
        include_generated: bool,
        #[command(flatten)]
        page: PageArgs,
    },
    /// Where does this text or pattern occur?
    Code {
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
        /// Repeat to match any of several globs.
        #[arg(long, value_name = "GLOB")]
        path: Vec<String>,
        #[arg(long)]
        language: Option<String>,
        #[arg(long, default_value_t = DEFAULT_PER_FILE, value_name = "N")]
        per_file: usize,
        #[arg(long, default_value_t = 0, value_name = "N")]
        context: usize,
        #[arg(long, value_enum, default_value_t = CodeSort::Relevance)]
        sort: CodeSort,
        #[arg(long)]
        include_generated: bool,
        #[command(flatten)]
        page: PageArgs,
    },
    /// Who uses X?
    Refs {
        name: String,
        #[arg(long, value_parser = parse_ref_kind)]
        kind: Option<RefKind>,
        /// Repeat to match any of several globs.
        #[arg(long, value_name = "GLOB")]
        path: Vec<String>,
        #[arg(long)]
        include_generated: bool,
        #[command(flatten)]
        page: PageArgs,
    },
    /// What calls X, and what does X call?
    Graph {
        name: String,
        #[arg(long, value_enum, default_value_t = Direction::Both)]
        direction: Direction,
        #[arg(long, default_value_t = DEFAULT_GRAPH_DEPTH, value_name = "N")]
        depth: usize,
        #[arg(long, default_value_t = DEFAULT_GRAPH_NODES, value_name = "N")]
        max_nodes: usize,
        #[arg(long, value_parser = parse_symbol_kind)]
        kind: Option<SymbolKind>,
        /// Repeat to match any of several globs.
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
    },
    /// What is in this file?
    Outline {
        file: String,
        #[command(flatten)]
        page: PageArgs,
    },
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
    let json_requested = std::env::args().any(|arg| arg == "--json");
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
    let cwd = match cli.cwd {
        Some(dir) if is_context_count(&dir) => {
            let count = dir.display();
            return Err(Error::new(
                ErrorKind::Usage,
                format!(
                    "-C sets the directory, and {count} does not exist; for context lines use --context {count}"
                ),
            ));
        }
        Some(dir) => dir,
        None => std::env::current_dir().map_err(|error| {
            Error::new(ErrorKind::Usage, format!("no current directory: {error}"))
        })?,
    };
    let lantern = Lantern::open(&Options {
        home: cli.home,
        timeout: (cli.timeout > 0).then(|| Duration::from_millis(cli.timeout)),
        max_file_size: cli.max_file_size,
    })?;
    let freshness = |stale_ok: bool| {
        if stale_ok {
            Freshness::Stale
        } else {
            Freshness::Walk
        }
    };
    match cli.command {
        Command::Index { path, rebuild } => {
            let mode = if rebuild {
                Mode::Rebuild
            } else {
                Mode::Incremental
            };
            let report = lantern.index(&cwd.join(path.unwrap_or_default()), mode)?;
            printer.document(&report);
        }
        Command::Status { path } => {
            printer.document(&lantern.status(&cwd.join(path.unwrap_or_default()))?)
        }
        Command::Prune => printer.document(&lantern.prune()?),
        Command::Symbols {
            query,
            kind,
            language,
            path,
            include_generated,
            page,
        } => {
            let query = SymbolQuery {
                query,
                kind,
                language,
                path,
                include_generated,
            };
            let found = lantern.symbols(
                &cwd,
                &query,
                freshness(page.stale_ok),
                &page.request(DEFAULT_LIMIT),
            )?;
            let note = unparsed_note(found.unparsed_files);
            let empty = format!(
                "no results; for a field, a key, or text, try: lantern code \"{}\"{note}",
                query.query
            );
            printer.page(&found, page.max_bytes, &[], &empty, output::symbol_lines)?;
        }
        Command::Code {
            pattern,
            dash_pattern,
            regex,
            case_sensitive,
            ignore_case: _,
            path,
            language,
            per_file,
            context,
            sort,
            include_generated,
            page,
        } => {
            let pattern = pattern
                .or(dash_pattern)
                .ok_or_else(|| Error::new(ErrorKind::Usage, "a pattern is required"))?;
            let query = CodeQuery {
                pattern,
                regex,
                case_sensitive,
                path,
                language,
                per_file,
                context,
                sort,
                include_generated,
            };
            let found = lantern.code(
                &cwd,
                &query,
                freshness(page.stale_ok),
                &page.request(DEFAULT_CODE_LIMIT),
            )?;
            printer.page(
                &found,
                page.max_bytes,
                &[],
                "no results",
                output::code_lines,
            )?;
        }
        Command::Refs {
            name,
            kind,
            path,
            include_generated,
            page,
        } => {
            let query = RefsQuery {
                name,
                kind,
                path,
                include_generated,
            };
            let found = lantern.refs(
                &cwd,
                &query,
                freshness(page.stale_ok),
                &page.request(DEFAULT_LIMIT),
            )?;
            let extra = [("resolution", serde_json::json!("name"))];
            let note = unparsed_note(found.unparsed_files);
            let empty = format!(
                "no results; refs records calls, types, imports, and implementations by exact, case-sensitive name. For a field, a key, or text, try: lantern code \"{}\"{note}",
                query.name
            );
            printer.page(&found, page.max_bytes, &extra, &empty, output::ref_lines)?;
        }
        Command::Graph {
            name,
            direction,
            depth,
            max_nodes,
            kind,
            path,
            include_generated,
            stale_ok,
            max_bytes,
        } => {
            let query = GraphQuery {
                name,
                direction,
                depth,
                max_nodes,
                kind,
                path,
                include_generated,
            };
            let graph = lantern.graph(&cwd, &query, freshness(stale_ok))?;
            let note = unparsed_note(graph.unparsed_files);
            let empty = format!(
                "no results; no definition named {}. To find it as text, try: lantern code \"{}\"{note}",
                query.name, query.name
            );
            printer.graph(&graph, max_bytes, &empty)?;
        }
        Command::Outline { file, page } => {
            let outline = lantern.outline(
                &cwd,
                &file,
                freshness(page.stale_ok),
                &page.request(DEFAULT_LIMIT),
            )?;
            let note = unparsed_note(outline.symbols.unparsed_files);
            printer.outline(&outline, page.max_bytes, &note)?;
        }
    }
    Ok(())
}

/// Text for an empty result: files that ran out of parse time hold no symbols to find.
fn unparsed_note(unparsed_files: u64) -> String {
    if unparsed_files == 0 {
        return String::new();
    }
    format!(
        " Parsing ran out of time for {unparsed_files} file(s), so their symbols are missing. To parse them again, run: lantern index --rebuild"
    )
}

/// `-C 3` is grep's context flag; here `-C` takes a directory.
fn is_context_count(dir: &std::path::Path) -> bool {
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

fn parse_symbol_kind(text: &str) -> Result<SymbolKind, String> {
    SymbolKind::from_name(text).ok_or_else(|| format!("unknown symbol kind {text:?}"))
}

fn parse_ref_kind(text: &str) -> Result<RefKind, String> {
    RefKind::from_name(text).ok_or_else(|| {
        format!("unknown reference kind {text:?}; use call, type, import, or implementation")
    })
}
