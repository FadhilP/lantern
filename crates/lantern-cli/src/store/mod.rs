//! SQLite storage: one shard per root.

pub(crate) mod refs;
pub(crate) mod shard;
pub(crate) mod trigram;

use std::fs;
use std::path::{Path, PathBuf};
use std::thread;
use std::time::{Duration, Instant};

use rusqlite::{Connection, ErrorCode, OpenFlags};

use crate::error::{Error, Result};

const SCHEMA_VERSION: i64 = 4;

/// Opens a database with the shared pragmas and runs `schema` when `user_version` is 0.
fn open(path: &Path, schema: &str) -> Result<Connection> {
    let flags = OpenFlags::SQLITE_OPEN_READ_WRITE
        | OpenFlags::SQLITE_OPEN_CREATE
        | OpenFlags::SQLITE_OPEN_NO_MUTEX;
    let mut conn = connect(path, flags)?;
    if (1..SCHEMA_VERSION).contains(&user_version(&conn)?) {
        // An index from an older Lantern is a cache; build it again.
        drop(conn);
        remove_database(path)?;
        conn = connect(path, flags)?;
    }
    enable_wal(&conn)?;
    conn.execute_batch("PRAGMA foreign_keys = ON; PRAGMA synchronous = NORMAL;")?;
    if user_version(&conn)? == 0 {
        let tx = conn.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        // Another process may have created the schema while this one waited for the lock.
        if user_version(&tx)? == 0 {
            tx.execute_batch(schema)?;
            tx.execute_batch(&format!("PRAGMA user_version = {SCHEMA_VERSION}"))?;
        }
        tx.commit()?;
    }
    check_version(path, &conn)?;
    Ok(conn)
}

/// Opens an existing database for queries; it never writes. `None` while its schema is not
/// yet committed, or when an older Lantern wrote it.
fn open_read_only(path: &Path) -> Result<Option<Connection>> {
    let flags = OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX;
    let conn = connect(path, flags)?;
    if user_version(&conn)? < SCHEMA_VERSION {
        return Ok(None);
    }
    check_version(path, &conn)?;
    Ok(Some(conn))
}

const BUSY_TIMEOUT: Duration = Duration::from_secs(5);

fn connect(path: &Path, flags: OpenFlags) -> Result<Connection> {
    let conn = Connection::open_with_flags(path, flags)
        .map_err(|error| Error::failure(format!("{}: {error}", path.display())))?;
    conn.busy_timeout(BUSY_TIMEOUT)?;
    Ok(conn)
}

/// Switches to WAL. SQLite does not call the busy handler when two processes switch a new
/// database at once, so this waits itself.
fn enable_wal(conn: &Connection) -> Result<()> {
    let deadline = Instant::now() + BUSY_TIMEOUT;
    loop {
        match conn.query_row("PRAGMA journal_mode = WAL", [], |row| {
            row.get::<_, String>(0)
        }) {
            Ok(_) => return Ok(()),
            Err(error)
                if error.sqlite_error_code() == Some(ErrorCode::DatabaseBusy)
                    && Instant::now() < deadline =>
            {
                thread::sleep(Duration::from_millis(25));
            }
            Err(error) => return Err(error.into()),
        }
    }
}

/// Deletes a database file with its WAL and shared-memory files.
pub(crate) fn remove_database(path: &Path) -> Result<()> {
    for suffix in ["", "-wal", "-shm"] {
        let mut name = path.as_os_str().to_owned();
        name.push(suffix);
        let file = PathBuf::from(name);
        match fs::remove_file(&file) {
            Ok(()) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(Error::io(&file, &error)),
        }
    }
    Ok(())
}

fn check_version(path: &Path, conn: &Connection) -> Result<()> {
    match user_version(conn)? {
        SCHEMA_VERSION => Ok(()),
        other => Err(Error::failure(format!(
            "{}: unsupported schema version {other}; upgrade lantern",
            path.display()
        ))),
    }
}

/// Reads a column stored as a name, rejecting names this binary does not know.
pub(crate) fn column_name<T>(
    value: rusqlite::types::ValueRef<'_>,
    what: &str,
    parse: impl FnOnce(&str) -> Option<T>,
) -> rusqlite::types::FromSqlResult<T> {
    let name = value.as_str()?;
    parse(name).ok_or_else(|| {
        rusqlite::types::FromSqlError::Other(format!("unknown {what} {name:?}").into())
    })
}

/// True when a newer Lantern wrote the shard, so this one must leave it and its files alone.
pub(crate) fn is_newer(path: &Path) -> Result<bool> {
    if !path.exists() {
        return Ok(false);
    }
    Ok(schema_version(path)? > SCHEMA_VERSION)
}

/// 0 while the schema is not committed.
pub(crate) fn schema_version(path: &Path) -> Result<i64> {
    let conn = connect(
        path,
        OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )?;
    user_version(&conn)
}

fn user_version(conn: &Connection) -> Result<i64> {
    Ok(conn.query_row("PRAGMA user_version", [], |row| row.get(0))?)
}
