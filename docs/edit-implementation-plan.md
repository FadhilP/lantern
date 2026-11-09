# Lantern Edit — Implementation Plan

Lantern Edit is a separate CLI that edits files by line number. The model cites what a Lantern read showed. It does not resend the old text.
It keeps the read-only promise of `lantern`. Only the `lantern-edit` binary writes inside a repository.

```
$ lantern read src/store.rs:30-90          # prints src/store.rs#k3f9 and numbered lines
$ lantern-edit <<'EOF'
@ src/store.rs#k3f9
@@ replace 40-44
    fn get(&self, id: Id) -> Option<&User> {
        self.users.get(&id)
    }
@@ delete 90-95
EOF
```

| Item | Value |
|---|---|
| Product / binary | Lantern Edit / `lantern-edit` |
| Proposed packages | Rust crate `crates/lantern-edit` (depends on `lantern-core`), npm package separate from `@fadhilp/lantern` |
| Reference | Pylon numbered line edits: `../Pylon/packages/pylon-core/src/line-edit.ts`, tests in `../Pylon/packages/pylon-core/test/line-edit.test.ts` |
| Implementation status | Not started. This plan is the design record. |

---

## 1. Why

Measured on the author's Claude Code transcripts (2,335 edits, 5,674 reads, deduplicated by `tool_use` id):

- `old_string` and unchanged context lines are about 39% of edit output. Edits are about 2% of all output tokens, so the cost saving is small (about 0.5%).
- Re-reads of lines already seen are about 0.5% of read volume. Delta reads are not worth building.

The main reasons are reliability, not cost:

1. **Unintended changes.** The model retypes lines that it means to keep and changes them by accident (a dropped `;`, a changed quote, a lost line). It finds this late, through a failing check, and loops to repair it.
2. **`sed -i`.** Models use `sed` for edits. A regex can change text far from the intended target, and nothing reports what changed.
3. **Output latency.** About 64 output tokens fewer per edit.

## 2. Scope

**In**
- `lantern read` and view tags on `lantern code --context` (section 4).
- `lantern-edit` with `replace`, `before`, `after`, `delete` by line (section 5).
- Guards against unintended changes, and a parse check (section 6).
- Rebase of edits over unrelated changes (section 7).
- Symbol operations (section 8).
- Atomic multi-file batches (section 9).

**Out**
- Regex replace-all and rename. Lantern resolves by name, not by type, so these are not safe. Use LSP.
- Automatic re-indent. It can silently break Python.
- Delta reads ("unchanged since your last view"). The measured gain is about 0.5%, and re-reads close to an action seem to help the model.
- Line numbers on only some lines. They would make the model misplace edits.

## 3. Principles

1. **No old text.** An edit names lines. It never repeats their content.
2. **Proof of view.** An edit can touch only lines that a cited view showed.
3. **Nothing outside the target changes.** Report every change inside the target.
4. **Every error is a read.** A failure prints the current numbered lines that the retry needs, with a new tag. The model never needs an extra read to retry.
5. **All or nothing.** One call validates every operation in every file before it writes any of them.
6. **The model never computes line shifts.** It always uses the numbers from the view it cites.

## 4. Views (`lantern`, read-only on the repository)

A **view** is one snapshot blob plus the line intervals that one output printed. A tag names a view, not a file revision. Holding the tag is the proof that the model saw those lines. This needs no session ID: a subagent that did not see the output does not have the tag.

### 4.1 Storage

A separate `views.sqlite` under `Home`:

- `views(tag, path, blob_hash, intervals, created_at)`
- `snapshots(blob_hash, zstd_bytes)`, keyed by blake3 of the raw bytes.

The index blobs (`blobs`, see [architecture.md](architecture.md)) are not reused. The index deletes a blob once no file uses it, but a view must keep its base content. Separate storage leaves the index cleanup unchanged.

Tags are 4–6 base32 characters. They are extended on a collision within the same path, like Pylon's `recordSnapshot` (`line-edit.ts:114-143`).

### 4.2 Commands

- `lantern read <path>[:a-b]` prints `path#tag`, then `N:text` lines. Output limits follow `--max-bytes`. Only lines that were printed go into the view.
- `lantern read --symbol <name>` prints exactly one definition. If the name is ambiguous, it lists the candidates and prints no text.
- `lantern code --context N` adds `#tag` to each file heading. Match lines and context lines go into the view.
- `lantern prune` removes views older than a set age, and snapshots that no view uses.

### 4.3 Tests

- A view records exactly the printed lines, including when `--max-bytes` cuts rows.
- Two reads of one blob give two tags.
- Prune keeps snapshots that live views still use.

## 5. Edit format and MVP (`lantern-edit`)

### 5.1 Input

Input comes on stdin (a heredoc) or from `--file`. There is no JSON, so code needs no escapes.

```
@ <path>#<tag>[,<tag>...]
@@ replace <a>-<b>
<new lines>
@@ before <n>
<new lines>
@@ after <n>
<new lines>
@@ delete <a>-<b>
```

- A header is a line that matches `^@@ (replace|before|after|delete) `. `--delimiter <marker>` changes `@@` for content that contains such lines, such as diff or patch files.
- All tags in one file block must share one snapshot.
- Exactly one trailing newline is stripped from each body. This avoids the extra blank line that Pylon's `normalizeNewText` (`line-edit.ts:177`) adds when `newText` ends with a newline.

### 5.2 Validation

Port Pylon's `parseOperations` (`line-edit.ts:276-322`). Collect every error before any write:

- Ranges exist in the snapshot.
- Every target line is inside the cited views (see `unseenRanges`, `line-edit.ts:97-109`).
- No operations overlap, and no two operations touch the same boundary (`claimedConflicts`, `boundaryConflicts`).
- Inserted text is not empty.

### 5.3 Write

1. Take the per-file lock (`Home::lock_hash`, `home.rs:73-90`).
2. Re-hash the file under the lock.
3. Write a temp file in the same directory, then rename it over the target.
4. Read the file back and compare bytes.

Keep the BOM, the line endings, and the file mode. Refuse files with mixed line endings, like Pylon's `decodeText` (`line-edit.ts:145-164`).

### 5.4 Output

For each file:

```
src/store.rs#p2xa 40-47 (+3 −0)
39 fn new() -> Self {
40+    fn get(&self, id: Id) -> Option<&User> {
...
```

Only changed lines are printed, numbered and marked `+`/`-`, with one line of context. The printed lines form a new view.

### 5.5 Tests

Port the cases in `line-edit.test.ts`: stale revision, unseen lines, overlap, shared boundary, BOM and CRLF, and disjoint operations in one batch. Add a test for the trailing newline.

## 6. Guards against unintended changes

Ship this right after the MVP. It addresses the main problem in section 1.

### 6.1 Collateral detector

Inside each replaced block, drop the lines that are identical at the start and the end. Pair the remaining old and new lines by similarity, then flag:

- `! 43 whitespace-only change`
- `! 45 1 char differs from old line 45`
- `! dropped 2 lines (46-47) with no replacement`

These are flags, not errors: an intended one-character fix looks the same. The model sees them in the same turn, before any check runs.

### 6.2 Parse check

Use lantern-core's language detection and tree-sitter. Count ERROR and MISSING nodes inside the changed byte ranges, before and after the edit. If the edit adds errors, reject it and print the error lines with a new tag. `--allow-errors` overrides this for intentional broken intermediate states.

### 6.3 Tests

- Each flag fires on its case and does not fire on an exact retype.
- An edit that breaks syntax is rejected, and its lines are shown.
- An error that was already in the file does not block an edit elsewhere.

## 7. Rebase

Diff the view's snapshot against the current file. Map a target range to its new position only if the range **and one line on each side** are unchanged. Otherwise, report a conflict. The conflict output prints the current numbered lines of that region with a new tag.

There is no fuzzy matching. A wrong clean apply is worse than a conflict.

The model may keep citing its old views after its own edits. Rebase moves the edit over those changes too.

### 7.1 Tests

- An edit after an unrelated change above the target lands on the correct lines.
- A change next to the target is a conflict.
- A formatter rewrite of the target is a conflict that shows the new text.
- Five edits in a row against one view.

## 8. Symbol operations

`@@ replace Store.get`, `@@ delete Store.get`, `@@ before Store.get`, `@@ after Store.get`. The file block needs a path but no tag.

- The range comes from parsing the **live** file, not from the index.
- If the name is ambiguous, list the candidates and refuse.
- The seen guard does not apply. It exists to stop edits based on guessed line positions, and replacing a whole unit by name is not a guess.

### 8.1 Tests

- A duplicate name is refused.
- A qualified name resolves.
- The range rule for doc comments and attributes (decide in section 11) is tested.

## 9. Multi-file batches

Validate and stage temp files for all files, then rename them all. If a rename fails, restore the files already renamed from their snapshots (section 4.1).

### 9.1 Tests

- A failure injected on the second file leaves the first file restored, byte for byte.

## 10. Phases

| Phase | Content | Depends on |
|---|---|---|
| 0 | Spikes (section 11) | — |
| 1 | Views and `lantern read` (section 4) | 0 |
| 2 | `lantern-edit` MVP (section 5) | 1 |
| 3 | Guards (section 6) | 2 |
| 4 | Rebase (section 7) | 2 |
| 5 | Symbol operations (section 8) | 2 |
| 6 | Multi-file batches (section 9) | 2 |
| 7 | Packaging: binary, npm package, `SKILL.md`, `docs/cli.md`, `docs/edit.md`; the README keeps "Lantern never writes inside your repository" | 2 |
| 8 | Evaluation (section 12) | 3 |

Phases 1–3 are the first release. Phases 4–6 stand alone.

## 11. Open questions

- **Diff engine.** Does `gix` 0.90 expose a line diff through its `blob-diff` feature (imara-diff)? If it does not, add `imara-diff` directly. A full 3-way merge is not needed.
- **Windows replace.** Does `std::fs::rename` over a file that an editor or IDE holds open work with their share modes? If it does not, use `ReplaceFileW`.
- **Symbol ranges.** Do the ranges include leading doc comments and attributes?
- **View retention.** What default age should `prune` use for views?

## 12. Evaluation

Run one task set with the native Edit tool and one with Lantern Edit. Deduplicate transcripts by `tool_use` id across files, because resumed sessions copy their history. Measure:

- Output tokens per edit.
- Edit failures and retries.
- **Collateral changes:** edits followed by a fix to lines they did not mean to change. This is the main metric.
- Turns per task.
- How often each collateral flag fires. A flag that fires too often trains the model to ignore it.
