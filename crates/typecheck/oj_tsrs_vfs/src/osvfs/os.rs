use std::fs;
use std::io;
use std::sync::LazyLock;
use std::time::SystemTime;

use tsrs_core::{stringutil, tspath};

use crate::internal::{self, Common, IoDirEntry, IoFS, RootFor};
use crate::{Entries, FS, FileInfo, FileMode, FsError};

// FS creates a new FS from the OS file system.
pub fn fs() -> &'static dyn FS {
    &*OS_VFS
}

static OS_VFS: LazyLock<OsFS> = LazyLock::new(|| OsFS {
    common: Common { root_for: OsRoots, is_reparse_point: Some(is_reparse_point) },
});

pub struct OsFS {
    common: Common<OsRoots>,
}

// The OS serves every root: os.DirFS(root) for each.
struct OsRoots;

impl RootFor for OsRoots {
    fn with_root<T>(&self, root: &str, f: impl FnOnce(&dyn IoFS) -> T) -> Option<T> {
        Some(f(&DirFS { dir: root }))
    }
}

// Go computes this right at startup to minimize the chance that executable gets moved or deleted.
static IS_FILE_SYSTEM_CASE_SENSITIVE: LazyLock<bool> = LazyLock::new(|| {
    // win32/win64 are case insensitive platforms
    if cfg!(windows) {
        return false;
    }

    if cfg!(target_arch = "wasm32") {
        // !!! Who knows; this depends on the host implementation.
        return true;
    }

    // As a proxy for case-insensitivity, we check if the current executable exists under a different case.
    // This is not entirely correct, since different OSs can have differing case sensitivity in different paths,
    // but this is largely good enough for our purposes (and what sys.ts used to do with __filename).
    let exe = match std::env::current_exe() {
        Ok(exe) => exe,
        Err(err) => panic!("vfs: failed to get executable path: {err}"),
    };
    let exe = exe.to_string_lossy().into_owned();

    // If the current executable exists under a different case, we must be case-insensitive.
    let swapped = swap_case(&exe);
    if let Err(err) = fs::metadata(&swapped) {
        if err.kind() == io::ErrorKind::NotFound {
            return true;
        }
        panic!("vfs: failed to stat {swapped:?}: {err}");
    }
    false
});

// Convert all lowercase chars to uppercase, and vice-versa
pub fn swap_case(str: &str) -> String {
    let mut out = String::with_capacity(str.len());
    for r in str.chars() {
        let r = r as stringutil::Rune;
        let upper = stringutil::unicode_to_upper(r);
        if upper == r {
            stringutil::push_rune(&mut out, stringutil::unicode_to_lower(r));
        } else {
            stringutil::push_rune(&mut out, upper);
        }
    }
    out
}

impl FS for OsFS {
    fn use_case_sensitive_file_names(&self) -> bool {
        *IS_FILE_SYSTEM_CASE_SENSITIVE
    }

    fn read_file(&self, path: &str) -> Option<String> {
        if tspath::is_network_path(path) {
            return None;
        }
        self.common.read_file(path)
    }

    fn directory_exists(&self, path: &str) -> bool {
        if tspath::is_network_path(path) {
            return false;
        }
        self.common.directory_exists(path)
    }

    fn file_exists(&self, path: &str) -> bool {
        if tspath::is_network_path(path) {
            return false;
        }
        self.common.file_exists(path)
    }

    fn get_accessible_entries(&self, path: &str) -> Entries {
        if tspath::is_network_path(path) {
            return Entries::default();
        }
        self.common.get_accessible_entries(path)
    }

    fn stat(&self, path: &str) -> Option<FileInfo> {
        if tspath::is_network_path(path) {
            return None;
        }
        self.common.stat(path)
    }

    fn realpath(&self, path: &str) -> String {
        os_fs_realpath(path)
    }

    fn write_file(&self, path: &str, content: &str) -> Result<(), String> {
        Self::write_file_ensuring_dir(path, content, false)
    }

    fn append_file(&self, path: &str, content: &str) -> Result<(), String> {
        Self::write_file_ensuring_dir(path, content, true)
    }

    fn remove(&self, path: &str) -> Result<(), String> {
        if tspath::is_network_path(path) {
            return Err("network paths are not supported".to_string());
        }
        // todo: #701 add retry mechanism?
        remove_all(path).map_err(|e| e.to_string())
    }

    fn chtimes(&self, path: &str, a_time: SystemTime, m_time: SystemTime) -> Result<(), String> {
        if tspath::is_network_path(path) {
            return Err("network paths are not supported".to_string());
        }
        let file = fs::File::options()
            .write(true)
            .open(path)
            .or_else(|_| fs::File::open(path))
            .map_err(|e| e.to_string())?;
        file.set_times(fs::FileTimes::new().set_accessed(a_time).set_modified(m_time))
            .map_err(|e| e.to_string())
    }
}

// osvfs/os.go:231
pub fn get_global_typings_cache_location() -> String {
    global_typings_cache_location(user_cache_dir().as_deref())
}

pub fn global_typings_cache_location(cache_dir: Option<&str>) -> String {
    let Some(cache_dir) = cache_dir.filter(|cache_dir| {
        tspath::path_is_absolute(cache_dir)
            && !(tspath::is_url(cache_dir) && cache_dir.contains(['?', '#']))
    }) else {
        return String::new();
    };
    let cache_path = tspath::get_normalized_absolute_path(cache_dir, "");
    let subdir = if cfg!(windows) { "Microsoft/TypeScript" } else { "typescript" };
    tspath::combine_paths(&cache_path, &[subdir, tsrs_core::version_major_minor()])
}

// Go os.UserCacheDir.
fn user_cache_dir() -> Option<String> {
    let non_empty = |name: &str| std::env::var(name).ok().filter(|v| !v.is_empty());
    if cfg!(windows) {
        return non_empty("LocalAppData");
    }
    if cfg!(target_os = "macos") {
        return non_empty("HOME").map(|home| home + "/Library/Caches");
    }
    if let Some(dir) = non_empty("XDG_CACHE_HOME") {
        if !dir.starts_with('/') {
            return None;
        }
        return Some(dir);
    }
    non_empty("HOME").map(|home| home + "/.cache")
}

fn os_fs_realpath(path: &str) -> String {
    let _ = internal::root_length(path); // Assert path is rooted

    let orig = path;
    if tspath::is_network_path(orig) {
        return orig.to_string();
    }
    let path = from_slash(path);
    let Ok(path) = native_realpath(&path) else {
        return orig.to_string();
    };
    let Ok(path) = abs(&path) else {
        return orig.to_string();
    };
    // A mapped drive may resolve to a network share. Keep using its local
    // spelling so later accesses do not open the share directly.
    if tspath::is_network_path(&path) {
        return orig.to_string();
    }
    tspath::normalize_slashes(&path)
}

fn from_slash(path: &str) -> String {
    if cfg!(windows) { path.replace('/', "\\") } else { path.to_string() }
}

// nativepath.Realpath: on Linux the kernel-resolved path (Go reads /proc/self/fd
// after an O_PATH open, which is what realpath(3) computes); elsewhere
// filepath.EvalSymlinks, which resolves symlinks without correcting case.
#[cfg(target_os = "linux")]
fn native_realpath(path: &str) -> io::Result<String> {
    Ok(fs::canonicalize(path)?.to_string_lossy().into_owned())
}

#[cfg(windows)]
pub(crate) fn native_realpath(path: &str) -> io::Result<String> {
    let p = fs::canonicalize(path)?.to_string_lossy().into_owned();
    if let Some(rest) = p.strip_prefix(r"\\?\UNC\") {
        return Ok(format!(r"\\{rest}"));
    }
    if let Some(rest) = p.strip_prefix(r"\\?\") {
        return Ok(rest.to_string());
    }
    Ok(p)
}

#[cfg(not(any(target_os = "linux", windows)))]
fn native_realpath(path: &str) -> io::Result<String> {
    walk_symlinks(path)
}

// filepath.walkSymlinks (the Unix variant, used by filepath.EvalSymlinks).
#[cfg(not(any(target_os = "linux", windows)))]
fn walk_symlinks(path: &str) -> io::Result<String> {
    let is_sep = |b: u8| b == b'/';
    let mut path = path.to_string();
    let mut vol_len = 0usize;

    if vol_len < path.len() && is_sep(path.as_bytes()[vol_len]) {
        vol_len += 1;
    }
    let mut vol = path[..vol_len].to_string();
    let mut dest = vol.clone();
    let mut links_walked = 0;
    let mut start = vol_len;
    while start < path.len() {
        let b = path.as_bytes();
        start += (start..b.len()).take_while(|&index| is_sep(b[index])).count();
        let mut end = start;
        end += (end..b.len()).take_while(|&index| !is_sep(b[index])).count();

        // The next path component is in path[start:end].
        if end == start {
            // No more path components.
            break;
        } else if &path[start..end] == "." {
            // Ignore path component ".".
            start = end;
            continue;
        } else if &path[start..end] == ".." {
            // Back up to previous component if possible.
            // Note that volLen includes any leading slash.

            // Set r to the index of the last slash in dest,
            // after the volume.
            let db = dest.as_bytes();
            let mut r = db.len() as isize - 1;
            while r >= vol_len as isize {
                if is_sep(db[r as usize]) {
                    break;
                }
                r -= 1;
            }
            if r < vol_len as isize || &dest[(r + 1) as usize..] == ".." {
                // Either path has no slashes
                // (it's empty or just "C:")
                // or it ends in a ".." we had to keep.
                // Either way, keep this "..".
                if dest.len() > vol_len {
                    dest.push('/');
                }
                dest.push_str("..");
            } else {
                // Discard everything since the last slash.
                dest.truncate(r as usize);
            }
            start = end;
            continue;
        }

        // Ordinary path component. Add it to result.

        if !dest.is_empty() && !is_sep(dest.as_bytes()[dest.len() - 1]) {
            dest.push('/');
        }

        dest.push_str(&path[start..end]);

        // Resolve symlink.
        let fi = fs::symlink_metadata(&dest)?;

        if !fi.file_type().is_symlink() {
            if !fi.is_dir() && end < path.len() {
                return Err(io::Error::from_raw_os_error(20 /* ENOTDIR */));
            }
            start = end;
            continue;
        }

        // Found symlink.

        links_walked += 1;
        if links_walked > 255 {
            return Err(io::Error::other("EvalSymlinks: too many links"));
        }

        let link = fs::read_link(&dest)?.to_string_lossy().into_owned();

        path = format!("{}{}", link, &path[end..]);

        if !link.is_empty() && is_sep(link.as_bytes()[0]) {
            // Symlink to absolute path.
            dest = link[..1].to_string();
            end = 1;
            vol = link[..1].to_string();
            vol_len = 1;
        } else {
            // Symlink to relative path; replace last
            // path component in dest.
            let db = dest.as_bytes();
            let mut r = db.len() as isize - 1;
            while r >= vol_len as isize {
                if is_sep(db[r as usize]) {
                    break;
                }
                r -= 1;
            }
            if r < vol_len as isize {
                dest.clone_from(&vol);
            } else {
                dest.truncate(r as usize);
            }
            end = 0;
        }
        start = end;
    }
    Ok(clean(&dest))
}

// filepath.Abs
fn abs(path: &str) -> io::Result<String> {
    if std::path::Path::new(path).is_absolute() {
        return Ok(clean(path));
    }
    let wd = std::env::current_dir()?.to_string_lossy().into_owned();
    Ok(clean(&format!("{wd}/{path}")))
}

// path.Clean (Unix semantics; on Windows the realpath result is already clean).
pub fn clean(path: &str) -> String {
    if cfg!(windows) {
        return path.to_string();
    }
    if path.is_empty() {
        return ".".to_string();
    }

    let b = path.as_bytes();
    let rooted = b[0] == b'/';
    let n = b.len();

    let mut out: Vec<u8> = Vec::with_capacity(n);
    let mut r = 0;
    let mut dotdot = 0;
    if rooted {
        out.push(b'/');
        r = 1;
        dotdot = 1;
    }

    while r < n {
        if b[r] == b'/' {
            // empty path element
            r += 1;
        } else if b[r] == b'.' && (r + 1 == n || b[r + 1] == b'/') {
            // . element
            r += 1;
        } else if b[r] == b'.' && b[r + 1] == b'.' && (r + 2 == n || b[r + 2] == b'/') {
            // .. element: remove to last /
            r += 2;
            if out.len() > dotdot {
                // can backtrack
                let mut w = out.len() - 1;
                w -= (dotdot + 1..=w).rev().take_while(|&i| out[i] != b'/').count();
                out.truncate(w);
            } else if !rooted {
                // cannot backtrack, but not rooted, so append .. element.
                if !out.is_empty() {
                    out.push(b'/');
                }
                out.extend_from_slice(b"..");
                dotdot = out.len();
            }
        } else {
            // real path element.
            // add slash if needed
            if (rooted && out.len() != 1) || (!rooted && !out.is_empty()) {
                out.push(b'/');
            }
            // copy element
            let end = r + b[r..n].iter().take_while(|&&byte| byte != b'/').count();
            out.extend_from_slice(&b[r..end]);
            r = end;
        }
    }

    // Turn empty string into "."
    if out.is_empty() {
        return ".".to_string();
    }

    tsrs_core::utf8::into_string(out).unwrap()
}

fn is_reparse_point(path: &str) -> bool {
    if tspath::is_network_path(path) {
        return false;
    }
    match fs::symlink_metadata(from_slash(path)) {
        Ok(info) => info.file_type().is_symlink(),
        Err(_) => false,
    }
}

fn remove_all(path: &str) -> io::Result<()> {
    match fs::symlink_metadata(path) {
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(e),
        Ok(md) if md.is_dir() => fs::remove_dir_all(path),
        Ok(_) => fs::remove_file(path),
    }
}

impl OsFS {
    fn write_file_with_flag(path: &str, content: &str, append: bool) -> io::Result<()> {
        use std::io::Write;
        let mut options = fs::File::options();
        options.write(true).create(true);
        if append {
            options.append(true);
        } else {
            options.truncate(true);
        }
        let mut file = options.open(path)?;
        file.write_all(content.as_bytes())
    }

    fn ensure_directory_exists(directory_path: &str) -> io::Result<()> {
        fs::create_dir_all(directory_path)
    }

    fn write_file_ensuring_dir(path: &str, content: &str, append: bool) -> Result<(), String> {
        let _ = internal::root_length(path); // Assert path is rooted
        if tspath::is_network_path(path) {
            return Err("network paths are not supported".to_string());
        }
        if Self::write_file_with_flag(path, content, append).is_ok() {
            return Ok(());
        }
        Self::ensure_directory_exists(&tspath::get_directory_path(&tspath::normalize_path(path)))
            .map_err(|e| e.to_string())?;
        Self::write_file_with_flag(path, content, append).map_err(|e| e.to_string())
    }
}

// DirFS is os.DirFS: an io/fs view of the OS file system rooted at dir, lent by OsRoots for one operation.
struct DirFS<'a> {
    dir: &'a str,
}

impl DirFS<'_> {
    fn join(&self, name: &str) -> Result<String, FsError> {
        if self.dir.is_empty() {
            return Err(FsError::Other("os: DirFS with empty root".to_string()));
        }
        if !internal::valid_path(name) || name.contains('\0') {
            return Err(FsError::Invalid);
        }
        // One allocation of the final size (module resolution stats and reads thousands of paths).
        let mut full = String::with_capacity(self.dir.len() + 1 + name.len());
        full.push_str(self.dir);
        let last = self.dir.as_bytes()[self.dir.len() - 1];
        if !(last == b'/' || (cfg!(windows) && last == b'\\')) {
            full.push(std::path::MAIN_SEPARATOR);
        }
        if cfg!(windows) {
            full.push_str(&from_slash(name));
        } else {
            full.push_str(name);
        }
        Ok(full)
    }
}

#[expect(
    clippy::needless_pass_by_value,
    reason = "passed as `map_err(io_error)`, which hands over the error by value"
)]
fn io_error(e: io::Error) -> FsError {
    match e.kind() {
        io::ErrorKind::NotFound => FsError::NotExist,
        io::ErrorKind::PermissionDenied => FsError::Permission,
        io::ErrorKind::AlreadyExists => FsError::Exist,
        _ => FsError::Other(e.to_string()),
    }
}

#[expect(
    clippy::filetype_is_file,
    reason = "regular files must remain distinct from the special-file modes handled below"
)]
fn file_type_mode(ft: fs::FileType) -> FileMode {
    if ft.is_dir() {
        return FileMode::Dir;
    }
    if ft.is_symlink() {
        return FileMode::Symlink;
    }
    if ft.is_file() {
        return FileMode::None;
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::FileTypeExt;
        if ft.is_fifo() {
            return FileMode::NamedPipe;
        }
        if ft.is_socket() {
            return FileMode::Socket;
        }
        if ft.is_char_device() {
            return FileMode::Device | FileMode::CharDevice;
        }
        if ft.is_block_device() {
            return FileMode::Device;
        }
    }
    FileMode::Irregular
}

fn metadata_mode(md: &fs::Metadata) -> FileMode {
    let mut mode = file_type_mode(md.file_type());
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        mode |= FileMode::from_bits_retain(md.permissions().mode() & 0o777);
    }
    #[cfg(not(unix))]
    {
        if md.permissions().readonly() {
            mode |= FileMode::from_bits_retain(0o444);
        } else {
            mode |= FileMode::from_bits_retain(0o666);
        }
        if md.is_dir() {
            mode |= FileMode::from_bits_retain(0o111);
        }
    }
    mode
}

// filepathlite.Base
fn base(path: &str) -> String {
    if path.is_empty() {
        return ".".to_string();
    }
    let is_sep = |c: char| c == '/' || (cfg!(windows) && c == '\\');
    let trimmed = path.trim_end_matches(is_sep);
    if trimmed.is_empty() {
        return std::path::MAIN_SEPARATOR.to_string();
    }
    match trimmed.rfind(is_sep) {
        Some(i) => trimmed[i + 1..].to_string(),
        None => trimmed.to_string(),
    }
}

impl IoFS for DirFS<'_> {
    fn stat(&self, name: &str) -> Result<FileInfo, FsError> {
        let full = self.join(name)?;
        let md = fs::metadata(&full).map_err(io_error)?;
        Ok(FileInfo {
            name: base(&full),
            size: md.len() as i64,
            mode: metadata_mode(&md),
            mod_time: md.modified().ok(),
        })
    }

    fn file_type(&self, name: &str) -> Result<FileMode, FsError> {
        let full = self.join(name)?;
        metadata_file_type(&full)
    }

    fn read_dir(&self, name: &str) -> Result<Vec<IoDirEntry>, FsError> {
        let full = self.join(name)?;
        let mut entries = Vec::new();
        for entry in fs::read_dir(&full).map_err(io_error)? {
            let entry = entry.map_err(io_error)?;
            let type_ = match entry.file_type() {
                Ok(ft) => file_type_mode(ft),
                Err(e) if e.kind() == io::ErrorKind::NotFound => continue,
                Err(e) => return Err(io_error(e)),
            };
            entries
                .push(IoDirEntry { name: entry.file_name().to_string_lossy().into_owned(), type_ });
        }
        entries.sort_by(|a, b| a.name.cmp(&b.name));
        Ok(entries)
    }

    fn read_file(&self, name: &str) -> Result<Vec<u8>, FsError> {
        let full = self.join(name)?;
        fs::read(&full).map_err(io_error)
    }
}

#[cfg(target_os = "linux")]
fn metadata_file_type(path: &str) -> Result<FileMode, FsError> {
    use rustix::fs::{AtFlags, CWD, FileType, StatxFlags};

    match rustix::fs::statx(CWD, path, AtFlags::empty(), StatxFlags::TYPE) {
        Ok(statx) => {
            let file_type = FileType::from_raw_mode(statx.stx_mode.into());
            Ok(if file_type.is_dir() { FileMode::Dir } else { FileMode::None })
        }
        Err(rustix::io::Errno::NOSYS) => {
            fs::metadata(path).map(|md| file_type_mode(md.file_type())).map_err(io_error)
        }
        Err(err) => Err(io_error(err.into())),
    }
}

#[cfg(not(target_os = "linux"))]
fn metadata_file_type(path: &str) -> Result<FileMode, FsError> {
    fs::metadata(path).map(|md| file_type_mode(md.file_type())).map_err(io_error)
}
