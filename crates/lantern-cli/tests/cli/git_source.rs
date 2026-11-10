//! Git source: inventory parity with Git, failure on an unreadable index, branches, and attributes.

use crate::common::{Fixture, count, files_with, names, paths, strings};

#[test]
fn inventory_equals_git_ls_files() {
    let f = Fixture::new();
    f.git("", &["init", "-q"]);
    f.write(".gitignore", "ignored/\n*.log\nlogs/*\n!logs/keep.ts\n");
    f.write("sub/.gitignore", "*.tmp\n");
    f.write(".git/info/exclude", "excluded.txt\n");
    for path in [
        "src/a.ts",
        ".dot.ts",
        "dist/bundle.js",
        "tracked.log",
        "ignored/x.ts",
        "ignored/forced.ts",
        "logs/keep.ts",
        "logs/drop.ts",
        "sub/a.tmp",
        "sub/b.ts",
        "excluded.txt",
        "untracked.md",
        "deleted.ts",
    ] {
        f.write(path, "MARK\n");
    }
    f.git("", &["add", ".gitignore", "src/a.ts", "deleted.ts"]);
    f.git("", &["add", "-f", "tracked.log", "ignored/forced.ts"]);
    f.git("", &["commit", "-qm", "init"]);
    f.remove("deleted.ts");
    f.write("nested/inner.ts", "MARK\n");
    f.git("nested", &["init", "-q"]);

    let listed = f.git("", &["ls-files", "-co", "--exclude-standard"]);
    let mut expected: Vec<String> = listed
        .lines()
        .filter(|path| !path.ends_with('/') && f.path(path).is_file())
        .filter(|path| {
            std::fs::read_to_string(f.path(path)).is_ok_and(|text| text.contains("MARK"))
        })
        .map(str::to_owned)
        .collect();
    expected.sort();
    for kept in [
        "tracked.log",
        "ignored/forced.ts",
        "logs/keep.ts",
        "sub/b.ts",
    ] {
        assert!(expected.contains(&kept.to_owned()), "{expected:?}");
    }
    assert_eq!(files_with(&f, &f.root, "MARK"), expected);
}

#[test]
fn unreadable_git_index_fails_without_folder_fallback() {
    let f = Fixture::new();
    f.git("", &["init", "-q"]);
    f.write("a.ts", "MARK\n");
    f.git("", &["add", "."]);
    f.write(".git/index", "not an index");
    let run = f.lantern(&["index"]);
    assert_eq!(run.code, 1, "{}", run.stderr);
}

#[test]
fn branch_switch_rereads_only_changed_files() {
    let f = Fixture::new();
    f.git("", &["init", "-q"]);
    for name in ["a", "b", "c"] {
        f.write(&format!("{name}.ts"), format!("export const {name} = 1;\n"));
    }
    f.git("", &["add", "."]);
    f.git("", &["commit", "-qm", "main"]);
    f.git("", &["checkout", "-qb", "feature"]);
    f.write(
        "b.ts",
        "export const b = 'changed on the feature branch';\n",
    );
    f.git("", &["commit", "-qam", "feature"]);
    f.git("", &["checkout", "-q", "main"]);
    f.ok(&["index"]);
    f.git("", &["checkout", "-q", "feature"]);
    let switched = f.ok(&["index"]);
    assert_eq!(count(&switched, "changed"), 1);
    assert_eq!(f.ok(&["status"])["branch"], "feature");
}

#[test]
fn status_reports_unborn_and_detached_heads() {
    let f = Fixture::new();
    f.git("", &["init", "-q"]);
    f.write("a.ts", "export const a = 1;\n");
    f.ok(&["index"]);
    let unborn = f.ok(&["status"]);
    assert_eq!(
        (unborn["head"].as_str(), unborn["branch"].as_str()),
        (Some("unborn"), Some("main"))
    );

    f.git("", &["add", "."]);
    f.git("", &["commit", "-qm", "one"]);
    let head = f.git("", &["rev-parse", "HEAD"]);
    f.git("", &["checkout", "-q", "--detach"]);
    f.ok(&["index"]);
    let detached = f.ok(&["status"]);
    assert_eq!(detached["head"], head.trim());
    assert_eq!(detached["branch"], "");
}

#[test]
fn generated_files_leave_symbols_and_rank_last_in_code() {
    let f = Fixture::new();
    f.git("", &["init", "-q"]);
    f.write(".gitattributes", "gen/** linguist-generated\n");
    f.write("gen/api.ts", "export function shared() {} // MARK MARK\n");
    f.write("src/app.ts", "export function shared() {} // MARK\n");
    f.write("package-lock.json", "{\"MARK\": true}\n");

    let found = f.ok(&["symbols", "shared"]);
    assert_eq!(paths(&found), strings(&["src/app.ts"]));
    let all = f.ok(&["symbols", "shared", "--include-generated"]);
    assert_eq!(paths(&all), strings(&["gen/api.ts", "src/app.ts"]));

    let ranked = f.ok(&["code", "MARK", "--case-sensitive"]);
    let order: Vec<&str> = ranked["results"]
        .as_array()
        .expect("results")
        .iter()
        .map(|row| row["path"].as_str().expect("path"))
        .collect();
    assert_eq!(order[0], "src/app.ts", "{order:?}");
    assert_eq!(order.len(), 3);
    assert!(names(&f.ok(&["symbols", "shared", "--path", "gen/**"])).is_empty());
}
