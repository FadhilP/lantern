---
name: lantern
description: Find code fast in large repositories with the `lantern` CLI. Use it to find where a symbol is defined, where text or a regex occurs, who references a name, what calls a function or what it calls, and what a file contains. Prefer it over grep or reading whole files when you explore unfamiliar code, trace a call path, or need results that fit a small output budget.
---

# Lantern

`lantern` is a read-only code index. It never writes inside the repository. Every query refreshes the index first (only changed files are read), so results match the disk.

## Pick the command

| Question | Command |
|---|---|
| Where is `X` defined? (instead of `grep -r "class X"`) | `lantern symbols X` (add `--kind class`) |
| What is the source of `X`? | `lantern symbols X --body` |
| Where does this text or pattern occur? | `lantern code "text"` or `lantern code "re" --regex` |
| Who uses `X`? | `lantern refs X` |
| What calls `X`, and what does `X` call? | `lantern graph X` |
| What is in this file? | `lantern outline path/to/file` |
| Is this repository indexed, and how big is it? | `lantern status` |

Lantern has no file-name search. To find a file by name, use your file-glob tool or `git ls-files '*name*'`.

Pick by what you search for:

- **A function, method, class, or type:** use `symbols`, `refs`, or `graph`. They skip comments and strings and name the enclosing definition.
- **A field or object key** (`row.technician_id`, `{ cedex: ... }`), or text that is not a name (strings, comments, config, SQL, docs): use `code`. `refs` does not record field or property uses.
- **To read a definition:** use `symbols X --body`. It prints the found definitions with their source lines, so you do not need a separate read.
- **Before you read a file:** run `outline`, then read only the line range you need. Do not `cat` a whole file or read a large `sed -n 1,200p` range.
- **To read around a match:** use `code --context N`. You do not need a separate read.

Run the command from inside the repository, or pass `-C <dir>`. `-C` takes a directory, not a context count; for context lines use `--context N`. The first query on a new repository builds the index; later queries are fast. You do not need to run `lantern index` first.

**Several queries in a row:** each query refreshes the index first (about 0.2 s on a large repository). When you chain queries and the files do not change between them, add `--stale-ok` to every query after the first: `lantern symbols A; lantern refs A --stale-ok`. Before a loop, run `lantern index > /dev/null` once, then use `--stale-ok` on every query in the loop. On a repository that was never indexed, `--stale-ok` exits 1.

## Read the output

The default output is compact text, made for agents. Paths are relative to the current directory (`../` for a file outside it), so you can pass a printed path to `--path` or `outline` from the same directory. Lines are 1-based.

- `symbols`: `path:line-end_line kind[ in Container]: signature`, one per line. With `--body`, `line:text` source lines follow each definition, then `-- body cut: N more lines` when it is longer than 100 lines.
- `code`: a heading line with the file path (`path (N matches, M shown)` when `--per-file` cut matches), then `line:text` for each match and `line-text` for `--context` lines.
- `refs`: a heading line with the file path, then `line kind[ in Container.name]: text` for each reference.
- `outline`: `line-end_line kind name`, indented two spaces per nesting level.
- `graph`: one line per node, `id kind Container.name path:line`, with roots marked `(root)`. Then one line per edge, `from -> to kind at line N`, where line N is in the file of the `from` node. Then `unresolved: ...`.
- `no results` means nothing matched. After `symbols`, `refs`, or `graph`, a hint gives the `lantern code` command to search for the name as text. Run it when the name can be a field, a key, or a string. After `symbols Container.name`, when other containers define the name, the hint lists them; query again with one of them. After a literal `code` search, a hint says when the pattern has regex syntax (`a|b`, `getStats\(`): run it again with `--regex`. After `outline`, a hint means no parser covers the file type (CSS, HTML); use `lantern code` with `--path` for that file. If the hint says parsing ran out of time, symbols can be missing: run `lantern index --rebuild`, then query again. With `--json`, an empty page or graph carries the hint in a `hint` field. When results look incomplete, check `unparsed_files` in `lantern status`.
- A last line `-- more: --cursor <value>` means more rows exist. `-- truncated` on a graph means `--max-nodes` or `--max-bytes` cut it.

For the next page, run the same command with `--cursor <value>`. `index`, `status`, and `prune` print a JSON report.

Add `--json` only when a program parses the output. Each command then prints one JSON document; null fields and `truncated: false` are left out. List pages are `{"schema":1,"freshness","indexed_at","truncated"?,"next_cursor"?,"hint"?,"results":[...]}`. Rows:
- `symbols`: `name, kind, container?, path, line, end_line, signature, body?: {line, text}`.
- `code`: `path, match_count, matches: [{line, column, text, before?, after?}]`.
- `refs`: `path, line, column, kind, text, enclosing?: {name, container?, kind}`.
- `outline`: top-level symbols with nested `children`; the page also has `path` and `language`.
- `graph`: `roots`, `nodes: [{id, name, container?, kind, path, line}]`, `edges: [{from, to, kind, line}]`, `truncated?`, `unresolved?`.

## Keep output small

- `--limit N` sets rows per page. Defaults: 30 (`code`: 10 files). Max 10 000.
- `--max-bytes N` cuts the page until the output fits in N bytes (accepts `K`/`M`). Cut rows come back on the next page. If not even one row fits, the command exits 2; raise the value.
- `--path GLOB` narrows by path, relative to the current directory. Repeat it to match any of several globs (`--path 'src/**' --path 'lib/**'`). A glob without `/` matches at any depth (`*.ts`); with `/` it is anchored (`src/**/*.ts`).
- `--language L` (for `symbols` and `code`): `c cpp csharp dart go java javascript php python ruby rust typescript tsx`. `typescript` also selects `tsx`. `javascript` includes `.jsx` files.
- `code --per-file N` caps matches per file (default 20); `match_count` still counts all of them. `code --context N` adds up to N lines around each match (at most 50).
- `symbols --body` drops the default `--limit` to 5. Each body has at most 100 lines. A body starts at the first decorator, attribute, annotation, or doc comment right before the definition (a blank line ends them), so it can start before the row's `line`.
- `graph --depth N` (1–3, default 1) and `--max-nodes N` (default 200) bound the graph.

## Command notes

- **symbols**: matches by exact name, prefix, initials, or substring (3 characters or more); letters with gaps between them do not match. Ranks exact name, then case-insensitive exact, then prefix, then initials (`gubi` finds `getUserById`), then substring. Narrow with `--kind function|method|class|interface|struct|enum|trait|type|module|constant|variable|field`. Qualify with a container: `UserStore.getUserById` or `Store::get`.
- **code**: literal and case-insensitive by default. For a pattern that starts with `-`, use `-e`: `lantern code -e "--flag"`. Add `--regex` for a Rust-syntax regex and `--case-sensitive` when case matters. Without `--regex`, `|`, `\(`, and `.*` match those characters literally. Matching is per line. `--sort path` gives a stable order instead of relevance.
- **refs**: exact, case-sensitive name. `--kind call|type|import|implementation`. Every language records `call` references. `type` references exist only for Dart, Go, and TypeScript/TSX; `import` references only for Dart and JavaScript/TypeScript/TSX; `implementation` references for Dart, Java, JavaScript/TypeScript/TSX, PHP, and Rust. In other languages, find type uses and imports with `lantern code`. `enclosing` names the definition that contains the reference.
- **graph**: `--direction callers|callees|both`. It follows calls only; use `refs` for type uses, imports, and `extends`/`implements` where the language records them (see **refs**). A node with kind `file` stands for calls outside any definition. Names in `unresolved` have no definition in the index (often library calls). Code outside tests never resolves to a definition in a test file.
- **outline**: accepts a path relative to the current directory or an absolute path. For a path that is not in the index, the error lists indexed files with the same file name.

## Know the limits

- **Name resolution, not types.** `refs` and `graph` link by name (`"resolution": "name"`). Two unrelated functions with the same name share references and edges. Check the node paths before you trust an edge; narrow with `--path` or `--kind`.
- **Generated files** (lockfiles, files marked `linguist-generated` or `linguist-vendored` in `.gitattributes`) are left out of `symbols`, `refs`, and `graph`, and ranked last in `code`. Add `--include-generated` to include them.
- **What is indexed.** In a Git repository: tracked files plus untracked files that Git does not ignore. Outside Git: all files except `.gitignore` matches and the defaults (dot-files, `node_modules/`, `dist/`, `build/`, `coverage/`, `target/`, `vendor/`). `.lanternignore` (gitignore syntax) excludes more in both. Files over 1 MiB (`--max-file-size`) and binary files are not searchable as text.
- **Nested repositories** (submodules that are checked out) are indexed with the repository that contains them; their paths carry the submodule prefix.
- **Windows: files open for writing.** In a Git repository on Windows, Lantern takes file metadata from directory listings, as Git does with `core.fscache`. It does not see an edit to a file that another process still holds open for writing until that process closes the file. It also does not see an edit made through another name of a hard-linked file until that name is opened. Editor saves close the file, so they are not affected.

## Handle errors

Errors go to stderr as `lantern: <message>` (with `--json`: `{"error":{"code":"...","message":"..."}}`). Act on the exit code:

| Exit | Code | What to do |
|---|---|---|
| 1 | `failure` | Read the message. "not indexed" with `--stale-ok`: drop `--stale-ok` or run `lantern index`. |
| 2 | `usage` | Fix the arguments (bad flag, limit, glob, cursor from another query, `--max-bytes` too small). |
| 3 | `root_moved` | A folder that was indexed alone is now inside a Git repository. Run from the intended root. |
| 4 | `stale_cursor` | The index changed between pages. Run the query again without `--cursor`. |
| 75 | `temporary` | Another refresh holds the lock, files kept changing, or (`index` only) a reader blocked cleanup. Retry after a short wait. |

## Speed

- Use `--stale-ok` when you run many queries in a row and the files have not changed (see **Several queries in a row**). It skips the refresh and labels results `stale`.
- In a very large repository the first query can take minutes. Run `lantern index` once up front if you can.
