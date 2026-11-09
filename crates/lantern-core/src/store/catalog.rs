//! The catalog: physical roots, workspaces, and the member shards of each workspace.

use std::path::Path;

use rusqlite::{Connection, OptionalExtension, params};

use crate::error::Result;
use crate::paths;
use crate::source::SourceKind;

const SCHEMA: &str = r"
CREATE TABLE roots (
  root_key TEXT PRIMARY KEY,
  root TEXT NOT NULL,
  shard TEXT NOT NULL,
  source TEXT NOT NULL,
  last_used_at INTEGER
) WITHOUT ROWID;
CREATE TABLE workspaces (
  root_key TEXT PRIMARY KEY,
  root TEXT NOT NULL,
  membership TEXT,
  indexed_at INTEGER
) WITHOUT ROWID;
CREATE TABLE workspace_members (
  workspace_key TEXT NOT NULL REFERENCES workspaces ON DELETE CASCADE,
  prefix TEXT NOT NULL,
  root_key TEXT NOT NULL REFERENCES roots ON DELETE CASCADE,
  PRIMARY KEY (workspace_key, prefix)
) WITHOUT ROWID;
CREATE INDEX workspace_members_root ON workspace_members(root_key);
";

#[derive(Debug)]
pub(crate) struct Catalog {
    conn: Connection,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct RootRow {
    pub root_key: String,
    pub root: String,
    pub shard: String,
    pub source: SourceKind,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Member {
    pub prefix: String,
    pub root: RootRow,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Workspace {
    pub root: String,
    pub indexed_at: Option<i64>,
    pub members: Vec<Member>,
}

impl Catalog {
    pub fn open(path: &Path) -> Result<Self> {
        Ok(Self {
            conn: super::open(path, SCHEMA)?,
        })
    }

    /// Opens the catalog for a query that does not refresh; `None` before the first index.
    pub fn open_read_only(path: &Path) -> Result<Option<Self>> {
        if !path.exists() {
            return Ok(None);
        }
        Ok(super::open_read_only(path)?.map(|conn| Self { conn }))
    }

    pub fn upsert_root(&self, row: &RootRow, now: i64) -> Result<()> {
        self.conn.execute(
            "INSERT INTO roots (root_key, root, shard, source, last_used_at) VALUES (?1, ?2, ?3, ?4, ?5)
             ON CONFLICT (root_key) DO UPDATE SET root = excluded.root, shard = excluded.shard,
               source = excluded.source, last_used_at = excluded.last_used_at",
            params![row.root_key, row.root, row.shard, row.source.as_str(), now],
        )?;
        Ok(())
    }

    pub fn root(&self, root_key: &str) -> Result<Option<RootRow>> {
        Ok(self
            .conn
            .query_row(
                "SELECT root_key, root, shard, source FROM roots WHERE root_key = ?1",
                [root_key],
                root_row,
            )
            .optional()?)
    }

    pub fn roots(&self) -> Result<Vec<RootRow>> {
        let mut statement = self
            .conn
            .prepare("SELECT root_key, root, shard, source FROM roots ORDER BY root_key")?;
        let rows = statement.query_map([], root_row)?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    /// Replaces a workspace's members in one transaction, so readers see the old or the new list.
    pub fn set_workspace(
        &mut self,
        root_key: &str,
        root: &str,
        members: &[(String, String)],
        indexed_at: i64,
    ) -> Result<()> {
        let mut sorted: Vec<&(String, String)> = members.iter().collect();
        sorted.sort();
        let mut hasher = blake3::Hasher::new();
        for (prefix, key) in &sorted {
            hasher.update(prefix.as_bytes());
            hasher.update(&[0]);
            hasher.update(key.as_bytes());
            hasher.update(&[0]);
        }
        let membership = hasher.finalize().to_hex().to_string();
        let tx = self
            .conn
            .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        tx.execute(
            "INSERT INTO workspaces (root_key, root, membership, indexed_at) VALUES (?1, ?2, ?3, ?4)
             ON CONFLICT (root_key) DO UPDATE SET root = excluded.root,
               membership = excluded.membership, indexed_at = excluded.indexed_at",
            params![root_key, root, membership, indexed_at],
        )?;
        tx.execute(
            "DELETE FROM workspace_members WHERE workspace_key = ?1",
            [root_key],
        )?;
        for (prefix, member_key) in sorted {
            tx.execute(
                "INSERT INTO workspace_members (workspace_key, prefix, root_key) VALUES (?1, ?2, ?3)",
                params![root_key, prefix, member_key],
            )?;
        }
        tx.commit()?;
        Ok(())
    }

    /// The published workspace of a root, or for a root indexed only as a member of another
    /// workspace, that root with the members below it.
    pub fn workspace(&self, root_key: &str) -> Result<Option<Workspace>> {
        let head: Option<(String, Option<i64>)> = self
            .conn
            .query_row(
                "SELECT root, indexed_at FROM workspaces WHERE root_key = ?1",
                [root_key],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .optional()?;
        let Some((root, indexed_at)) = head else {
            return self.member_workspace(root_key);
        };
        Ok(Some(Workspace {
            root,
            indexed_at,
            members: self.members(root_key)?,
        }))
    }

    fn member_workspace(&self, root_key: &str) -> Result<Option<Workspace>> {
        let Some(root) = self.root(root_key)? else {
            return Ok(None);
        };
        let containing: Option<(String, String)> = self
            .conn
            .query_row(
                "SELECT workspace_key, prefix FROM workspace_members
                 WHERE root_key = ?1 ORDER BY workspace_key LIMIT 1",
                [root_key],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .optional()?;
        let members = match containing {
            Some((workspace_key, prefix)) => self
                .members(&workspace_key)?
                .into_iter()
                .filter_map(|member| {
                    let inner = paths::strip_dir(&member.prefix, &prefix)?.to_owned();
                    Some(Member {
                        prefix: inner,
                        root: member.root,
                    })
                })
                .collect(),
            None => vec![Member {
                prefix: String::new(),
                root: root.clone(),
            }],
        };
        Ok(Some(Workspace {
            root: root.root,
            indexed_at: None,
            members,
        }))
    }

    fn members(&self, workspace_key: &str) -> Result<Vec<Member>> {
        let mut statement = self.conn.prepare(
            "SELECT m.prefix, r.root_key, r.root, r.shard, r.source
             FROM workspace_members m JOIN roots r ON r.root_key = m.root_key
             WHERE m.workspace_key = ?1 ORDER BY m.prefix",
        )?;
        let rows = statement.query_map([workspace_key], |row| {
            Ok(Member {
                prefix: row.get(0)?,
                root: root_row_at(row, 1)?,
            })
        })?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    pub fn delete_root(&self, root_key: &str) -> Result<()> {
        self.conn
            .execute("DELETE FROM workspaces WHERE root_key = ?1", [root_key])?;
        self.conn
            .execute("DELETE FROM roots WHERE root_key = ?1", [root_key])?;
        Ok(())
    }
}

fn root_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<RootRow> {
    root_row_at(row, 0)
}

fn root_row_at(row: &rusqlite::Row<'_>, at: usize) -> rusqlite::Result<RootRow> {
    Ok(RootRow {
        root_key: row.get(at)?,
        root: row.get(at + 1)?,
        shard: row.get(at + 2)?,
        source: row.get(at + 3)?,
    })
}
