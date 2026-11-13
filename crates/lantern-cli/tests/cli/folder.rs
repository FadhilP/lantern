//! Folder source: scoped rules, links, reconciliation, freshness, limits, and source transitions.

use std::process::Command;

use crate::common::{Fixture, count, deny_dir, files_with, git, names, paths, strings};

#[test]
fn scoped_ignores_negation_and_default_opt_ins() {
    let f = Fixture::new();
    f.write(".gitignore", "!.config/\n*.tmp\n");
    f.write("sub/.gitignore", "*.log\n");
    for path in [
        "a.ts",
        "x.tmp",
        "top.log",
        "sub/s.log",
        "sub/kept.ts",
        "node_modules/pkg/index.js",
        "dist/bundle.js",
        ".hidden.ts",
        ".config/settings.ts",
    ] {
        f.write(path, "MARK\n");
    }
    assert_eq!(
        files_with(&f, &f.root, "MARK"),
        strings(&[".config/settings.ts", "a.ts", "sub/kept.ts", "top.log"])
    );
}

#[test]
fn links_and_junctions_are_excluded() {
    let f = Fixture::new();
    f.write("real/target.ts", "MARK\n");
    #[cfg(unix)]
    {
        std::os::unix::fs::symlink(f.path("real"), f.path("dir-link")).expect("dir symlink");
        std::os::unix::fs::symlink(f.path("real/target.ts"), f.path("file-link.ts"))
            .expect("file symlink");
    }
    #[cfg(windows)]
    {
        let status = Command::new("cmd")
            .args(["/C", "mklink", "/J"])
            .arg(f.path("junction"))
            .arg(f.path("real"))
            .output()
            .expect("run mklink");
        assert!(status.status.success(), "mklink /J failed");
    }
    assert_eq!(
        files_with(&f, &f.root, "MARK"),
        strings(&["real/target.ts"])
    );
}

#[test]
fn edits_renames_exclusions_and_deletions_reconcile() {
    let f = Fixture::new();
    f.write("a.ts", "export function alpha() {}\n");
    f.write("b.ts", "export function beta() {}\n");
    f.ok(&["index"]);
    let found = f.ok(&["symbols", "alpha"]);
    assert_eq!(names(&found), strings(&["alpha"]));

    f.write("a.ts", "export function alphaRenamed() {}\n");
    assert_eq!(
        names(&f.ok(&["symbols", "alpha"])),
        strings(&["alphaRenamed"])
    );

    std::fs::rename(f.path("b.ts"), f.path("c.ts")).expect("rename");
    assert_eq!(f.ok(&["symbols", "beta"])["results"][0]["path"], "c.ts");

    f.write(".gitignore", "c.ts\n");
    assert!(names(&f.ok(&["symbols", "beta"])).is_empty());

    f.remove("a.ts");
    assert!(names(&f.ok(&["symbols", "alphaRenamed"])).is_empty());
    assert_eq!(count(&f.ok(&["status"]), "files"), 0);
}

#[test]
fn incomplete_inventory_keeps_committed_content_then_recovers() {
    let f = Fixture::new();
    f.write("a.ts", "export function kept() {}\n");
    let before = f.ok(&["index"]);
    f.write("sub/.gitignore", vec![b'#'; (4 << 20) + 1]);
    let failed = f.lantern(&["symbols", "kept"]);
    assert_eq!(failed.code, 1, "{}", failed.stderr);
    let status = f.ok(&["status"]);
    assert_eq!(status["indexed_at"], before["indexed_at"]);
    assert_eq!(count(&status, "files"), 1);

    f.remove("sub/.gitignore");
    assert_eq!(names(&f.ok(&["symbols", "kept"])), strings(&["kept"]));
}

#[test]
fn work_limits_fail_without_publishing() {
    if !cfg!(debug_assertions) {
        return;
    }
    let f = Fixture::new();
    for index in 0..5 {
        f.write(&format!("f{index}.ts"), "MARK\n");
    }
    let run = f.lantern_in(&f.root, &["index"], &[("LANTERN_TEST_MAX_ENTRIES", "3")]);
    assert_eq!(run.code, 1, "{}", run.stderr);
    assert_eq!(f.ok(&["status"])["indexed"], false);
    let query = f.lantern_in(&f.root, &["symbols", "MARK"], &[]);
    assert_eq!(
        query.code, 2,
        "a failed first index must not unlock queries"
    );
}

#[test]
fn a_failed_rebuild_leaves_no_staging_copy() {
    if !cfg!(debug_assertions) {
        return;
    }
    let f = Fixture::new();
    for index in 0..5 {
        f.write(&format!("f{index}.ts"), "MARK\n");
    }
    f.ok(&["index"]);
    let run = f.lantern_in(
        &f.root,
        &["index", "--rebuild"],
        &[("LANTERN_TEST_MAX_ENTRIES", "3")],
    );
    assert_eq!(run.code, 1, "{}", run.stderr);
    let staged: Vec<_> = std::fs::read_dir(f.home.join("roots"))
        .expect("roots")
        .filter_map(|entry| entry.ok())
        .filter(|entry| entry.path().extension().is_some_and(|ext| ext == "rebuild"))
        .collect();
    assert!(staged.is_empty(), "{staged:?}");
}

#[test]
fn a_killed_rebuild_leaves_no_staging_copy_after_the_next_query() {
    let f = Fixture::new();
    f.write("a.ts", "export function alpha() {}\n");
    f.ok(&["index"]);
    let staging = f.shard().with_extension("rebuild");
    std::fs::write(&staging, "MARK\n").expect("write staging copy");
    f.ok(&["symbols", "alpha"]);
    assert!(!staging.exists());
}

#[test]
fn a_damaged_shard_asks_for_a_rebuild_that_replaces_it() {
    let f = Fixture::new();
    f.write("a.ts", "export function alpha() {}\n");
    f.ok(&["index"]);
    // A damaged header is SQLITE_NOTADB; damage after the 100-byte header is SQLITE_CORRUPT.
    for keep in [0, 100] {
        damage(&f.shard(), keep);
        let damaged = f.lantern(&["symbols", "alpha"]);
        assert_eq!(damaged.code, 1, "{}", damaged.stderr);
        assert!(
            damaged.stderr.contains("lantern index --rebuild"),
            "{}",
            damaged.stderr
        );
        f.ok(&["index", "--rebuild"]);
        assert_eq!(names(&f.ok(&["symbols", "alpha"])), strings(&["alpha"]));
    }
    damage(&f.shard(), 0);
    f.ok(&["prune"]);
    assert!(
        !f.home
            .join("roots")
            .read_dir()
            .expect("roots")
            .any(|entry| {
                entry.is_ok_and(|entry| entry.path().extension().is_some_and(|ext| ext == "sqlite"))
            })
    );
}

fn damage(shard: &std::path::Path, keep: usize) {
    let mut bytes = std::fs::read(shard).expect("read shard");
    bytes[keep..].fill(0xAB);
    std::fs::write(shard, bytes).expect("damage the shard");
}

#[test]
fn a_shard_from_an_older_schema_is_built_again() {
    let f = Fixture::new();
    f.write("a.ts", "export function alpha() {}\n");
    f.ok(&["index"]);
    rusqlite::Connection::open(f.shard())
        .expect("open shard")
        .execute_batch("PRAGMA user_version = 2")
        .expect("downgrade the shard");
    f.write("b.ts", "export function beta() {}\n");
    assert_eq!(names(&f.ok(&["symbols", "beta"])), strings(&["beta"]));
    let version: i64 = rusqlite::Connection::open(f.shard())
        .expect("open shard")
        .query_row("PRAGMA user_version", [], |row| row.get(0))
        .expect("read version");
    assert!(version > 2, "{version}");
}

#[test]
fn prune_leaves_a_newer_shard_alone_and_still_prunes_the_rest() {
    let f = Fixture::new();
    f.write("a.ts", "export function alpha() {}\n");
    f.ok(&["index"]);
    let shard = f.shard();
    rusqlite::Connection::open(&shard)
        .expect("open shard")
        .execute_batch("PRAGMA user_version = 99")
        .expect("mark the shard as newer");
    let stray = f.home.join("roots").join("0123456789abcdef.rebuild");
    std::fs::write(&stray, "MARK\n").expect("write stray file");
    assert_eq!(count(&f.ok(&["prune"]), "removed_files"), 1);
    assert!(shard.exists());
    assert!(!stray.exists());
}

#[test]
fn an_unreadable_file_is_skipped_counted_and_read_once_readable() {
    let f = Fixture::new();
    f.write("a.ts", "export function alpha() {}\n");
    f.write("held.ts", "export function before() {}\n");
    f.ok(&["index"]);
    f.write("held.ts", "export function after() {}\n");
    // Past the 2 s racy window, so only the unreadable mark makes the next refresh read it.
    std::thread::sleep(std::time::Duration::from_millis(2_100));
    let Some(denied) = deny_read(&f.path("held.ts")) else {
        return;
    };
    assert_eq!(names(&f.ok(&["symbols", "alpha"])), strings(&["alpha"]));
    assert!(names(&f.ok(&["symbols", "before"])).is_empty());
    assert_eq!(count(&f.ok(&["status"]), "unreadable_files"), 1);
    drop(denied);
    assert_eq!(names(&f.ok(&["symbols", "after"])), strings(&["after"]));
    assert_eq!(count(&f.ok(&["status"]), "unreadable_files"), 0);
}

/// Holds the file open with no sharing, as another process can on Windows.
#[cfg(windows)]
fn deny_read(path: &std::path::Path) -> Option<std::fs::File> {
    use std::os::windows::fs::OpenOptionsExt;
    let held = std::fs::OpenOptions::new()
        .read(true)
        .share_mode(0)
        .open(path)
        .expect("hold the file");
    Some(held)
}

/// Removes read permission until dropped; `None` when the user can read it anyway (root).
#[cfg(unix)]
fn deny_read(path: &std::path::Path) -> Option<impl Drop + use<>> {
    use std::os::unix::fs::PermissionsExt;
    struct Restore(std::path::PathBuf);
    impl Drop for Restore {
        fn drop(&mut self) {
            let _ = std::fs::set_permissions(&self.0, std::fs::Permissions::from_mode(0o644));
        }
    }
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o000)).expect("chmod");
    let restore = Restore(path.to_path_buf());
    std::fs::File::open(path).is_err().then_some(restore)
}

#[test]
fn lowering_max_file_size_removes_larger_files() {
    let f = Fixture::new();
    f.write(
        "big.ts",
        format!("export function big() {{}}\n{}\n", "/".repeat(2_000)),
    );
    f.write("small.ts", "export function small() {}\n");
    f.ok(&["index"]);
    assert_eq!(names(&f.ok(&["symbols", "big"])), strings(&["big"]));
    let lowered = f.ok(&["--max-file-size", "1K", "symbols", "big"]);
    assert!(names(&lowered).is_empty());
    assert_eq!(
        names(&f.ok(&["--max-file-size", "1K", "symbols", "small"])),
        strings(&["small"])
    );
}

#[test]
fn folder_becomes_git_at_the_same_root_and_parent_expansion_is_refused() {
    let f = Fixture::new();
    f.write("project/file.ts", "export function commonSymbol() {}\n");
    f.write("project/.hidden.ts", "export function gitOnlySymbol() {}\n");
    let project = f.path("project");
    assert!(names(&f.ok_in(&project, &["symbols", "gitOnlySymbol"])).is_empty());

    git(&project, &["init", "-q"]);
    assert_eq!(
        names(&f.ok_in(&project, &["symbols", "gitOnlySymbol"])),
        strings(&["gitOnlySymbol"])
    );
    assert_eq!(f.ok_in(&project, &["status"])["source"], "git");

    f.write("outer/inner/file.ts", "export function inner() {}\n");
    let inner = f.path("outer/inner");
    f.ok_in(&inner, &["index"]);
    git(&f.path("outer"), &["init", "-q"]);
    let refused = f.lantern_in(&inner, &["symbols", "inner"], &[]);
    assert_eq!(refused.code, 3, "{}", refused.stderr);
}

#[test]
fn unchanged_content_keeps_symbols_and_rebuild_stays_correct() {
    let f = Fixture::new();
    f.write(
        "a.ts",
        "export function one() { two(); }\nexport function two() {}\n",
    );
    let first = f.ok(&["index"]);
    let again = f.ok(&["index"]);
    assert_eq!(count(&again, "changed"), 0);
    assert_eq!(count(&again, "symbols"), count(&first, "symbols"));
    let rebuilt = f.ok(&["index", "--rebuild"]);
    assert_eq!(count(&rebuilt, "symbols"), count(&first, "symbols"));
    assert_eq!(count(&rebuilt, "refs"), count(&first, "refs"));
    assert_eq!(names(&f.ok(&["symbols", "two"])), strings(&["two"]));
}

#[test]
fn concurrent_index_processes_share_one_root() {
    let f = Fixture::new();
    for index in 0..40 {
        f.write(
            &format!("src/f{index}.ts"),
            format!("export function f{index}() {{}}\n"),
        );
    }
    let spawn = || {
        Command::new(env!("CARGO_BIN_EXE_lantern"))
            .args(["index", "--json"])
            .current_dir(&f.root)
            .env("LANTERN_HOME", &f.home)
            .spawn()
            .expect("spawn lantern")
    };
    let (mut first, mut second) = (spawn(), spawn());
    assert!(first.wait().expect("wait").success());
    assert!(second.wait().expect("wait").success());
    let status = f.ok(&["status"]);
    assert_eq!(
        (count(&status, "files"), count(&status, "symbols")),
        (40, 40)
    );
}

#[test]
fn same_bytes_under_two_languages_keep_their_own_symbols() {
    let f = Fixture::new();
    let source = "def hello_world():\n    pass\n";
    f.write("notes.txt", source);
    f.write("tool", source);
    f.ok(&["index"]);
    std::fs::rename(f.path("tool"), f.path("tool.py")).expect("rename");
    let found = f.ok(&["symbols", "hello_world"]);
    assert_eq!(crate::common::paths(&found), strings(&["tool.py"]));
}

#[test]
fn rebuild_is_not_bound_by_the_refresh_timeout() {
    let f = Fixture::new();
    for index in 0..200 {
        f.write(
            &format!("src/f{index}.ts"),
            format!("export function f{index}() {{}}\n"),
        );
    }
    f.ok(&["index"]);
    let rebuilt = f.ok(&["--timeout", "1", "index", "--rebuild"]);
    assert_eq!(count(&rebuilt, "symbols"), 200);
}

#[test]
fn an_upgrade_rereads_files_whose_language_detection_changed() {
    let f = Fixture::new();
    f.write("a.ts", "export function kept() {}\n");
    f.write("notes.txt", "def stale():\n    pass\n");
    f.ok(&["index"]);
    let shard = std::fs::read_dir(f.home.join("roots"))
        .expect("roots dir")
        .flatten()
        .map(|entry| entry.path())
        .find(|path| path.extension().is_some_and(|ext| ext == "sqlite"))
        .expect("one shard");
    // As if an older binary had detected `notes.txt` as Python.
    let conn = rusqlite::Connection::open(&shard).expect("open shard");
    conn.execute_batch(
        "UPDATE meta SET extractor = 'older';
         UPDATE blobs SET language = 'python', key = randomblob(32)
           WHERE id = (SELECT blob_id FROM files WHERE path = 'notes.txt');",
    )
    .expect("simulate an upgrade");
    drop(conn);
    f.ok(&["index"]);
    assert!(names(&f.ok(&["symbols", "stale"])).is_empty());
    assert_eq!(names(&f.ok(&["symbols", "kept"])), strings(&["kept"]));
}

#[test]
fn refs_follow_edits_deletions_and_a_new_extractor() {
    let f = Fixture::new();
    f.write("a.ts", "export function one() { shared(); }\n");
    f.write("b.ts", "export function two() { shared(); other(); }\n");
    f.ok(&["index"]);
    assert_eq!(
        paths(&f.ok(&["refs", "shared"])),
        strings(&["a.ts", "b.ts"])
    );

    f.write("a.ts", "export function one() { other(); }\n");
    assert_eq!(paths(&f.ok(&["refs", "shared"])), strings(&["b.ts"]));
    assert_eq!(paths(&f.ok(&["refs", "other"])), strings(&["a.ts", "b.ts"]));

    f.remove("b.ts");
    assert!(paths(&f.ok(&["refs", "shared"])).is_empty());
    // The new blob takes the id that the removed one freed.
    f.write("c.ts", "export function three() { shared(); }\n");
    assert_eq!(paths(&f.ok(&["refs", "shared"])), strings(&["c.ts"]));

    let conn = rusqlite::Connection::open(f.shard()).expect("open shard");
    conn.execute_batch("UPDATE meta SET extractor = 'older'")
        .expect("simulate an upgrade");
    drop(conn);
    assert_eq!(count(&f.ok(&["index"]), "refs"), 2);
    let refs = f.ok(&["refs", "other"]);
    assert_eq!(paths(&refs), strings(&["a.ts"]));
    assert_eq!(refs["results"][0]["enclosing"]["name"], "one");
    assert_eq!(f.ok(&["graph", "one"])["unresolved"][0], "other");
}

#[test]
fn a_timed_out_parse_is_retried_on_the_next_refresh() {
    let f = Fixture::new();
    f.write("a.ts", "export function retried() {}\n");
    f.ok(&["index"]);
    let shard = std::fs::read_dir(f.home.join("roots"))
        .expect("roots dir")
        .flatten()
        .map(|entry| entry.path())
        .find(|path| path.extension().is_some_and(|ext| ext == "sqlite"))
        .expect("one shard");
    // As if the first parse had run out of time under load.
    let conn = rusqlite::Connection::open(&shard).expect("open shard");
    conn.execute_batch(
        "DELETE FROM refs; DELETE FROM ref_names; DELETE FROM symbols;
         UPDATE blobs SET timeouts = 1;",
    )
    .expect("simulate a timeout");
    drop(conn);
    assert_eq!(names(&f.ok(&["symbols", "retried"])), strings(&["retried"]));
}

#[test]
fn files_out_of_parse_retries_stay_visible_until_a_rebuild() {
    let f = Fixture::new();
    f.write("a.ts", "export function lost() {}\n");
    f.write("b.ts", "// no symbols\n");
    f.ok(&["index"]);
    assert_eq!(count(&f.ok(&["status"]), "unparsed_files"), 0);
    assert!(
        !f.text(&["symbols", "lost_nothing"])
            .contains("ran out of time")
    );

    let shard = std::fs::read_dir(f.home.join("roots"))
        .expect("roots dir")
        .flatten()
        .map(|entry| entry.path())
        .find(|path| path.extension().is_some_and(|ext| ext == "sqlite"))
        .expect("one shard");
    let conn = rusqlite::Connection::open(&shard).expect("open shard");
    conn.execute_batch(
        "DELETE FROM refs; DELETE FROM ref_names; DELETE FROM symbols;
         UPDATE blobs SET timeouts = 3
         WHERE id = (SELECT blob_id FROM files WHERE path = 'a.ts');",
    )
    .expect("simulate three timeouts");
    drop(conn);

    let refreshed = f.ok(&["index"]);
    assert_eq!(count(&refreshed, "parse_timeouts"), 0);
    assert_eq!(count(&refreshed, "unparsed_files"), 1);
    let status = f.ok(&["status"]);
    assert_eq!(count(&status, "unparsed_files"), 1);
    assert_eq!(status["unparsed_files"], 1);
    for args in [
        &["symbols", "lost"][..],
        &["refs", "lost"],
        &["graph", "lost"],
        &["outline", "a.ts"],
    ] {
        let text = f.text(args);
        assert!(
            text.starts_with("no results;") && text.contains("for 1 file(s)"),
            "{args:?}: {text}"
        );
    }

    f.ok(&["index", "--rebuild"]);
    assert_eq!(count(&f.ok(&["status"]), "unparsed_files"), 0);
    assert_eq!(names(&f.ok(&["symbols", "lost"])), strings(&["lost"]));
    assert_eq!(f.text(&["outline", "b.ts"]), "no results\n");
}

#[test]
fn a_denied_directory_with_rules_is_skipped() {
    let f = Fixture::new();
    f.write("a.ts", "export function alpha() {}\n");
    f.write("sub/.gitignore", "*.log\n");
    f.write("sub/s.ts", "export function inside() {}\n");
    f.ok(&["index"]);
    let Some(denied) = deny_dir(&f.path("sub")) else {
        return;
    };
    f.write("b.ts", "export function beta() {}\n");
    assert_eq!(names(&f.ok(&["symbols", "beta"])), strings(&["beta"]));
    assert!(names(&f.ok(&["symbols", "inside"])).is_empty());
    drop(denied);
    assert_eq!(names(&f.ok(&["symbols", "inside"])), strings(&["inside"]));
}
