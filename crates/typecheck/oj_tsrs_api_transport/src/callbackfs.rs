// Port of tsc/internal/api/callbackfs.go: a vfs wrapper that delegates selected operations to the
// API client through server-to-client calls, with the client's serverFS sentinels
// (useOS / identity / fakeStat / noop / error) honored exactly as the pinned server does.
//
// Failures follow the pinned server: an invalid or failed callback panics, the panic unwinds to the
// connection's request dispatch, and the client receives an error response for the request that
// triggered the filesystem access (writeFile/removeFile call failures are returned as errors).

use std::sync::{Arc, OnceLock};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use rustc_hash::FxHashSet;
use serde::Deserialize;
use serde_json::value::RawValue;
use tsrs_core::tspath;
use tsrs_vfs::{Entries, FS, FileInfo, FileMode};

use crate::handler::Caller;

const CALLBACK_READ_FILE: &str = "readFile";
const CALLBACK_FILE_EXISTS: &str = "fileExists";
const CALLBACK_DIRECTORY_EXISTS: &str = "directoryExists";
const CALLBACK_GET_ACCESSIBLE_ENTRIES: &str = "getAccessibleEntries";
const CALLBACK_REALPATH: &str = "realpath";
const CALLBACK_STAT: &str = "stat";
const CALLBACK_WRITE_FILE: &str = "writeFile";
const CALLBACK_REMOVE_FILE: &str = "removeFile";

const CALLBACK_NAMES: [&str; 8] = [
    CALLBACK_READ_FILE,
    CALLBACK_FILE_EXISTS,
    CALLBACK_DIRECTORY_EXISTS,
    CALLBACK_GET_ACCESSIBLE_ENTRIES,
    CALLBACK_REALPATH,
    CALLBACK_STAT,
    CALLBACK_WRITE_FILE,
    CALLBACK_REMOVE_FILE,
];

fn is_callback_name(name: &str) -> bool {
    CALLBACK_NAMES.contains(&name)
}

/// A failed or invalid filesystem callback unwinds with a String payload (Go: panic(...) recovered by
/// the connection); the connection reports it to the client as `panic: <message>`. `resume_unwind`
/// skips the panic hook, so an expected callback failure (e.g. the client disconnecting while a callback
/// is pending) does not print a panic report to stderr, matching Go's silent recover.
fn fail(message: impl Into<String>) -> ! {
    std::panic::resume_unwind(Box::new(message.into()))
}

/// Parses the `--callbacks` list (comma-separated by the CLI). Unknown names are an error
/// (Go panics with "unknown callback name: <name>").
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct CallbackConfig {
    enabled: Vec<String>,
    realpath_identity: bool,
    fake_stat: bool,
    write_file_noop: bool,
    remove_file_noop: bool,
    error_callbacks: Vec<String>,
}

impl CallbackConfig {
    pub fn parse<S: AsRef<str>>(callbacks: &[S]) -> Result<CallbackConfig, String> {
        let mut config = CallbackConfig::default();
        for cb in callbacks {
            let cb = cb.as_ref();
            if let Some(name) = cb.strip_suffix(":error") {
                if !is_callback_name(name) {
                    return Err(format!("unknown callback name: {name}"));
                }
                config.error_callbacks.push(name.to_string());
                continue;
            }
            match cb {
                "realpath:identity" => config.realpath_identity = true,
                "stat:fakeStat" => config.fake_stat = true,
                "writeFile:noop" => config.write_file_noop = true,
                "removeFile:noop" => config.remove_file_noop = true,
                _ if is_callback_name(cb) => config.enabled.push(cb.to_string()),
                _ => return Err(format!("unknown callback name: {cb}")),
            }
        }
        Ok(config)
    }
}

pub struct CallbackFs {
    base: Arc<dyn FS>,
    enabled: FxHashSet<String>,
    errors: FxHashSet<String>,
    realpath_identity: bool,
    fake_stat: bool,
    write_file_noop: bool,
    remove_file_noop: bool,
    case_sensitive: Option<bool>,
    conn: OnceLock<Arc<dyn Caller>>,
}

#[derive(Deserialize)]
struct CallbackResponse<'a> {
    #[serde(default)]
    kind: Option<String>,
    // Present `null` must stay distinguishable from an absent value (Go: zero value vs unexpected EOF).
    #[serde(borrow, default, deserialize_with = "present_raw")]
    value: Option<&'a RawValue>,
}

fn present_raw<'de, D: serde::Deserializer<'de>>(d: D) -> Result<Option<&'de RawValue>, D::Error> {
    <&RawValue>::deserialize(d).map(Some)
}

struct Decoded {
    kind: String,
    value: Option<String>,
}

impl Decoded {
    fn value<'a, T: Deserialize<'a>>(&'a self, name: &str) -> T {
        let raw = self.value.as_deref().unwrap_or("null");
        serde_json::from_str(raw)
            .unwrap_or_else(|e| fail(format!("invalid {name} callback value: {e}")))
    }

    /// json.Unmarshal(value, &string) as callbackfs.go does: an absent value is `unexpected EOF`, `null`
    /// leaves the zero value, any other JSON kind is Go's type error (the panic text is Go's).
    fn string_value(&self, _name: &str) -> String {
        match self.go_scalar("string") {
            Some(raw) => serde_json::from_str(raw).unwrap_or_else(|e| fail(format!("json: {e}"))),
            None => String::new(),
        }
    }

    fn bool_value(&self, _name: &str) -> bool {
        match self.go_scalar("bool") {
            Some(raw) => serde_json::from_str(raw).unwrap_or_else(|e| fail(format!("json: {e}"))),
            None => false,
        }
    }

    /// The raw value when it has the Go target's JSON kind; None for `null`; panics with Go's text otherwise.
    fn go_scalar(&self, go_type: &str) -> Option<&str> {
        let Some(raw) = self.value.as_deref() else { fail("jsontext: unexpected EOF") };
        let raw = raw.trim_matches(|c: char| c == ' ' || c == '\t' || c == '\n' || c == '\r');
        let kind = match raw.as_bytes().first() {
            Some(b'n') => return None,
            Some(b'"') => "string",
            Some(b't' | b'f') => "boolean",
            Some(b'[') => "array",
            Some(b'{') => "object",
            _ => "number",
        };
        let wanted = if go_type == "string" { "string" } else { "boolean" };
        if kind != wanted {
            fail(format!("json: cannot unmarshal JSON {kind} into Go {go_type}"));
        }
        Some(raw)
    }
}

fn decode_callback_response(name: &str, result: &[u8]) -> Decoded {
    // json.Unmarshal(result, &response): duplicate names, invalid UTF-8 and unpaired surrogate escapes
    // anywhere in the response are errors (the callback panics; the request fails).
    if let Err(e) = crate::strictjson::validate(result) {
        fail(e);
    }
    let response: CallbackResponse<'_> = serde_json::from_slice(result)
        .unwrap_or_else(|e| fail(format!("invalid {name} callback response: {e}")));
    let kind = response.kind.unwrap_or_default();
    if kind.is_empty() {
        fail("filesystem callback response is missing a kind");
    }
    if kind == "error" {
        fail(format!("filesystem callback returned serverFS.error: {name}"));
    }
    Decoded { kind, value: response.value.map(|v| v.get().to_string()) }
}

fn invalid_kind(name: &str, response: &Decoded) -> ! {
    fail(format!("invalid {name} callback response kind: {}", response.kind))
}

impl CallbackFs {
    pub fn new(
        base: Arc<dyn FS>,
        config: &CallbackConfig,
        case_sensitive: Option<bool>,
    ) -> CallbackFs {
        CallbackFs {
            base,
            enabled: config.enabled.iter().cloned().collect(),
            errors: config.error_callbacks.iter().cloned().collect(),
            realpath_identity: config.realpath_identity,
            fake_stat: config.fake_stat,
            write_file_noop: config.write_file_noop,
            remove_file_noop: config.remove_file_noop,
            case_sensitive,
            conn: OnceLock::new(),
        }
    }

    /// `SetConnection`: must be called once the transport connection exists, before any filesystem operation
    /// that needs a callback. Later calls are ignored.
    pub fn set_connection(&self, conn: Arc<dyn Caller>) {
        let _ = self.conn.set(conn);
    }

    fn is_enabled(&self, name: &str) -> bool {
        self.enabled.contains(name)
    }

    fn panic_if_error(&self, name: &str) {
        if self.errors.contains(name) {
            fail(format!("filesystem operation configured with serverFS.error: {name}"));
        }
    }

    fn call(&self, name: &str, arg: &[u8]) -> Result<Vec<u8>, String> {
        let Some(conn) = self.conn.get() else {
            return Err(format!("CallbackFS: {name} called before connection set"));
        };
        conn.call(name, Some(arg)).map_err(|e| e.to_string())
    }

    fn call_path(&self, name: &str, path: &str) -> Decoded {
        let arg = serde_json::to_vec(path).expect("string serializes");
        let result = self.call(name, &arg).unwrap_or_else(|e| fail(e));
        decode_callback_response(name, &result)
    }

    fn fake_stat_for_path(&self, path: &str) -> Option<FileInfo> {
        let name = base_file_name(path);
        if self.directory_exists(path) {
            return Some(FileInfo {
                name,
                size: 0,
                mode: FileMode::Dir | FileMode::from_bits_retain(0o555),
                mod_time: None,
            });
        }
        if self.file_exists(path) {
            return Some(FileInfo {
                name,
                size: 0,
                mode: FileMode::from_bits_retain(0o444),
                mod_time: None,
            });
        }
        None
    }
}

impl FS for CallbackFs {
    fn use_case_sensitive_file_names(&self) -> bool {
        self.case_sensitive.unwrap_or_else(|| self.base.use_case_sensitive_file_names())
    }

    fn read_file(&self, path: &str) -> Option<String> {
        self.panic_if_error(CALLBACK_READ_FILE);
        if self.is_enabled(CALLBACK_READ_FILE) {
            let response = self.call_path(CALLBACK_READ_FILE, path);
            return match response.kind.as_str() {
                "value" => Some(response.string_value(CALLBACK_READ_FILE)),
                "missing" => None,
                "useOS" => self.base.read_file(path),
                _ => invalid_kind(CALLBACK_READ_FILE, &response),
            };
        }
        self.base.read_file(path)
    }

    fn file_exists(&self, path: &str) -> bool {
        self.panic_if_error(CALLBACK_FILE_EXISTS);
        if self.is_enabled(CALLBACK_FILE_EXISTS) {
            let response = self.call_path(CALLBACK_FILE_EXISTS, path);
            return match response.kind.as_str() {
                "value" => response.bool_value(CALLBACK_FILE_EXISTS),
                "useOS" => self.base.file_exists(path),
                _ => invalid_kind(CALLBACK_FILE_EXISTS, &response),
            };
        }
        self.base.file_exists(path)
    }

    fn directory_exists(&self, path: &str) -> bool {
        self.panic_if_error(CALLBACK_DIRECTORY_EXISTS);
        if self.is_enabled(CALLBACK_DIRECTORY_EXISTS) {
            let response = self.call_path(CALLBACK_DIRECTORY_EXISTS, path);
            return match response.kind.as_str() {
                "value" => response.bool_value(CALLBACK_DIRECTORY_EXISTS),
                "useOS" => self.base.directory_exists(path),
                _ => invalid_kind(CALLBACK_DIRECTORY_EXISTS, &response),
            };
        }
        self.base.directory_exists(path)
    }

    fn get_accessible_entries(&self, path: &str) -> Entries {
        self.panic_if_error(CALLBACK_GET_ACCESSIBLE_ENTRIES);
        if self.is_enabled(CALLBACK_GET_ACCESSIBLE_ENTRIES) {
            let response = self.call_path(CALLBACK_GET_ACCESSIBLE_ENTRIES, path);
            return match response.kind.as_str() {
                "value" => {
                    #[derive(Deserialize)]
                    struct RawEntries {
                        #[serde(default)]
                        files: Option<Vec<String>>,
                        #[serde(default)]
                        directories: Option<Vec<String>>,
                        #[serde(default)]
                        symlinks: Option<Vec<String>>,
                    }
                    let Some(raw) =
                        response.value::<Option<RawEntries>>(CALLBACK_GET_ACCESSIBLE_ENTRIES)
                    else {
                        fail("invalid getAccessibleEntries callback value: null");
                    };
                    Entries {
                        files: raw.files.unwrap_or_default(),
                        directories: raw.directories.unwrap_or_default(),
                        symlinks: raw.symlinks.map(|s| s.into_iter().collect()),
                    }
                }
                "useOS" => self.base.get_accessible_entries(path),
                _ => invalid_kind(CALLBACK_GET_ACCESSIBLE_ENTRIES, &response),
            };
        }
        self.base.get_accessible_entries(path)
    }

    fn realpath(&self, path: &str) -> String {
        self.panic_if_error(CALLBACK_REALPATH);
        if self.is_enabled(CALLBACK_REALPATH) {
            let response = self.call_path(CALLBACK_REALPATH, path);
            return match response.kind.as_str() {
                "value" => tspath::get_normalized_absolute_path(
                    &response.string_value(CALLBACK_REALPATH),
                    &tspath::get_directory_path(path),
                ),
                "identity" => path.to_string(),
                "useOS" => self.base.realpath(path),
                _ => invalid_kind(CALLBACK_REALPATH, &response),
            };
        }
        if self.realpath_identity {
            return path.to_string();
        }
        self.base.realpath(path)
    }

    fn stat(&self, path: &str) -> Option<FileInfo> {
        self.panic_if_error(CALLBACK_STAT);
        if self.is_enabled(CALLBACK_STAT) {
            let response = self.call_path(CALLBACK_STAT, path);
            return match response.kind.as_str() {
                "value" => {
                    #[derive(Deserialize)]
                    struct RawStat {
                        #[serde(default)]
                        mode: u32,
                        #[serde(default)]
                        size: i64,
                        #[serde(default)]
                        mtime: String,
                    }
                    let stat = response.value::<RawStat>(CALLBACK_STAT);
                    let mod_time = parse_rfc3339(&stat.mtime).unwrap_or_else(|e| {
                        fail(format!("parsing time {:?} as RFC3339: {e}", stat.mtime))
                    });
                    Some(FileInfo {
                        name: base_file_name(path),
                        size: stat.size,
                        mode: node_file_mode_to_go_file_mode(stat.mode),
                        mod_time: Some(mod_time),
                    })
                }
                "missing" => None,
                "fakeStat" => self.fake_stat_for_path(path),
                "useOS" => self.base.stat(path),
                _ => invalid_kind(CALLBACK_STAT, &response),
            };
        }
        if self.fake_stat {
            return self.fake_stat_for_path(path);
        }
        self.base.stat(path)
    }

    fn write_file(&self, path: &str, data: &str) -> Result<(), String> {
        self.panic_if_error(CALLBACK_WRITE_FILE);
        if self.is_enabled(CALLBACK_WRITE_FILE) {
            #[derive(serde::Serialize)]
            struct Payload<'a> {
                path: &'a str,
                data: &'a str,
            }
            let arg = serde_json::to_vec(&Payload { path, data }).expect("payload serializes");
            let result = self.call(CALLBACK_WRITE_FILE, &arg)?;
            let response = decode_callback_response(CALLBACK_WRITE_FILE, &result);
            return match response.kind.as_str() {
                "value" | "noop" => Ok(()),
                "useOS" => self.base.write_file(path, data),
                _ => invalid_kind(CALLBACK_WRITE_FILE, &response),
            };
        }
        if self.write_file_noop {
            return Ok(());
        }
        self.base.write_file(path, data)
    }

    fn append_file(&self, path: &str, data: &str) -> Result<(), String> {
        self.base.append_file(path, data)
    }

    fn remove(&self, path: &str) -> Result<(), String> {
        self.panic_if_error(CALLBACK_REMOVE_FILE);
        if self.is_enabled(CALLBACK_REMOVE_FILE) {
            let arg = serde_json::to_vec(path).expect("string serializes");
            let result = self.call(CALLBACK_REMOVE_FILE, &arg)?;
            let response = decode_callback_response(CALLBACK_REMOVE_FILE, &result);
            return match response.kind.as_str() {
                "value" | "noop" => Ok(()),
                "useOS" => self.base.remove(path),
                _ => invalid_kind(CALLBACK_REMOVE_FILE, &response),
            };
        }
        if self.remove_file_noop {
            return Ok(());
        }
        self.base.remove(path)
    }

    fn chtimes(&self, path: &str, a_time: SystemTime, m_time: SystemTime) -> Result<(), String> {
        self.base.chtimes(path, a_time, m_time)
    }
}

/// tspath.GetBaseFileName for the absolute, normalized paths the compiler passes to the FS.
fn base_file_name(path: &str) -> String {
    let trimmed = path.trim_end_matches('/');
    if trimmed.is_empty() {
        return String::new();
    }
    match trimmed.rfind('/') {
        Some(i) => trimmed[i + 1..].to_string(),
        None => trimmed.to_string(),
    }
}

/// callbackfs.go nodeFileModeToGoFileMode.
fn node_file_mode_to_go_file_mode(mode: u32) -> FileMode {
    let mut result = FileMode::from_bits_retain(mode & 0o777);
    if mode & 0o4000 != 0 {
        result |= FileMode::Setuid;
    }
    if mode & 0o2000 != 0 {
        result |= FileMode::Setgid;
    }
    if mode & 0o1000 != 0 {
        result |= FileMode::Sticky;
    }
    match mode & 0o170_000 {
        0o010_000 => result |= FileMode::NamedPipe,
        0o020_000 => result |= FileMode::Device | FileMode::CharDevice,
        0o040_000 => result |= FileMode::Dir,
        0o060_000 => result |= FileMode::Device,
        0o100_000 => {}
        0o120_000 => result |= FileMode::Symlink,
        0o140_000 => result |= FileMode::Socket,
        _ => result |= FileMode::Irregular,
    }
    result
}

/// time.Parse(time.RFC3339Nano, s): `YYYY-MM-DDTHH:MM:SS[.frac](Z|±HH:MM)`.
fn parse_rfc3339(s: &str) -> Result<SystemTime, String> {
    let b = s.as_bytes();
    let num = |range: std::ops::Range<usize>| -> Result<i64, String> {
        let part = b.get(range).ok_or("too short")?;
        if !part.iter().all(u8::is_ascii_digit) {
            return Err(format!("bad digits in {s:?}"));
        }
        Ok(std::str::from_utf8(part).unwrap().parse::<i64>().unwrap())
    };
    if b.len() < 20
        || b[4] != b'-'
        || b[7] != b'-'
        || !(b[10] == b'T' || b[10] == b't')
        || b[13] != b':'
        || b[16] != b':'
    {
        return Err("cannot parse".to_string());
    }
    let (year, month, day) = (num(0..4)?, num(5..7)?, num(8..10)?);
    let (hour, min, sec) = (num(11..13)?, num(14..16)?, num(17..19)?);
    if !(1..=12).contains(&month)
        || day < 1
        || day > days_in_month(year, month)
        || hour > 23
        || min > 59
        || sec > 59
    {
        return Err("value out of range".to_string());
    }
    let mut i = 19;
    let mut nanos: u32 = 0;
    if b[i] == b'.' || b[i] == b',' {
        i += 1;
        let start = i;
        i += (i..b.len()).take_while(|&index| b[index].is_ascii_digit()).count();
        if i == start || i - start > 9 {
            return Err("bad fractional second".to_string());
        }
        let digits = std::str::from_utf8(&b[start..i]).unwrap();
        nanos = format!("{digits:0<9}").parse::<u32>().unwrap();
    }
    let offset_secs: i64 = match b.get(i) {
        Some(b'Z' | b'z') if i + 1 == b.len() => 0,
        Some(&sign @ (b'+' | b'-')) if b.len() == i + 6 && b[i + 3] == b':' => {
            let (oh, om) = (num(i + 1..i + 3)?, num(i + 4..i + 6)?);
            if oh > 23 || om > 59 {
                return Err("time zone offset out of range".to_string());
            }
            let off = oh * 3600 + om * 60;
            if sign == b'-' { -off } else { off }
        }
        _ => return Err("bad time zone".to_string()),
    };
    let days = days_from_civil(year, month, day);
    let secs = days * 86400 + hour * 3600 + min * 60 + sec - offset_secs;
    Ok(if secs >= 0 {
        UNIX_EPOCH + Duration::new(secs as u64, nanos)
    } else {
        UNIX_EPOCH - Duration::from_secs((-secs) as u64) + Duration::from_nanos(nanos as u64)
    })
}

fn days_in_month(year: i64, month: i64) -> i64 {
    match month {
        4 | 6 | 9 | 11 => 30,
        2 if (year % 4 == 0 && year % 100 != 0) || year % 400 == 0 => 29,
        2 => 28,
        _ => 31,
    }
}

// Howard Hinnant's days_from_civil.
fn days_from_civil(y: i64, m: i64, d: i64) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = if y >= 0 { y } else { y - 399 } / 400;
    let yoe = y - era * 400;
    let mp = (m + 9) % 12;
    let doy = (153 * mp + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}
