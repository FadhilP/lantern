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
- **Git-aware.** In a Git repository it indexes tracked files plus untracked files that Git does not ignore. Checked-out submodules are part of the same workspace. Outside Git it indexes a folder with `.gitignore` rules and sensible default excludes.
- **Built for agents.** JSON output, stable cursors, a `--max-bytes` output budget, and exit codes that say what to do next.
- **13 grammars.** Symbols and references for C, C++, C#, Dart, Go, Java, JavaScript, PHP, Python, Ruby, Rust, TypeScript, and TSX. `code` searches every text file.

## Install

```sh
npm install -g @fadhilp/lantern
```

The npm package ships prebuilt binaries for Linux (x64, arm64; glibc and musl), macOS (x64, arm64), and Windows (x64, arm64). It runs no install script.

From source (Rust 1.90 or later):

```sh
cargo install --locked --git https://github.com/fadhilp/lantern lantern-cli
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

## Use with agents

[`skills/lantern/SKILL.md`](skills/lantern/SKILL.md) teaches an agent which command answers which question, how to read and page the output, how to stay inside an output budget, and what each exit code means. The npm package includes it at `skills/lantern/SKILL.md`.

For Claude Code, copy it into your skills directory:

```sh
mkdir -p ~/.claude/skills/lantern
cp "$(npm root -g)/@fadhilp/lantern/skills/lantern/SKILL.md" ~/.claude/skills/lantern/
```

## Ignoring files

Put gitignore-syntax rules in a `.lanternignore` file at any level to keep paths out of the index, for example secrets or large fixtures. Removing a file from the index also removes its content from the database files.

Lockfiles and paths marked `linguist-generated` or `linguist-vendored` in `.gitattributes` count as generated: `symbols`, `refs`, and `graph` leave them out unless you pass `--include-generated`, and `code` ranks them last.

## Documentation

- [CLI reference](docs/cli.md): every command, flag, output field, and exit code.
- [Architecture](docs/architecture.md): sources, the shard and catalog layout, the refresh pipeline, and queries.
- [Releasing](docs/releasing.md): the tag-driven release workflow and the npm setup.
- [Implementation plan](docs/implementation-plan.md): the design record and its reasons.

## Development

```sh
cargo build
cargo test                                   # unit tests and CLI integration tests
cargo clippy --all-targets -- -D warnings
cargo fmt --all
```

`crates/lantern-core` is the library; `crates/lantern-cli` is the `lantern` binary and its integration tests (`crates/lantern-cli/tests/cli/`). The integration tests need `git` on the `PATH`.

## License

No license is chosen yet.
