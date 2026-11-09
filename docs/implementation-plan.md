# Lantern — Implementation Plan

Lantern is a standalone, read-only code discovery CLI for large codebases.
It keeps a local SQLite index of code text, symbols, and references. It answers five questions fast, for people and for agents.
Lantern is a new design. It keeps the safety guarantees of its reference, and it does not copy the reference interfaces or internals.

```
$ lantern symbols getUserById     # 1. Where is X defined?
$ lantern code "retry\(" --regex  # 2. Where does this text or pattern occur?
$ lantern refs getUserById        # 3. Who uses X?
$ lantern graph getUserById       # 4. What calls X, and what does X call?
$ lantern outline src/user.ts     # 5. What is in this file?
```

| Item | Value |
|---|---|
| Product / binary | Lantern / `lantern` |
| Proposed packages | Rust crate `lantern` (name to confirm), npm `@fadhilp/lantern` with prebuilt binaries |
| Language | Rust (stable, edition 2024) |
| Reference | The pi-discover code index and `relationship_graph` (TypeScript), in the Pylon repository at `../Pylon/packages/pi-discover` (relative to this repository). |
| Reference citations | `*.ts` files are in `../Pylon/packages/pi-discover/src/`. `*.test.ts` files are in `../Pylon/packages/pi-discover/test/`. |
| Integration | None in v1. Lantern is a standalone tool. Integration with other hosts comes later. |
| Implementation status | v1 implemented. This plan is the design record; the code and [cli.md](cli.md) describe current behavior. |

## Changes since this plan

The code no longer follows these parts of the plan. The sections below still show the first design.

| Plan | Now | Why |
|---|---|---|
| A catalog, and one shard per physical repository; a workspace lists member shards (7.1, 8) | One shard per root. The Git inventory adds the files of each checked-out gitlink under its prefix. No catalog. | Much less code. A start directory inside a submodule resolves to the outermost repository that holds it as a gitlink, so each file is indexed once. Cost: a query from the submodule refreshes the whole outer repository. |
| `--verify-interval` rereads every file after 300 s (6.6) | A stored row is trusted when its metadata is equal and its file times are at least 2 s older than the last committed scan (Git's racy rule). The option is removed. | No full reread of the tree every 5 minutes. Cost: an edit that keeps size, mtime, ctime, and identity is not seen. |
| A blocked checkpoint exits 75 (6.4) | Only `index` exits 75. A query succeeds and the next refresh tries again. | A query must not fail because another query was reading. |
| `nucleo-matcher` fuzzy ranking (3.2, 9.4) | Exact, prefix, initials, then substring; no subsequence matching. | Precise results, and every candidate comes from an index. |
| `lantern-core` library and `lantern-cli` binary (4) | One crate, `lantern-cli`. | The library API was not public or stable. |
| Fingerprint and identity stored as text | Integer columns in the platform's unit. Schema version 2; an older shard is built again. | No string formatting per file per query. |

---

## 1. Scope

**In (v1)**
- Index with two inventory sources: plain folders and Git working trees.
- Nested Git repositories (gitlinks) in both directions (section 7).
- Commands: `index`, `status`, `prune`, `symbols`, `code`, `refs`, `graph`, `outline`.
- `.lanternignore` files (6.4).
- Tree-sitter extraction for C, C++, C#, Go, Java, JavaScript, PHP, Python, Ruby, Rust, TypeScript/TSX. Each has a published `tags.scm`.
- Prebuilt binaries for x64 and arm64: Windows MSVC, macOS, Linux GNU, Linux musl (eight targets).

**Out (v1)**
- Everything in section 12: `lantern mcp`, `lantern watch`, more grammars, private and global ignore rules, staged batch commits, blob reuse between overlapping folder roots.

**Not planned**
- Type-aware resolution (LSP or SCIP). Lantern stays name-based.
- Semantic or embedding search.
- A config file. Flags and environment variables are enough.
- `GIT_CEILING_DIRECTORIES`, `GIT_DIR`, and `GIT_WORK_TREE` overrides.

---

## 2. Architecture

```
  lantern symbols | code | refs | graph | outline | index | status | prune
  (person or agent; one process per call, no daemon)
                       │
                       ▼
  ┌──────────────────────────────────────────┐
  │ lantern-cli: clap, JSON / TTY output,    │
  │ exit codes                               │
  └────────────────────┬─────────────────────┘
                       ▼
  ┌──────────────────────────────────────────┐     ┌──────────────────────────────┐
  │ resolve root → workspace members         │ ◄─► │ catalog.sqlite               │
  │ (root shard + nested Git shards)         │     │ roots · workspaces · members │
  └────────────────────┬─────────────────────┘     └──────────────────────────────┘
                       ▼  per member shard, under its root lock
  ┌──────────────────────────────────────────┐     ┌──────────────────────────────┐
  │ refresh (skipped with --stale-ok)        │ ◄── │ indexed root (never written) │
  │ inventory: folder (ignore) | Git (gix)   │     │ folder or Git working tree   │
  │ fingerprint diff → prepare (rayon):      │     └──────────────────────────────┘
  │ stable read, blake3, zstd, tree-sitter   │
  │ one writer, one transaction → commit     │ ──── write ───┐
  └────────────────────┬─────────────────────┘               ▼
                       ▼                           ┌──────────────────────────────┐
  ┌──────────────────────────────────────────┐     │ roots/<hash>.sqlite          │
  │ query member shards in parallel          │ ◄── │ one shard per physical root  │
  │ (read-only) → merge with a bounded heap  │     │ blobs · FTS · names ·        │
  │ → page + next_cursor                     │     │ symbols · refs · files       │
  └──────────────────────────────────────────┘     └──────────────────────────────┘
```

Details: inventory sources (9.1), extraction (9.2), refresh pipeline (9.3), queries (9.4), storage (section 8).

**Principles**

1. **Fresh or fail.** By default, a query refreshes first with a full walk. Lantern never publishes partial content. It returns stale results only when the caller asks with `--stale-ok`, and then it labels them.
2. **Read-only on sources.** Lantern never writes inside an indexed root.
3. **Bounded and pageable output.** Every result has a limit, a truncation flag, and a cursor for the next page.
4. **No runtime dependencies.** One static binary. No `git`, `rg`, Node, or system SQLite.
5. **One engine.** One change detector, one extractor, and one store. Git and folder are two inventory sources (9.1).
6. **Isolation per codebase.** Each physical root has its own database (section 8).
7. **Honest heuristics.** Name-based resolution says so in the output (`"resolution": "name"`). Lantern does not claim type-aware results.
8. **Few knobs.** A value becomes a flag only when users need to change it. Other limits are constants, with test-only environment overrides.

---

## 3. Key decisions

### 3.1 Changes from the reference

| Area | pi-discover (reference) | Lantern | Why |
|---|---|---|---|
| Symbol extraction | Line-based regex per language (`symbols.ts:53-154`) | Tree-sitter parse with tag queries | Correct methods, nesting, containers, and end lines. Few false positives. |
| References | None in the index | Calls (and other kinds where the tag query has them) from the same parse | Enables `refs` and an index-based graph. |
| Relationship graph | Two live `rg` passes, then a regex role guess per line (`relationship-graph.ts:22-40`, `:192-244`) | Name-resolved graph from indexed definitions and references | No full-tree scan per query. Real caller and callee edges. |
| What is indexed | Only 17 code extensions; other files are invisible (`symbols.ts:5-30`) | Every text file for code search. Symbols for languages with a grammar. | Config, docs, SQL, YAML, and proto files are searchable. |
| Code search | Trigram AND of terms of 3+ chars; short terms silently dropped; one excerpt per file (`index.ts:37-72`) | Literal and regex search, accelerated by trigrams, verified on content; every matching line, with optional context | Same model as `rg`, which agents and people already use. Nothing silently dropped. |
| Symbol ranking | Fixed stages (exact, prefix, substring) plus a separate navigation mode (`index.ts:575-638`) | One fuzzy-ranked search, with tie-breaks on kind, tests, and generated files | One mode serves people and agents. |
| Content storage | Plain text in `files.content` and again in FTS (`index-schema.ts:39-66`) | One zstd-compressed copy and a contentless FTS index | Less disk: one compressed copy instead of two plain copies. The trigram index stays and is the largest part. Phase 0 measures the saving. |
| Storage layout | One database for all codebases | One shard per physical root, plus a catalog | Codebases never block or slow each other. |
| Change detection | Git dirty set in Git mode, fingerprints in folder mode | Fingerprints in both sources | One mechanism. A branch switch rereads only changed files. |
| Git access | `git` subprocesses, English stderr matching (`repository-scanner.ts:154-170`) | `gix` in process, including its `dirwalk` for untracked files | No runtime dependency. No locale problem. Git-faithful untracked semantics. |
| Directory walk | Custom scanner (`filesystem-scanner.ts`) | `ignore::WalkBuilder` for the folder source | Parallel walk and scoped ignore rules come from a maintained crate. |
| Extractor upgrades | Schema drop and full reread (`index-schema.ts:85-95`) | Re-extract from stored blobs | A new grammar version does not reread the disk. |
| Large output | Shrink results until the JSON fits a byte cap (`search-common.ts:114-129`) | Pagination cursors, plus an optional byte cap | Stable pages: a cursor fails if the index changed between pages. No row is lost to the byte cap. |
| Freshness | Full refresh walk before every query | Parallel walk with fingerprint diff before every query. `--stale-ok` returns the committed state, labeled. A watcher is designed for after v1 (12.2). | Never silently stale. |
| Ignoring paths | `.gitignore` and fixed defaults only | `.lanternignore` files on top of the source rules | Remove noise or secrets from the index without changing Git rules. |
| Generated and vendored files | Not recognized | `.gitattributes` `linguist-generated` / `linguist-vendored`, plus known lockfiles, ranked last or excluded | Less noise in results. |
| Concurrency | Worker thread and an in-process queue (`worker-index.ts`, `index.ts:493-508`) | One process per call, an advisory lock per root | Many processes and clients share one index safely. |

**Kept from the reference** (correct and hard to get right)
- Fresh-or-fail refresh. Nothing partial is published (`index.ts:424-430`, `:235-331`).
- Stable reads with fingerprint checks (`filesystem-scanner.ts:102-135`).
- Scoped `.gitignore` semantics for plain folders (`filesystem-scanner.ts:158-169`, `:232-245`).
- The nested repository model and root-key canonicalization (`repository-scanner.ts:182-242`, `search-common.ts:22-32`).
- Refusal to silently expand scope to a parent repository (`repository-scanner.ts:161-162`).
- Bounded output with explicit truncation counts.

### 3.2 Dependencies

| Need | Crate |
|---|---|
| CLI parsing | `clap` (derive) |
| SQLite, FTS5 trigram, contentless delete | `rusqlite` with `bundled` and `functions` |
| Compression | `zstd` |
| Parsing and tags | `tree-sitter`, `tree-sitter-tags`, one grammar crate per language |
| Folder walk and ignore matching | `ignore` (`WalkBuilder`, `gitignore::GitignoreBuilder`) |
| Git discovery, index, untracked walk, attributes | `gix` (only the needed features, including `dirwalk` and `attributes`) |
| Regex and literal extraction | `regex`, `regex-syntax` |
| Fuzzy ranking | `nucleo-matcher` |
| Parallel work | `rayon` |
| Content hash | `blake3` |
| Path canonicalization on Windows | `dunce` |
| File identity on Windows | `winapi-util` (already a dependency of `ignore`) |
| JSON | `serde`, `serde_json` |

---

## 4. Layout

```text
lantern/
├── crates/lantern-core      sources, extractor, store, queries (library)
├── crates/lantern-cli       clap front end, JSON and TTY output, exit codes
├── queries/<language>/      tree-sitter tag queries (tags.scm), versioned with the binary
├── npm/                     @fadhilp/lantern + per-platform packages
└── tests/                   CLI integration tests with fixture trees
```

- `lantern-core` has no CLI knowledge.
- The CLI is the only public interface in v1. The library API is not stable.

---

## 5. CLI surface

```text
lantern index   [PATH] [--rebuild]
lantern status  [PATH]
lantern prune
lantern symbols <query>   [--kind K] [--language L] [--path GLOB]
lantern code    <pattern> [--regex] [--case-sensitive] [--path GLOB] [--language L]
                          [--per-file N] [--context N] [--sort relevance|path] [--include-generated]
lantern refs    <name>    [--kind call|type|import|implementation] [--path GLOB]
lantern graph   <name>    [--direction callers|callees|both] [--depth N] [--max-nodes N]
lantern outline <file>
```

List commands (`symbols`, `code`, `refs`, `outline`) also take `--limit`, `--cursor`, `--stale-ok`, and `--max-bytes`. `graph` takes `--stale-ok` and `--max-bytes`. A graph is not a list, so `--max-nodes` is its limit and it has no cursor.

**Global flags**

| Flag | Env | Default |
|---|---|---|
| `-C, --cwd <dir>` | — | process cwd |
| `--home <dir>` | `LANTERN_HOME` | platform data dir |
| `--timeout <ms>` | `LANTERN_TIMEOUT_MS` | 120000 for a warm refresh; `0` = none. A cold index has no timeout. |
| `--verify-interval <ms>` | `LANTERN_VERIFY_INTERVAL_MS` | 300000 (0–300000) |
| `--max-file-size <bytes>` | `LANTERN_MAX_FILE_SIZE` | 1M (max 64M) |
| `--json` | — | off; the default is compact text |

- Sizes accept `K`, `M`, and `G` suffixes.
- `--path` values are relative to the cwd and must stay inside the workspace.

**Output**
- Compact text on stdout by default, for agents. With `--json`, one JSON document: every document has `"schema": 1`, `"freshness"` (`walk` or `stale`), `"indexed_at"`, and `"truncated"` and `"next_cursor"` when more results exist. Null fields are left out.
- TTY mode prints `path:line  text` lines, like `rg`. This is presentation only. Do not test it.
- `lantern index` prints `{ root, source, files, text_files, code_files, symbols, refs, unparsed_files, indexed_at, changed, removed, reused, parse_timeouts, elapsed_ms }`.

**Errors and exit codes**

Errors go to stderr as `{"error":{"code","message"}}`. The message is capped at 4 KiB.

| Exit | Meaning |
|---|---|
| 0 | Success, including no results |
| 1 | Refresh or query failure (unreadable Git index or rule, unstable read, limit exceeded) |
| 2 | Usage error (bad flag, empty query, invalid regex, path escapes the workspace) |
| 3 | Root moved outside the indexed directory; run from the intended root |
| 4 | Cursor is stale; run the query again |
| 75 | Temporary: content kept changing for 3 attempts, the lock timed out, SQLite stayed busy, or a reader blocked the WAL checkpoint after a commit |

---

## 6. Behavior contract

### 6.1 Roots and sources

- Resolve `realpath(cwd)` (or the path argument) first. A missing path is an error.
- Look for a Git working tree from that path upward (a `.git` directory, or a `.git` file with `gitdir:`). If one exists, the root is its top level and the source is **Git**.
- If none exists, the root is the path itself and the source is **folder**.
- A Git directory that exists but cannot be read (corrupt index, unsupported index extension, permission error) is an error. Lantern never falls back to folder rules for it.
- If a `.git` appears in a **parent** of an indexed folder root, the refresh fails with exit 3: "run from the intended root". Lantern does not silently expand scope.
- If a `.git` appears at the folder root itself, the source switches to Git at the next refresh, with a full reconcile.
- Root key = resolved path, lowercased on Windows.
- A path inside the root scopes every query to that subtree.

### 6.2 Git source inventory

- Inventory = every index entry, plus untracked files that Git's standard excludes do not ignore (`.gitignore` files, `.git/info/exclude`, the global excludes file). This is the set that `git ls-files -co --exclude-standard` lists. Tracked files that match an ignore rule stay in the inventory.
- No default excludes. Dotfiles and `dist/` are indexed when Git includes them.
- Gitlink entries (mode `160000`) are not files. They become nested roots (7.1).
- An untracked nested repository is one opaque untracked directory, as in Git. Lantern does not descend into it.
- An index entry with no file on disk (deleted, or sparse checkout) is not in the inventory.
- `status` reports `head` (`unborn` before the first commit) and `branch` (`""` when detached).

### 6.3 Folder source inventory

- Default excludes: `.*`, `node_modules/`, `dist/`, `build/`, `coverage/`, `target/`, `vendor/`.
- Nested `.gitignore` files apply only within their own directory. Rules are case-sensitive on all platforms. The last matching rule wins across scopes, so a root `!.config/` re-includes a default-excluded directory.
- An excluded parent must be reopened explicitly before its children can be included.
- A `.gitignore` read failure is an error.
- Nested repositories are ordinary subtrees.
- The root must be a real directory and canonical.

### 6.4 `.lanternignore`

A `.lanternignore` file uses `.gitignore` syntax.
It can be in any directory of the root. It applies to that directory and below, like `.gitignore`.
It has higher precedence than the source rules. Commit it to share it.

- **Exclusion** works on everything, including tracked files in the Git source.
- **Negation (`!`)**
  - Folder source: re-includes anything the source rules excluded, including default excludes. An excluded parent must be reopened first, as with `.gitignore`.
  - Git source: re-includes only paths that are in Git's inventory. It cannot add Git-ignored files, because Lantern does not walk Git-ignored trees such as `node_modules/`.
- Ignored files are not in the index, so every command skips them.
- The content of every `.lanternignore` file is part of the root's policy token. A change causes a full reconcile at the next refresh.
- **Removal is secure.** Shards always run with `PRAGMA secure_delete = ON`. A commit that removes paths or changes the policy also runs FTS `optimize`, deletes unused names, then runs `wal_checkpoint(TRUNCATE)`. If a reader keeps the checkpoint from finishing, `meta.checkpoint_pending` stays set, the command exits 75, and the next refresh finishes the checkpoint first. Ignored content does not stay readable in free pages, the FTS index, the names, or the WAL. Known limit: the trigrams of text that an edit replaced stay in FTS segments until SQLite merges them; an `optimize` on every edit costs too much (about 350 ms on a 38 MB shard). Content that a non-ignored file shares (same blob) stays, because it is still indexed under that path.
- **Nested roots.** A root's `.lanternignore` files apply to its own files. A parent rule that matches a path inside a gitlink child does not change the child. A parent rule that matches the gitlink path itself removes the child from the parent's members.

### 6.5 File eligibility and classification

- Always excluded: any `.git` path component, symlinks, junctions, and the Lantern data directory.
- A file is **text** if it is a regular file, at most `--max-file-size`, and has no NUL byte in its first 8 KiB. Invalid UTF-8 is decoded lossily. Line numbers stay correct.
- A text file is **code** if its language is known (by extension, then shebang). Code files get symbols and references. All text files get code search.
- A file is **generated** if `.gitattributes` marks it `linguist-generated` or `linguist-vendored`, or its name is a known lockfile (`package-lock.json`, `yarn.lock`, `pnpm-lock.yaml`, `Cargo.lock`, `poetry.lock`, `go.sum`, and similar).
  - Generated files are excluded from `symbols`, `refs`, and `graph`, and ranked last in `code`.
  - `--include-generated` includes them.
- A file is a **test** if its path matches common test conventions (`test/`, `tests/`, `__tests__/`, `*_test.*`, `*.test.*`, `*.spec.*`). Tests rank below non-tests in `symbols`. They are not excluded.

### 6.6 Freshness and publication

- By default, every query refreshes first with a full walk. A query that arrives during a refresh of the same root waits for it, then refreshes. That refresh is cheap when nothing changed. The output has `"freshness": "walk"`.
- `--stale-ok` skips the refresh. The output has `"freshness": "stale"` and `"indexed_at"`. If the root was never indexed, the query fails.
- A path is a candidate when it is new, its fingerprint changed, its `verified_at` is missing or in the future, or `now - verified_at >= --verify-interval`. Default 300 000 ms. `0` rereads every file on every refresh.
- Fingerprint = size, mtime ns, change time ns, and file identity (device and inode, or volume serial and file index).
- Stable read: the fingerprint before open, at open, after read, and after close must match, and the read length must equal the size.
- Lantern checks the inventory again before the final commit. If it differs, it retries, up to 3 attempts. Then it fails.
- The final commit uses a generation check. A mismatch forces a content-reading retry.
- These fail without publishing: an incomplete scan, an unreadable rule, file, or Git index, an unstable read, an exceeded limit, a timeout.
- Lantern removes rows for missing or excluded files only after a complete, successful reconciliation.
- A refresh that changes nothing does not increment the generation. Cursors (9.5) stay valid.

### 6.7 Limits

- Every work limit stops the refresh with an error and publishes nothing.
- Every output limit truncates and reports it.
- Limits without a flag are constants. Tests lower them through `LANTERN_TEST_*` environment variables, read only in test builds.

| Limit | Reference | Lantern default | Lantern max | Flag |
|---|---|---|---|---|
| File size | 512 KiB | 1 MiB | 64 MiB | `--max-file-size` |
| One ignore file / all ignore bytes | 512 KiB / 8 MiB | 4 MiB / 64 MiB | same | — |
| Folder entries visited | 100 000 | 10 000 000 | same | — (`LANTERN_TEST_MAX_ENTRIES`) |
| Git source entries | none | none | none | — |
| Directory depth | 128 | 1 024 | 1 024 | — |
| Nested repositories | 100 | 1 000 | same | — (`LANTERN_TEST_MAX_NESTED`) |
| Refresh attempts | 3 | 3 | 3 | — |
| Warm refresh timeout | 30 s | 120 s | none (`0`) | `--timeout` |
| Cold index timeout | 30 s | none | none | — |
| Results per page | max 100 | default 30 (`code`: 10 files) | 10 000 | `--limit` |
| Code matches per file | 1 excerpt | 20 | 1 000 | `--per-file` |
| Code context lines | 0 | 0 | 20 | `--context` |
| Output line length | 240 chars | 500 chars | same | — |
| Graph depth | — | 1 | 3 | `--depth` |
| Graph nodes | — | 200 | 5 000 | `--max-nodes` |
| Query length | 200–2 000 | 4 000 | 4 000 | — |
| Output bytes | host cap | none | — | `--max-bytes` |

- Query text rejects NUL, CR, and LF.
- A change to `--max-file-size` changes the index policy. The next refresh fully reconciles the root.

---

## 7. Nesting contract (both directions)

### 7.1 Git submodules and gitlinks

An aggregate repository `A` contains a gitlink to child repository `C`.
Each physical repository is one shard. A workspace lists its member shards in the catalog (section 8).
All cases must work whichever side Lantern indexes first.

| Order | Required result |
|---|---|
| Index `A`, then run in `C` | `C` uses the shard that `A` created. No file is read again if fingerprints match. Results in `C` have paths relative to `C`. (Reference test: `pi-discover.test.ts:1146-1163`.) |
| Index `C`, then run in `A` | `A` discovers `C` through the gitlink and uses `C`'s existing shard. Results in `A` have `C`'s prefix. (No reference test. Add one.) |
| Two gitlinks to one physical `C` | One shard. Both prefixes appear in results (`pi-discover.test.ts:1227-1263`). |
| Gitlink removed from `A`'s index | `A` stops returning `C`'s results. `C` still returns them. `C`'s shard remains (`:1213-1218`). |
| Prune after `C` is deleted | `C`'s shard and workspace are removed. `A` keeps working after its next refresh (`:1174-1178`). |

**Discovery rules**
- Realpath each gitlink.
- Skip missing gitlinks.
- Reject gitlinks that escape the root.
- Require the child's own working tree top level to equal itself.
- Guard against ancestor cycles.
- Cap at 1 000 nested repositories.
- Refresh the root first, then its children breadth-first. The root's inventory lists the gitlinks, so the order follows discovery.
- Publish membership in the catalog only after every member shard has committed.

`refs` and `graph` resolve names across all member shards of the workspace. A call in `A` to a function defined in `C` is an edge.

### 7.2 Subdirectories inside a Git repository

- A cwd inside a repository resolves to the repository root and scopes queries to the subdirectory.
- Running at the root sees the subdirectory. Running in the subdirectory uses the root's shard. Both directions share one shard.

### 7.3 Overlapping folder roots

- Folder roots `P` and `P/s` each have their own shard, ignore scopes, inventory, and freshness.
- Rules are root-relative, so the inventories can differ.
- Each root is indexed on its own. Lantern does not share blobs between them in v1.

---

## 8. Storage: one shard per physical root, plus a catalog

```text
<data-dir>/
├── catalog.sqlite                      roots, workspaces, members
├── folder-defaults.ignore              folder source default excludes (9.1)
├── roots/<blake3(root_key)>.sqlite     one shard per physical root
└── locks/<blake3(root_key)>.lock       advisory refresh lock per root
```

`<data-dir>` = `--home`, else `LANTERN_HOME`, else `%LOCALAPPDATA%\lantern`, `~/Library/Application Support/lantern`, or `${XDG_DATA_HOME:-~/.local/share}/lantern`.

**Why shards**
- Each root has its own writer. A large cold index of one repository never blocks another repository.
- A query reads only the shards of its own workspace. FTS posting lists contain only that codebase.
- Delete, prune, or rebuild replaces one file. There is no global `VACUUM`.
- Corruption or a long checkpoint affects one codebase.

All databases use `journal_mode=WAL`, `foreign_keys=ON`, `busy_timeout=5000`, `synchronous=NORMAL`.

**Catalog** (`user_version = 1`)

```sql
CREATE TABLE roots (
  root_key TEXT PRIMARY KEY,
  root TEXT NOT NULL,
  shard TEXT NOT NULL,
  source TEXT NOT NULL,             -- 'git' | 'folder'
  last_used_at INTEGER
) WITHOUT ROWID;

CREATE TABLE workspaces (
  root_key TEXT PRIMARY KEY,
  root TEXT NOT NULL,
  membership TEXT,                  -- blake3 of sorted (prefix, root_key)
  indexed_at INTEGER
) WITHOUT ROWID;

CREATE TABLE workspace_members (
  workspace_key TEXT NOT NULL REFERENCES workspaces ON DELETE CASCADE,
  prefix TEXT NOT NULL,
  root_key TEXT NOT NULL REFERENCES roots ON DELETE CASCADE,
  PRIMARY KEY (workspace_key, prefix)
) WITHOUT ROWID;
CREATE INDEX workspace_members_root ON workspace_members(root_key);
```

**Shard** (`user_version = 1`)

```sql
CREATE TABLE meta (                 -- exactly one row
  root TEXT NOT NULL, root_key TEXT NOT NULL, source TEXT NOT NULL,
  head TEXT, branch TEXT,
  policy TEXT NOT NULL,             -- blake3 of .lanternignore content + limits that change eligibility
  extractor TEXT NOT NULL,          -- lantern version + grammar and query hashes
  indexed_at INTEGER, generation INTEGER NOT NULL DEFAULT 0
);

CREATE TABLE blobs (                -- content-addressed within the shard
  id INTEGER PRIMARY KEY,
  key BLOB NOT NULL UNIQUE,         -- blake3(bytes)
  language TEXT,                    -- NULL for plain text
  lines INTEGER NOT NULL,
  content BLOB NOT NULL             -- zstd
);
CREATE VIRTUAL TABLE blobs_fts USING fts5(
  content, content='', contentless_delete=1, tokenize='trigram'
);                                  -- rowid = blobs.id; index only, no second copy

CREATE TABLE names (                -- each distinct identifier once
  id INTEGER PRIMARY KEY,
  name TEXT NOT NULL UNIQUE,        -- binary collation: exact, case-sensitive
  initials TEXT NOT NULL            -- camel/snake initials, lowercased
);
CREATE INDEX names_nocase ON names(name COLLATE NOCASE);
CREATE INDEX names_initials ON names(initials);
CREATE VIRTUAL TABLE names_fts USING fts5(
  name, content='names', content_rowid='id', tokenize='trigram'
);

CREATE TABLE symbols (
  id INTEGER PRIMARY KEY,
  blob_id INTEGER NOT NULL REFERENCES blobs ON DELETE CASCADE,
  name_id INTEGER NOT NULL REFERENCES names,
  kind TEXT NOT NULL,               -- function, method, class, interface, type, enum, module, constant, ...
  container TEXT,                   -- e.g. 'Parser' for Parser.parse
  line INTEGER NOT NULL, col INTEGER NOT NULL, end_line INTEGER NOT NULL,
  signature TEXT NOT NULL           -- first line, max 300 chars
);
CREATE INDEX symbols_name ON symbols(name_id);
CREATE INDEX symbols_blob ON symbols(blob_id, line);

CREATE TABLE refs (
  blob_id INTEGER NOT NULL REFERENCES blobs ON DELETE CASCADE,
  name_id INTEGER NOT NULL REFERENCES names,
  kind TEXT NOT NULL,               -- call, type, import, implementation
  line INTEGER NOT NULL, col INTEGER NOT NULL,
  enclosing INTEGER REFERENCES symbols(id) ON DELETE CASCADE
);
CREATE INDEX refs_name ON refs(name_id);
CREATE INDEX refs_blob ON refs(blob_id, line);

CREATE TABLE files (
  path TEXT PRIMARY KEY,
  blob_id INTEGER NOT NULL REFERENCES blobs,
  size INTEGER NOT NULL,
  flags INTEGER NOT NULL,           -- bit 0 generated, bit 1 test
  identity TEXT NOT NULL,
  fingerprint TEXT NOT NULL,
  verified_at INTEGER NOT NULL
) WITHOUT ROWID;
CREATE INDEX files_blob ON files(blob_id);
CREATE INDEX files_identity ON files(identity, fingerprint);
```

**Design notes**
- **Blobs are content-addressed.** The blob key hashes the detected language and the bytes, because symbols depend on the language. Identical files with the same language are compressed, tokenized, and parsed once per shard. A move that keeps the file name reuses everything; a rename that can change the language (`tool` to `tool.py`) reads the file again.
- **Contentless FTS5.** The index stores trigram postings only. The text is stored once, compressed. Queries decompress candidate blobs to verify matches and cut lines.
- **Interned names.** `symbols` and `refs` store a `name_id`. The trigram index covers distinct names only.
  - Lantern writes `names_fts` rows in the same transaction as `names` rows. A secure commit deletes names that no symbol or reference uses.
  - Unused names stay until `index --rebuild`. They cause no wrong results, because every query joins through `symbols` or `refs`.
  - Name ids are local to a shard. Cross-shard resolution uses the name text.
- **Extractor upgrades.** If `meta.extractor` differs from the binary, the next refresh re-extracts symbols and references from stored blobs, under each blob's stored language. It does not read the disk again, except for files whose file name the new binary maps to another language. Those are read again, because the language is part of the blob key.
- **Orphans.** The final commit deletes each touched blob that no `files` row references. FTS delete runs before the row delete.

---

## 9. Core subsystems

### 9.1 Inventory sources

Both sources produce the same result: `{ eligible paths → (identity, fingerprint, flags), policy token, nested roots, head, branch }`. Everything after this step is shared.

**Folder source:** `ignore::WalkBuilder`, parallel, with:
- `standard_filters(false)`, `hidden(false)`, `follow_links(false)`, `max_depth(1024)`.
- `git_ignore(true)`, `require_git(false)`, `parents(false)`, `git_global(false)`, `git_exclude(false)`. Only the root's own `.gitignore` files apply.
- `add_ignore(<data-dir>/folder-defaults.ignore)`: the default excludes (6.3), at the lowest precedence. A root `.gitignore` negation therefore re-includes a default-excluded directory.
- `add_custom_ignore_filename(".lanternignore")`: higher precedence than `.gitignore`.
- `filter_entry` drops `.git` components and the data directory.
- An entry error (unreadable directory or ignore file) fails the refresh.

Phase 0 confirms this precedence order against the reference's ignore cases.

**Git source**
1. `gix` finds the working tree and Git directory, including worktrees with a `.git` file.
2. `gix` reads the index and lists tracked paths and gitlinks. Lantern reads `HEAD` for `head` and `branch`.
3. `gix` `dirwalk` lists untracked files with Git's standard excludes. It emits each untracked, non-ignored file and does not recurse into nested repositories. Phase 0 confirms the options.
4. Inventory = tracked paths ∪ untracked paths, minus gitlinks.
5. `.lanternignore` files in the inventory filter it, through one `GitignoreBuilder` matcher per directory. The deepest matching rule wins. Negation can only keep paths that are already in the inventory.
6. `gix` attributes read `.gitattributes` for the generated flag.

Both sources then `lstat` each path for identity and fingerprint, and apply 6.5.

### 9.2 Extraction

- One `tree-sitter` parse per new blob. Tag queries (`queries/<language>/tags.scm`) capture `@definition.*` and `@reference.*`.
- Start from each grammar's published `tags.scm`. Most capture definitions and `reference.call`. Some also capture `reference.class`, `reference.implementation`, or type references. Few capture imports. In v1, `refs --kind type|import` returns results only for languages whose query captures them. Add captures per language over time.
- **Containers:** sort definitions by byte range. A definition inside another definition, or inside a Rust `impl` block, gets the outer name as its `container`. An `impl` block is not a symbol. A reference gets the innermost definition that contains it as `enclosing`.
- **Parse errors:** tree-sitter recovers from errors. Keep what it finds. A parse timeout (1 s per MiB, at least 100 ms) keeps the blob as text only and reports it in `index` output. A timeout depends on load, so later refreshes parse the blob again, up to 3 timeouts in a row.
- **Kinds:** normalize to `function`, `method`, `class`, `interface`, `struct`, `enum`, `trait`, `type`, `module`, `constant`, `variable`, `field`.
- Extraction runs on `rayon`, one parser per thread per language.

### 9.3 Refresh pipeline

```text
resolve root ─▶ lock root ─▶ inventory (source) ─▶ diff against files (fingerprints)
  ─▶ prepare in parallel (identity reuse │ stable read, text check, blake3,
     blob lookup, compress, extract) ─▶ one writer thread inserts into one transaction
  ─▶ re-inventory and compare ─▶ final writes: files, removals, orphans, meta ─▶ commit
  ─▶ unlock ─▶ (aggregate) catalog membership commit
```

1. **One transaction per refresh.** Prepared items go through a bounded channel to one writer thread.
   - It inserts blobs, FTS rows, names, symbols, and references in the open transaction.
   - Then it writes `files`, removals, and `meta`, deletes orphans, and commits.
   - Readers do not take the lock. WAL gives them the previous snapshot until the commit.
   - Memory stays bounded by the channel.
   - A failed or killed refresh rolls back. Nothing partial is published.
   - A cold index of a large repository grows the WAL to about the shard size. Phase 1 measures it. If it is a problem, staged batch commits come back (12.3).
2. **Blob lookup before work.** If the blob `key` already exists, skip compression, FTS, and extraction.
3. **Identity reuse before reading.** In the shard, a row with the same identity and fingerprint that is still within the verify interval supplies the blob without opening the file.
4. **Parallelism.** The walk and `lstat` calls run on a bounded pool. Prepare runs on `rayon`. Limits (entries, deadline) are atomic counters. If one is exceeded, the refresh is cancelled and nothing is published.
5. **Cross-process serialization.** One advisory lock per root (`std::fs::File::lock`). Readers do not take the lock.
6. **Aggregates.** Refresh member shards first and the root shard last, each under its own lock. Then commit membership to the catalog. A reader sees either the old or the new membership list.
7. **Cancellation.** A deadline and Ctrl+C stop preparation before the commit. A killed process leaves the WAL rolled back.
8. **Maintenance.** Run `PRAGMA optimize` after a refresh that changed rows. `index --rebuild` builds a staging shard (`<hash>.rebuild`) under the lock, then copies it into the live shard with the SQLite backup API in one transaction. WAL readers keep their snapshot, and no file is renamed while readers hold it open, which also works on Windows.

### 9.4 Query engine

A `--stale-ok` query opens the catalog read-only. A walking query opens it read-write for the refresh first. Every query then gets the workspace members and opens each member shard read-only on its own connection, in one read transaction per shard.
Shards are queried in parallel and merged in Rust with a bounded heap of `limit + 1` rows.
Lantern does not use `ATTACH`: SQLite allows at most 125 attached databases, and the nested repository cap is 1 000.

**`symbols <query>`**
1. Candidate names: `names_fts` trigram match for queries of 3 or more characters; `names_nocase` prefix range and `names_initials` prefix for shorter queries and for camel-case input (`gUBI` → `getUserById`). `Container.name` queries match on the name, then filter on `container`.
2. Join candidate names to `symbols`. Score with `nucleo-matcher`. Tie-break in this order: exact name, prefix, initials, kind (types and functions before variables and fields), non-test before test, shorter path.
3. Filters: `--kind`, `--language`, `--path` (glob).
4. Output row: `name, container, kind, path, line, end_line, signature`.

**`code <pattern>`**
1. Literal by default. `--regex` uses `regex` syntax. `--case-sensitive` is off by default.
2. Candidates: extract the required literals from the pattern with `regex-syntax` (alternations become OR groups). Each literal of 3 or more characters becomes a quoted `blobs_fts` phrase. If the pattern has no such literal, every blob in scope is a candidate. That is a full scan, as with `rg`.
3. Verify: decompress each candidate and run the real matcher. The trigram step only removes non-matches. It never decides a match. This includes case-sensitive search, because the trigram index is case-insensitive.
4. Order: `--sort relevance` (default: non-generated first, then `bm25` when the FTS step ran, then path) or `--sort path`.
5. Output row: `path, match_count, matches: [{ line, column, text, before?, after? }]`. Up to `--per-file` matches, with `--context` lines.
6. Filters: `--path` (glob), `--language`, `--include-generated`.

**`refs <name>`**
- Look up `name` in `names` (exact, case-sensitive by default), then rows from `refs` with that `name_id`. Filters: `--kind call|type|import|implementation`, `--path`.
- Output row: `path, line, column, kind, text, enclosing?: { name, container, kind }`.
- Output has `"resolution": "name"`. Two unrelated functions with the same name share references.

**`graph <name>`**
- Roots: the definitions of `name`. `--path` and `--kind` narrow them. If more than one definition matches, all are roots, and the output says so.
- The graph follows only `call` references.
- `--direction callers`: calls to the name, grouped by enclosing symbol.
- `--direction callees`: references whose innermost enclosing definition is this one (the same rule as callers), resolved to definitions by name. A call inside a nested function belongs to the nested function. A definition outside test files never resolves to one in a test file.
- `--direction both` (default). `--depth` repeats the expansion breadth-first, up to `--max-nodes`.
- Output: `{ resolution: "name", roots: [node ids], nodes: [{ id, name, container, kind, path, line }], edges: [{ from, to, kind, line }], truncated, unresolved: [names with no definition] }`.
- An edge's `line` is in the file of its `from` node.
- `--max-bytes` drops the last nodes found, with their edges, until the output fits, and sets `truncated`.

**`outline <file>`**
- The file's symbols, nested by container, in line order. A page holds top-level symbols, each with all of its children.
- It reads only the shard. It is fresh after the refresh like every other query.

### 9.5 Pagination and output size

- Every list result has `next_cursor` when more rows exist. `--cursor <c>` returns the next page. Lantern runs the query again and skips to the offset.
- A cursor encodes the query, the filters, the offset, and the generation of each member shard. If a generation changed since then, the query fails with exit 4: "results changed; run the query again without `--cursor`". Pages are therefore stable.
- `--max-bytes` removes rows from the end of the page until the output fits, and then sets `next_cursor` to the first removed row. Clients never lose rows. If not even one row fits, the command exits 2, because an empty page would return the same cursor forever.

---

## 10. Platform notes

| Topic | Note |
|---|---|
| Windows identity | Use `winapi-util::file::information` for volume serial and file index. Use `GetFileInformationByHandleEx` for the change time. The `std` file index APIs are unstable. |
| Junctions | `symlink_metadata().file_type().is_symlink()` must report junctions. Add a Windows test that creates a junction and checks that it is excluded. |
| Case | Root keys are lowercased on Windows. Ignore rules stay case-sensitive. |
| Paths | Store paths with `/` separators. Use `dunce` so that no `\\?\` prefix appears in output. |
| Rebuild swap on Windows | Readers hold shard files open, so a rename can fail. The backup API replaces the pages in place instead (9.3, step 8). |
| Binary size | 11 grammars. Phase 0 measures it. |

---

## 11. Milestones

Build a thin vertical slice first, with both sources, so the performance gates run early. Then widen.

| # | Deliverable | Done when |
|---|---|---|
| **Phase 0** | Spikes: name check for the crate and npm package, and pick the license. Tree-sitter tag quality on the corpus for the 11 v1 languages, grammar crate ABI compatibility with one `tree-sitter` version, parse throughput, binary size. Contentless FTS5 trigram + zstd: index size against the reference, candidate speed, verify speed against `rg` on the large repository. `regex-syntax` literal extraction correctness on a pattern set. `WalkBuilder` precedence (defaults file, `.gitignore`, `.lanternignore`) against the reference's ignore cases. `gix` `dirwalk` against `git ls-files -co --exclude-standard`, and `gix` index reading on a split index and a sparse index. Warm parallel walk time on the large repository on Windows, macOS, and Linux. | Each spike meets its target, or the plan changes before Phase 1. |
| **Phase 1** | Vertical slice: root resolution, both inventory sources (9.1), classification, change detection, stable reads, deadline, retries. Catalog and one shard per root (no nesting yet), one-transaction refresh, generation, lock. `index`, `status`, `code`, and `symbols` for TypeScript/JavaScript, Python, Rust, and Go. | The 13.1 rows without nesting pass, plus the Git inventory and code search tests. First performance numbers are recorded against the reference, including peak WAL size. |
| **Phase 2** | Extraction and queries: remaining v1 grammars, containers, extractor upgrades. `outline`, `refs`, `graph`, pagination, `--max-bytes`, `--stale-ok`. `.gitattributes` generated flag, `.lanternignore`, and secure removal. | Extraction, graph, pagination, and `.lanternignore` tests pass. The `refs` and `graph` performance rows pass. |
| **Phase 3** | Nesting and hardening: gitlinks as member shards, prefix projection, catalog membership, cross-shard `refs` and `graph`. Folder → Git transition and parent expansion refusal. Limits, `prune`, rebuild swap. | All of 13.1 and 13.2 pass. The reference comparison is reviewed. |
| **Phase 4** | Release v1: CI matrix `x86_64`/`aarch64` for Windows MSVC, macOS, Linux GNU and musl. npm `@fadhilp/lantern` with optional per-platform packages, GitHub release archives, `cargo install`. | `npx @fadhilp/lantern status` works on all eight targets in CI. All performance gates pass. |

---

## 12. Later (after v1)

### 12.1 `lantern mcp`

- An MCP stdio server that exposes `symbols`, `code`, `refs`, `graph`, `outline`, and `status` as tools, with the same JSON as the CLI.
- No new query logic. It is a thin layer over `lantern-core`.

### 12.2 `lantern watch`

Build it if the Phase 0 and Phase 1 warm walk times are too slow for agents on large repositories.
Its priority depends on the warm walk time that Phase 0 measures.

`lantern watch [PATH...]` runs in the foreground until Ctrl+C. Run it in a terminal or as a user service (launchd, systemd, Task Scheduler).

**Shard additions**

```sql
ALTER TABLE meta ADD COLUMN needs_walk INTEGER NOT NULL DEFAULT 1;  -- set on rule changes and lost events

CREATE TABLE watch_lease (          -- at most one row
  pid INTEGER NOT NULL, host TEXT NOT NULL,
  started_at INTEGER NOT NULL, renewed_at INTEGER NOT NULL,
  state TEXT NOT NULL               -- 'scanning' | 'healthy'
);

CREATE TABLE journal (              -- paths changed since the last drain
  path TEXT PRIMARY KEY,
  subtree INTEGER NOT NULL,         -- 1 = walk everything below this directory
  seen_at INTEGER NOT NULL
) WITHOUT ROWID;
```

**Watcher**
1. **Start.** Resolve each root and take its lease. If a live process holds it, exit 5. Set `state = scanning`, run a full refresh, then set `state = healthy`.
2. **Events.** `notify` with the native backend (inotify, FSEvents, ReadDirectoryChangesW). Filter each event path through the root's rules, then write it to `journal` at once, in small transactions. A directory create, remove, or rename writes the directory with `subtree = 1`.
3. **Drain.** After 100 ms with no new events, or 1 s under constant events, take the root lock and run the refresh pipeline (9.3) with the journal as the candidate list. A `subtree` row walks only that directory. The commit deletes the drained journal rows.
4. **Rule and Git changes.** A change to any `.gitignore`, `.lanternignore`, `.gitattributes`, `.git/index`, or `.git/HEAD` sets `needs_walk`. The next drain runs a full walk.
5. **Lost events.** An overflow (inotify `IN_Q_OVERFLOW`, FSEvents `MustScanSubDirs`, a Windows buffer overflow), a watcher error, or more than 100 000 journal paths sets `needs_walk` and `state = scanning` until a full walk completes.
6. **Watch limits.** If inotify runs out of watches (`ENOSPC`), the watcher prints the fix (`fs.inotify.max_user_watches`) and stays in `state = scanning`. Queries then walk.
7. **Safety net.** A full walk every 30 min.
8. **Lease.** Renewed every 2 s. A killed watcher's lease expires after 10 s.
9. **Nested roots.** One watcher on `A` also covers its gitlink children. Events go to the member shard by prefix. The watcher takes the lease on each member shard.
10. **Network filesystems.** `lantern watch` detects NFS, SMB, and similar mounts and exits 1 with a message.
11. **Shutdown.** Ctrl+C cancels a drain before its commit, deletes the lease, and exits.

**Query side**
- A query trusts the watcher only if `state = healthy`, `renewed_at` is within 10 s, the process is alive (same host), and `needs_walk = 0`.
- Then it drains the journal (or waits for the watcher's drain) instead of walking. The output has `"freshness": "watch"`.
- `--strict` forces a walk. Without trust, the query walks.

### 12.3 Other deferred work

- Grammars: Kotlin, Swift, shell (write `tags.scm`), Vue and Svelte (parse each `<script>` block as TypeScript or JavaScript, with the block's line offset). Code search covers these languages in v1.
- Private per-root ignore rules in the catalog (`lantern ignore add|remove|list|check`), and a global ignore file in the data directory.
- Staged batch commits with a `staging` table, if a single transaction gives a WAL that is too large (only if Phase 1 measures the problem).
- Blob reuse between overlapping folder roots through `files_identity`.

---

## 13. Testing

Test behavior, failure paths, persistence, concurrency, and regressions. Do not test registration, schemas, help text, or TTY formatting.

- Run the real binary against temporary fixture trees and real SQLite files.
- Use the real `git` CLI only to *create* fixture repositories.
- Mock nothing except the clock where the verify interval needs it (`LANTERN_TEST_NOW_MS`, test builds only).
- Lower constant limits with `LANTERN_TEST_*` variables (6.7).

### 13.1 Guarantees carried over from the reference

| Reference test | Behavior |
|---|---|
| `filesystem-scanner.test.ts:19` | Scoped ignores, negation, default opt-ins |
| `filesystem-scanner.test.ts:53` | Links and unsupported entries excluded |
| `filesystem-scanner.test.ts:91`, `filesystem-index.test.ts:363` | Mutation after inventory never publishes |
| `filesystem-index.test.ts:34` | Edits, renames, exclusions, deletions reconcile |
| `filesystem-index.test.ts:209` | Incomplete inventory keeps committed content and freshness, then recovers |
| `filesystem-index.test.ts:301` | Verify interval 0 rereads content |
| `filesystem-index.test.ts:331` | Folder → Git at the same root; parent expansion refused |
| `pi-discover.test.ts:955` | A file that changes during refresh causes a retry |
| `pi-discover.test.ts:1099`, `:1227` | Gitlink dedup; one physical repository through two prefixes |
| `worker-index.test.ts:173` | Unchanged content keeps symbols; rebuild stays correct |

### 13.2 New behavior tests

- **Git inventory:** equals `git ls-files -co --exclude-standard` on fixtures with tracked-but-ignored files, `info/exclude`, a global excludes file, dotfiles, an untracked nested repository, a worktree, and a sparse checkout. An unreadable Git index fails.
- **Branch switch** rereads only the files that changed.
- **Nesting:** child-first gitlink indexing (7.1 row 2). A cross-repository call appears in `graph`. Overlapping folder roots `P` and `P/s` each return correct results under their own rules.
- **Interrupted refresh:** a query during a cold index sees the previous committed state. `kill -9` during a cold index: queries still see the previous state, and the next run succeeds.
- **Limits:** a file over `--max-file-size`, a walk over the entry limit, and nesting over the nested limit each fail without publishing. Lowering `--max-file-size` removes the larger files at the next refresh.
- **Concurrency:** two `lantern index` processes on one root both succeed, with no duplicate rows. Processes on different roots do not wait on each other. `index --rebuild` during a query: the query gets the old or the new shard.
- **Extractor upgrade:** a changed `meta.extractor` re-extracts without reading files (assert with a file made unreadable after the first index).
- **Extraction (unit tests, pure logic):** per language, a fixture snippet maps to expected definitions (name, kind, container, line, end line) and references (name, kind, enclosing). Include methods in classes, nested functions, and a file with syntax errors.
- **Code search:** every literal and regex query on the corpus returns the same `(path, line)` set as `rg` over the same inventory. This proves the trigram step never drops a match. Include patterns with no 3-character literal, alternations, case-sensitive search, and invalid UTF-8.
- **Pagination:** pages concatenate to the unpaged result. A change between pages gives exit 4. A no-op refresh between pages does not.
- **`--max-bytes`:** truncated rows come back on the next page.
- **`--stale-ok`:** returns the committed state with `freshness: "stale"` and does not read the tree.
- **`.lanternignore`:**
  - A file in a subdirectory excludes only below it.
  - Adding a rule removes the files from `symbols`, `code`, `refs`, and `graph` at the next query. Removing the rule brings them back.
  - A negation re-includes a default-excluded folder in the folder source.
  - In the Git source, a negation cannot add a Git-ignored file, and an exclusion removes a tracked file.
  - After removal, a marker string from the ignored file is not in the shard file or its WAL.
  - A parent rule on a gitlink path removes the child from the parent's members.
- **Graph:** on a fixture project with known call edges, callers and callees at depth 1 and 2 match. Same-name functions are both roots.
- **Generated files:** `linguist-generated` files and lockfiles are excluded from `symbols` and ranked last in `code`.
- **Windows:** junction exclusion (section 10).

### 13.3 Reference comparison

On a corpus (the Pylon repository, a gitlink fixture, a folder fixture, two large public repositories), compare with pi-discover:
- `symbols`: for v1 languages, Lantern finds every definition that pi-discover finds correctly. Review the differences once and keep them as fixtures.
- `code`: for queries that pi-discover handles (terms of 3+ characters), Lantern returns a superset of its files.

This is a quality check, not a parity gate.

### 13.4 Performance gates

Measure pi-discover first, on the same machine and corpus. Record time, peak memory, disk size, and peak WAL size for each.
Run the gates from Phase 1 on, for the features that exist.

- Cold index of about 10 000 files, and of a repository with more than 300 000 files.
- Warm query with no changes (Git source and folder source). The time on the large repository decides the priority of `lantern watch` (12.2).
- Query after one file edit, and after a branch switch.
- Query on a small repository while a large cold index runs in another process.
- `refs` and `graph --depth 2` on the large repository (reference: `relationship_graph` with `rg`).

Lantern must beat the reference on every row that the reference can finish.

---

## 14. Risks

| Risk | Mitigation |
|---|---|
| Grammar crates need different `tree-sitter` versions | Phase 0 pins one version for the 11 v1 grammars. Defer a grammar that does not fit. |
| Published tag queries capture few reference kinds | v1 documents that `type` and `import` refs exist only where the query captures them. Add captures per language. |
| Binary grows large with grammars | Phase 0 measures size. 11 grammars in v1. |
| Regex literal extraction has a bug and drops matches | The trigram step only filters. The `rg` comparison test (13.2) covers it on the whole corpus. |
| Contentless FTS5 with `contentless_delete` needs SQLite 3.43 or later | Bundled SQLite is pinned in `rusqlite`. |
| The trigram index is larger than expected | Phase 0 measures it. The 1 MiB default file size keeps large generated files out. |
| `WalkBuilder` precedence differs from the reference's folder rules | Phase 0 spike against the reference's ignore cases. |
| `WalkBuilder` follows a `.gitignore` that is a symlink; the reference ignores it | Accepted difference. Symlinked files themselves are still excluded from the index. |
| `gix` `dirwalk` or index reading differs from Git in edge cases, or cannot read some index formats | Git inventory comparison test. Fail closed with a clear error. |
| A single-transaction cold index grows the WAL to about the shard size | Phase 1 measures it. Staged batches come back if needed (12.3). |
| Name-based graph links unrelated functions with the same name | The output says `"resolution": "name"`. Lists every root. Filters by path and kind narrow it. |
| Warm refresh on huge trees is slow without a watcher | Phase 0 measures it. `--stale-ok`. `lantern watch` after v1 (12.2). |
| No cold-index timeout lets a hung network filesystem block | Ctrl+C and kill are safe. |
| Data directory grows across many codebases | `prune`, orphan cleanup on each commit, and `last_used_at` for a later age-based prune. |
| Prebuilt binaries are blocked by antivirus or quarantine | Sign the macOS binaries. Document `cargo install`. |
