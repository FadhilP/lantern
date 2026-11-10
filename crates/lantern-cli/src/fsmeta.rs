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
    #[cfg_attr(
        windows,
        expect(dead_code, reason = "a Windows path is a file, a directory, or a link")
    )]
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

/// One directory entry: its name and its stat. Names that are not UTF-8 are left out.
pub(crate) type Listed = (String, io::Result<Stat>);

#[cfg(unix)]
pub(crate) fn list_dir(dir: &Path) -> io::Result<Vec<Listed>> {
    list_dir_by_path(dir)
}

/// Lists `dir` with one `lstat` per file. A directory's stat holds only its kind.
fn list_dir_by_path(dir: &Path) -> io::Result<Vec<Listed>> {
    let mut listed = Vec::new();
    for entry in std::fs::read_dir(dir)? {
        let entry = entry?;
        let Ok(name) = entry.file_name().into_string() else {
            continue;
        };
        let stat = match entry.file_type() {
            Ok(file_type) if file_type.is_dir() => Ok(Stat::other(EntryKind::Dir)),
            _ => lstat(&entry.path()),
        };
        push_found(&mut listed, name, stat);
    }
    Ok(listed)
}

/// Adds an entry unless it vanished since the listing.
fn push_found(listed: &mut Vec<Listed>, name: String, stat: io::Result<Stat>) {
    if !stat
        .as_ref()
        .is_err_and(|error| error.kind() == io::ErrorKind::NotFound)
    {
        listed.push((name, stat));
    }
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
        FILE_ATTRIBUTE_REPARSE_POINT, FILE_FLAG_BACKUP_SEMANTICS, FILE_FLAG_OPEN_REPARSE_POINT,
    };

    // One open per path: the reparse flag opens a link itself, not its target, so the handle
    // tells the kind. A second open per file made the inventory twice as slow.
    let file = OpenOptions::new()
        .access_mode(0)
        .custom_flags(FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_OPEN_REPARSE_POINT)
        .open(path)?;
    let (stat, attributes) = handle_stat(&file)?;
    if attributes & FILE_ATTRIBUTE_REPARSE_POINT != 0 && is_name_surrogate(&file)? {
        return Ok(Stat::other(EntryKind::Link));
    }
    if stat.kind == EntryKind::Dir {
        return Ok(Stat::other(EntryKind::Dir));
    }
    Ok(stat)
}

#[cfg(windows)]
pub(crate) fn fstat(file: &File) -> io::Result<Stat> {
    Ok(handle_stat(file)?.0)
}

/// Lists `dir` with one open: the directory listing carries each file's stat, so files are not
/// opened. Like Git's `core.fscache`, it trusts NTFS directory entries, which can lag behind a
/// file that is open for writing or a hard link changed under another name.
#[cfg(windows)]
pub(crate) fn list_dir(dir: &Path) -> io::Result<Vec<Listed>> {
    use std::fs::OpenOptions;
    use std::os::windows::fs::OpenOptionsExt;
    use std::os::windows::io::AsRawHandle;
    use windows_sys::Win32::Foundation::{
        ERROR_INVALID_LEVEL, ERROR_INVALID_PARAMETER, ERROR_NO_MORE_FILES, ERROR_NOT_SUPPORTED,
    };
    use windows_sys::Win32::Storage::FileSystem::{
        FILE_ATTRIBUTE_DIRECTORY, FILE_ATTRIBUTE_REPARSE_POINT, FILE_FLAG_BACKUP_SEMANTICS,
        FILE_ID_BOTH_DIR_INFO, FILE_LIST_DIRECTORY, FileIdBothDirectoryInfo,
        FileIdBothDirectoryRestartInfo, GetFileInformationByHandleEx,
    };
    const BUFFER_WORDS: usize = 8 * 1024;

    let handle = OpenOptions::new()
        .access_mode(FILE_LIST_DIRECTORY)
        .custom_flags(FILE_FLAG_BACKUP_SEMANTICS)
        .open(dir)?;
    let device = handle_stat(&handle)?.0.device;
    // u64 words keep each entry 8-byte aligned.
    let mut buffer = vec![0u64; BUFFER_WORDS];
    let mut class = FileIdBothDirectoryRestartInfo;
    let mut listed = Vec::new();
    loop {
        // SAFETY: the handle is open, and the buffer is valid for the size passed.
        let ok = unsafe {
            GetFileInformationByHandleEx(
                handle.as_raw_handle(),
                class,
                buffer.as_mut_ptr().cast(),
                (BUFFER_WORDS * size_of::<u64>()) as u32,
            )
        };
        if ok == 0 {
            let error = io::Error::last_os_error();
            let code = error.raw_os_error().map(|code| code as u32);
            if code == Some(ERROR_NO_MORE_FILES) {
                return Ok(listed);
            }
            let unsupported = [
                ERROR_INVALID_LEVEL,
                ERROR_INVALID_PARAMETER,
                ERROR_NOT_SUPPORTED,
            ];
            if listed.is_empty() && code.is_some_and(|code| unsupported.contains(&code)) {
                return list_dir_by_path(dir);
            }
            return Err(error);
        }
        class = FileIdBothDirectoryInfo;
        let mut entry = buffer.as_ptr().cast::<FILE_ID_BOTH_DIR_INFO>();
        loop {
            // SAFETY: the call filled the buffer with a chain of entries; each `NextEntryOffset`
            // stays inside it, and each name has `FileNameLength` bytes after the header.
            let (info, name) = unsafe {
                let name = std::slice::from_raw_parts(
                    (&raw const (*entry).FileName).cast::<u16>(),
                    (*entry).FileNameLength as usize / 2,
                );
                (&*entry, name)
            };
            if let Ok(name) = String::from_utf16(name)
                && name != "."
                && name != ".."
            {
                let attributes = info.FileAttributes;
                let stat = if attributes & FILE_ATTRIBUTE_REPARSE_POINT != 0 || info.FileId == 0 {
                    lstat(&dir.join(&name))
                } else if attributes & FILE_ATTRIBUTE_DIRECTORY != 0 {
                    Ok(Stat::other(EntryKind::Dir))
                } else {
                    Ok(Stat {
                        kind: EntryKind::File,
                        device,
                        inode: info.FileId as u64,
                        size: info.EndOfFile as u64,
                        modified: info.LastWriteTime,
                        changed: info.ChangeTime,
                    })
                };
                push_found(&mut listed, name, stat);
            }
            if info.NextEntryOffset == 0 {
                break;
            }
            // SAFETY: see above.
            entry = unsafe { entry.byte_add(info.NextEntryOffset as usize) };
        }
    }
}

/// True for symlinks and junctions. `std`'s `is_symlink` uses the same test, so junctions count
/// as links.
#[cfg(windows)]
fn is_name_surrogate(file: &File) -> io::Result<bool> {
    use std::mem::{size_of, zeroed};
    use std::os::windows::io::AsRawHandle;
    use windows_sys::Win32::Storage::FileSystem::{
        FILE_ATTRIBUTE_TAG_INFO, FileAttributeTagInfo, GetFileInformationByHandleEx,
    };
    // `IsReparseTagNameSurrogate` is a C macro, so windows-sys has no binding for it.
    const NAME_SURROGATE_BIT: u32 = 0x2000_0000;

    // SAFETY: the struct is plain C data for which all-zero bytes are valid.
    let mut tag: FILE_ATTRIBUTE_TAG_INFO = unsafe { zeroed() };
    // SAFETY: the handle is open for the lifetime of `file`, and `tag` is a valid out-pointer of
    // the size passed for the `FileAttributeTagInfo` class.
    let ok = unsafe {
        GetFileInformationByHandleEx(
            file.as_raw_handle(),
            FileAttributeTagInfo,
            (&raw mut tag).cast(),
            size_of::<FILE_ATTRIBUTE_TAG_INFO>() as u32,
        )
    };
    if ok == 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(tag.ReparseTag & NAME_SURROGATE_BIT != 0)
}

/// The stat of an open handle, and its file attributes.
#[cfg(windows)]
fn handle_stat(file: &File) -> io::Result<(Stat, u32)> {
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
    let stat = Stat {
        kind,
        device: u64::from(info.dwVolumeSerialNumber),
        inode: (u64::from(info.nFileIndexHigh) << 32) | u64::from(info.nFileIndexLow),
        size: (u64::from(info.nFileSizeHigh) << 32) | u64::from(info.nFileSizeLow),
        modified: basic.LastWriteTime,
        changed: basic.ChangeTime,
    };
    Ok((stat, info.dwFileAttributes))
}
