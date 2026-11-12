//! `changes`: git's diff mapped to definitions, without an index.

use std::fs;
use std::time::{Duration, SystemTime};

use serde_json::{Value, json};

use crate::common::Fixture;

fn repository() -> Fixture {
    let f = Fixture::new();
    f.git("", &["init", "-q"]);
    f
}

fn commit(f: &Fixture, message: &str) {
    f.git("", &["add", "-A"]);
    f.git("", &["commit", "-qm", message]);
}

/// `(status, name)` of each changed definition of the file at `path`.
fn statuses(document: &Value, path: &str) -> Vec<(String, String)> {
    let file = document["results"]
        .as_array()
        .expect("results")
        .iter()
        .find(|file| file["path"] == path)
        .unwrap_or_else(|| panic!("no {path} in {document}"));
    file["symbols"]
        .as_array()
        .expect("symbols")
        .iter()
        .map(|symbol| {
            let text = |key: &str| symbol[key].as_str().expect("text").to_owned();
            (text("status"), text("name"))
        })
        .collect()
}

fn pair(status: &str, name: &str) -> (String, String) {
    (status.to_owned(), name.to_owned())
}

#[test]
fn the_working_tree_lists_definitions_by_status_and_leaves_the_git_index_alone() {
    let f = repository();
    f.write(
        "src/main.ts",
        "export function keep() {\n  return 1;\n}\n\nexport function change() {\n  return 2;\n}\n\nexport function drop() {\n  return 3;\n}\n",
    );
    f.write("src/gone.ts", "export function gone() {\n  return 0;\n}\n");
    // The removed line prints as `--- note` in the patch, like a file header.
    f.write("a.sql", "-- note\nselect 1;\n");
    f.write("same.txt", "same\n");
    commit(&f, "init");
    f.write(
        "src/main.ts",
        "export function keep() {\n  return 1;\n}\n\nexport function change() {\n  return 20;\n}\n\nexport function fresh() {\n  return 4;\n}\n",
    );
    f.remove("src/gone.ts");
    f.write("a.sql", "++ note\nselect 1;\n");
    f.write("src/staged.ts", "export const LIMIT = 3;\n");
    f.git("", &["add", "src/staged.ts"]);
    f.write("src/new.ts", "export class Fresh {\n  run() {}\n}\n");
    f.write("src/z.bin", b"\0\x01binary");
    // Git lists an untracked repository as `nested/`, which is not a file.
    f.write("nested/inner.ts", "export const INNER = 1;\n");
    f.git("nested", &["init", "-q"]);
    // Same content, new time: without `diff.autoRefreshIndex=false`, git would refresh its index.
    fs::File::options()
        .write(true)
        .open(f.path("same.txt"))
        .expect("open file")
        .set_modified(SystemTime::now() + Duration::from_secs(60))
        .expect("set time");
    let index = fs::read(f.path(".git/index")).expect("git index");

    let document = f.ok(&["changes"]);

    assert_eq!(fs::read(f.path(".git/index")).expect("git index"), index);
    let files: Vec<(&str, &str)> = document["results"]
        .as_array()
        .expect("results")
        .iter()
        .map(|file| {
            let text = |key: &str| file[key].as_str().expect("text");
            (text("path"), text("status"))
        })
        .collect();
    assert_eq!(
        files,
        [
            ("a.sql", "modified"),
            ("src/gone.ts", "deleted"),
            ("src/main.ts", "modified"),
            ("src/new.ts", "untracked"),
            ("src/staged.ts", "added"),
            ("src/z.bin", "untracked"),
        ]
    );
    assert_eq!(document["results"][5]["unparsed"], "binary");
    let sql = &document["results"][0];
    assert_eq!(
        (&sql["added"], &sql["removed"], &sql["unparsed"]),
        (&json!(1), &json!(1), &json!("no_parser"))
    );
    assert_eq!(
        statuses(&document, "src/main.ts"),
        [
            pair("modified", "change"),
            pair("removed", "drop"),
            pair("added", "fresh")
        ]
    );
    assert_eq!(
        statuses(&document, "src/gone.ts"),
        [pair("removed", "gone")]
    );
    assert_eq!(statuses(&document, "src/new.ts"), [pair("added", "Fresh")]);
    assert_eq!(
        statuses(&document, "src/staged.ts"),
        [pair("added", "LIMIT")]
    );

    let diff = f.ok(&["changes", "--diff", "change"]);
    assert_eq!(diff["results"].as_array().map(Vec::len), Some(1));
    assert_eq!(
        diff["results"][0]["symbols"][0]["lines"],
        json!([
            { "sign": "-", "line": 6, "text": "  return 2;" },
            { "sign": "+", "line": 6, "text": "  return 20;" },
        ])
    );
    let container = f.ok(&["changes", "--diff", "Fresh"]);
    let lines = &container["results"][0]["symbols"][0]["lines"];
    assert_eq!(lines.as_array().map(Vec::len), Some(3), "{container}");
    let member = f.ok(&["changes", "--diff", "Fresh.run"]);
    assert_eq!(
        member["results"][0]["symbols"][0]["lines"],
        json!([{ "sign": "+", "line": 2, "text": "  run() {}" }])
    );
}

#[test]
fn a_range_follows_renames_and_the_start_directory() {
    let f = repository();
    f.write(
        "lib/util.py",
        "def one():\n    return 1\n\n\ndef two():\n    return 2\n",
    );
    commit(&f, "init");
    f.git("", &["mv", "lib/util.py", "lib/helpers.py"]);
    f.write(
        "lib/helpers.py",
        "def one():\n    return 1\n\n\ndef two():\n    return 22\n",
    );
    f.write("other/x.py", "def x():\n    pass\n");
    commit(&f, "move");
    // Git looks past a `.git` that is no repository, such as one an aborted clone left.
    fs::create_dir(f.path("lib/.git")).expect("create .git");

    let text = f.text(&["-C", "lib", "changes", "HEAD~1..HEAD"]);
    let symbols = f.text(&["-C", "lib", "symbols", "two"]);

    assert_eq!(
        text,
        "helpers.py (renamed from util.py, +1 -1)\n  modified function two 5-6 (+1 -1)\n"
    );
    assert_eq!(symbols, "helpers.py:5-6 function: def two():\n");
}

/// To find the rename, git hashes the edited file but does not store the blob.
#[test]
fn a_renamed_file_edited_in_the_working_tree_is_read_from_the_disk() {
    let f = repository();
    f.write(
        "a.py",
        "def one():\n    return 1\n\n\ndef two():\n    return 2\n",
    );
    commit(&f, "init");
    f.git("", &["mv", "a.py", "c.py"]);
    f.write(
        "c.py",
        "def one():\n    return 1\n\n\ndef two():\n    return 22\n",
    );

    for range in [None, Some("HEAD")] {
        let text = f.text(&[&["changes"][..], range.as_slice()].concat());
        assert_eq!(
            text,
            "c.py (renamed from a.py, +1 -1)\n  modified function two 5-6 (+1 -1)\n"
        );
    }
}

#[test]
fn lines_follow_git_not_the_environment_or_the_disk() {
    let f = repository();
    f.write("out/a.ts", "export function f() {\n  return 1;\n}\n");
    f.write("in/b.ts", "export const B = 1;\n");
    f.write(".gitattributes", "*.py ident\n");
    f.write("c.py", "def f():\n    return 1\n");
    f.write(
        "crcr.ts",
        "export function a() {\r\r\n  return 1;\r\r\n}\r\r\n",
    );
    commit(&f, "one");
    f.write(
        "out/a.ts",
        "export function f() {\n  const x = 2;\n  return x;\n}\n",
    );
    f.write(
        "crcr.ts",
        "export function a() {\r\r\n  return 2;\r\r\n}\r\r\n",
    );
    commit(&f, "two");
    // Git compares a file outside the sparse checkout with the index, not the missing file.
    f.git("", &["sparse-checkout", "set", "in"]);
    // Git cleans `$Id: … $` to `$Id$`, so the line on the disk is not git's line.
    f.write("c.py", "def f():\n    return 2  # $Id: abc $\n");

    let sparse = f.text(&["changes", "HEAD~1"]);
    let unified = f.lantern_in(&f.path(""), &["changes"], &[("GIT_DIFF_OPTS", "-u7")]);

    assert_eq!(
        sparse,
        "c.py (+1 -1, the file differs from git's diff)\ncrcr.ts (+1 -1)\n  modified function a 1-3 (+1 -1)\nout/a.ts (+2 -1)\n  modified function f 1-4 (+2 -1)\n"
    );
    assert_eq!(unified.code, 0, "{}", unified.stderr);
}

/// `std::fs::canonicalize` keeps the `\\?\` prefix of such a path.
#[cfg(windows)]
#[test]
fn a_path_longer_than_260_characters_is_read() {
    let f = repository();
    f.git("", &["config", "core.longpaths", "true"]);
    let long = format!("{0}/{0}/{0}/a.ts", "d".repeat(100));
    f.write(&long, "export function a() {\n  return 1;\n}\n");
    commit(&f, "init");
    f.write(&long, "export function a() {\n  return 2;\n}\n");

    let document = f.ok(&["changes"]);

    assert_eq!(statuses(&document, &long), [pair("modified", "a")]);
}

/// A junction is no symbolic link, but it leads outside too.
#[cfg(windows)]
#[test]
fn files_behind_a_junction_are_left_out() {
    let f = repository();
    f.write("keep.txt", "keep\n");
    commit(&f, "init");
    let outside = Fixture::new();
    outside.write("secret.ts", "export const SECRET = 1;\n");
    let made = std::process::Command::new("cmd")
        .args(["/C", "mklink", "/J"])
        .arg(f.path("link"))
        .arg(outside.path(""))
        .output()
        .expect("mklink");
    assert!(made.status.success(), "{made:?}");

    assert_eq!(f.text(&["changes"]), "no results; no changes\n");
}

/// Git quotes such a path in the patch header; Windows does not allow a tab in a file name. An
/// untracked symlink is left out, so its target is not read.
#[cfg(unix)]
#[test]
fn a_path_that_git_quotes_keeps_its_lines() {
    let f = repository();
    f.write("tab\tname.ts", "export const A = 1;\n");
    std::os::unix::fs::symlink("tab\tname.ts", f.path("was_link")).expect("symlink");
    commit(&f, "init");
    f.write("tab\tname.ts", "export const A = 2;\n");
    std::os::unix::fs::symlink("/etc", f.path("link")).expect("symlink");
    // A type change gives git's patch two sections for one path.
    fs::remove_file(f.path("was_link")).expect("remove symlink");
    f.write("was_link", "export const B = 1;\n");

    let document = f.ok(&["changes"]);

    assert_eq!(statuses(&document, "tab\tname.ts"), [pair("modified", "A")]);
    assert_eq!(document["results"].as_array().map(Vec::len), Some(1));
}

#[test]
fn a_rewritten_one_line_definition_is_modified_and_a_new_overload_is_added() {
    let f = repository();
    f.write(
        "A.java",
        "class A {\n  int size() { return 1; }\n\n  void run(int x) {\n    go(x);\n  }\n}\n",
    );
    commit(&f, "init");
    f.write(
        "A.java",
        "class A {\n  int size() { return 2; }\n\n  void run(String s) {\n    go(s);\n  }\n\n  void run(int x) {\n    go(x);\n  }\n}\n",
    );

    let document = f.ok(&["changes"]);

    assert_eq!(
        statuses(&document, "A.java"),
        [pair("modified", "size"), pair("added", "run")]
    );
    assert_eq!(document["results"][0]["symbols"][1]["line"], 4);
}

#[test]
fn moved_definitions_name_the_other_place_on_every_page() {
    let f = repository();
    f.write(
        "a.ts",
        "export function keep() {\n  return 1;\n}\n\nexport function travel(a) {\n  const b = a + 1;\n  return b * 2;\n}\n",
    );
    f.write(
        "p.py",
        "def f():\n    return 1\n\n\ndef g():\n    return \"a long greeting\"\n",
    );
    f.write("m.py", "def run(x):\n    if x:\n        a()\n        b()\n");
    f.write("n.py", "def n():\n    return 0\n");
    f.write(
        "h.py",
        "def helper():\n    return \"some helper text\"\ndef keep():\n    print(\"a line of keep\")\n    x = 1\n",
    );
    let constants =
        "export const FIRST_CONSTANT_VALUE = 1;\nexport const SECOND_CONSTANT_VALUE = 2;\n";
    f.write("consts.ts", format!("{constants}export const STAY = 0;\n"));
    f.write(
        "views.py",
        "@login_required\n@permission_required(\"admin\")\ndef delete_account(user):\n    user.delete()\n    return \"account deleted\"\n",
    );
    commit(&f, "init");
    f.write("views.py", "");
    f.write(
        "admin.py",
        "def delete_account(user):\n    user.delete()\n    return \"account deleted\"\n",
    );
    // Adjacent definitions move together: each next line is at the same depth.
    f.write("consts.ts", "export const STAY = 0;\n");
    f.write("more.ts", constants);
    // The removed `helper` and a line of `keep` are no block, and the new `helper` goes on.
    f.write("h.py", "def keep():\n    x = 1\n");
    f.write(
        "i.py",
        "def helper():\n    return \"some helper text\"\n    print(\"a line of keep\")\n",
    );
    // `b()` leaves the `if`: the same trimmed lines, but a change.
    f.write("m.py", "");
    f.write(
        "n.py",
        "def n():\n    return 0\n\n\ndef run(x):\n    if x:\n        a()\n    b()\n",
    );
    f.write("a.ts", "export function keep() {\n  return 1;\n}\n");
    f.write(
        "b.ts",
        "export function travel(a) {\n  const b = a + 1;\n  return b * 2;\n}\n",
    );
    // `g` moves into a new class; `f` is only indented in place, which is no move.
    f.write("p.py", "if True:\n    def f():\n        return 1\n");
    f.write(
        "k.py",
        "class K:\n    def g():\n        return \"a long greeting\"\n",
    );
    f.git("", &["add", "k.py"]);

    // The untracked file it moved to is not on this page.
    let page = f.text(&["changes", "--limit", "1", "--path", "a.ts"]);
    let document = f.ok(&["changes"]);

    assert_eq!(page, "a.ts (-5)\n  moved function travel 5-8 to b.ts:1\n");
    assert_eq!(statuses(&document, "b.ts"), [pair("moved", "travel")]);
    let b = document["results"]
        .as_array()
        .and_then(|files| files.iter().find(|file| file["path"] == "b.ts"));
    assert_eq!(
        b.map(|file| &file["symbols"][0]["from"]),
        Some(&json!({ "path": "a.ts", "line": 5 }))
    );
    assert_eq!(
        statuses(&document, "k.py"),
        [pair("added", "K"), pair("moved", "g")]
    );
    assert_eq!(statuses(&document, "n.py"), [pair("added", "run")]);
    assert_eq!(
        statuses(&document, "h.py"),
        [pair("removed", "helper"), pair("modified", "keep")]
    );
    assert_eq!(statuses(&document, "i.py"), [pair("added", "helper")]);
    assert_eq!(
        statuses(&document, "admin.py"),
        [pair("added", "delete_account")]
    );
    assert_eq!(
        statuses(&document, "more.ts"),
        [
            pair("moved", "FIRST_CONSTANT_VALUE"),
            pair("moved", "SECOND_CONSTANT_VALUE")
        ]
    );
    assert_eq!(
        statuses(&document, "p.py"),
        [pair("modified", "f"), pair("moved", "g")]
    );
}

#[test]
fn a_bare_diff_prints_every_changed_line_and_a_copy_names_its_source() {
    let f = repository();
    f.write("b.ts", "export function other() {\n  return 9;\n}\n");
    commit(&f, "init");
    // Git finds a copy only from a file that the diff also changes.
    f.write("b.ts", "export function other() {\n  return 10;\n}\n");
    f.write(
        "c.ts",
        "export function other() {\n  return 9;\n}\n\nconsole.log(1);\n",
    );
    f.git("", &["add", "c.ts"]);

    let text = f.text(&["changes", "HEAD", "--diff"]);

    assert_eq!(
        text,
        "b.ts (+1 -1)\n  modified function other 1-3 (+1 -1)\n-2:  return 9;\n+2:  return 10;\nc.ts (copied from b.ts, +2)\n+5:console.log(1);\n"
    );
}

#[test]
fn pages_go_stale_when_the_diff_changes() {
    let f = repository();
    f.write("a.ts", "export const A = 1;\n");
    commit(&f, "init");
    for path in ["b.ts", "c.ts", "d.ts", "sub/e.ts"] {
        f.write(path, "export const B = 1;\n");
    }

    let first = f.ok(&["changes", "--limit", "1"]);
    let cursor = first["next_cursor"].as_str().expect("cursor").to_owned();
    let second = f.ok(&["changes", "--limit", "1", "--cursor", &cursor]);
    assert_eq!(second["results"][0]["path"], "c.ts");
    let elsewhere = f.lantern(&["-C", "sub", "changes", "--limit", "1", "--cursor", &cursor]);
    assert_eq!(elsewhere.code, 2, "{}", elsewhere.stderr);
    let none = f.lantern(&["changes", "--limit", "0"]);
    assert_eq!(none.code, 2, "{}", none.stderr);
    f.write("a.ts", "export const A = 2;\n");
    let stale = f.lantern(&["changes", "--limit", "1", "--cursor", &cursor]);
    assert_eq!(stale.code, 4, "{}", stale.stderr);
}

#[test]
fn bad_ranges_and_folders_without_git_are_usage_errors() {
    let plain = Fixture::new();
    let outside = plain.lantern(&["changes"]);
    assert_eq!(outside.code, 2, "{}", outside.stderr);

    let f = repository();
    f.write("a.ts", "export const A = 1;\n");
    let unborn = f.lantern(&["changes"]);
    assert_eq!(unborn.code, 2, "{}", unborn.stderr);
    assert!(unborn.stderr.contains("no commit yet"), "{}", unborn.stderr);

    commit(&f, "init");
    let unknown = f.lantern(&["changes", "nope..HEAD"]);
    assert_eq!(unknown.code, 2, "{}", unknown.stderr);
    // `git diff` would take a path outside the repository for `--no-index`.
    let path = f.lantern(&["changes", "../outside.ts"]);
    assert_eq!(path.code, 2, "{}", path.stderr);
    assert!(path.stderr.contains("names no revision"), "{}", path.stderr);
    f.git("", &["checkout", "-qb", "side"]);
    f.write("b.ts", "export const B = 1;\n");
    commit(&f, "side");
    f.git("", &["checkout", "-q", "-"]);
    f.write("c.ts", "export const C = 1;\n");
    commit(&f, "main");
    f.git("", &["merge", "-q", "--no-edit", "side"]);
    let combined = f.lantern(&["changes", "HEAD^!"]);
    assert_eq!(combined.code, 2, "{}", combined.stderr);
    let option = f.lantern(&["changes", "--", "--output=leak.txt"]);
    assert_eq!(option.code, 2, "{}", option.stderr);
    assert!(!f.path("leak.txt").exists());
}
