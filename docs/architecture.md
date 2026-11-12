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
  src/git.rs          the git CLI, for `changes`
  src/changes.rs      changed lines to changed definitions
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
- `trigrams`: Lantern's own trigram index over blob text, for `code` candidates. A row holds the postings of one trigram for a block of 256 blob ids: each posting has the blob, how often the trigram occurs, and a one-byte filter of the characters that follow it. Text is case-folded as the regex engine folds it, so a case-insensitive match is never missed. New blobs take the free ids below the largest one first, so blocks stay full.
- `names` and `names_fts`: interned identifiers, with initials for `symbols`.
- `symbols` and `refs`: tree-sitter definitions and references per blob, with kinds stored as numbers. A reference records its innermost enclosing definition. `refs` is keyed by blob and position, so the table itself serves a file's references and a definition's line range. A signature is not stored; queries take the definition's line from the blob text.
- `meta`: root, source, Git head, policy token, extractor version, generation, and the start time of the last committed scan.

The files of a checked-out submodule are stored in the shard of the repository that contains it, under the submodule's path. A start directory inside a submodule resolves to that outer repository, so the submodule has no shard of its own.

A shard written by an older schema version is deleted and built again at the next refresh. A shard file that SQLite cannot read fails each command with a hint to run `lantern index --rebuild`, which replaces the file; `prune` deletes it.

## Refresh

A query first refreshes the shard of its root, unless `--stale-ok` is set.

1. **Inventory.** The Git source reads the index, then lists the working tree one directory level at a time with a pool of 4 threads (more threads only contend on Windows). It enters directories that hold tracked paths, and untracked directories that Git's excludes keep and that are not repositories of their own. It keeps tracked files and the untracked files that the excludes keep. On Windows the directory listing carries each file's metadata, so files are not opened. Like Git's `core.fscache`, this trusts NTFS directory entries, which can lag behind a file that is open for writing or a hard link changed under another name. The source then does the same for each checked-out gitlink and adds its paths under the gitlink's prefix. A repository's `.lanternignore` files apply to its own paths; a parent rule that matches the gitlink path removes the submodule. The folder source walks the tree in parallel with scoped `.gitignore`, `.lanternignore`, and the defaults. Both return the same list of paths with their metadata.
2. **Diff.** A path is a candidate when it is new, when its stored metadata differs, or when its modification or change time is not at least 2 seconds older than the start of the last committed scan (Git's racy rule: such a file can change again and keep its metadata). A change to the rules, the source, or `--max-file-size` (the policy token) makes every path a candidate.
3. **Prepare.** A rayon pool reads candidates with a stable read: the file's own metadata, taken again because a directory entry can lag, must match before the open, after the open, after the read, and after the close. A file that cannot be opened or read (for example access denied, or locked by another process) is stored with no content and a mark, so every refresh reads it again; running out of memory or file handles fails the refresh instead. A directory below the root that cannot be listed is skipped with its files. It detects the language, parses with a time budget (1 s per MiB, at least 100 ms), and compresses the content.
4. **Commit.** One writer thread takes the prepared results through a bounded channel. It writes everything in one SQLite transaction under the root's lock. A new shard builds its secondary indexes after the load, in one sorted pass each. After the last read, a second thread reads the inventory again while the writer finishes. Files that changed in the meantime are read and written in the same transaction, up to 3 rounds; a change to the ignore rules starts the attempt over. A `--rebuild` writes its staging file with a memory journal and no sync, because a failure deletes that file. A killed rebuild cannot delete it, so every refresh deletes a staging file it finds under the lock.

The generation of a shard goes up only when results change. Cursors carry the generation, so a page from a changed index fails with exit 4 instead of skipping or repeating rows.

**Secure removal.** Shards run with SQLite `secure_delete`, so freed pages are zeroed. A commit that removes paths or changes the policy also removes the trigram postings of the deleted text, deletes unused names, and truncates the WAL. Postings are removed in place: the commit counts the trigrams of each deleted text again and rewrites the rows that hold them. If a reader blocks the truncation, the next refresh tries again. A query still succeeds; only `index` exits 75. Limit: SQLite does not clear the unused gap inside a page when it moves cells, so a few strings of removed text (a path or an identifier) can stay in those gaps. `lantern index --rebuild` writes a new file and removes them.

**Upgrades.** When the extractor version changes (Lantern version, tag queries, or grammar shape), the next refresh re-extracts symbols from the stored blobs without reading the disk. It reads only files that the new binary assigns to a different language.

## Queries

Each query opens the shard read-only in one read transaction. Every statement of a query therefore sees one commit, even while another process refreshes.

- `symbols`: candidates from the trigram name index, a case-insensitive prefix range, and an initials range; ranked in Rust by match tier (exact, prefix, initials, substring), then word start, then name length. Each file on the page is decompressed once for the signatures, and with `--body` for the lines from the stored start line, where decorators and doc comments start.
- `code`: literals extracted from the pattern select candidates from the trigram index; candidates are decompressed and matched line by line in parallel. A file scores `n / (n + 1)` for each distinct literal it holds `n` times, so each repeat adds less than the one before. The index gives each candidate an upper bound of that score; candidates are checked in bound order, and a file goes on the page once no unchecked candidate can rank before it.
- `refs`: an exact name lookup with the enclosing definition.
- `graph`: a breadth-first expansion over calls, resolved by name.
- `outline`: one file's symbols, nested by line range and by container.

## Changes

`changes` does not use the index. It is the only command that starts another program: one `git diff --raw -z --patch --unified=0 --find-copies` call (plus `git rev-parse --revs-only` for a `RANGE`, which tells a single revision compared with the working tree from a range, and `git ls-files --others` for the working tree), then one `git cat-file --batch` for the blobs of the page's files. Paths and blob ids come from the NUL-separated raw records; each patch section is matched to its record by its `diff --git` line, and hunk lines are read by the counts in the hunk header. A working-tree side is read from the disk, unless git named a blob it stored (an entry outside a sparse checkout is compared with the index). Each side must have the diff's lines at their line numbers, or the file is `out_of_sync`: a clean filter can make git's lines differ from the disk. Both sides are parsed in parallel with the same extractor as a refresh, and each changed line goes to the innermost definition around it on its side. Moves are found from text: the changed lines of every file in the diff, without filler, are indexed by trimmed text, and a wholly added or removed definition is moved when its lines appear in a row on the other side, with the same indentation relative to the first line. The other side is then parsed, and a definition there must span exactly the matched lines, so the text is not part of a longer change. The index is keyed by the first two lines, since many definitions start with the same one (`@Override`). Each line carries its hunk number, so a match in the same hunk (a reindent or a line-ending change in place) is not a move. So a move does not depend on which files the page parses; the cost is reading every untracked file. Git runs with `diff.autoRefreshIndex=false`, so it does not rewrite `.git/index`, and with options that keep external diff drivers, text conversion, colors, and prefixes out of the output. Git's own diff was measured against gix's: the same results, within 110 ms on large ranges, while gix added 3.4 MB to the binary; parsing the changed files costs more than either.

## Platform notes

- Windows file identity and change time come from `GetFileInformationByHandle(Ex)`; junctions count as links and are not followed.
- `--rebuild` copies a staging shard into the live one with the SQLite backup API, so it never renames a file that readers hold open.
