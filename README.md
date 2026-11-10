# Lantern

Lantern is a read-only code discovery CLI for large codebases. It keeps a local SQLite index of code text, symbols, and references, and it answers five questions fast, for people and for agents:

```sh
lantern symbols getUserById      # Where is X defined?
lantern code "retry\(" --regex   # Where does this text or pattern occur?
lantern refs getUserById         # Who uses X?
lantern graph getUserById        # What calls X, and what does X call?
lantern outline src/user.ts      # What is in this file?
```

- **Always fresh.** Every query refreshes the index first and reads only changed files, so results match the disk.
- **Read-only.** Lantern never writes inside your repository. The index lives in your user data directory.
- **Git-aware.** In a Git repository it indexes tracked files plus untracked files that Git does not ignore. Checked-out submodules are part of the same index. Outside Git it indexes a folder with `.gitignore` rules and sensible default excludes.
- **Built for agents.** JSON output, stable cursors, a `--max-bytes` output budget, and exit codes that say what to do next.
- **13 grammars.** Symbols and references for C, C++, C#, Dart, Go, Java, JavaScript, PHP, Python, Ruby, Rust, TypeScript, and TSX. `code` searches every text file.

## Install

> **Note:** The npm package is not published yet, so `npm install` does not work. Until it is, install from source (below).

```sh
npm install -g @fadhilp/lantern
```

The npm package ships prebuilt binaries for Linux (x64, arm64; glibc and musl), macOS (x64, arm64), and Windows (x64, arm64). On a global install, an install script points the `lantern` command at the binary, so a call does not start Node.js. If install scripts are off, `lantern` still works through a small Node.js launcher.

From source (Rust 1.90 or later):

```sh
cargo install --locked --git https://github.com/FadhilP/lantern lantern-cli
```

## Quick start

```sh
cd your-repo
lantern symbols UserStore          # the first query builds the index
lantern refs loadUser --limit 5
lantern code "TODO" --path "src/**" --per-file 3
```

Results print as compact text, sized for an agent's context:

```sh
$ lantern refs loadUser
src/main.ts
3 call in main: loadUser("42");
$ lantern symbols UserStore.get
src/user.ts:4-9 method in UserStore: get(id: string): User {
```

With `--json`, every command prints one JSON document for scripts:

```sh
$ lantern refs loadUser --json
{"schema":1,"freshness":"walk","indexed_at":1794058791146,"resolution":"name",
 "results":[{"path":"src/main.ts","line":3,"column":3,"kind":"call","text":"loadUser(\"42\");",
             "enclosing":{"name":"main","kind":"function"}}]}
```

Large results come in pages. A last line `-- more: --cursor <value>` (or `next_cursor` in JSON) gives the next page with `--cursor`, and `--max-bytes 8K` caps the size. Add `--stale-ok` to skip the refresh when you run many queries in a row.

## Compared with grep and ripgrep

Measured on [microsoft/vscode](https://github.com/microsoft/vscode) at `819bb82` (21,014 indexed files, a 394 MB working tree): Windows 11, Intel i7-12700 (20 threads), 32 GB RAM, SSD, ripgrep 14.1.0, and GNU grep 3.0 from Git for Windows. Each time is the median of 5 runs with a warm file cache. Lantern times include the refresh that every query runs first; the time in parentheses is with `--stale-ok`, which skips it.

| Question | Command | Time | Output |
|---|---|---|---|
| Where is `TextModel` defined? | `lantern symbols TextModel` | 203 ms (16 ms) | 3.8 KB, first page of 30 definitions, exact matches first |
| | `rg -n -w TextModel` | 883 ms | 1.48 MB, 476 lines of every use |
| | `rg -n -w -M 500 TextModel` | 936 ms | 70 KB, 476 lines |
| | `rg -n -w "class TextModel"` | 924 ms | 0.2 KB, 3 lines |
| | `grep -rn -w TextModel .` | 4,286 ms | 1.48 MB, 476 lines |
| Every line with `onDidChangeContent` (478 matches in 198 files) | `lantern code onDidChangeContent --limit 10000` | 251 ms (66 ms) | 55 KB |
| | `rg -n -i onDidChangeContent` | 965 ms | 606 KB |
| | `rg -n -i -M 500 onDidChangeContent` | 1,064 ms | 69 KB |
| | `grep -rn -i onDidChangeContent .` | 3,411 ms | 607 KB |
| Who calls `getWordAtPosition`? (70 calls) | `lantern refs getWordAtPosition` | 191 ms (14 ms) | 5.4 KB, first page of 30 calls, each with its calling function |
| | `rg -n -w getWordAtPosition` | 1,350 ms | 52 KB, 80 lines: calls, definitions, and comments |
| What is in `textModel.ts` (2,745 lines)? | `lantern outline src/vs/editor/common/model/textModel.ts` | 188 ms (10 ms) | 8.4 KB, all 231 definitions, nested |
| | `cat src/vs/editor/common/model/textModel.ts` | 15 ms | 112 KB |

What the numbers show:

- **Output size.** Lantern prints less because it pages results, cuts lines at 500 characters, and answers with definitions instead of every line that contains the name. One test fixture in vscode has a 1.4 MB line, so plain `rg` and `grep` print megabytes for a common name. `rg -M 500` removes most of that gap for text search.
- **Speed.** With the refresh, each query here was about 4 times faster than `rg`; the refresh takes most of that time. With `--stale-ok`, queries take 10–66 ms.
- **When `rg` is better.** A precise pattern such as `rg -w "class TextModel"` prints less than `symbols`, if you know the syntax of the language. Reading a whole file with `cat` is faster than `outline`, but it prints 13 times more.
- **Costs.** The first query builds the index: 60 s for vscode. The index takes 786 MB of disk, two times the working tree. A query after you change one file takes about 0.6 s, because it parses and stores that file again.

## Use with agents

[`skills/lantern/SKILL.md`](skills/lantern/SKILL.md) teaches an agent which command answers which question, how to read and page the output, how to stay inside an output budget, and what each exit code means. The npm package includes it at `skills/lantern/SKILL.md`.

For Claude Code, copy it into your skills directory:

```sh
mkdir -p ~/.claude/skills/lantern
cp "$(npm root -g)/@fadhilp/lantern/skills/lantern/SKILL.md" ~/.claude/skills/lantern/
```

Until the npm package is published, copy `skills/lantern/SKILL.md` from a clone of this repository instead.

## Ignoring files

Put gitignore-syntax rules in a `.lanternignore` file at any level to keep paths out of the index, for example secrets or large fixtures. Removing a file from the index also removes its content from the database files.

Lockfiles and paths marked `linguist-generated` or `linguist-vendored` in `.gitattributes` count as generated: `symbols`, `refs`, and `graph` leave them out unless you pass `--include-generated`, and `code` ranks them last.

## Documentation

- [CLI reference](docs/cli.md): every command, flag, output field, and exit code.
- [Architecture](docs/architecture.md): sources, the shard layout, the refresh pipeline, and queries.

## Development

```sh
cargo build
cargo test                                   # unit tests and CLI integration tests
cargo clippy --all-targets -- -D warnings
cargo fmt --all
```

`crates/lantern-cli` is the `lantern` binary and its integration tests (`crates/lantern-cli/tests/cli/`). The integration tests need `git` on the `PATH`.

## License

[MIT](LICENSE)
