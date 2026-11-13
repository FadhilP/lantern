# CLI reference

```text
lantern [GLOBAL OPTIONS] <COMMAND>

lantern index   [PATH] [--rebuild] [--force]
lantern status  [PATH]
lantern prune
lantern symbols [query]...[--kind K,...] [--language L] [--path GLOB]... [--include-generated] [--body] [--fuzzy]
lantern code    <pattern> | -e <pattern> [--regex] [--case-sensitive] [--path GLOB]... [--language L]
                          [--per-file N | -l] [--context N] [--sort relevance|path] [--include-generated]
lantern refs    <name>    [--kind call|type|import|implementation,...] [--path GLOB]... [--include-generated]
lantern graph   <name>    [--direction callers|callees|both] [--depth N] [--max-nodes N]
                          [--kind K,...] [--path GLOB]... [--include-generated] [--stale-ok] [--max-bytes N]
lantern outline <file>...
lantern changes [RANGE]   [--path GLOB]... [--diff [NAME]]
```

The list commands (`symbols`, `code`, `refs`, `outline`, `changes`) also take `--limit`, `--cursor`, `--stale-ok`, and `--max-bytes`.

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
- A query builds the first index only inside a Git repository. In a folder outside Git that has no index, a query exits 2: run `lantern index <dir>` once, and later queries refresh as usual. The first index of the home folder, a folder that holds it, or a drive or filesystem root needs `lantern index <dir> --force`; without it, both a query and `index` exit 2.
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

Builds or refreshes the index of the root at `PATH` (default: the current directory). `--rebuild` builds a fresh shard from the disk and swaps it in. `--force` allows the first index of the home folder, a folder that holds it, or a drive or filesystem root.

```json
{"schema":1,"root":"/repo","source":"git","files":2,"text_files":2,"code_files":2,"symbols":4,"refs":3,"unparsed_files":0,"unreadable_files":0,
 "indexed_at":1794058791146,"changed":2,"removed":0,"reused":0,"parse_timeouts":0,"elapsed_ms":118}
```

`indexed_at` is a Unix time in milliseconds: the last commit that changed results. `reused` counts files that were read again, or moved, and whose content was already stored. `parse_timeouts` counts the files that ran out of parse time in this refresh. Later refreshes try them again, up to 3 timeouts in a row. `unparsed_files` counts all files that hold no symbols because their last parse ran out of time, including files out of retries. `--rebuild` parses them again. `unreadable_files` counts files that the last refresh could not open or read, for example because access was denied or another process locked them. They are indexed with no content, and every refresh tries them again; running out of memory or file handles still fails the refresh. A directory below the root that cannot be listed is skipped with its files, as Git skips it; a root that cannot be listed fails. When `symbols`, `refs`, `graph`, or `outline` finds nothing and `unparsed_files` or `unreadable_files` is not 0, the text output says so; `code` reports only `unreadable_files`.

### `status [PATH]`

Reports the committed state without refreshing: `indexed`, `root`, `source`, `head`, `branch`, `indexed_at`, `generation`, and the counts.

### `prune`

Deletes the shards of roots that no longer exist, shards that SQLite cannot read, and stray files in the data directory. Reports `removed_roots` and `removed_files`; a shard that cannot be read counts as a removed file. It leaves a shard that a newer Lantern wrote, and that shard's files, alone.

### `symbols [query]...`

Search over definitions by exact name, prefix, initials, or substring; a query whose letters are not adjacent in the name does not match. Ranking: exact name, case-insensitive exact name, prefix, initials (`gubi` finds `getUserById` and `get_user_by_id`), then substring (3 characters or more), with a match at the start of a word before a match inside a word. Ties go to the shorter name, the more important kind, non-test files, and shorter paths.

- With no query, it lists every definition that passes `--path`, `--kind`, and `--language`, in path and line order.
- Several queries run one after another. Each page follows a `== <query>` line, and `--limit` and `--max-bytes` apply to each page. A query that fails prints its error and the rest still run; the exit code is that of the first failure. `--json` and `--cursor` take one query.
- A constructor shares its type's name. When the type's row is in the results, the constructor row is left out, since the type's row spans it. `--kind method` or `Type.Type` finds the constructor.
- When a name matches the query exactly (in any case), only the exact matches are shown. A first text line `-- more: --fuzzy (N)`, or `fuzzy_hidden` in JSON, counts the prefix, initials, and substring matches it hid. `--fuzzy` lists them after the exact matches. With no exact match, they are always listed.
- `Container.name` or `Container::name` narrows to definitions inside that container (case-insensitive).
- `--body` adds the source lines of each definition, up to 100 lines, to `end_line`. A body starts at the first decorator, attribute, annotation, or doc comment right before the definition (a blank line ends them), so it can start before the row's `line`. The start is stored in the index, so a body needs no second parse. The default limit drops to 5.
- `--kind`: `function`, `method`, `class`, `interface`, `struct`, `enum`, `trait`, `type`, `module`, `constant`, `variable`, `field`. Separate several with commas (`--kind class,interface`).
- `--language`: `c`, `cpp`, `csharp`, `dart`, `go`, `java`, `javascript` (includes `.jsx` files), `php`, `python`, `ruby`, `rust`, `typescript` (also selects `tsx`), `tsx`.

Row: `name, kind, container, path, line, end_line, signature, body?`. `body` is `{line, text}`: the first line number and the source lines. Default limit 30.

### `code <pattern>`

Line search. The pattern is a literal unless `--regex` is set (Rust `regex` syntax). Matching is case-insensitive unless `--case-sensitive` is set. A regex matches within one line. Alternation is `a|b`; grep's `a\|b` matches a literal `|`.

- `-e <pattern>` gives the pattern as an option, also when it starts with `-` (`-e --flag`).
- `-i` is accepted and changes nothing, since matching already ignores case.

- `--per-file N` caps the matches per file (default 5, max 1000). `match_count` still counts every match. `--per-file 0`, or `-l`/`--files`, lists only the files and their match counts.
- `--context N` adds up to N lines as `before` and `after` on each match. A value over 50 counts as 50. Windows of nearby matches do not overlap: a line is shown once, so `before` and `after` can be shorter than N.
- `--sort relevance` (default) ranks files by the pattern's literals they hold, with generated files last: each literal a file holds adds up to 1, and each repeat adds less than the one before. `--sort path` orders by path.

Row (one per file): `path, match_count, matches: [{line, column, text, before?, after?, enclosing?: {name, container?, kind}}]`. `column` is a 1-based byte column. Line text is cut at 200 characters in text output and at 500 in JSON, and a cut line ends with `…`. The same cap applies to `refs` text, `symbols` bodies, and signatures (at most 300). When a match starts too late to show, the match line starts with `…` and shows a window from a fifth of the cap (40 or 100 characters) before the match. Default limit 10 files.

### `refs <name>`

References to an exact, case-sensitive name. `--kind call|type|import|implementation` filters them; separate several with commas (`--kind call,import`). Every language records `call` references. `type` references cover every type use in Dart, Go, and TypeScript/TSX; in C#, C++, Java, JavaScript, and PHP they cover only `new X` (C# also variable types and generic constraints). `import` references exist only for Dart and JavaScript/TypeScript/TSX. `implementation` references (base classes, interfaces, mixins, trait impls) exist in every language except C and Go. Find other type uses and imports with `lantern code`.

Row: `path, line, column, kind, text, enclosing`. `enclosing` is `{name, container, kind}` of the innermost definition around the reference; it is left out at the top level. The page has `"resolution": "name"`.

### `graph <name>`

Callers and callees of a name, breadth-first. The graph follows only `call` references; use `refs` for type uses, imports, and implementations.

- Roots are the definitions of `name`; `--kind` and `--path` narrow them. Every match is a root. `Container.name` or `Container::name` keeps the definitions inside that container (case-insensitive).
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

### `outline <file>...`

The symbols of one file, nested by line range. A member on the line of its container (`enum E { A, B }`) nests in it. A method defined outside its type (a Rust `impl`, a Go receiver) nests under the type when the type comes first in the file, so it can print before symbols on earlier lines; otherwise symbols are in line order. `file` is relative to the current directory, or absolute. A page holds top-level symbols, each with all of its `children`. Several files run one after another, each page under a `== <file>` line; `--json` and `--cursor` take one file.

Row: `name, kind, line, end_line, signature, children?`. The page also has `path` and `language`. Default limit 100.

### `changes [RANGE]`

The definitions that a Git diff changed, per file. It runs `git diff` and parses both sides of each changed file; it does not use or refresh the index, so it works before the first `lantern index`. It needs `git` on `PATH` and a Git repository at or above the start directory (the nearest one, as git finds it).

- `RANGE` is a revision range as `git diff` takes it: `A..B`, `A...B` (from the merge base), or `A` (against the working tree). A `RANGE` that names no revision, such as a path, or more than one base, such as `M^!` for a merge, is a usage error. Without it: the working tree and the index against HEAD, plus untracked files that are not ignored.
- Rows are files in path order: `path, old_path?, status, added, removed, unparsed?, symbols, lines?`. `status` is `modified`, `added`, `deleted`, `renamed`, `copied`, or `untracked`. Git finds a copy only from a file that the diff also changes; `old_path` is then the source, and only the differences from it are listed. `added` and `removed` count changed lines. `unparsed` says why a file has no symbols: `binary`, `too_large` (over `--max-file-size`), `no_parser`, `timed_out`, `unreadable` (a working-tree file that vanished, cannot be read, or that a symbolic link or junction on its path leads outside the repository), or `out_of_sync` (the file's text does not have the diff's lines where git put them: a clean filter or `working-tree-encoding` changed it for git, or it changed since git read it). A UTF-16 `working-tree-encoding` file is `binary`.
- Symbol: `status, kind, name, container?, line, end_line, from?, to?, added, removed, lines?`. Each changed line belongs to the innermost definition around it. A definition is `added` when all its lines are new, `removed` when all are gone (its lines are then on the old side), and `modified` otherwise. A `modified` definition that the new side does not have under the same container and name (after a rename) also gives its old lines. A definition that changed on every line, such as a one-line one, is `modified` when the other side has one of the same name. Definitions inside an added or removed one are not listed. Lines without a letter or digit, such as blank lines and lone braces, do not make a definition changed on their own.
- A wholly added or removed definition is `moved` when the diff has the same lines, without blank lines and lone braces, on the other side in another place (so an edit of such a line, `[],` to `{},`, does not count): in any changed file, also one outside the page or `--path`. A moved definition at its new place has `from: {path, line}`; at its old place it has `to`. The line is the first line there with a letter or digit. Indentation counts only relative to the definition's first line, so a function can move into a class, but a line that leaves its block is a change. At the other place, a definition must have exactly these lines, with its decorators and doc comments, so the text is not part of a longer change. Inside one file, it must be in another hunk, so a definition reindented in place is `modified` when it keeps its container (and `removed` and `added` when it does not, such as a function wrapped in a new class). The removed lines of a copy are no source of a move, since its source still has them. A definition with fewer than 20 letters and digits never moves, as in git's `--color-moved`, since such text (`FAILED,`, `Long getId();`) also occurs by chance. When an added or removed definition did not move as a whole, the moved definitions inside it are listed (a method moved into a new class). A definition moved with edits is `removed` and `added`, or `modified` when it stays in its file under the same container and name. A moved definition has `added` and `removed` 0 and no lines, also with `--diff`; its lines count only in the file totals.
- `--diff` gives each definition its changed lines: `sign, line, text`. A file also gets `lines`: its changed lines outside every listed definition, without blank lines and lone braces (all of them when the file is unparsed). Without `--diff`, these lines count only in the file totals.
- `--diff NAME` keeps the definitions named `NAME` (`Container.name` too) and the files that have them, also inside an added or removed definition. Each gets every changed line inside it, also the lines of the definitions it holds. `--diff` takes the next word as `NAME`, so put `RANGE` before a bare `--diff`.
- `--path` narrows the files. Default limit 30 files. Only the files of the page are parsed, except with `--diff NAME`. Untracked files are all read, since a definition can move into one. A cursor goes stale (exit 4) when the diff changes.
- Git runs without its index refresh, so `changes` does not write `.git/index`. Submodules, symbolic links (also a file that was one, or became one), files behind a symbolic link or junction that leads outside the repository, and untracked nested repositories are left out. `GIT_DIFF_OPTS` is removed from git's environment, since it overrides `--unified=0`. A file path that is not UTF-8 is left out.

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
- `--max-bytes N` removes rows from the end of the page until the output fits, and points `next_cursor` at the first removed row, so no row is lost. If not even one row fits, the command exits 2, and the message gives the size that one row needs.

## `--path`

A glob relative to the current directory. Without a `/` it matches at any depth (`*.test.ts`). With a `/` it is anchored to the current directory (`src/**/*.rs`, `../lib/*`). It must stay inside the workspace. Repeat `--path` to match any of several globs (`--path 'src/**' --path 'lib/**'`). A value without glob characters also matches the files inside a folder of that name, by the same depth rule: `--path src` also matches `**/src/**`, and `--path lib/src` also matches `lib/src/**`. A glob that starts with `!` excludes its matches (`--path '!**/test/**'`). With only exclude globs, every other path passes. On Windows, `\` works as a separator.

## Errors and exit codes

Errors go to stderr as `{"error":{"code":"...","message":"..."}}`, with the message capped at 4 KiB.

| Exit | `code` | Meaning |
|---|---|---|
| 0 | — | Success. |
| 1 | `failure` | The command failed: unreadable rule files or Git data, an exceeded limit, a refresh timeout, a root that is not indexed under `--stale-ok`, or a shard that SQLite cannot read (the message says to run `lantern index --rebuild`). |
| 2 | `usage` | Invalid arguments, glob, or cursor, a `--max-bytes` smaller than one result, a refused first index (see Roots and scope), or for `changes`: no Git repository, a revision git does not know, or no commit yet. |
| 3 | `root_moved` | A directory indexed as a folder root is now inside a Git repository. |
| 4 | `stale_cursor` | The index (for `changes`, the diff) changed since the cursor was made. |
| 75 | `temporary` | Another refresh holds the lock, files kept changing during 3 attempts, SQLite stayed busy, or (`index` only) a reader blocked the cleanup checkpoint after a commit. Retrying can succeed. |

## Text output

Without `--json`, list commands and `graph` print compact text for agents and people. Scripts should use `--json`; the text format can change. Each result line is one line: a control character other than tab inside it, such as a newline in a file name, prints escaped: `\n`, `\r`, `\u{1b}`.

- `symbols`: `path:line-end_line kind[ in Container]: signature`, one per line. With `--body`, `line:text` lines follow, then `-- body cut: N more lines` when the body is longer than 100 lines.
- `code`: a heading line with the file path (`path (N matches, M shown)` when `--per-file` cut matches; `path (N matches)` with `-l`), then `line:text` for each match and `line-text` for `--context` lines. Where the innermost definition around the matches changes, an indented line right above the match names it: `  in kind Container.name`, or `  not in a named definition` after one ends (code in an anonymous function counts as outside). A definition's decorators and doc comments count as inside it. It names only the matches; `--context` lines can be in other definitions.
- `refs`: a heading line with the file path, then `line:column kind[ in Container.name]: text` for each reference.
- `outline`: `line-end_line kind name`, indented two spaces per nesting level.
- `changes`: a heading line per file, `path (status, +added -removed, why unparsed)` with the parts that apply (`renamed from OLD`, `copied from OLD`). With `--diff`, the file's own `+line:text` and `-line:text` lines follow. Then `  status kind Container.name line-end_line (+added -removed)` per definition, with `from PATH:LINE` or `to PATH:LINE` before the counts for a moved one, and with `--diff` its lines.
- `graph`: one line per node, `id kind Container.name path:line`, with roots marked `(root)`. Then one line per edge, `from -> to kind at line N`, where line N is in the file of the `from` node. Then `unresolved: ...`.
- `no results` means nothing matched. With `--json`, an empty page or graph carries the same hint in a `hint` field. For `symbols`, `refs`, and `graph`, a hint follows with the `lantern code` command that finds the name as text, since fields, keys, and strings are not recorded as definitions or references. For `symbols Container.name` when other containers define the name, the hint lists them instead. For a literal `code` pattern with regex syntax (`a|b`, `getStats\(`), the hint says to add `--regex`. When the pattern has grep's `\|`, with or without `--regex`, the hint says to write `a|b`. For `outline`, a hint follows when no parser covers the file type (CSS, HTML), so a file with no symbols stays apart from a file Lantern cannot outline. When `--path` matches no indexed file, the hint says so: ignored files are not indexed. For `changes`, the hint says there are no changes, or no changed file or definition matches.
- A first line `-- more: --cursor <value>` means more rows exist. `-- truncated` as the first line of a graph means `--max-nodes` or `--max-bytes` cut it. These lines come first, so `| head` cannot cut them off.

`index`, `status`, and `prune` print their report as indented JSON. Errors print as `lantern: <message>`. When `outline` gets a path that is not in the index, the error lists up to 5 indexed files with the same file name. For a folder, a usage error lists up to 10 indexed files in it. For a regex with an unclosed group, the error says to write `\(` for a literal `(`. `-C` takes a directory; `-C 3` with no directory `3` is a usage error that points to `--context 3`.
