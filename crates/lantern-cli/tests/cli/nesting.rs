//! Checked-out gitlinks as part of the parent's index, plus removal and prune.

use crate::common::{Fixture, names, paths, remove_tree, strings};

/// An aggregate repository `A` with a committed gitlink to child repository `A/C`.
fn aggregate() -> Fixture {
    let f = Fixture::new();
    f.write("A/C/lib.ts", "export function childFn() {}\n");
    f.git("A/C", &["init", "-q"]);
    f.git("A/C", &["add", "."]);
    f.git("A/C", &["commit", "-qm", "child"]);
    f.write("A/main.ts", "export function mainFn() { childFn(); }\n");
    f.git("A", &["init", "-q"]);
    f.git("A", &["add", "main.ts", "C"]);
    f.git("A", &["commit", "-qm", "aggregate"]);
    f
}

#[test]
fn parent_indexes_the_files_of_a_gitlink_with_its_prefix() {
    let f = aggregate();
    let a = f.path("A");
    assert_eq!(
        paths(&f.ok_in(&a, &["symbols", "childFn"])),
        strings(&["C/lib.ts"])
    );

    let graph = f.ok_in(&a, &["graph", "childFn", "--direction", "callers"]);
    let callers: Vec<&str> = graph["nodes"]
        .as_array()
        .expect("nodes")
        .iter()
        .map(|node| node["name"].as_str().expect("name"))
        .collect();
    assert!(callers.contains(&"mainFn"), "{graph}");
}

#[test]
fn a_gitlink_resolves_to_its_parent_scoped_to_itself() {
    let f = aggregate();
    let (a, c) = (f.path("A"), f.path("A/C"));
    assert_eq!(
        paths(&f.ok_in(&c, &["symbols", "childFn"])),
        strings(&["lib.ts"])
    );
    assert!(names(&f.ok_in(&c, &["symbols", "mainFn"])).is_empty());
    assert_eq!(
        f.ok_in(&c, &["status"])["root"],
        f.ok_in(&a, &["status"])["root"]
    );
    let shards = std::fs::read_dir(f.home.join("roots"))
        .expect("roots dir")
        .flatten()
        .filter(|entry| entry.path().extension().is_some_and(|ext| ext == "sqlite"))
        .count();
    assert_eq!(shards, 1, "the gitlink has no index of its own");
}

#[test]
fn outline_reaches_a_gitlink_outside_the_start_directory() {
    let f = aggregate();
    f.write("A/src/top.ts", "export function top() {}\n");
    let src = f.path("A/src");
    let outline = f.ok_in(&src, &["outline", "../C/lib.ts"]);
    assert_eq!(outline["path"], "../C/lib.ts");
    assert_eq!(outline["results"][0]["name"], "childFn");
}

#[test]
fn a_gitlink_inside_a_gitlink_is_indexed() {
    let f = Fixture::new();
    f.write("A/C/G/g.ts", "export function grandFn() {}\n");
    f.git("A/C/G", &["init", "-q"]);
    f.git("A/C/G", &["add", "."]);
    f.git("A/C/G", &["commit", "-qm", "grandchild"]);
    f.write("A/C/lib.ts", "export function childFn() {}\n");
    f.git("A/C", &["init", "-q"]);
    f.git("A/C", &["add", "lib.ts", "G"]);
    f.git("A/C", &["commit", "-qm", "child"]);
    f.write("A/main.ts", "export function mainFn() {}\n");
    f.git("A", &["init", "-q"]);
    f.git("A", &["add", "main.ts", "C"]);
    f.git("A", &["commit", "-qm", "aggregate"]);
    assert_eq!(
        paths(&f.ok_in(&f.path("A"), &["symbols", "grandFn"])),
        strings(&["C/G/g.ts"])
    );
}

#[test]
fn removed_gitlink_and_prune() {
    let f = aggregate();
    let (a, c) = (f.path("A"), f.path("A/C"));
    f.ok_in(&a, &["index"]);
    f.git("A", &["rm", "-q", "--cached", "C"]);
    assert!(names(&f.ok_in(&a, &["symbols", "childFn"])).is_empty());
    assert_eq!(
        names(&f.ok_in(&c, &["symbols", "childFn"])),
        strings(&["childFn"])
    );

    remove_tree(&c);
    let pruned = f.ok_in(&a, &["prune"]);
    assert_eq!(
        pruned["removed_roots"].as_array().map(Vec::len),
        Some(1),
        "{pruned}"
    );
    assert_eq!(
        names(&f.ok_in(&a, &["symbols", "mainFn"])),
        strings(&["mainFn"])
    );
}

#[test]
fn child_lanternignore_applies_to_the_child_and_a_parent_rule_drops_the_gitlink() {
    let f = aggregate();
    let a = f.path("A");
    f.write("A/C/extra.ts", "export function extraFn() {}\n");
    f.write("A/C/.lanternignore", "extra.ts\n");
    assert!(names(&f.ok_in(&a, &["symbols", "extraFn"])).is_empty());
    assert_eq!(
        names(&f.ok_in(&a, &["symbols", "childFn"])),
        strings(&["childFn"])
    );

    f.write("A/.lanternignore", "C\n");
    assert!(names(&f.ok_in(&a, &["symbols", "childFn"])).is_empty());
}
