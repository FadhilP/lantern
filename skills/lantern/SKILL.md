---
name: lantern
description: Find code fast in large repositories with the `lantern` CLI. Use it to find where a symbol is defined, where text or a regex occurs, who references a name, what calls a function or what it calls, and what a file contains. Prefer it over grep, find, or reading whole files when you explore unfamiliar code, trace a call path, or need results that fit a small output budget.
---

# Lantern

`lantern` is a read-only code index. It never writes inside the repository. Every query refreshes the index first (only changed files are read), so results match the disk.

## Pick the command

| Question | Command |
|---|---|
| Where is `X` defined? | `lantern symbols X` |
| Where does this text or pattern occur? | `lantern code "text"` or `lantern code "re" --regex` |
| Who uses `X`? | `lantern refs X` |
| What calls `X`, and what does `X` call? | `lantern graph X` |
| What is in this file? | `lantern outline path/to/file` |
| Is this repository indexed, and how big is it? | `lantern status` |

Pick by what you search for:

- **A function, method, class, or type:** use `symbols`, `refs`, or `graph`. They skip comments and strings and name the enclosing definition.
- **A field or object key** (`row.technician_id`, `{ cedex: ... }`), or text that is not a name (strings, comments, config, SQL, docs): use `code`. `refs` does not record field or property uses.
- **Before you read a file:** run `outline`, then read only the line range you need. Do not `cat` a whole file or read a large `sed -n 1,200p` range.
- **To read around a match:** use `code --context N`. You do not need a separate read.

Run the command from inside the repository, or pass `-C <dir>`. `-C` takes a directory, not a context count; for context lines use `--context N`. The first query on a new repository builds the index; later queries are fast. You do not need to run `lantern index` first.

## Read the output

The default output is compact text, made for agents. Paths are relative to the current directory (`../` for a file outside it), so you can pass a printed path to `--path` or `outline` from the same directory. Lines are 1-based.

- `symbols`: `path:line-end_line kind[ in Container]: signature`, one per line.
- `code`: a heading line with the file path (`path (N matches, M shown)` when `--per-file` cut matches), then `line:text` for each match and `line-text` for `--context` lines.
- `refs`: a heading line with the file path, then `line kind[ in Container.name]: text` for each reference.
- `outline`: `line-end_line kind name`, indented two spaces per nesting level.
- `graph`: one line per node, `id kind Container.name path:line`, with roots marked `(root)`. Then one line per edge, `from -> to kind at line N`, where line N is in the file of the `from` node. Then `unresolved: ...`.
- `no results` means nothing matched. After `symbols`, `refs`, or `graph`, a hint gives the `lantern code` command to search for the name as text. Run it when the name can be a field, a key, or a string. After `outline`, a hint means no parser covers the file type (CSS, HTML); use `lantern code` with `--path` for that file. If the hint says parsing ran out of time, symbols can be missing: run `lantern index --rebuild`, then query again. The hint shows only on an empty text result, so with `--json`, or when results look incomplete, check `unparsed_files` in `lantern status`.
- A last line `-- more: --cursor <value>` means more rows exist. `-- truncated` on a graph means `--max-nodes` or `--max-bytes` cut it.

For the next page, run the same command with `--cursor <value>`. `index`, `status`, and `prune` print a JSON report.

Add `--json` only when a program parses the output. Each command then prints one JSON document; null fields and `truncated: false` are left out. List pages are `{"schema":1,"freshness","indexed_at","truncated"?,"next_cursor"?,"results":[...]}`. Rows:
- `symbols`: `name, kind, container?, path, line, end_line, signature`.
- `code`: `path, match_count, matches: [{line, column, text, before?, after?}]`.
- `refs`: `path, line, column, kind, text, enclosing?: {name, container?, kind}`.
- `outline`: top-level symbols with nested `children`; the page also has `path` and `language`.
- `graph`: `roots`, `nodes: [{id, name, container?, kind, path, line}]`, `edges: [{from, to, kind, line}]`, `truncated?`, `unresolved?`.

## Keep output small

- `--limit N` sets rows per page. Defaults: 30 (`code`: 10 files). Max 10 000.
- `--max-bytes N` cuts the page until the output fits in N bytes (accepts `K`/`M`). Cut rows come back on the next page. If not even one row fits, the command exits 2; raise the value.
- `--path GLOB` narrows by path, relative to the current directory. Repeat it to match any of several globs (`--path 'src/**' --path 'lib/**'`). A glob without `/` matches at any depth (`*.ts`); with `/` it is anchored (`src/**/*.ts`).
- `--language L` (for `symbols` and `code`): `c cpp csharp dart go java javascript php python ruby rust typescript tsx`. `typescript` also selects `tsx`. `javascript` includes `.jsx` files.
- `code --per-file N` caps matches per file (default 20); `match_count` still counts all of them. `code --context N` adds up to 20 lines around each match.
- `graph --depth N` (1–3, default 1) and `--max-nodes N` (default 200) bound the graph.

## Command notes

- **symbols**: fuzzy. Ranks exact name, then case-insensitive exact, then prefix, then initials (`gubi` finds `getUserById`), then fuzzy. Narrow with `--kind function|method|class|interface|struct|enum|trait|type|module|constant|variable|field`. Qualify with a container: `UserStore.getUserById` or `Store::get`.
- **code**: literal and case-insensitive by default. For a pattern that starts with `-`, use `-e`: `lantern code -e "--flag"`. Add `--regex` for a Rust-syntax regex and `--case-sensitive` when case matters. Matching is per line. `--sort path` gives a stable order instead of relevance.
- **refs**: exact, case-sensitive name. `--kind call|type|import|implementation`. `enclosing` names the definition that contains the reference.
- **graph**: `--direction callers|callees|both`. It follows calls only; use `refs` for type uses, imports, and `extends`/`implements`. A node with kind `file` stands for calls outside any definition. Names in `unresolved` have no definition in the index (often library calls). Code outside tests never resolves to a definition in a test file.
- **outline**: accepts a path relative to the current directory or an absolute path.

## Know the limits

- **Name resolution, not types.** `refs` and `graph` link by name (`"resolution": "name"`). Two unrelated functions with the same name share references and edges. Check the node paths before you trust an edge; narrow with `--path` or `--kind`.
- **Generated files** (lockfiles, files marked `linguist-generated` or `linguist-vendored` in `.gitattributes`) are left out of `symbols`, `refs`, and `graph`, and ranked last in `code`. Add `--include-generated` to include them.
- **What is indexed.** In a Git repository: tracked files plus untracked files that Git does not ignore. Outside Git: all files except `.gitignore` matches and the defaults (dot-files, `node_modules/`, `dist/`, `build/`, `coverage/`, `target/`, `vendor/`). `.lanternignore` (gitignore syntax) excludes more in both. Files over 1 MiB (`--max-file-size`) and binary files are not searchable as text.
- **Nested repositories** (submodules that are checked out) are part of the workspace; their paths carry the submodule prefix.

## Handle errors

Errors go to stderr as `lantern: <message>` (with `--json`: `{"error":{"code":"...","message":"..."}}`). Act on the exit code:

| Exit | Code | What to do |
|---|---|---|
| 1 | `failure` | Read the message. "not indexed" with `--stale-ok`: drop `--stale-ok` or run `lantern index`. |
| 2 | `usage` | Fix the arguments (bad flag, limit, glob, cursor from another query, `--max-bytes` too small). |
| 3 | `root_moved` | A folder that was indexed alone is now inside a Git repository. Run from the intended root. |
| 4 | `stale_cursor` | The index changed between pages. Run the query again without `--cursor`. |
| 75 | `temporary` | Another refresh holds the lock, files kept changing, or a reader blocked cleanup. Retry after a short wait. |

## Speed

- Use `--stale-ok` when you run many queries in a row and the files have not changed. It skips the refresh and labels results `stale`.
- In a very large repository the first query can take minutes. Run `lantern index` once up front if you can.
