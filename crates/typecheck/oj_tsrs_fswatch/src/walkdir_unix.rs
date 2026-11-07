// walkdir_unix.go. Go reads entries with getdents/getdirentries and opens children with openat(O_NOFOLLOW); this
// uses `std::fs::read_dir`, which is readdir(3) over the same directory stream: the same order, and
// `DirEntry::file_type` is d_type with an lstat fallback for DT_UNKNOWN (Go's `ent.typ == DT_UNKNOWN` branch).
// Symlinks are never followed (a symlinked directory is reported as a non-directory, like DT_LNK in Go).
// walkdir.go's `walkDirGeneric` (the Windows / other-OS implementation) is not ported.

use std::fs;
use std::io::ErrorKind;

use crate::watcher::Error;

// walkdir_unix.go:21
// walkDir walks dir, optionally recursively, invoking fn for each entry.
pub fn walk_dir(
    dir: &str,
    recursive: bool,
    f: &mut dyn FnMut(&str, bool) -> Result<(), Error>,
) -> Result<(), Error> {
    let md = fs::symlink_metadata(dir).map_err(Error::from_io)?;
    if md.file_type().is_symlink() {
        // open(O_NOFOLLOW) on a symlink.
        return Err(Error::from_errno(libc::ELOOP));
    }
    if !md.is_dir() {
        return Err(Error::from_errno(libc::ENOTDIR));
    }
    let rd = fs::read_dir(dir).map_err(Error::from_io)?;
    iterate_dir(rd, dir, recursive, f)
}

// walkdir_unix.go:44
// iterateDir reads the entries of an open directory, invokes fn for the dir and each entry, and recurses into
// subdirectories.
fn iterate_dir(
    rd: fs::ReadDir,
    dirname: &str,
    recursive: bool,
    f: &mut dyn FnMut(&str, bool) -> Result<(), Error>,
) -> Result<(), Error> {
    f(dirname, true)?;
    let mut entries = Vec::new();
    entries = rd.into_iter().try_fold(entries, |mut entries, ent| {
        let ent = ent.map_err(Error::from_io)?;
        let name = ent.file_name().to_string_lossy().into_owned();
        let is_dir = match ent.file_type() {
            Ok(t) => t.is_dir(),
            // Go: lstat failed for a DT_UNKNOWN entry -> skip it.
            Err(_) => return Ok(entries),
        };
        entries.push((name, is_dir));
        Ok::<_, Error>(entries)
    })?;

    for (name, is_dir) in entries {
        let full_path = format!("{dirname}/{name}");
        if !is_dir {
            f(&full_path, false)?;
            continue;
        }
        if !recursive {
            f(&full_path, true)?;
            continue;
        }
        let child = match fs::read_dir(&full_path) {
            Ok(child) => child,
            Err(err) => {
                if matches!(err.kind(), ErrorKind::PermissionDenied | ErrorKind::NotFound)
                    || err.raw_os_error() == Some(libc::ENOTDIR)
                {
                    continue;
                }
                return Err(Error::from_io(err));
            }
        };
        iterate_dir(child, &full_path, recursive, f)?;
    }
    Ok(())
}
