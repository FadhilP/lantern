//! Fixture trees, the real binary, and the real `git` CLI (only to create fixtures).

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use serde_json::Value;
use tempfile::TempDir;

pub struct Fixture {
    _dirs: (TempDir, TempDir),
    pub root: PathBuf,
    pub home: PathBuf,
}

#[derive(Debug)]
pub struct Run {
    pub code: i32,
    pub json: Value,
    pub stderr: String,
}

impl Fixture {
    pub fn new() -> Self {
        let root_dir = tempfile::tempdir().expect("temp root");
        let home_dir = tempfile::tempdir().expect("temp home");
        let root = dunce::canonicalize(root_dir.path()).expect("canonical root");
        let home = home_dir.path().to_path_buf();
        assert!(
            root.ancestors()
                .skip(1)
                .all(|dir| !dir.join(".git").exists()),
            "the temp directory must not be inside a Git repository"
        );
        Self {
            _dirs: (root_dir, home_dir),
            root,
            home,
        }
    }

    pub fn path(&self, relative: &str) -> PathBuf {
        self.root.join(relative)
    }

    pub fn write(&self, relative: &str, content: impl AsRef<[u8]>) {
        let path = self.path(relative);
        fs::create_dir_all(path.parent().expect("file has a parent")).expect("create parent");
        fs::write(&path, content).expect("write fixture file");
    }

    pub fn remove(&self, relative: &str) {
        fs::remove_file(self.path(relative)).expect("remove fixture file");
    }

    pub fn lantern(&self, args: &[&str]) -> Run {
        self.lantern_in(&self.root, args, &[])
    }

    pub fn lantern_in(&self, dir: &Path, args: &[&str], envs: &[(&str, &str)]) -> Run {
        let output = Command::new(env!("CARGO_BIN_EXE_lantern"))
            .args(args)
            .arg("--json")
            .current_dir(dir)
            .env("LANTERN_HOME", &self.home)
            .envs(envs.iter().copied())
            .output()
            .expect("run lantern");
        let stdout = String::from_utf8_lossy(&output.stdout);
        let stderr = String::from_utf8_lossy(&output.stderr).into_owned();
        let json = serde_json::from_str(stdout.trim()).unwrap_or(Value::Null);
        Run {
            code: output.status.code().unwrap_or(-1),
            json,
            stderr,
        }
    }

    /// Runs a command that must succeed and returns its document.
    pub fn ok(&self, args: &[&str]) -> Value {
        self.ok_in(&self.root, args)
    }

    pub fn ok_in(&self, dir: &Path, args: &[&str]) -> Value {
        let run = self.lantern_in(dir, args, &[]);
        assert_eq!(run.code, 0, "lantern {args:?} failed: {}", run.stderr);
        run.json
    }

    /// Runs a command that must succeed and returns its default text output.
    pub fn text(&self, args: &[&str]) -> String {
        let output = Command::new(env!("CARGO_BIN_EXE_lantern"))
            .args(args)
            .current_dir(&self.root)
            .env("LANTERN_HOME", &self.home)
            .output()
            .expect("run lantern");
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(output.status.success(), "lantern {args:?} failed: {stderr}");
        String::from_utf8_lossy(&output.stdout).into_owned()
    }

    pub fn git(&self, relative: &str, args: &[&str]) -> String {
        git(&self.path(relative), args)
    }
}

pub fn git(dir: &Path, args: &[&str]) -> String {
    let output = Command::new("git")
        .args([
            "-c",
            "init.defaultBranch=main",
            "-c",
            "core.autocrlf=false",
            "-c",
            "user.name=Lantern Test",
            "-c",
            "user.email=test@example.com",
            "-c",
            "commit.gpgsign=false",
        ])
        .args(args)
        .current_dir(dir)
        .output()
        .expect("run git");
    assert!(
        output.status.success(),
        "git {args:?} failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8_lossy(&output.stdout).into_owned()
}

/// The sorted `path` of every result row.
pub fn paths(document: &Value) -> Vec<String> {
    let mut paths: Vec<String> = document["results"]
        .as_array()
        .expect("results array")
        .iter()
        .map(|row| row["path"].as_str().expect("path").to_owned())
        .collect();
    paths.sort();
    paths.dedup();
    paths
}

/// Every indexed text file that contains `marker`.
pub fn files_with(fixture: &Fixture, dir: &Path, marker: &str) -> Vec<String> {
    paths(&fixture.ok_in(
        dir,
        &["code", marker, "--case-sensitive", "--limit", "10000"],
    ))
}

pub fn names(document: &Value) -> Vec<String> {
    document["results"]
        .as_array()
        .expect("results array")
        .iter()
        .map(|row| row["name"].as_str().expect("name").to_owned())
        .collect()
}

pub fn strings(values: &[&str]) -> Vec<String> {
    values.iter().map(|value| (*value).to_owned()).collect()
}

/// Removes a tree, clearing read-only flags first (Git objects are read-only on Windows).
pub fn remove_tree(path: &Path) {
    for entry in fs::read_dir(path).expect("read dir").flatten() {
        let child = entry.path();
        if entry.file_type().expect("file type").is_dir() {
            remove_tree(&child);
            continue;
        }
        let mut permissions = fs::metadata(&child).expect("metadata").permissions();
        #[expect(
            clippy::permissions_set_readonly_false,
            reason = "test cleanup of Git objects"
        )]
        permissions.set_readonly(false);
        fs::set_permissions(&child, permissions).expect("make writable");
    }
    fs::remove_dir_all(path).expect("remove tree");
}

pub fn count(document: &Value, field: &str) -> u64 {
    document[field]
        .as_u64()
        .unwrap_or_else(|| panic!("{field} in {document}"))
}
