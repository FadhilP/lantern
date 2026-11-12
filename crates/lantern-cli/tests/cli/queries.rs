//! Query behavior: code search against a line-scan oracle, ranking, pagination, freshness, graph.

use std::collections::BTreeSet;

use regex::RegexBuilder;
use serde_json::Value;

use crate::common::{Fixture, count, names, strings};

const CORPUS: &[(&str, &[u8])] = &[
    (
        "src/retry.ts",
        b"export function retry(fn) {\n  return retry(fn); // Retry(\n}\n",
    ),
    (
        "src/mixed.rs",
        b"fn alphaGamma() {}\nfn betaGamma() {}\n  fn indented() {}\nlet x = 123;\n",
    ),
    (
        "src/crlf.py",
        b"def foo():\r\n    return 'foobar'\r\nx = 1\r\n",
    ),
    (
        "docs/notes.md",
        b"Say \"hi\" to fooooobar.\nUnicode: \xc3\x9cn\xc3\xafcode and \xc3\xbcn\xc3\xafcode\nab\n",
    ),
    (
        "data/latin1.txt",
        b"caf\xe9 retry( here\nplain foobar line\n",
    ),
    ("config.yaml", b"retries: 3\nendpoint: x\n"),
    (
        "docs/units.txt",
        "took 5 \u{b5}s, \u{17f}ome heat in \u{212a}elvin\n".as_bytes(),
    ),
];

fn corpus() -> Fixture {
    let f = Fixture::new();
    for (path, content) in CORPUS {
        f.write(path, content);
    }
    f
}

/// What line-oriented matching over the fixture files returns.
fn oracle(pattern: &str, regex: bool, case_sensitive: bool) -> BTreeSet<(String, u64)> {
    let source = if regex {
        pattern.to_owned()
    } else {
        regex::escape(pattern)
    };
    let matcher = RegexBuilder::new(&source)
        .case_insensitive(!case_sensitive)
        .build()
        .expect("oracle regex");
    let mut found = BTreeSet::new();
    for (path, content) in CORPUS {
        let text = String::from_utf8_lossy(content);
        for (index, line) in text.lines().enumerate() {
            if matcher.is_match(line) {
                found.insert(((*path).to_owned(), index as u64 + 1));
            }
        }
    }
    found
}

fn search(
    f: &Fixture,
    pattern: &str,
    regex: bool,
    case_sensitive: bool,
) -> BTreeSet<(String, u64)> {
    let mut args = vec!["code", pattern, "--per-file", "1000", "--limit", "10000"];
    if regex {
        args.push("--regex");
    }
    if case_sensitive {
        args.push("--case-sensitive");
    }
    let document = f.ok(&args);
    let mut found = BTreeSet::new();
    for hit in document["results"].as_array().expect("results") {
        let path = hit["path"].as_str().expect("path");
        let matches = hit["matches"].as_array().expect("matches");
        assert_eq!(count(hit, "match_count"), matches.len() as u64, "{hit}");
        for found_match in matches {
            found.insert((path.to_owned(), count(found_match, "line")));
        }
    }
    found
}

#[test]
fn code_search_finds_exactly_what_a_line_scan_finds() {
    let f = corpus();
    f.ok(&["index"]);
    let cases: &[(&str, bool, bool)] = &[
        ("retry(", false, false),
        ("Retry(", false, true),
        ("ab", false, false),
        ("say \"hi\"", false, false),
        ("fo+bar", true, false),
        ("(alpha|beta)Gamma", true, true),
        (r"^\s*fn ", true, false),
        (r"\d{3}", true, false),
        ("foo$", true, false),
        ("ünïcode", false, false),
        ("café", false, false),
        (r"retr(y|ies)", true, false),
        ("5 \u{3bc}s", false, false),
        ("some heat", false, false),
        ("kelvin", false, false),
    ];
    for &(pattern, regex, case_sensitive) in cases {
        assert_eq!(
            search(&f, pattern, regex, case_sensitive),
            oracle(pattern, regex, case_sensitive),
            "pattern {pattern:?} regex={regex} case_sensitive={case_sensitive}"
        );
    }
}

#[test]
fn code_ranks_more_alternatives_before_more_repeats() {
    let f = Fixture::new();
    f.write("one.ts", "alpha alpha alpha alpha\n");
    f.write("both.ts", "alpha\nbeta\n");
    f.write("few.ts", "beta\n");
    f.write("many.ts", "beta beta beta\n");
    let ranked = |args: &[&str]| -> Vec<String> {
        f.ok(args)["results"]
            .as_array()
            .expect("results")
            .iter()
            .map(|hit| hit["path"].as_str().expect("path").to_owned())
            .collect()
    };
    assert_eq!(
        ranked(&["code", "alpha|beta", "--regex"]),
        strings(&["both.ts", "one.ts", "many.ts", "few.ts"])
    );
    assert_eq!(
        ranked(&["code", "beta"]),
        strings(&["many.ts", "both.ts", "few.ts"])
    );
}

#[test]
fn code_ranks_by_folded_counts_beyond_the_first_batch() {
    let f = Fixture::new();
    for index in 0..100 {
        f.write(&format!("a{index:03}.ts"), "beta\n");
    }
    f.write("z.ts", "beta beta beta\n");
    let top = f.ok(&["code", "BETA", "--limit", "1"]);
    assert_eq!(crate::common::paths(&top), strings(&["z.ts"]));
}

#[test]
fn code_search_follows_edits_and_removals() {
    let f = Fixture::new();
    for index in 0..6 {
        f.write(&format!("f{index}.ts"), format!("needle {index}\n"));
    }
    f.ok(&["index"]);
    let found = |pattern: &str| crate::common::paths(&f.ok(&["code", pattern]));
    for round in 0..3 {
        f.write("f0.ts", format!("haystack {round}\n"));
        f.remove(&format!("f{}.ts", round + 1));
        f.write(&format!("g{round}.ts"), "needle again\n");
        let mut expected: Vec<String> =
            (round + 2..6).map(|index| format!("f{index}.ts")).collect();
        expected.extend((0..=round).map(|index| format!("g{index}.ts")));
        expected.sort();
        assert_eq!(found("needle"), expected, "round {round}");
        assert_eq!(found(&format!("haystack {round}")), strings(&["f0.ts"]));
    }
}

#[test]
fn invalid_patterns_and_escaping_paths_are_usage_errors() {
    let f = corpus();
    f.ok(&["index"]);
    assert_eq!(f.lantern(&["code", "(", "--regex"]).code, 2);
    assert_eq!(
        f.lantern(&["code", "x", "--path", "../elsewhere/**"]).code,
        2
    );
    assert_eq!(f.lantern(&["symbols", " "]).code, 2);
}

#[test]
fn symbol_ranking_puts_exact_then_prefix_then_initials_then_substring() {
    let f = Fixture::new();
    f.write(
        "symbols.ts",
        "export function bWT() {}\nexport function bWTExtra() {}\nexport function buildWorkspaceTree() {}\nexport function xbWT() {}\nexport type DEFAULT_EXPLORER_STATE = {};\nexport function getUserById() {}\nexport function streetreeIndex() {}\nexport function parseTreeNode() {}\n",
    );
    assert_eq!(
        names(&f.ok(&["symbols", "tree"])),
        strings(&["parseTreeNode", "buildWorkspaceTree", "streetreeIndex"]),
        "a match at a word start ranks first, then the shorter name"
    );
    let top = f.ok(&["symbols", "bWT", "--limit", "3"]);
    assert_eq!(
        names(&top),
        strings(&["bWT", "bWTExtra", "buildWorkspaceTree"])
    );
    assert!(top["next_cursor"].is_string());
    assert_eq!(
        names(&f.ok(&["symbols", "des"]))[0],
        "DEFAULT_EXPLORER_STATE"
    );
    assert_eq!(
        names(&f.ok(&["symbols", "gUBI"])),
        strings(&["getUserById"])
    );
}

fn numbered(f: &Fixture, total: usize) {
    for index in 0..total {
        f.write(
            &format!("src/item{index:02}.ts"),
            format!("export function item{index:02}() {{}}\n"),
        );
    }
}

/// Follows `next_cursor` until the end and returns every page's names.
fn all_pages(f: &Fixture, args: &[&str]) -> (Vec<String>, usize) {
    let mut collected = Vec::new();
    let mut pages = 0;
    let mut cursor: Option<String> = None;
    loop {
        let mut page_args: Vec<&str> = args.to_vec();
        if let Some(cursor) = &cursor {
            page_args.extend(["--cursor", cursor.as_str()]);
        }
        let document = f.ok(&page_args);
        collected.extend(names(&document));
        pages += 1;
        match document["next_cursor"].as_str() {
            Some(next) => cursor = Some(next.to_owned()),
            None => return (collected, pages),
        }
    }
}

#[test]
fn pages_concatenate_and_a_changed_index_invalidates_the_cursor() {
    let f = Fixture::new();
    numbered(&f, 25);
    let unpaged = names(&f.ok(&["symbols", "item", "--limit", "100"]));
    assert_eq!(unpaged.len(), 25);
    let (paged, pages) = all_pages(&f, &["symbols", "item", "--limit", "10"]);
    assert_eq!((paged, pages), (unpaged, 3));

    let first = f.ok(&["symbols", "item", "--limit", "10"]);
    let cursor = first["next_cursor"].as_str().expect("cursor").to_owned();
    let (_, rest) = cursor.split_once('.').expect("cursor offset");
    let huge = format!("{}.{rest}", usize::MAX);
    let overflow = f.lantern(&["symbols", "item", "--limit", "10", "--cursor", &huge]);
    assert_eq!(overflow.code, 2, "{}", overflow.stderr);
    f.ok(&["index"]);
    assert_eq!(
        f.lantern(&["symbols", "item", "--limit", "10", "--cursor", &cursor])
            .code,
        0
    );
    f.write("src/item99.ts", "export function item99() {}\n");
    let stale = f.lantern(&["symbols", "item", "--limit", "10", "--cursor", &cursor]);
    assert_eq!(stale.code, 4, "{}", stale.stderr);
}

#[test]
fn rows_cut_by_max_bytes_come_back_on_the_next_page() {
    let f = Fixture::new();
    numbered(&f, 12);
    let unpaged = names(&f.ok(&["symbols", "item", "--limit", "100"]));
    let first = f.ok(&["symbols", "item", "--limit", "20", "--max-bytes", "1500"]);
    assert!(first.to_string().len() <= 1500);
    assert!(names(&first).len() < unpaged.len());
    let (paged, _) = all_pages(
        &f,
        &["symbols", "item", "--limit", "20", "--max-bytes", "1500"],
    );
    assert_eq!(paged, unpaged);
}

#[test]
fn stale_ok_returns_the_committed_state_without_a_walk() {
    let f = Fixture::new();
    f.write("a.ts", "export function old() {}\n");
    assert_eq!(f.lantern(&["symbols", "old", "--stale-ok"]).code, 1);
    let indexed = f.ok(&["index"]);
    f.write("b.ts", "export function fresh() {}\n");
    let stale = f.ok(&["symbols", "fresh", "--stale-ok"]);
    assert_eq!(stale["freshness"], "stale");
    assert_eq!(stale["indexed_at"], indexed["indexed_at"]);
    assert!(names(&stale).is_empty());
    let walked = f.ok(&["symbols", "fresh"]);
    assert_eq!(
        (walked["freshness"].as_str(), names(&walked)),
        (Some("walk"), strings(&["fresh"]))
    );
}

fn graph_fixture() -> Fixture {
    let f = Fixture::new();
    f.write(
        "a.ts",
        "export function root() {\n  mid();\n}\nexport function mid() {\n  leaf();\n  missing();\n}\nexport function leaf() {}\n",
    );
    f.write("b.ts", "export function mid() {\n  other();\n}\n");
    f.write("c.ts", "export function top() {\n  root();\n}\n");
    f
}

/// Edges as `(from name, to name)` pairs.
fn edges(graph: &Value) -> BTreeSet<(String, String)> {
    let nodes = graph["nodes"].as_array().expect("nodes");
    let name = |id: &Value| {
        nodes[usize::try_from(id.as_u64().expect("id")).expect("id fits usize")]["name"]
            .as_str()
            .expect("name")
            .to_owned()
    };
    graph["edges"]
        .as_array()
        .expect("edges")
        .iter()
        .map(|edge| (name(&edge["from"]), name(&edge["to"])))
        .collect()
}

fn pairs(values: &[(&str, &str)]) -> BTreeSet<(String, String)> {
    values
        .iter()
        .map(|(from, to)| ((*from).to_owned(), (*to).to_owned()))
        .collect()
}

#[test]
fn graph_follows_callers_and_callees_by_name() {
    let f = graph_fixture();
    let both = f.ok(&["graph", "mid"]);
    assert_eq!(both["resolution"], "name");
    assert_eq!(both["roots"].as_array().map(Vec::len), Some(2));
    assert_eq!(edges(&both), pairs(&[("root", "mid"), ("mid", "leaf")]));
    let unresolved: Vec<&str> = both["unresolved"]
        .as_array()
        .expect("unresolved")
        .iter()
        .filter_map(Value::as_str)
        .collect();
    assert_eq!(unresolved, ["missing", "other"]);

    let callers = f.ok(&["graph", "leaf", "--direction", "callers", "--depth", "2"]);
    assert_eq!(edges(&callers), pairs(&[("mid", "leaf"), ("root", "mid")]));
    let callees = f.ok(&["graph", "top", "--direction", "callees", "--depth", "2"]);
    assert_eq!(edges(&callees), pairs(&[("top", "root"), ("root", "mid")]));
}

#[test]
fn refs_name_their_enclosing_definition() {
    let f = graph_fixture();
    let refs = f.ok(&["refs", "leaf"]);
    assert_eq!(refs["resolution"], "name");
    let row = &refs["results"][0];
    assert_eq!(
        (row["path"].as_str(), row["line"].as_u64()),
        (Some("a.ts"), Some(5))
    );
    assert_eq!(row["enclosing"]["name"], "mid");
    assert_eq!(row["text"], "leaf();");
}

#[test]
fn outline_nests_members_in_line_order() {
    let f = Fixture::new();
    f.write(
        "pkg/parser.ts",
        "export class Parser {\n  parse() {}\n  reset() {}\n}\nexport function helper() {}\n",
    );
    let outline = f.ok_in(&f.path("pkg"), &["outline", "parser.ts"]);
    // A folder root is the start directory itself.
    assert_eq!(outline["path"], "parser.ts");
    let symbols = outline["results"].as_array().expect("symbols");
    let top: Vec<&str> = symbols
        .iter()
        .filter_map(|node| node["name"].as_str())
        .collect();
    assert_eq!(top, ["Parser", "helper"]);
    let children: Vec<&str> = symbols[0]["children"]
        .as_array()
        .expect("children")
        .iter()
        .filter_map(|node| node["name"].as_str())
        .collect();
    assert_eq!(children, ["parse", "reset"]);
}

#[test]
fn max_bytes_below_one_row_is_a_usage_error() {
    let f = Fixture::new();
    numbered(&f, 3);
    f.ok(&["index"]);
    let run = f.lantern(&["symbols", "item", "--max-bytes", "60"]);
    assert_eq!(run.code, 2, "{}", run.stderr);
    let needed = run
        .stderr
        .split("needs ")
        .nth(1)
        .and_then(|rest| rest.split(' ').next())
        .unwrap_or_else(|| panic!("no size in: {}", run.stderr));
    let fitted = f.lantern(&["symbols", "item", "--max-bytes", needed]);
    assert_eq!(fitted.code, 0, "{}", fitted.stderr);
}

#[test]
fn graph_callers_follow_the_path_filter() {
    let f = graph_fixture();
    f.write("other/d.ts", "export function far() {\n  leaf();\n}\n");
    let all = f.ok(&["graph", "leaf", "--direction", "callers"]);
    assert_eq!(edges(&all), pairs(&[("mid", "leaf"), ("far", "leaf")]));
    let scoped = f.ok(&["graph", "leaf", "--direction", "callers", "--path", "a.ts"]);
    assert_eq!(edges(&scoped), pairs(&[("mid", "leaf")]));
}

#[test]
fn graph_takes_a_name_qualified_by_its_container() {
    let f = Fixture::new();
    f.write(
        "a.ts",
        "class Cart {\n  total() { sum(); }\n}\nclass Order {\n  total() { tax(); }\n}\nfunction sum() {}\nfunction tax() {}\n",
    );
    for query in ["Cart.total", "cart::total"] {
        let graph = f.ok(&["graph", query, "--direction", "callees"]);
        assert_eq!(edges(&graph), pairs(&[("total", "sum")]), "{query}");
    }
}

#[test]
fn symbols_without_a_query_list_definitions_in_path_and_line_order() {
    let f = Fixture::new();
    f.write("src/b.ts", "function second() {}\nclass Third {}\n");
    f.write("src/a.ts", "function first() {}\n");
    f.write("lib/c.ts", "function elsewhere() {}\n");
    assert_eq!(
        names(&f.ok(&["symbols", "--path", "src"])),
        strings(&["first", "second", "Third"])
    );
    assert_eq!(
        names(&f.ok(&["symbols", "--kind", "function", "--limit", "2"])),
        strings(&["elsewhere", "first"])
    );
    let (paged, pages) = all_pages(&f, &["symbols", "--limit", "1"]);
    assert_eq!(
        (paged, pages),
        (strings(&["elsewhere", "first", "second", "Third"]), 4)
    );
}

#[test]
fn an_empty_result_tells_when_no_indexed_file_matches_the_path() {
    let f = Fixture::new();
    f.write("src/a.ts", "function load() {}\n");
    f.write("build/out.ts", "function load() {}\n");
    let note = "No indexed file matches --path";
    let ignored = f.text(&["code", "load", "--path", "build/**"]);
    assert!(ignored.contains(note), "{ignored}");
    let indexed = f.text(&["code", "missing", "--path", "src/**"]);
    assert!(!indexed.contains(note), "{indexed}");
}

#[test]
fn outline_accepts_an_absolute_path() {
    let f = graph_fixture();
    let absolute = f.path("a.ts");
    let outline = f.ok(&["outline", absolute.to_str().expect("UTF-8 path")]);
    assert_eq!(outline["path"], "a.ts");
    let outside = f.lantern(&["outline", f.home.to_str().expect("UTF-8 path")]);
    assert_eq!(outside.code, 2, "{}", outside.stderr);
}

#[test]
fn outline_pages_over_top_level_symbols() {
    let f = Fixture::new();
    f.write(
        "a.ts",
        "export class First {
  inner() {}
}
export function second() {}
export function third() {}
",
    );
    let first = f.ok(&["outline", "a.ts", "--limit", "2"]);
    let top = |document: &Value| -> Vec<String> {
        document["results"]
            .as_array()
            .expect("results")
            .iter()
            .filter_map(|node| node["name"].as_str().map(str::to_owned))
            .collect()
    };
    assert_eq!(top(&first), ["First", "second"]);
    assert_eq!(first["results"][0]["children"][0]["name"], "inner");
    let cursor = first["next_cursor"].as_str().expect("next cursor");
    let rest = f.ok(&["outline", "a.ts", "--limit", "2", "--cursor", cursor]);
    assert_eq!(top(&rest), ["third"]);
    assert!(rest["next_cursor"].is_null());
}

#[test]
fn graph_max_bytes_keeps_whole_nodes_and_their_edges() {
    let f = graph_fixture();
    let full = f.ok(&["graph", "mid"]);
    let size = full.to_string().len();
    let cut = f.ok(&["graph", "mid", "--max-bytes", &(size - 1).to_string()]);
    assert!(cut.to_string().len() < size);
    assert_eq!(cut["truncated"], true);
    let kept = cut["nodes"].as_array().expect("nodes").len();
    for edge in cut["edges"].as_array().expect("edges") {
        assert!(edge["from"].as_u64().expect("from") < kept as u64);
        assert!(edge["to"].as_u64().expect("to") < kept as u64);
    }
}

#[test]
fn symbols_on_one_line_page_without_repeats() {
    let f = Fixture::new();
    let line: String = (0..40)
        .map(|index| format!("function foo{index:02}() {{}} "))
        .collect();
    f.write("one.ts", format!("{line}\n"));
    let (paged, _) = all_pages(&f, &["symbols", "foo", "--limit", "3"]);
    let unique: BTreeSet<&String> = paged.iter().collect();
    assert_eq!((paged.len(), unique.len()), (40, 40));
}

#[test]
fn graph_names_a_shared_definition_by_its_path_in_scope() {
    let f = Fixture::new();
    let source = "export function target() {}\n";
    f.write("a/x.ts", source);
    f.write("src/x.ts", source);
    let graph = f.ok(&["graph", "target", "--path", "src/*"]);
    let nodes = graph["nodes"].as_array().expect("nodes");
    assert_eq!(nodes.len(), 1, "{graph}");
    assert_eq!(nodes[0]["path"], "src/x.ts");
}

#[test]
fn callees_of_one_definition_skip_a_neighbor_on_the_same_line() {
    let f = Fixture::new();
    f.write(
        "one.ts",
        "function a() { x(); } function b() { y(); }\nfunction x() {}\nfunction y() {}\n",
    );
    let graph = f.ok(&["graph", "a", "--direction", "callees"]);
    assert_eq!(edges(&graph), pairs(&[("a", "x")]));
}

#[test]
fn path_with_backslashes_matches_on_windows() {
    if !cfg!(windows) {
        return;
    }
    let f = Fixture::new();
    f.write("src/a.ts", "export const needle = 1;\n");
    let found = f.ok(&["code", "needle", "--path", r"src\a.ts"]);
    assert_eq!(crate::common::paths(&found), strings(&["src/a.ts"]));
}

#[test]
fn repeated_path_globs_match_any_of_them() {
    let f = Fixture::new();
    f.write("src/a.ts", "export const needle = 1;\n");
    f.write("lib/b.ts", "export const needle = 2;\n");
    f.write("other/c.ts", "export const needle = 3;\n");
    let found = f.ok(&[
        "code", "needle", "--path", "src/**", "--path", "lib/**", "--sort", "path",
    ]);
    assert_eq!(
        crate::common::paths(&found),
        strings(&["lib/b.ts", "src/a.ts"])
    );
}

#[test]
fn text_output_groups_by_file_and_ends_a_page_with_its_cursor() {
    let f = Fixture::new();
    f.write(
        "a.ts",
        "function load() {}\nfunction main() {\n  load();\n  load(load());\n}\n",
    );
    f.write("b.ts", "load();\n");
    assert_eq!(
        f.text(&["refs", "load"]),
        "a.ts\n3:3 call in main: load();\n4:3 call in main: load(load());\n4:8 call in main: load(load());\nb.ts\n1:1 call: load();\n"
    );
    assert_eq!(
        f.text(&[
            "code",
            "load",
            "--context",
            "1",
            "--per-file",
            "1",
            "--sort",
            "path"
        ]),
        "a.ts (3 matches, 1 shown)\n1:function load() {}\n2-function main() {\nb.ts\n1:load();\n"
    );
    let page = f.text(&["outline", "a.ts", "--limit", "1"]);
    let (first, more) = page
        .trim_end()
        .split_once('\n')
        .expect("a row and a cursor");
    assert_eq!(first, "1-1 function load");
    assert!(more.starts_with("-- more: --cursor "), "{page}");
}

#[test]
fn graph_follows_calls_and_keeps_test_definitions_out_of_production_callees() {
    let f = Fixture::new();
    f.write(
        "src/app.ts",
        "import { load } from \"./load\";\nexport function main(): Result {\n  load();\n}\n",
    );
    f.write("src/load.ts", "export function load() {}\n");
    f.write(
        "test/app.test.ts",
        "function load() {}\nfunction check() {\n  main();\n  load();\n}\n",
    );
    let main = f.ok(&["graph", "main"]);
    assert_eq!(edges(&main), pairs(&[("check", "main"), ("main", "load")]));
    assert!(main["unresolved"].is_null(), "{main}");
    let loads = |graph: &Value| {
        graph["nodes"]
            .as_array()
            .expect("nodes")
            .iter()
            .filter(|node| node["name"] == "load")
            .count()
    };
    assert_eq!(loads(&main), 1);
    assert_eq!(
        loads(&f.ok(&["graph", "check", "--direction", "callees"])),
        2
    );
    let callers = f.ok(&["graph", "load", "--direction", "callers"]);
    assert_eq!(
        edges(&callers),
        pairs(&[("main", "load"), ("check", "load")])
    );
}

#[test]
fn printed_paths_work_as_arguments_from_a_subdirectory() {
    let f = Fixture::new();
    f.git(".", &["init", "-q"]);
    f.write(
        "src/a.ts",
        "export function needle() {}
",
    );
    let src = f.path("src");
    assert_eq!(
        crate::common::paths(&f.ok_in(&src, &["code", "needle"])),
        strings(&["a.ts"])
    );
    assert_eq!(
        crate::common::paths(&f.ok_in(&src, &["code", "needle", "--path", "a.ts"])),
        strings(&["a.ts"])
    );
    assert_eq!(f.ok_in(&src, &["outline", "a.ts"])["path"], "a.ts");
}

#[test]
fn empty_name_queries_point_to_code_search() {
    let f = Fixture::new();
    f.write(
        "a.ts",
        "const row = { technician_id: 1 };
",
    );
    for command in ["symbols", "refs", "graph"] {
        let text = f.text(&[command, "technician_id"]);
        assert!(
            text.starts_with("no results;") && text.contains("lantern code \"technician_id\""),
            "{command}: {text}"
        );
        let hint = &f.ok(&[command, "technician_id"])["hint"];
        assert!(
            hint.as_str()
                .is_some_and(|hint| hint.contains("lantern code \"technician_id\"")),
            "{command}: {hint}"
        );
    }
    assert!(f.ok(&["code", "absent_text"]).get("hint").is_none());
    assert_eq!(
        f.text(&["code", "absent_text"]),
        "no results
"
    );
}

#[test]
fn empty_outline_points_to_code_search_only_for_a_file_type_without_a_parser() {
    let f = Fixture::new();
    f.write("a.css", ".dock {}\n");
    f.write("b.ts", "// no symbols\n");
    let text = f.text(&["outline", "a.css"]);
    assert!(
        text.starts_with("no results;") && text.contains("lantern code"),
        "{text}"
    );
    assert_eq!(f.text(&["outline", "b.ts"]), "no results\n");
}

#[test]
fn grep_habits_work_or_explain_themselves() {
    let f = Fixture::new();
    f.write(
        "a.ts",
        "run --flag --json
Needle
",
    );
    let flag_line = "a.ts
1:run --flag --json
";
    assert_eq!(f.text(&["code", "-e", "--flag"]), flag_line);
    assert_eq!(f.text(&["code", "-e", "--json"]), flag_line);
    assert_eq!(
        f.text(&["code", "-i", "needle"]),
        f.text(&["code", "needle"])
    );
    let context = f.lantern(&["code", "needle", "-C", "3"]);
    assert_eq!(context.code, 2);
    assert!(context.stderr.contains("--context 3"), "{}", context.stderr);
    assert_eq!(f.lantern(&["code", "needle", "--context", "500"]).code, 0);
}

#[test]
fn a_non_unicode_argument_is_an_error_not_a_panic() {
    let f = Fixture::new();
    let output = std::process::Command::new(env!("CARGO_BIN_EXE_lantern"))
        .arg("-C")
        .arg(non_unicode())
        .arg("status")
        .env("LANTERN_HOME", &f.home)
        .output()
        .expect("run lantern");
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.starts_with("lantern: "), "{stderr}");
}

#[cfg(windows)]
fn non_unicode() -> std::ffi::OsString {
    use std::os::windows::ffi::OsStringExt;
    std::ffi::OsString::from_wide(&[0xD800])
}

#[cfg(unix)]
fn non_unicode() -> std::ffi::OsString {
    use std::os::unix::ffi::OsStringExt;
    std::ffi::OsString::from_vec(vec![0xff])
}

#[test]
fn empty_results_point_from_a_wrong_guess_to_what_exists() {
    let f = Fixture::new();
    f.write(
        "lib/user.ts",
        "export class UserStore {\n  load() { getStats(1); }\n}\n",
    );
    let literal = f.text(&["code", r"getStats\(1"]);
    assert!(literal.contains("add --regex"), "{literal}");
    assert!(
        f.text(&["code", r"getStats\(1", "--regex"])
            .contains("lib/user.ts")
    );
    let qualified = f.text(&["symbols", "Store.load"]);
    assert!(
        qualified.contains("defined in: UserStore") && qualified.contains("UserStore.load"),
        "{qualified}"
    );
    let missing = f.lantern(&["outline", "src/user.ts"]);
    assert_eq!(missing.code, 1);
    assert!(
        missing.stderr.contains("named user.ts: lib/user.ts"),
        "{}",
        missing.stderr
    );
    let folder = f.lantern(&["outline", "lib"]);
    assert_eq!(folder.code, 2);
    assert!(
        folder.stderr.contains("is a folder") && folder.stderr.contains("lib/user.ts"),
        "{}",
        folder.stderr
    );
}

#[test]
fn grep_alternation_and_a_literal_paren_in_a_regex_explain_themselves() {
    let f = Fixture::new();
    f.write("a.ts", "getStats(1);\n");
    let regex = f.text(&["code", r"getStats\|load", "--regex"]);
    assert!(regex.contains(r"`\|` matches a literal `|`"), "{regex}");
    let literal = f.text(&["code", r"getStats\|load("]);
    assert!(
        literal.contains("add --regex") && literal.contains("`a|b`"),
        "{literal}"
    );
    let unclosed = f.lantern(&["code", r"getStats\|load(", "--regex"]);
    assert_eq!(unclosed.code, 2);
    let error: Value = serde_json::from_str(&unclosed.stderr).expect("error document");
    let message = error["error"]["message"].as_str().unwrap_or_default();
    assert!(
        message.contains(r"write `\(`") && message.contains("`a|b`"),
        "{message}"
    );
}

#[test]
fn excluded_path_globs_drop_their_matches() {
    let f = Fixture::new();
    f.write("src/a.ts", "export const needle = 1;\n");
    f.write("src/a.test.ts", "export const needle = 2;\n");
    f.write("lib/b.ts", "export const needle = 3;\n");
    let excluded = f.ok(&["code", "needle", "--path", "!src/**", "--sort", "path"]);
    assert_eq!(crate::common::paths(&excluded), strings(&["lib/b.ts"]));
    let narrowed = f.ok(&[
        "code",
        "needle",
        "--path",
        "src/**",
        "--path",
        "!*.test.ts",
        "--sort",
        "path",
    ]);
    assert_eq!(crate::common::paths(&narrowed), strings(&["src/a.ts"]));
    let folder = f.ok(&["code", "needle", "--path", "src", "--sort", "path"]);
    assert_eq!(
        crate::common::paths(&folder),
        strings(&["src/a.test.ts", "src/a.ts"])
    );
    let not_folder = f.ok(&["code", "needle", "--path", "!src/", "--sort", "path"]);
    assert_eq!(crate::common::paths(&not_folder), strings(&["lib/b.ts"]));
}

#[test]
fn only_a_git_repository_is_indexed_by_a_query_and_the_home_folder_needs_force() {
    let f = Fixture::new();
    f.write("a.ts", "export const needle = 1;\n");
    let plain_folder = f.lantern(&["code", "needle"]);
    assert_eq!(plain_folder.code, 2, "{}", plain_folder.stderr);
    assert!(
        plain_folder.stderr.contains("ask the user"),
        "{}",
        plain_folder.stderr
    );
    let root = f.root.to_str().expect("UTF-8 root");
    let as_home = [("HOME", root), ("USERPROFILE", root)];
    assert_eq!(f.lantern_in(&f.root, &["index"], &as_home).code, 2);
    assert_eq!(
        f.lantern_in(&f.root, &["index", "--force"], &as_home).code,
        0
    );
    let found = f.lantern_in(&f.root, &["code", "needle"], &as_home);
    assert_eq!(crate::common::paths(&found.json), strings(&["a.ts"]));
}

#[test]
fn symbols_take_several_queries_and_queries_take_a_list_of_kinds() {
    let f = Fixture::new();
    f.write(
        "a.ts",
        "export class Shop {}\nexport enum ShopKind { Red }\nexport function shopNow() {}\n",
    );
    assert_eq!(
        f.text(&["symbols", "shopNow", "missing_name"]),
        "== shopNow
a.ts:3-3 function: export function shopNow() {}
== missing_name
no results; for a field, a key, or text, try: lantern code \"missing_name\"
"
    );
    assert_eq!(f.lantern(&["symbols", "Shop", "shopNow"]).code, 2);
    let kinds = f.ok(&["symbols", "shop", "--kind", "enum,function"]);
    let found: BTreeSet<String> = names(&kinds).into_iter().collect();
    assert_eq!(
        found,
        BTreeSet::from(["ShopKind".to_owned(), "shopNow".to_owned()])
    );
    f.write("b.ts", "import { shopNow } from \"./a\";\nshopNow();\n");
    let ref_kinds = |kinds: &str| -> BTreeSet<String> {
        let found = f.ok(&["refs", "shopNow", "--kind", kinds]);
        found["results"]
            .as_array()
            .expect("results")
            .iter()
            .map(|row| row["kind"].as_str().unwrap_or_default().to_owned())
            .collect()
    };
    assert_eq!(ref_kinds("call"), BTreeSet::from(["call".to_owned()]));
    assert_eq!(
        ref_kinds("call,import"),
        BTreeSet::from(["call".to_owned(), "import".to_owned()])
    );
}

#[test]
fn code_context_shows_each_line_once_a_late_match_shows_and_files_list_alone() {
    let f = Fixture::new();
    f.write("a.ts", "one\nneedle\ntwo\nneedle\nthree\n");
    f.write("b.ts", format!("{}needle\n", "x".repeat(600)));
    assert_eq!(
        f.text(&["code", "needle", "--context", "1", "--path", "a.ts"]),
        "a.ts\n1-one\n2:needle\n3-two\n4:needle\n5-three\n"
    );
    assert_eq!(
        f.text(&["code", "needle", "--path", "b.ts"]),
        format!("b.ts\n1:…{}needle\n", "x".repeat(100))
    );
    assert_eq!(
        f.text(&["code", "needle", "-l", "--sort", "path"]),
        "a.ts (2 matches)\nb.ts (1 matches)\n"
    );
}

#[test]
fn outline_takes_several_files_and_a_body_keeps_to_the_exact_name() {
    let f = Fixture::new();
    f.write(
        "a.ts",
        "export function load() {}\nexport function loadAll() {}\n",
    );
    f.write("b.ts", "export class B {}\n");
    assert_eq!(
        f.text(&["outline", "a.ts", "b.ts"]),
        "== a.ts\n1-1 function load\n2-2 function loadAll\n== b.ts\n1-1 class B\n"
    );
    let partial = std::process::Command::new(env!("CARGO_BIN_EXE_lantern"))
        .args(["outline", "a.ts", "missing.ts", "b.ts"])
        .current_dir(&f.root)
        .env("LANTERN_HOME", &f.home)
        .output()
        .expect("run lantern");
    assert_eq!(partial.status.code(), Some(1));
    let printed = String::from_utf8_lossy(&partial.stdout);
    assert!(printed.ends_with("== b.ts\n1-1 class B\n"), "{printed}");
    let exact = f.text(&["symbols", "load", "--body"]);
    assert!(!exact.contains("loadAll"), "{exact}");
    let limited = f.text(&["symbols", "load", "--body", "--limit", "5"]);
    assert!(limited.contains("loadAll"), "{limited}");
}

#[test]
fn a_constructor_is_listed_only_without_its_type() {
    let f = Fixture::new();
    f.write("Svc.java", "class Svc {\n  Svc() {}\n}\n");
    assert_eq!(
        f.text(&["symbols", "Svc"]),
        "Svc.java:1-3 class: class Svc {\n"
    );
    assert_eq!(
        f.text(&["symbols", "Svc", "--kind", "method"]),
        "Svc.java:2-2 method in Svc: Svc() {}\n"
    );
}

#[test]
fn symbol_bodies_are_the_file_lines_from_the_doc_comment_and_page_whole() {
    let f = Fixture::new();
    let source = "/** Doc. */\nfunction stepOne() {\n  return 1;\n}\n\nfunction stepTwo() {\n  return 2;\n}\n";
    f.write("a.ts", source);
    let lines: Vec<&str> = source.lines().collect();
    let found = &f.ok(&["symbols", "stepOne", "--body"])["results"][0];
    assert_eq!(found["line"], 2);
    assert_eq!(
        found["body"],
        serde_json::json!({"line": 1, "text": lines[0..4]})
    );

    let both = f.text(&["symbols", "step", "--body"]);
    assert!(both.contains("1:/** Doc. */\n2:function stepOne"), "{both}");
    assert!(both.contains("7:  return 2;"), "{both}");
    let one_body = f.text(&["symbols", "stepOne", "--body"]).len();
    let budget = (one_body + 60).to_string();
    let paged = f.text(&["symbols", "step", "--body", "--max-bytes", &budget]);
    assert_eq!(paged.matches("  return").count(), 1, "{paged}");
    let cursor = paged
        .split("-- more: --cursor ")
        .nth(1)
        .expect("a cursor")
        .trim();
    let rest = f.text(&[
        "symbols",
        "step",
        "--body",
        "--max-bytes",
        &budget,
        "--cursor",
        cursor,
    ]);
    assert_eq!(
        paged.matches("return 1;").count() + rest.matches("return 1;").count(),
        1,
        "{paged}{rest}"
    );
    assert!(rest.contains("  return"), "{rest}");
}
