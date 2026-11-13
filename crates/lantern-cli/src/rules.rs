//! Directory-scoped `.gitignore`-syntax rules, used for `.lanternignore` in the Git source and for
//! the generated flag from `.gitattributes`.

use std::collections::HashMap;

use ignore::gitignore::{Gitignore, GitignoreBuilder};

use crate::error::{Error, Result};
use crate::paths;

#[derive(Debug, Default)]
pub(crate) struct ScopedRules {
    /// Deepest directory first, so the first scope with a match decides.
    scopes: Vec<(String, Gitignore)>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Verdict {
    Exclude,
    Keep,
    Unmatched,
}

impl ScopedRules {
    /// Adds the rules of one file in directory `dir` (relative to the root, `""` for the root).
    pub fn add(&mut self, dir: &str, file: &str, content: &str) -> Result<()> {
        let mut builder = GitignoreBuilder::new(".");
        for line in content.lines() {
            builder
                .add_line(None, line)
                .map_err(|error| Error::failure(format!("{file}: {error}")))?;
        }
        let matcher = builder
            .build()
            .map_err(|error| Error::failure(format!("{file}: {error}")))?;
        self.scopes.push((dir.to_owned(), matcher));
        self.scopes
            .sort_by_key(|(dir, _)| std::cmp::Reverse(depth(dir)));
        Ok(())
    }

    pub fn is_empty(&self) -> bool {
        self.scopes.is_empty()
    }

    fn verdict(&self, path: &str, is_dir: bool) -> Verdict {
        for (dir, matcher) in &self.scopes {
            let Some(relative) = paths::strip_dir(path, dir) else {
                continue;
            };
            if relative.is_empty() {
                continue;
            }
            let matched = matcher.matched(relative, is_dir);
            if matched.is_ignore() {
                return Verdict::Exclude;
            }
            if matched.is_whitelist() {
                return Verdict::Keep;
            }
        }
        Verdict::Unmatched
    }

    /// True when the rules match the file `path` itself; parents are not consulted.
    pub fn matches_file(&self, path: &str) -> bool {
        self.verdict(path, false) == Verdict::Exclude
    }
}

/// Answers `.gitignore`-style exclusion, where an excluded directory hides everything below it.
#[derive(Debug)]
pub(crate) struct Exclusions<'rules> {
    rules: &'rules ScopedRules,
    dirs: HashMap<String, bool>,
}

impl<'rules> Exclusions<'rules> {
    pub fn new(rules: &'rules ScopedRules) -> Self {
        Self {
            rules,
            dirs: HashMap::new(),
        }
    }

    pub fn excludes(&mut self, path: &str, is_dir: bool) -> bool {
        if self.rules.is_empty() {
            return false;
        }
        if let Some((parent, _)) = path.rsplit_once('/')
            && self.excludes_dir(parent)
        {
            return true;
        }
        self.rules.verdict(path, is_dir) == Verdict::Exclude
    }

    fn excludes_dir(&mut self, dir: &str) -> bool {
        if let Some(&known) = self.dirs.get(dir) {
            return known;
        }
        let excluded = self.excludes(dir, true);
        self.dirs.insert(dir.to_owned(), excluded);
        excluded
    }
}

fn depth(dir: &str) -> usize {
    if dir.is_empty() {
        0
    } else {
        dir.matches('/').count() + 1
    }
}

/// Turns `.gitattributes` lines into rules that match files marked generated or vendored.
pub(crate) fn generated_rules(attributes: &str) -> String {
    let mut out = String::new();
    for line in attributes.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') || line.starts_with('"') {
            continue;
        }
        let mut words = line.split_whitespace();
        let Some(pattern) = words.next() else {
            continue;
        };
        match words.filter_map(linguist_state).next_back() {
            Some(true) => out.push_str(pattern),
            Some(false) => {
                out.push('!');
                out.push_str(pattern);
            }
            None => continue,
        }
        out.push('\n');
    }
    out
}

fn linguist_state(word: &str) -> Option<bool> {
    let (negated, word) = match word.strip_prefix(['-', '!']) {
        Some(rest) => (true, rest),
        None => (false, word),
    };
    let (name, value) = word.split_once('=').unwrap_or((word, "true"));
    if !matches!(name, "linguist-generated" | "linguist-vendored") {
        return None;
    }
    Some(!negated && value != "false")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rules(files: &[(&str, &str)]) -> ScopedRules {
        let mut rules = ScopedRules::default();
        for (dir, content) in files {
            rules.add(dir, "rules", content).expect("valid rules");
        }
        rules
    }

    #[test]
    fn excluded_parent_hides_children_and_deeper_scope_wins() {
        let rules = rules(&[
            ("", "build/\n!build/keep.txt\n*.log\n"),
            ("sub", "!*.log\n"),
        ]);
        let mut exclusions = Exclusions::new(&rules);
        assert!(exclusions.excludes("build/keep.txt", false));
        assert!(exclusions.excludes("a.log", false));
        assert!(!exclusions.excludes("sub/a.log", false));
        assert!(!exclusions.excludes("src/main.rs", false));
    }

    #[test]
    fn scope_applies_only_below_its_directory() {
        let rules = rules(&[("pkg", "secret.txt\n")]);
        let mut exclusions = Exclusions::new(&rules);
        assert!(exclusions.excludes("pkg/secret.txt", false));
        assert!(!exclusions.excludes("secret.txt", false));
    }

    #[test]
    fn attributes_become_generated_rules() {
        let text = "*.pb.go linguist-generated\ngen/** linguist-generated=true\ngen/keep.go -linguist-generated\n*.md text\n";
        let rules = rules(&[("", &generated_rules(text))]);
        assert!(rules.matches_file("api/x.pb.go"));
        assert!(rules.matches_file("gen/a.go"));
        assert!(!rules.matches_file("gen/keep.go"));
        assert!(!rules.matches_file("README.md"));
    }
}
