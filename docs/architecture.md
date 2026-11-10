# Architecture

## Layout

```text
crates/lantern-cli    the `lantern` binary
  src/main.rs         arguments
  src/output.rs       compact text and JSON output
  src/engine.rs       the entry point of every command
  src/source/         Git and folder inventories
  src/extract.rs      tree-sitter symbols and references
  src/store/          SQLite shards
  src/refresh.rs      one refresh: diff, prepare, commit
  src/workspace.rs    root resolution
  src/query/          symbols, code, refs, graph, outline, pages
  queries/<lang>/     tree-sitter tag queries
  tests/cli/          integration tests that run the binary
npm/                  npm package: launcher and install script
skills/lantern/       agent skill shipped with the npm package
```

## Data model

```text
<data dir>/
  folder-defaults.ignore  default excludes of the folder source
  roots/<hash>.sqlite     one shard per root
  locks/<hash>.lock       one refresh lock per root
```

A **shard** holds one root: a Git working tree with its checked-out submodules, or a folder.

- `files`: every eligible path with its identity (device and inode), size, modification and change times, flags, and blob. The times stay in the platform's unit.
- `blobs`: zstd-compressed content. The key hashes the detected language and the bytes, so identical files share one blob and a move reuses it.
- `blobs_fts`: a contentless FTS5 trigram index over blob text, for `code` candidates.
- `names` and `names_fts`: interned identifiers, with initials for `symbols`.
- `symbols` and `refs`: tree-sitter definitions and references per blob. A reference records its innermost enclosing definition.
- `meta`: root, source, Git head, policy token, extractor version, generation, and the start time of the last committed scan.

The files of a checked-out submodule are stored in the shard of the repository that contains it, under the submodule's path. A start directory inside a submodule resolves to that outer repository, so the submodule has no shard of its own.

A shard written by an older schema version is deleted and built again at the next refresh.

## Refresh

A query first refreshes the shard of its root, unless `--stale-ok` is set.

1. **Inventory.** The Git source reads the index and walks untracked files with Git's excludes, then does the same for each checked-out gitlink and adds its paths under the gitlink's prefix. A repository's `.lanternignore` files apply to its own paths; a parent rule that matches the gitlink path removes the submodule. The folder source walks the tree in parallel with scoped `.gitignore`, `.lanternignore`, and the defaults. Both return the same list of paths with their metadata.
2. **Diff.** A path is a candidate when it is new, when its stored metadata differs, or when its modification or change time is not at least 2 seconds older than the start of the last committed scan (Git's racy rule: such a file can change again and keep its metadata). A change to the rules, the source, or `--max-file-size` (the policy token) makes every path a candidate.
3. **Prepare.** A rayon pool reads candidates with a stable read: the file's metadata must match before the open, after the open, after the read, and after the close. It detects the language, parses with a time budget (1 s per MiB, at least 100 ms), and compresses the content.
4. **Commit.** One writer thread takes the prepared results through a bounded channel. It writes everything in one SQLite transaction under the root's lock. It reads the inventory again before the commit; if a file changed, the attempt starts over, up to 3 times.

The generation of a shard goes up only when results change. Cursors carry the generation, so a page from a changed index fails with exit 4 instead of skipping or repeating rows.

**Secure removal.** Shards run with `secure_delete`. A commit that removes paths or changes the policy also merges the FTS index, deletes unused names, and truncates the WAL. If a reader blocks the truncation, the next refresh tries again. A query still succeeds; only `index` exits 75.

**Upgrades.** When the extractor version changes (Lantern version, tag queries, or grammar shape), the next refresh re-extracts symbols from the stored blobs without reading the disk. It reads only files that the new binary assigns to a different language.

## Queries

Each query opens the shard read-only in one read transaction. Every statement of a query therefore sees one commit, even while another process refreshes.

- `symbols`: candidates from the trigram name index, a case-insensitive prefix range, and an initials range; ranked in Rust by match tier (exact, prefix, initials, substring), then word start, then name length. With `--body`, each file on the page is parsed again, because the shard stores a definition's name line but not the line where its decorators and doc comments start.
- `code`: literals extracted from the pattern give an FTS candidate query; candidates are decompressed and matched line by line in parallel.
- `refs`: an exact name lookup with the enclosing definition.
- `graph`: a breadth-first expansion over calls, resolved by name.
- `outline`: one file's symbols, nested by line range.

## Platform notes

- Windows file identity and change time come from `GetFileInformationByHandle(Ex)`; junctions count as links and are not followed.
- `--rebuild` copies a staging shard into the live one with the SQLite backup API, so it never renames a file that readers hold open.
