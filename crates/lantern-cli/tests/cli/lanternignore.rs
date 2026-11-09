//! `.lanternignore`: scope, add and remove, negation per source, and secure removal.

use std::fs;

use crate::common::{Fixture, files_with, names, strings};

#[test]
fn a_subdirectory_file_excludes_only_below_it() {
    let f = Fixture::new();
    f.write("secret.ts", "MARK\n");
    f.write("sub/secret.ts", "MARK\n");
    f.write("sub/.lanternignore", "secret.ts\n");
    assert_eq!(files_with(&f, &f.root, "MARK"), strings(&["secret.ts"]));
}

#[test]
fn adding_a_rule_hides_a_file_from_every_query_and_removing_it_restores() {
    let f = Fixture::new();
    f.write("a.ts", "export function hidden() { helper(); }\n");
    f.write("b.ts", "export function helper() {}\n");
    f.ok(&["index"]);
    f.write(".lanternignore", "a.ts\n");
    assert!(names(&f.ok(&["symbols", "hidden"])).is_empty());
    assert!(
        f.ok(&["code", "hidden"])["results"]
            .as_array()
            .is_some_and(Vec::is_empty)
    );
    assert!(
        f.ok(&["refs", "helper"])["results"]
            .as_array()
            .is_some_and(Vec::is_empty)
    );
    assert!(
        f.ok(&["graph", "hidden"])["roots"]
            .as_array()
            .is_some_and(Vec::is_empty)
    );

    f.remove(".lanternignore");
    assert_eq!(names(&f.ok(&["symbols", "hidden"])), strings(&["hidden"]));
}

#[test]
fn folder_negation_reopens_a_default_exclude() {
    let f = Fixture::new();
    f.write("node_modules/pkg/index.js", "MARK\n");
    f.write("dist/out.js", "MARK\n");
    f.write(".lanternignore", "!node_modules/\n");
    assert_eq!(
        files_with(&f, &f.root, "MARK"),
        strings(&["node_modules/pkg/index.js"])
    );
}

#[test]
fn git_negation_cannot_add_ignored_files_and_exclusion_removes_tracked_files() {
    let f = Fixture::new();
    f.git("", &["init", "-q"]);
    f.write(".gitignore", "ignored.ts\n");
    f.write(".lanternignore", "!ignored.ts\ntracked.ts\n");
    f.write("ignored.ts", "MARK\n");
    f.write("tracked.ts", "MARK\n");
    f.write("kept.ts", "MARK\n");
    f.git("", &["add", "."]);
    f.git("", &["commit", "-qm", "init"]);
    assert_eq!(files_with(&f, &f.root, "MARK"), strings(&["kept.ts"]));
}

#[test]
fn removed_content_is_not_left_in_the_shard_or_its_wal() {
    let f = Fixture::new();
    // An identifier, so it is also stored as a symbol name.
    let marker = "LanternSecretMarker7f3a9c";
    f.write("secret.ts", format!("export function {marker}() {{}}\n"));
    f.write("public.txt", "nothing to see\n");
    f.ok(&["index"]);
    assert_eq!(names(&f.ok(&["symbols", marker])), strings(&[marker]));
    f.write(".lanternignore", "secret.ts\n");
    f.ok(&["index"]);
    for entry in fs::read_dir(f.home.join("roots"))
        .expect("roots dir")
        .flatten()
    {
        let bytes = fs::read(entry.path()).expect("read shard file");
        let found = bytes
            .windows(marker.len())
            .any(|window| window == marker.as_bytes());
        assert!(!found, "marker found in {}", entry.path().display());
    }
}

#[test]
fn a_blocked_checkpoint_exits_temporary_and_the_next_refresh_finishes_it() {
    let f = Fixture::new();
    f.write("a.ts", "export function a() {}\n");
    f.write("b.ts", "export function b() {}\n");
    f.ok(&["index"]);
    let shard = fs::read_dir(f.home.join("roots"))
        .expect("roots dir")
        .flatten()
        .map(|entry| entry.path())
        .find(|path| path.extension().is_some_and(|ext| ext == "sqlite"))
        .expect("one shard");
    let reader = rusqlite::Connection::open(&shard).expect("open shard");
    reader
        .execute_batch("BEGIN; SELECT count(*) FROM files;")
        .expect("hold a read transaction");
    f.remove("b.ts");
    let blocked = f.lantern(&["index"]);
    assert_eq!(blocked.code, 75, "{}", blocked.stderr);
    drop(reader);
    f.ok(&["index"]);
    let pending: i64 = rusqlite::Connection::open(&shard)
        .expect("open shard")
        .query_row("SELECT checkpoint_pending FROM meta", [], |row| row.get(0))
        .expect("read meta");
    assert_eq!(pending, 0);
    assert!(names(&f.ok(&["symbols", "b"])).is_empty());
}

#[test]
fn a_rebuild_blocked_by_a_reader_keeps_the_checkpoint_pending() {
    let f = Fixture::new();
    f.write("a.ts", "export function a() {}\n");
    f.ok(&["index"]);
    let shard = fs::read_dir(f.home.join("roots"))
        .expect("roots dir")
        .flatten()
        .map(|entry| entry.path())
        .find(|path| path.extension().is_some_and(|ext| ext == "sqlite"))
        .expect("one shard");
    let pending = || -> i64 {
        rusqlite::Connection::open(&shard)
            .expect("open shard")
            .query_row("SELECT checkpoint_pending FROM meta", [], |row| row.get(0))
            .expect("read meta")
    };
    let reader = rusqlite::Connection::open(&shard).expect("open shard");
    reader
        .execute_batch("BEGIN; SELECT count(*) FROM files;")
        .expect("hold a read transaction");
    let blocked = f.lantern(&["index", "--rebuild"]);
    assert_eq!(blocked.code, 75, "{}", blocked.stderr);
    drop(reader);
    assert_eq!(pending(), 1);
    f.ok(&["index"]);
    assert_eq!(pending(), 0);
}
