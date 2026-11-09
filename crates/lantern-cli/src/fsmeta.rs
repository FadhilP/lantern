//! File identity, fingerprints, and stable reads.

use std::fs::File;
use std::io::{self, Read};
use std::path::Path;

use crate::error::{Error, Result};
use crate::limits::TEXT_SNIFF_BYTES;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub(crate) enum EntryKind {
    File,
    Dir,
    Link,
    Other,
}

/// What `lstat` (or `fstat`) reports about one path.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub(crate) struct Stat {
    pub kind: EntryKind,
    /// Device and inode, or volume serial and file index.
    pub device: u64,
    pub inode: u64,
    pub size: u64,
    /// Modification and change times in the platform's unit: nanoseconds since 1970 on Unix,
    /// 100 ns ticks since 1601 on Windows.
    pub modified: i64,
    pub changed: i64,
}

impl Stat {
    #[cfg(windows)]
    fn other(kind: EntryKind) -> Self {
        Self {
            kind,
            device: 0,
            inode: 0,
            size: 0,
            modified: 0,
            changed: 0,
        }
    }

    /// The later of the modification and change times, in milliseconds since the Unix epoch.
    pub fn last_change_ms(&self) -> i64 {
        let latest = self.modified.max(self.changed);
        if cfg!(windows) {
            const TICKS_PER_MS: i64 = 10_000;
            const MS_FROM_1601_TO_1970: i64 = 11_644_473_600_000;
            latest / TICKS_PER_MS - MS_FROM_1601_TO_1970
        } else {
            latest / 1_000_000
        }
    }
}

/// The content of a regular file after a stable read.
#[derive(Debug)]
pub(crate) enum Content {
    Text(Vec<u8>),
    Binary,
    TooLarge,
}

/// Reads `path` and proves it matched `expected` before open, at open, after read, and after close.
/// A temporary error when the file changed or vanished, so the refresh retries.
pub(crate) fn read_stable(path: &Path, expected: &Stat, max_size: u64) -> Result<Content> {
    if expected.size > max_size {
        return Ok(Content::TooLarge);
    }
    let changed = || Error::temporary(format!("{} changed while it was read", path.display()));
    let check = |stat: io::Result<Stat>| match stat {
        Ok(stat) if stat == *expected => Ok(()),
        Ok(_) => Err(changed()),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Err(changed()),
        Err(error) => Err(Error::io(path, &error)),
    };

    let file = match File::open(path) {
        Ok(file) => file,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Err(changed()),
        Err(error) => return Err(Error::io(path, &error)),
    };
    check(fstat(&file))?;
    let mut bytes = Vec::with_capacity(usize::try_from(expected.size).unwrap_or(0) + 1);
    (&file)
        .take(expected.size + 1)
        .read_to_end(&mut bytes)
        .map_err(|error| Error::io(path, &error))?;
    check(fstat(&file))?;
    drop(file);
    check(lstat(path))?;
    if bytes.len() as u64 != expected.size {
        return Err(changed());
    }
    let sniff = &bytes[..bytes.len().min(TEXT_SNIFF_BYTES)];
    if sniff.contains(&0) {
        return Ok(Content::Binary);
    }
    Ok(Content::Text(bytes))
}

#[cfg(unix)]
pub(crate) fn lstat(path: &Path) -> io::Result<Stat> {
    Ok(unix::stat(&std::fs::symlink_metadata(path)?))
}

#[cfg(unix)]
pub(crate) fn fstat(file: &File) -> io::Result<Stat> {
    Ok(unix::stat(&file.metadata()?))
}

#[cfg(unix)]
mod unix {
    use std::fs::Metadata;
    use std::os::unix::fs::MetadataExt;

    use super::{EntryKind, Stat};

    pub fn stat(meta: &Metadata) -> Stat {
        let file_type = meta.file_type();
        let kind = if file_type.is_symlink() {
            EntryKind::Link
        } else if file_type.is_file() {
            EntryKind::File
        } else if file_type.is_dir() {
            EntryKind::Dir
        } else {
            EntryKind::Other
        };
        Stat {
            kind,
            device: meta.dev(),
            inode: meta.ino(),
            size: meta.len(),
            modified: meta.mtime() * 1_000_000_000 + meta.mtime_nsec(),
            changed: meta.ctime() * 1_000_000_000 + meta.ctime_nsec(),
        }
    }
}

#[cfg(windows)]
pub(crate) fn lstat(path: &Path) -> io::Result<Stat> {
    use std::fs::OpenOptions;
    use std::os::windows::fs::OpenOptionsExt;
    use windows_sys::Win32::Storage::FileSystem::{
        FILE_FLAG_BACKUP_SEMANTICS, FILE_FLAG_OPEN_REPARSE_POINT,
    };

    // `is_symlink` covers every name-surrogate reparse point, so junctions count as links.
    let file_type = std::fs::symlink_metadata(path)?.file_type();
    if file_type.is_symlink() {
        return Ok(Stat::other(EntryKind::Link));
    }
    if file_type.is_dir() {
        return Ok(Stat::other(EntryKind::Dir));
    }
    if !file_type.is_file() {
        return Ok(Stat::other(EntryKind::Other));
    }
    let file = OpenOptions::new()
        .access_mode(0)
        .custom_flags(FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_OPEN_REPARSE_POINT)
        .open(path)?;
    fstat(&file)
}

#[cfg(windows)]
pub(crate) fn fstat(file: &File) -> io::Result<Stat> {
    use std::mem::{size_of, zeroed};
    use std::os::windows::io::AsRawHandle;
    use windows_sys::Win32::Storage::FileSystem::{
        BY_HANDLE_FILE_INFORMATION, FILE_ATTRIBUTE_DIRECTORY, FILE_BASIC_INFO, FileBasicInfo,
        GetFileInformationByHandle, GetFileInformationByHandleEx,
    };

    let handle = file.as_raw_handle();
    // SAFETY: both structs are plain C data for which all-zero bytes are valid.
    let (mut info, mut basic): (BY_HANDLE_FILE_INFORMATION, FILE_BASIC_INFO) =
        unsafe { (zeroed(), zeroed()) };
    // SAFETY: `handle` is open for the lifetime of `file`, and `info` is a valid out-pointer.
    if unsafe { GetFileInformationByHandle(handle, &mut info) } == 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: `basic` is a valid out-pointer of the size passed for the `FileBasicInfo` class.
    let ok = unsafe {
        GetFileInformationByHandleEx(
            handle,
            FileBasicInfo,
            (&raw mut basic).cast(),
            size_of::<FILE_BASIC_INFO>() as u32,
        )
    };
    if ok == 0 {
        return Err(io::Error::last_os_error());
    }
    let kind = if info.dwFileAttributes & FILE_ATTRIBUTE_DIRECTORY != 0 {
        EntryKind::Dir
    } else {
        EntryKind::File
    };
    Ok(Stat {
        kind,
        device: u64::from(info.dwVolumeSerialNumber),
        inode: (u64::from(info.nFileIndexHigh) << 32) | u64::from(info.nFileIndexLow),
        size: (u64::from(info.nFileSizeHigh) << 32) | u64::from(info.nFileSizeLow),
        modified: basic.LastWriteTime,
        changed: basic.ChangeTime,
    })
}
