---
name: lantern
description: Find code fast in large repositories with the `lantern` CLI. Use it to find where a symbol is defined, where text or a regex occurs, who references a name, what calls a function or what it calls, what a file contains, and which definitions a Git diff or branch changed. Prefer it over grep or reading whole files when you explore unfamiliar code, trace a call path, or need results that fit a small output budget.
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
| Which definitions did my edits or this branch change? (instead of `git diff`) | `lantern changes` or `lantern changes main...HEAD` |
| Is this repository indexed, and how big is it? | `lantern status` |

Lantern has no file-name search. To find a file by name, use your file-glob tool or `git ls-files '*name*'`.

Pick by what you search for:

- **A function, method, class, or type:** use `symbols`, `refs`, or `graph`. They skip comments and strings and name the enclosing definition.
- **A field or object key** (`row.technician_id`, `{ cedex: ... }`), or text that is not a name (strings, comments, config, SQL, docs): use `code`. `refs` does not record field or property uses.
- **To read a definition:** use `symbols X --body`. It prints the found definitions with their source lines, so you do not need a separate read.
- **Before you read a file:** run `outline`, then read only the line range you need. Do not `cat` a whole file or read a large `sed -n 1,200p` range.
- **To read around a match:** use `code --context N`. You do not need a separate read.
- **To find which function a match is in:** read the indented `in kind Container.name` line above it in `code` output. For that function's source, run `symbols Container.name --body`. You do not need `outline` or a read of the file.
- **To review a diff or a branch:** run `lantern changes RANGE` for the changed definitions per file, then `lantern changes RANGE --diff NAME` for the changed lines of one definition, or `lantern changes RANGE --diff` for all changed lines. A `moved` definition has the same text as at its old place, apart from lines without letters or digits, and lists no lines; check only that it fits its new place.

Run the command from inside the repository, or pass `-C <dir>`. `-C` takes a directory, not a context count; for context lines use `--context N`. In a Git repository, the first query builds the index and later queries are fast; you do not need to run `lantern index` first. Outside Git, a folder with no index makes a query exit 2. Run `lantern index <dir>` only if the user asked to search that folder; otherwise ask them first. Never index the home folder or a drive root unless the user asks for it (that needs `--force`).

**Several queries in a row:** each query refreshes the index first (about 0.2 s on a large repository). When you chain queries and the files do not change between them, add `--stale-ok` to every query after the first: `lantern symbols A; lantern refs A --stale-ok`. Before a loop, run `lantern index > /dev/null` once, then use `--stale-ok` on every query in the loop. On a repository that was never indexed, `--stale-ok` exits 1.

## Read the output

The default output is compact text, made for agents. Paths are relative to the current directory (`../` for a file outside it), so you can pass a printed path to `--path` or `outline` from the same directory. Lines are 1-based.

- `symbols`: `path:line-end_line kind[ in Container]: signature`, one per line. With `--body`, `line:text` source lines follow each definition, then `-- body cut: N more lines` when it is longer than 100 lines.
- `code`: a heading line with the file path (`path (N matches, M shown)` when `--per-file` cut matches; `path (N matches)` with `-l`), then `line:text` for each match and `line-text` for `--context` lines. Where the innermost definition around the matches changes, an indented line right above the match names it: `  in kind Container.name`, or `  not in a named definition` after one ends (code in an anonymous function counts as outside). A definition's decorators and doc comments count as inside it. It names only the matches; `--context` lines can be in other definitions.
- `refs`: a heading line with the file path, then `line:column kind[ in Container.name]: text` for each reference. Two calls on one line share the text and differ in column.
- `outline`: `line-end_line kind name`, indented two spaces per nesting level. Members nest under their type, also when they are on the same line (`enum E { A, B }`) or outside it (a Rust `impl`, a Go receiver method).
- `changes`: a heading line per file, `path (status, +added -removed, why unparsed)` with the parts that apply, then `  status kind Container.name line-end_line (+added -removed)` per changed definition (`added`, `removed`, `modified`, or `moved`). A moved one adds `from PATH:LINE` at its new place or `to PATH:LINE` at its old place. With `--diff`, `+line:text` and `-line:text` lines follow: right after the heading for the file's lines outside every definition (imports, top-level code, or all lines of an unparsed file), and after each definition for its lines. A removed definition gives its old lines.
- `graph`: one line per node, `id kind Container.name path:line`, with roots marked `(root)`. Then one line per edge, `from -> to kind at line N`, where line N is in the file of the `from` node. Then `unresolved: ...`.
- `no results` means nothing matched. After `symbols`, `refs`, or `graph`, a hint gives the `lantern code` command to search for the name as text. Run it when the name can be a field, a key, or a string. After `symbols Container.name`, when other containers define the name, the hint lists them; query again with one of them. After a literal `code` search, a hint says when the pattern has regex syntax (`a|b`, `getStats\(`): run it again with `--regex`. After `outline`, a hint means no parser covers the file type (CSS, HTML); use `lantern code` with `--path` for that file. When `--path` matches no indexed file, the hint says so; ignored files are not indexed. If the hint says parsing ran out of time, symbols can be missing: run `lantern index --rebuild`, then query again. With `--json`, an empty page or graph carries the hint in a `hint` field. When results look incomplete, check `unparsed_files` and `unreadable_files` (files Lantern could not open) in `lantern status`.
- A first line `-- more: --cursor <value>` means more rows exist. A first line `-- more: --fuzzy (N)` after `symbols` means it showed only the exact matches and hid N looser ones; add `--fuzzy` to see them. `-- truncated` as the first line of a graph means `--max-nodes` or `--max-bytes` cut it.
- Text output cuts lines at 200 characters and ends a cut line with `…`; `--json` keeps 500. A control character in a file name prints escaped (`\n`, `\u{1b}`).
- Do not pipe Lantern into `| head` or `| cut`. Use `--limit`, `--per-file`, or `--max-bytes`: they keep the counts and the cursor.

For the next page, run the same command with `--cursor <value>`.

`index`, `status`, and `prune` print a JSON report. `files`, `symbols`, and `refs` are counts. `indexed_at` is the Unix time in milliseconds of the last change to the index. `unparsed_files` and `unreadable_files` count files whose symbols or content are missing. `prune` reports `removed_roots` and `removed_files`.

Add `--json` only when a program parses the output. Each command then prints one JSON document; null fields and `truncated: false` are left out. List pages are `{"schema":1,"freshness","indexed_at","truncated"?,"next_cursor"?,"hint"?,"results":[...]}`. `freshness` is `"walk"` when the query refreshed the index first, and `"stale"` with `--stale-ok`. Rows:
- `symbols`: `name, kind, container?, path, line, end_line, signature, body?: {line, text}`.
- `code`: `path, match_count, matches: [{line, column, text, before?, after?, enclosing?: {name, container?, kind}}]`.
- `refs`: `path, line, column, kind, text, enclosing?: {name, container?, kind}`.
- `outline`: top-level symbols with nested `children`; the page also has `path` and `language`.
- `changes`: `path, old_path?, status, added, removed, unparsed?, symbols: [{status, kind, name, container?, line, end_line, from?: {path, line}, to?: {path, line}, added, removed, lines?: [{sign, line, text}]}], lines?`.
- `graph`: `roots`, `nodes: [{id, name, container?, kind, path, line}]`, `edges: [{from, to, kind, line}]`, `truncated?`, `unresolved?`.

## Keep output small

- `--limit N` sets rows per page. Defaults: 30 (`code`: 10 files; `outline`: 100). Max 10 000.
- `--max-bytes N` cuts the page until the output fits in N bytes (accepts `K`/`M`). Cut rows come back on the next page. If not even one row fits, the command exits 2, and the message gives the size that one row needs.
- `--path GLOB` narrows by path, relative to the current directory. Repeat it to match any of several globs (`--path 'src/**' --path 'lib/**'`). A glob that starts with `!` excludes (`--path '!**/test/**'`). A plain folder name also matches the files inside it (`--path src`). A glob without `/` matches at any depth (`*.ts`); with `/` it is anchored (`src/**/*.ts`).
- `--language L` (for `symbols` and `code`): `c cpp csharp dart go java javascript php python ruby rust typescript tsx`. `typescript` also selects `tsx`. `javascript` includes `.jsx` files.
- `code --per-file N` caps matches per file (default 5); `match_count` still counts all of them. `code --context N` adds up to N lines around each match (at most 50); each line is shown once. `code -l` lists only the files that match, with counts (like `rg -l`).
- `symbols`: when a name matches exactly, it shows only the exact matches; `--fuzzy` adds the prefix, initials, and substring matches. `symbols --body` drops the default `--limit` to 5. Each body has at most 100 lines. A body starts at the first decorator, attribute, annotation, or doc comment right before the definition (a blank line ends them), so it can start before the row's `line`.
- `graph --depth N` (1–3, default 1) and `--max-nodes N` (default 200) bound the graph.

## Command notes

- **symbols**: matches by exact name, prefix, initials, or substring (3 characters or more); letters with gaps between them do not match, and `_` counts as a letter: `get_user` does not find `getUser` (`gu` and `user` find both). Ranks exact name, then case-insensitive exact, then prefix, then initials (`gubi` finds `getUserById`), then substring. Narrow with `--kind function|method|class|interface|struct|enum|trait|type|module|constant|variable|field`; separate several with commas (`--kind class,interface`). Qualify with a container: `UserStore.getUserById` or `Store::get`. To look up several names, give them all: `lantern symbols A B C` prints each page under a `== A` line. With no name, `lantern symbols --path src/api` lists every definition there, in path and line order.
- **code**: literal and case-insensitive by default. For a pattern that starts with `-`, use `-e`: `lantern code -e "--flag"`. Add `--regex` for a Rust-syntax regex and `--case-sensitive` when case matters. Without `--regex`, `|`, `\(`, and `.*` match those characters literally. With `--regex`, write alternation as `a|b`, not grep's `a\|b`: `\|` matches a literal `|`. Matching is per line. `--sort path` gives a stable order instead of relevance.
- **refs**: exact, case-sensitive name. `--kind call|type|import|implementation`; separate several with commas. Every language records `call` references. `type` references cover every type use in Dart, Go, and TypeScript/TSX; in C#, C++, Java, JavaScript, and PHP they cover only `new X` (C# also variable types). `import` references exist only for Dart and JavaScript/TypeScript/TSX. `implementation` references (base classes, interfaces, mixins, trait impls) exist in every language except C and Go. Find other type uses and imports with `lantern code`. `enclosing` names the definition that contains the reference.
- **graph**: `--direction callers|callees|both`. Qualify the name as in `symbols`: `Store.get` or `Store::get`. It follows calls only; use `refs` for type uses, imports, and `extends`/`implements` where the language records them (see **refs**). A node with kind `file` stands for calls outside any definition. Names in `unresolved` have no definition in the index (often library calls). Code outside tests never resolves to a definition in a test file.
- **outline**: accepts a path relative to the current directory or an absolute path. It takes one or more files (`lantern outline a.ts b.ts`; each page follows a `== a.ts` line), not a folder. For a path that is not in the index, the error lists indexed files with the same file name. A folder is a usage error (exit 2) that lists up to 10 indexed files in it. To list the definitions in a folder, use `lantern symbols --path <folder>`.

- **changes**: runs `git diff` and parses both sides of each changed file; it does not use or refresh the index, so `--stale-ok` changes nothing. `RANGE` is as in `git diff`: `A..B`, `A...B` (from the merge base, for a branch review), or `A` against the working tree. Without `RANGE`: uncommitted changes (working tree and index against HEAD) plus untracked files. `--path` narrows files. `--diff` adds the changed lines; `--diff NAME` (or `Container.name`) also keeps only that definition. `--diff` takes the next word as the name, so put `RANGE` first: `lantern changes main...HEAD --diff`. File status `copied` (with `old_path`) shows only when git also changed the source file; its lines are the differences from the source. Lines without letters or digits (blank lines, lone braces) do not make a definition changed on their own. A definition is `moved` when the same lines (indentation relative to its first line) are removed in one place and added in another, and they hold at least 20 letters and digits; one moved with edits shows as removed and added. A Git repository is required (exit 2 outside one).

## Know the limits

- **Name resolution, not types.** `refs` and `graph` link by name (`"resolution": "name"`). Two unrelated functions with the same name share references and edges. Check the node paths before you trust an edge; narrow with `--path` or `--kind`.
- **Generated files** (lockfiles, files marked `linguist-generated` or `linguist-vendored` in `.gitattributes`) are left out of `symbols`, `refs`, and `graph`, and ranked last in `code`. Add `--include-generated` to include them.
- **What is indexed.** In a Git repository: tracked files plus untracked files that Git does not ignore. Outside Git: all files except `.gitignore` matches and the defaults (dot-files, `node_modules/`, `dist/`, `build/`, `coverage/`, `target/`, `vendor/`). `.lanternignore` (gitignore syntax) excludes more in both. An ignored file is not in the index at all, so no command finds it; a generated file is indexed and only ranked last. Files over 1 MiB (`--max-file-size`) and binary files are not searchable as text.
- **Nested repositories** (submodules that are checked out) are indexed with the repository that contains them; their paths carry the submodule prefix.
- **Windows: files open for writing.** In a Git repository on Windows, Lantern takes file metadata from directory listings, as Git does with `core.fscache`. It does not see an edit to a file that another process still holds open for writing until that process closes the file. It also does not see an edit made through another name of a hard-linked file until that name is opened. Editor saves close the file, so they are not affected.

## Handle errors

Errors go to stderr as `lantern: <message>` (with `--json`: `{"error":{"code":"...","message":"..."}}`). Act on the exit code:

| Exit | Code | What to do |
|---|---|---|
| 1 | `failure` | Read the message. "not indexed" with `--stale-ok`: drop `--stale-ok` or run `lantern index`. A damaged index: run `lantern index --rebuild`. |
| 2 | `usage` | Fix the arguments (bad flag, limit, glob, cursor from another query or changed by hand, `--max-bytes` too small). For a refused first index outside Git, follow the message: ask the user before you index. |
| 3 | `root_moved` | A folder that was indexed alone is now inside a Git repository. Run from the intended root. |
| 4 | `stale_cursor` | The index changed between pages. Run the query again without `--cursor`. |
| 75 | `temporary` | Another refresh holds the lock, files kept changing, or (`index` only) a reader blocked cleanup. Retry after a short wait. |

## Speed

- Use `--stale-ok` when you run many queries in a row and the files have not changed (see **Several queries in a row**). It skips the refresh; with `--json`, results carry `"freshness": "stale"`.
- In a very large repository the first query can take minutes. Run `lantern index` once up front if you can.
