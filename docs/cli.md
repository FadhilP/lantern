# CLI reference

```text
lantern [GLOBAL OPTIONS] <COMMAND>

lantern index   [PATH] [--rebuild]
lantern status  [PATH]
lantern prune
lantern symbols <query>   [--kind K] [--language L] [--path GLOB]... [--include-generated] [--body]
lantern code    <pattern> | -e <pattern> [--regex] [--case-sensitive] [--path GLOB]... [--language L]
                          [--per-file N] [--context N] [--sort relevance|path] [--include-generated]
lantern refs    <name>    [--kind call|type|import|implementation] [--path GLOB]... [--include-generated]
lantern graph   <name>    [--direction callers|callees|both] [--depth N] [--max-nodes N]
                          [--kind K] [--path GLOB]... [--include-generated] [--stale-ok] [--max-bytes N]
lantern outline <file>
```

The list commands (`symbols`, `code`, `refs`, `outline`) also take `--limit`, `--cursor`, `--stale-ok`, and `--max-bytes`.

## Global options

| Option | Env | Default | Meaning |
|---|---|---|---|
| `-C, --cwd <DIR>` | — | current directory | Run as if started in this directory. |
| `--home <DIR>` | `LANTERN_HOME` | platform data directory | Where the index is stored. |
| `--timeout <MS>` | `LANTERN_TIMEOUT_MS` | `120000` | Limit for a warm refresh. `0` waits without a limit. A first index and `--rebuild` have no limit. |
| `--max-file-size <BYTES>` | `LANTERN_MAX_FILE_SIZE` | `1M` | Larger files are not indexed as text. Accepts `K`, `M`, `G`. Max `64M`. |
| `--json` | — | off | One JSON document instead of text. |

The data directory is `%LOCALAPPDATA%\lantern` on Windows, `~/Library/Application Support/lantern` on macOS, and `$XDG_DATA_HOME/lantern` (or `~/.local/share/lantern`) on Linux.

## Roots and scope

- Inside a Git working tree, the root is the top level of that tree. For a checked-out submodule, the root is the top level of the outermost repository that holds it as a gitlink. Outside Git, the root is the start directory itself.
- A start directory below the root scopes every query to that subtree. Paths in results are relative to the start directory (`../` for a file outside it), so a printed path works as a `--path` or `outline` argument from the same directory.
- Checked-out submodules (gitlinks) are indexed with the repository that contains them. Their paths carry the submodule prefix, and `refs` and `graph` resolve names across them. A start directory inside a submodule scopes queries to it, like any other subdirectory.

## What is indexed

| Source | Files |
|---|---|
| Git | Tracked files, plus untracked files that Git's excludes do not ignore. |
| Folder | Every file, minus `.gitignore` matches (scoped per directory) and the defaults: dot-files and dot-directories, `node_modules/`, `dist/`, `build/`, `coverage/`, `target/`, `vendor/`. |

- `.lanternignore` files use gitignore syntax and exclude more paths in both sources. In a Git root they cannot bring back files that Git ignores. In a folder root, a `!` rule can bring back a default exclude.
- Binary files and files over `--max-file-size` are listed but not searchable as text.
- **Generated** files are lockfiles and paths marked `linguist-generated` or `linguist-vendored` in `.gitattributes`. They are left out of `symbols`, `refs`, and `graph`, and ranked last in `code`. `--include-generated` includes them.
- **Test** files (under `test/`, `tests/`, `__tests__/`, or named like `*_test.*`, `*.test.*`, `*.spec.*`) rank after other files in `symbols`.
- Symbols and references come from tree-sitter for C, C++, C#, Dart, Go, Java, JavaScript, PHP, Python, Ruby, Rust, TypeScript, and TSX. The language comes from the file name, or from the shebang line.

## Freshness

By default every query refreshes the index first. The refresh reads only new files, files whose metadata (identity, size, modification time, change time) changed, and files that changed within 2 seconds before the previous refresh or after it started. An edit that keeps all of that metadata is not seen until `index --rebuild`. Results are labeled `"freshness": "walk"`.

`--stale-ok` skips the refresh and answers from the last committed index, labeled `"freshness": "stale"`. It fails with exit 1 when the root was never indexed.

## Commands

### `index [PATH]`

Builds or refreshes the index of the root at `PATH` (default: the current directory). `--rebuild` builds a fresh shard from the disk and swaps it in.

```json
{"schema":1,"root":"/repo","source":"git","files":2,"text_files":2,"code_files":2,"symbols":4,"refs":3,"unparsed_files":0,
 "indexed_at":1794058791146,"changed":2,"removed":0,"reused":0,"parse_timeouts":0,"elapsed_ms":118}
```

`indexed_at` is a Unix time in milliseconds: the last commit that changed results. `reused` counts files that were read again, or moved, and whose content was already stored. `parse_timeouts` counts the files that ran out of parse time in this refresh. Later refreshes try them again, up to 3 timeouts in a row. `unparsed_files` counts all files that hold no symbols because their last parse ran out of time, including files out of retries. `--rebuild` parses them again. When `symbols`, `refs`, `graph`, or `outline` finds nothing and this count is not 0, the text output says so.

### `status [PATH]`

Reports the committed state without refreshing: `indexed`, `root`, `source`, `head`, `branch`, `indexed_at`, `generation`, and the counts.

### `prune`

Deletes the shards of roots that no longer exist, and stray files in the data directory. Reports `removed_roots` and `removed_files`.

### `symbols <query>`

Search over definitions by exact name, prefix, initials, or substring; a query whose letters are not adjacent in the name does not match. Ranking: exact name, case-insensitive exact name, prefix, initials (`gubi` finds `getUserById` and `get_user_by_id`), then substring (3 characters or more), with a match at the start of a word before a match inside a word. Ties go to the shorter name, the more important kind, non-test files, and shorter paths.

- `Container.name` or `Container::name` narrows to definitions inside that container (case-insensitive).
- `--body` adds the source lines of each definition, up to 100 lines, to `end_line`. A body starts at the first decorator, attribute, annotation, or doc comment right before the definition (a blank line ends them), so it can start before the row's `line`. Each file on the page is parsed again to find that start; if the parse runs out of time, the body starts at `line`. The default limit drops to 5.
- `--kind`: `function`, `method`, `class`, `interface`, `struct`, `enum`, `trait`, `type`, `module`, `constant`, `variable`, `field`.
- `--language`: `c`, `cpp`, `csharp`, `dart`, `go`, `java`, `javascript` (includes `.jsx` files), `php`, `python`, `ruby`, `rust`, `typescript` (also selects `tsx`), `tsx`.

Row: `name, kind, container, path, line, end_line, signature, body?`. `body` is `{line, text}`: the first line number and the source lines. Default limit 30.

### `code <pattern>`

Line search. The pattern is a literal unless `--regex` is set (Rust `regex` syntax). Matching is case-insensitive unless `--case-sensitive` is set. A regex matches within one line.

- `-e <pattern>` gives the pattern as an option, also when it starts with `-` (`-e --flag`).
- `-i` is accepted and changes nothing, since matching already ignores case.

- `--per-file N` caps the matches per file (default 20, max 1000). `match_count` still counts every match.
- `--context N` adds up to N lines as `before` and `after` on each match. A value over 50 counts as 50.
- `--sort relevance` (default) ranks files by FTS score with generated files last. `--sort path` orders by path.

Row (one per file): `path, match_count, matches: [{line, column, text, before?, after?}]`. `column` is a 1-based byte column. Line text is capped at 500 characters. Default limit 10 files.

### `refs <name>`

References to an exact, case-sensitive name. `--kind call|type|import|implementation` filters them. Every language records `call` references. `type` references exist only for Dart, Go, and TypeScript/TSX; `import` references only for Dart and JavaScript/TypeScript/TSX; `implementation` references for Dart, Java, JavaScript/TypeScript/TSX, PHP, and Rust. In other languages, find type uses and imports with `lantern code`.

Row: `path, line, column, kind, text, enclosing`. `enclosing` is `{name, container, kind}` of the innermost definition around the reference; it is left out at the top level. The page has `"resolution": "name"`.

### `graph <name>`

Callers and callees of a name, breadth-first. The graph follows only `call` references; use `refs` for type uses, imports, and implementations.

- Roots are the definitions of `name`; `--kind` and `--path` narrow them. Every match is a root.
- `--direction callers`: calls to the name, grouped by their enclosing definition. A call outside any definition becomes a node of kind `file`.
- `--direction callees`: calls whose innermost enclosing definition is this one, resolved to definitions by name. A definition outside test files never resolves to one in a test file.
- `--depth N` (1–3, default 1) repeats the expansion. `--max-nodes N` (default 200, max 5000) stops it and sets `truncated`.
- `--max-bytes N` drops the last nodes found, with their edges, until the output fits, and sets `truncated`.

```json
{"schema":1,"freshness":"walk","indexed_at":1794058791146,"resolution":"name","roots":[0],
 "nodes":[{"id":0,"name":"loadUser","kind":"function","path":"src/user.ts","line":6}],
 "edges":[{"from":1,"to":0,"kind":"call","line":3}],
 "unresolved":["fetchUser"]}
```

An edge's `line` is in the file of its `from` node. `unresolved` lists called names with no definition in the workspace. A graph is not a list, so it has no `--limit` or `--cursor`.

### `outline <file>`

The symbols of one file, nested by line range, in line order. `file` is relative to the current directory, or absolute. A page holds top-level symbols, each with all of its `children`.

Row: `name, kind, line, end_line, signature, children?`. The page also has `path` and `language`. Default limit 30.

## Pages

List results look like this:

```json
{"schema":1,"freshness":"walk","indexed_at":1794058791146,"truncated":true,"next_cursor":"30.21ce295eb3fdedeb.fef4123b6a059c1a","results":[...]}
```

- `--limit N` sets the page size (1–10000).
- Null fields are left out, and so are `truncated` and `next_cursor` on the last page.
- When `truncated` is true, pass `next_cursor` back with `--cursor` and the same query to get the next page.
- An empty page carries `hint` when the text output would show one: a better command to try, or why nothing matched.
- A cursor belongs to one query. Using it with another query is a usage error (exit 2). If the index changed since the cursor was made, the command fails with exit 4; run the query again without `--cursor`.
- `--max-bytes N` removes rows from the end of the page until the output fits, and points `next_cursor` at the first removed row, so no row is lost. If not even one row fits, the command exits 2.

## `--path`

A glob relative to the current directory. Without a `/` it matches at any depth (`*.test.ts`). With a `/` it is anchored to the current directory (`src/**/*.rs`, `../lib/*`). It must stay inside the workspace. Repeat `--path` to match any of several globs (`--path 'src/**' --path 'lib/**'`). On Windows, `\` works as a separator.

## Errors and exit codes

Errors go to stderr as `{"error":{"code":"...","message":"..."}}`, with the message capped at 4 KiB.

| Exit | `code` | Meaning |
|---|---|---|
| 0 | — | Success. |
| 1 | `failure` | The command failed: unreadable files or Git data, an exceeded limit, a refresh timeout, a root that is not indexed under `--stale-ok`. |
| 2 | `usage` | Invalid arguments, glob, or cursor, or a `--max-bytes` smaller than one result. |
| 3 | `root_moved` | A directory indexed as a folder root is now inside a Git repository. |
| 4 | `stale_cursor` | The index changed since the cursor was made. |
| 75 | `temporary` | Another refresh holds the lock, files kept changing during 3 attempts, SQLite stayed busy, or (`index` only) a reader blocked the cleanup checkpoint after a commit. Retrying can succeed. |

## Text output

Without `--json`, list commands and `graph` print compact text for agents and people. Scripts should use `--json`; the text format can change.

- `symbols`: `path:line-end_line kind[ in Container]: signature`, one per line. With `--body`, `line:text` lines follow, then `-- body cut: N more lines` when the body is longer than 100 lines.
- `code`: a heading line with the file path (`path (N matches, M shown)` when `--per-file` cut matches), then `line:text` for each match and `line-text` for `--context` lines.
- `refs`: a heading line with the file path, then `line kind[ in Container.name]: text` for each reference.
- `outline`: `line-end_line kind name`, indented two spaces per nesting level.
- `graph`: one line per node, `id kind Container.name path:line`, with roots marked `(root)`. Then one line per edge, `from -> to kind at line N`, where line N is in the file of the `from` node. Then `unresolved: ...`.
- `no results` means nothing matched. With `--json`, an empty page or graph carries the same hint in a `hint` field. For `symbols`, `refs`, and `graph`, a hint follows with the `lantern code` command that finds the name as text, since fields, keys, and strings are not recorded as definitions or references. For `symbols Container.name` when other containers define the name, the hint lists them instead. For a literal `code` pattern with regex syntax (`a|b`, `getStats\(`), the hint says to add `--regex`. For `outline`, a hint follows when no parser covers the file type (CSS, HTML), so a file with no symbols stays apart from a file Lantern cannot outline.
- A last line `-- more: --cursor <value>` means more rows exist. `-- truncated` on a graph means `--max-nodes` or `--max-bytes` cut it.

`index`, `status`, and `prune` print their report as indented JSON. Errors print as `lantern: <message>`. When `outline` gets a path that is not in the index, the error lists up to 5 indexed files with the same file name. `-C` takes a directory; `-C 3` with no directory `3` is a usage error that points to `--context 3`.
