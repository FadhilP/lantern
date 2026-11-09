//! Gitlinks as member shards, in both indexing orders, plus removal and prune.

use crate::common::{Fixture, count, names, paths, remove_tree, strings};

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
fn parent_reuses_the_shard_of_a_child_indexed_first() {
    let f = aggregate();
    let (a, c) = (f.path("A"), f.path("A/C"));
    assert_eq!(count(&f.ok_in(&c, &["index"]), "changed"), 1);
    let parent = f.ok_in(&a, &["index"]);
    assert_eq!(count(&parent, "members"), 2);
    assert_eq!(count(&parent, "changed"), 1, "only A's own file is read");
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
fn child_uses_the_shard_its_parent_created() {
    let f = aggregate();
    let (a, c) = (f.path("A"), f.path("A/C"));
    f.ok_in(&a, &["index"]);
    assert_eq!(count(&f.ok_in(&c, &["index"]), "changed"), 0);
    assert_eq!(
        paths(&f.ok_in(&c, &["symbols", "childFn"])),
        strings(&["lib.ts"])
    );
}

#[test]
fn child_indexed_through_its_parent_answers_stale_queries() {
    let f = aggregate();
    let (a, c) = (f.path("A"), f.path("A/C"));
    f.ok_in(&a, &["index"]);
    assert_eq!(f.ok_in(&c, &["status"])["indexed"], true);
    assert_eq!(
        paths(&f.ok_in(&c, &["symbols", "childFn", "--stale-ok"])),
        strings(&["lib.ts"])
    );
}

#[test]
fn outline_reaches_a_nested_member_outside_the_start_directory() {
    let f = aggregate();
    f.write("A/src/top.ts", "export function top() {}\n");
    let src = f.path("A/src");
    let outline = f.ok_in(&src, &["outline", "../C/lib.ts"]);
    assert_eq!(outline["path"], "../C/lib.ts");
    assert_eq!(outline["results"][0]["name"], "childFn");
}

#[test]
fn a_member_only_root_includes_its_own_members_in_stale_queries() {
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
    f.ok_in(&f.path("A"), &["index"]);
    assert_eq!(
        paths(&f.ok_in(&f.path("A/C"), &["symbols", "grandFn", "--stale-ok"])),
        strings(&["G/g.ts"])
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
fn parent_lanternignore_on_the_gitlink_drops_the_member() {
    let f = aggregate();
    let a = f.path("A");
    f.write("A/.lanternignore", "C\n");
    assert_eq!(count(&f.ok_in(&a, &["index"]), "members"), 1);
    assert!(names(&f.ok_in(&a, &["symbols", "childFn"])).is_empty());
}
