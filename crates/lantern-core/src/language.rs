//! Language detection and path classification.

use std::fmt;

/// A language that Lantern extracts symbols and references from.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum Language {
    C,
    Cpp,
    CSharp,
    Dart,
    Go,
    Java,
    JavaScript,
    Php,
    Python,
    Ruby,
    Rust,
    TypeScript,
    Tsx,
}

impl Language {
    pub const ALL: [Self; 13] = [
        Self::C,
        Self::Cpp,
        Self::CSharp,
        Self::Dart,
        Self::Go,
        Self::Java,
        Self::JavaScript,
        Self::Php,
        Self::Python,
        Self::Ruby,
        Self::Rust,
        Self::TypeScript,
        Self::Tsx,
    ];

    /// The name stored in the index and accepted by `--language`.
    #[must_use]
    pub fn name(self) -> &'static str {
        match self {
            Self::C => "c",
            Self::Cpp => "cpp",
            Self::CSharp => "csharp",
            Self::Dart => "dart",
            Self::Go => "go",
            Self::Java => "java",
            Self::JavaScript => "javascript",
            Self::Php => "php",
            Self::Python => "python",
            Self::Ruby => "ruby",
            Self::Rust => "rust",
            Self::TypeScript => "typescript",
            Self::Tsx => "tsx",
        }
    }

    #[must_use]
    pub fn from_name(name: &str) -> Option<Self> {
        Self::ALL
            .into_iter()
            .find(|language| language.name() == name)
    }

    /// Detects the language by extension or file name, then by shebang.
    pub(crate) fn detect(path: &str, content: &str) -> Option<Self> {
        let file_name = path.rsplit('/').next().unwrap_or(path);
        Self::from_file_name(file_name).or_else(|| Self::from_shebang(content))
    }

    pub(crate) fn from_file_name(file_name: &str) -> Option<Self> {
        match file_name {
            "Rakefile" | "Gemfile" | "Podfile" | "Vagrantfile" => return Some(Self::Ruby),
            _ => {}
        }
        let (_, extension) = file_name.rsplit_once('.')?;
        let language = match extension.to_ascii_lowercase().as_str() {
            "c" | "h" => Self::C,
            "cc" | "cpp" | "cxx" | "c++" | "hh" | "hpp" | "hxx" | "h++" | "ipp" => Self::Cpp,
            "cs" => Self::CSharp,
            "dart" => Self::Dart,
            "go" => Self::Go,
            "java" => Self::Java,
            "js" | "mjs" | "cjs" | "jsx" => Self::JavaScript,
            "php" | "phtml" => Self::Php,
            "py" | "pyi" | "pyw" => Self::Python,
            "rb" | "rake" | "gemspec" | "ru" => Self::Ruby,
            "rs" => Self::Rust,
            "ts" | "mts" | "cts" => Self::TypeScript,
            "tsx" => Self::Tsx,
            _ => return None,
        };
        Some(language)
    }

    fn from_shebang(content: &str) -> Option<Self> {
        let line = content.lines().next()?.strip_prefix("#!")?;
        let mut words = line.split_whitespace();
        let mut program = words.next()?.rsplit('/').next()?;
        if program == "env" {
            program = words.find(|word| !word.starts_with('-'))?;
        }
        let program = program.trim_end_matches(|c: char| c.is_ascii_digit() || c == '.');
        match program {
            "node" | "nodejs" | "deno" | "bun" => Some(Self::JavaScript),
            "python" => Some(Self::Python),
            "ruby" => Some(Self::Ruby),
            "php" => Some(Self::Php),
            "dart" => Some(Self::Dart),
            _ => None,
        }
    }
}

impl fmt::Display for Language {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.name())
    }
}

/// Bit flags stored in `files.flags`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub(crate) struct FileFlags(pub u8);

impl FileFlags {
    pub const GENERATED: u8 = 1;
    pub const TEST: u8 = 2;

    pub fn classify(path: &str, generated_by_attributes: bool) -> Self {
        let mut bits = 0;
        if generated_by_attributes || is_lockfile(path) {
            bits |= Self::GENERATED;
        }
        if is_test_path(path) {
            bits |= Self::TEST;
        }
        Self(bits)
    }

    pub fn is_generated(self) -> bool {
        self.0 & Self::GENERATED != 0
    }

    pub fn is_test(self) -> bool {
        self.0 & Self::TEST != 0
    }
}

const LOCKFILES: &[&str] = &[
    "package-lock.json",
    "npm-shrinkwrap.json",
    "yarn.lock",
    "pnpm-lock.yaml",
    "bun.lock",
    "Cargo.lock",
    "poetry.lock",
    "Pipfile.lock",
    "uv.lock",
    "go.sum",
    "composer.lock",
    "Gemfile.lock",
    "Podfile.lock",
    "pubspec.lock",
    "mix.lock",
    "flake.lock",
    "packages.lock.json",
];

fn is_lockfile(path: &str) -> bool {
    let file_name = path.rsplit('/').next().unwrap_or(path);
    LOCKFILES.contains(&file_name)
}

fn is_test_path(path: &str) -> bool {
    let mut components: Vec<&str> = path.split('/').collect();
    let file_name = components.pop().unwrap_or_default();
    let in_test_dir = components
        .iter()
        .any(|dir| matches!(*dir, "test" | "tests" | "__tests__"));
    let stem = file_name.split('.').next().unwrap_or_default();
    in_test_dir
        || stem.ends_with("_test")
        || file_name.contains(".test.")
        || file_name.contains(".spec.")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detects_by_extension_then_shebang() {
        assert_eq!(Language::detect("src/a.TSX", ""), Some(Language::Tsx));
        assert_eq!(
            Language::detect("bin/tool", "#!/usr/bin/env python3\n"),
            Some(Language::Python)
        );
        assert_eq!(
            Language::detect("bin/run", "#!/usr/bin/node\n"),
            Some(Language::JavaScript)
        );
        assert_eq!(
            Language::detect("README.md", "#!/usr/bin/env python\n"),
            Some(Language::Python)
        );
        assert_eq!(Language::detect("lib/a.dart", ""), Some(Language::Dart));
        assert_eq!(
            Language::detect("src/a.jsx", ""),
            Some(Language::JavaScript)
        );
        assert_eq!(
            Language::detect("bin/tool", "#!/usr/bin/env dart\n"),
            Some(Language::Dart)
        );
        assert_eq!(Language::detect("notes.txt", "plain"), None);
    }

    #[test]
    fn classifies_tests_and_lockfiles() {
        assert!(FileFlags::classify("pkg/__tests__/a.js", false).is_test());
        assert!(FileFlags::classify("a/b_test.go", false).is_test());
        assert!(FileFlags::classify("a/b.spec.ts", false).is_test());
        assert!(!FileFlags::classify("a/contest.rs", false).is_test());
        assert!(FileFlags::classify("web/yarn.lock", false).is_generated());
        assert!(!FileFlags::classify("web/yarn.lockx", false).is_generated());
    }
}
