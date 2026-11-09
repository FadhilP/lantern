# Architecture

This is an overview. The full design record, with every rule and its reason, is in [implementation-plan.md](implementation-plan.md).

## Layout

```text
crates/lantern-core   library: sources, extraction, storage, refresh, queries
  src/source/         Git and folder inventories
  src/extract.rs      tree-sitter symbols and references
  src/store/          SQLite catalog and shards
  src/refresh.rs      one shard refresh: diff, prepare, commit
  src/workspace.rs    root resolution and nested members
  src/query/          symbols, code, refs, graph, outline, pages
  queries/<lang>/     tree-sitter tag queries
crates/lantern-cli    the `lantern` binary: arguments and JSON output
  tests/cli/          integration tests that run the binary
npm/                  npm launcher and platform package staging
skills/lantern/       agent skill shipped with the npm package
```

## Data model

```text
<data dir>/
  catalog.sqlite          roots, workspaces, and the member shards of each workspace
  folder-defaults.ignore  default excludes of the folder source
  roots/<hash>.sqlite     one shard per physical root
  locks/<hash>.lock       one refresh lock per root
```

A **shard** holds one physical root:

- `files`: every eligible path with its identity, fingerprint, flags, and blob.
- `blobs`: zstd-compressed content. The key hashes the detected language and the bytes, so identical files share one blob and a move reuses it.
- `blobs_fts`: a contentless FTS5 trigram index over blob text, for `code` candidates.
- `names` and `names_fts`: interned identifiers, with initials for `symbols`.
- `symbols` and `refs`: tree-sitter definitions and references per blob. A reference records its innermost enclosing definition.
- `meta`: root, source, Git head, policy token, extractor version, generation.

The **catalog** maps each workspace to its members: the root shard at prefix `""` and one shard per checked-out submodule at its path. A submodule indexed on its own and the same submodule inside a parent share one shard.

## Refresh

A query first refreshes every member shard, unless `--stale-ok` is set.

1. **Inventory.** The Git source reads the index and walks untracked files with Git's excludes. The folder source walks the tree in parallel with scoped `.gitignore`, `.lanternignore`, and the defaults. Both return the same list of paths with their metadata.
2. **Diff.** A path is a candidate when it is new, when its identity or fingerprint changed, or when its last verification is older than `--verify-interval`. A change to the rules, the source, or `--max-file-size` (the policy token) makes every path a candidate.
3. **Prepare.** A rayon pool reads candidates with a stable read: the file's metadata must match before the open, after the open, after the read, and after the close. It detects the language, parses with a time budget (1 s per MiB, at least 100 ms), and compresses the content.
4. **Commit.** One writer thread takes the prepared results through a bounded channel. It writes everything in one SQLite transaction under the root's lock. It reads the inventory again before the commit; if a file changed, the attempt starts over, up to 3 times.
5. **Publish.** After every member has committed, the catalog publishes the workspace membership in one transaction.

The generation of a shard goes up only when results change. Cursors carry the generations, so a page from a changed index fails with exit 4 instead of skipping or repeating rows.

**Secure removal.** Shards run with `secure_delete`. A commit that removes paths or changes the policy also merges the FTS index, deletes unused names, and truncates the WAL. If a reader blocks the truncation, the command exits 75 and the next refresh finishes it.

**Upgrades.** When the extractor version changes (Lantern version, tag queries, or grammar shape), the next refresh re-extracts symbols from the stored blobs without reading the disk. It reads only files that the new binary assigns to a different language.

## Queries

Each query opens the members that intersect the start directory, read-only, with one read transaction per shard. Every statement of a query therefore sees one commit, even while another process refreshes.

- `symbols`: candidates from the trigram name index, a case-insensitive prefix range, and an initials range; ranked in Rust with `nucleo-matcher`.
- `code`: literals extracted from the pattern give an FTS candidate query; candidates are decompressed and matched line by line in parallel.
- `refs`: an exact name lookup with the enclosing definition.
- `graph`: a breadth-first expansion over calls, resolved by name across all members.
- `outline`: one file's symbols, nested by line range.

## Platform notes

- Windows file identity and change time come from `GetFileInformationByHandle(Ex)`; junctions count as links and are not followed.
- `--rebuild` copies a staging shard into the live one with the SQLite backup API, so it never renames a file that readers hold open.
