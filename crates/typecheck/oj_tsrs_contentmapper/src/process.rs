// Go cmd/tsc/sys.go:68-120: the production Spawner, which starts a content mapper's command as a child process.
//
// Go reads the child's stdout through the runtime's poller, so the Wait in Close also unblocks a read loop still
// waiting on it. A blocking read of a pipe cannot be interrupted from another thread here; once the child is killed
// the read ends with EOF, unless a grandchild the mapper started still holds the pipe, and then the host's read-loop
// thread, which nobody joins (hostimpl.rs), waits for that grandchild instead of Close.

use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::mpsc::{self, Receiver};
use std::sync::{Arc, Mutex, MutexGuard, OnceLock, PoisonError, TryLockError};
use std::thread;
use std::time::Duration;

use tsrs_ipc::{Closer, ReadWriteCloser};

use crate::host::quote;
use crate::hostimpl::Spawner;

// sys.go:79 cmd.WaitDelay: once the child has exited, how long Close waits for the copy of its stderr to finish.
const WAIT_DELAY: Duration = Duration::from_secs(1);

// The thread that copies a logged child's stderr to the spawner's writer (Go's os/exec goroutine).
const STDERR_THREAD: &str = "contentmapper-stderr";

// The production Spawner, Go's osSys.Spawn (sys.go:68), which the CLI's System hands to the content mapper host.
pub struct ProcessSpawner;

impl Spawner for ProcessSpawner {
    // sys.go:68
    fn spawn(
        &self,
        command: &[String],
        dir: &str,
        stderr: Option<Box<dyn Write + Send>>,
    ) -> Result<ReadWriteCloser, String> {
        spawn_process(command, dir, stderr)
    }
}

// spawnProcess launches a process and adapts its stdio to an io.ReadWriteCloser (Read is its stdout,
// Write is its stdin).
// sys.go:73 (The errors are os/exec's texts: exec.Command's PATH lookup of a bare name, os.StartProcess's check of
// the working directory, and `fork/exec` for a command that cannot be executed. A None stderr is Go's io.Discard:
// the child writes it to the null device instead of to a copying thread.)
pub fn spawn_process(
    command: &[String],
    dir: &str,
    stderr: Option<Box<dyn Write + Send>>,
) -> Result<ReadWriteCloser, String> {
    // exec.Command(command[0], command[1:]...): only a bare name is looked up in PATH.
    let name = command[0].as_str();
    let has_directory = name.contains('/') || (cfg!(windows) && name.contains(['\\', ':']));
    let path = if !name.is_empty() && !has_directory { look_path(name)? } else { name.to_string() };
    if path.is_empty() {
        return Err("exec: no command".to_string());
    }
    // os.StartProcess checks the working directory before it starts the process.
    if !dir.is_empty()
        && let Err(err) = std::fs::metadata(dir)
    {
        return Err(format!("chdir {dir}: {}", errno_text(&err)));
    }
    // Go's child changes to cmd.Dir before it executes cmd.Path, so a relative path names a file in that directory.
    let mut program = PathBuf::from(&path);
    if !dir.is_empty() && program.is_relative() {
        program = Path::new(dir).join(program);
    }
    let mut cmd = Command::new(program);
    cmd.args(&command[1..]);
    if !dir.is_empty() {
        cmd.current_dir(dir);
    }
    cmd.stdin(Stdio::piped());
    cmd.stdout(Stdio::piped());
    cmd.stderr(if stderr.is_some() { Stdio::piped() } else { Stdio::null() });
    let mut child = match cmd.spawn() {
        Ok(child) => child,
        Err(err) => return Err(format!("fork/exec {path}: {}", errno_text(&err))),
    };
    let stdin = child.stdin.take().expect("the child's stdin is piped");
    let stdout = child.stdout.take().expect("the child's stdout is piped");
    let mut stderr_copied = None;
    if let (Some(mut from), Some(mut to)) = (child.stderr.take(), stderr) {
        let (copied, wait) = mpsc::channel::<()>();
        let started = thread::Builder::new().name(STDERR_THREAD.to_string()).spawn(move || {
            let _ = io::copy(&mut from, &mut to);
            drop(copied);
        });
        if let Err(err) = started {
            let _ = child.kill();
            let _ = child.wait();
            return Err(err.to_string());
        }
        stderr_copied = Some(wait);
    }
    let stdin = Arc::new(Mutex::new(Some(stdin)));
    Ok(ReadWriteCloser {
        reader: Box::new(stdout),
        writer: Box::new(SharedChildStdin(Arc::clone(&stdin))),
        closer: Arc::new(ChildProcess {
            child: Mutex::new(Some(child)),
            stdin,
            stderr_copied: Mutex::new(stderr_copied),
            exit_code: OnceLock::new(),
        }),
    })
}

// childProcess adapts a spawned process's stdout (read) and stdin (write) into one io.ReadWriteCloser.
// Close kills and reaps the process.
// sys.go:94 (The reader is the child's stdout; the writer and this closer share its stdin, so Close can close it.)
struct ChildProcess {
    // cmd: taken by Close, which waits for it.
    child: Mutex<Option<Child>>,
    stdin: Arc<Mutex<Option<ChildStdin>>>,
    // Disconnected once the stderr copy is done.
    stderr_copied: Mutex<Option<Receiver<()>>>,
    // cmd.ProcessState's exit code, known once Close has waited for the process.
    exit_code: OnceLock<i32>,
}

impl Closer for ChildProcess {
    // sys.go:108 (Go's Wait also closes the parent's end of stdout; see the module comment.)
    fn close(&self) -> io::Result<()> {
        // p.stdin.Close(). A write blocked on a full pipe holds the stdin; the kill below makes that write fail.
        match self.stdin.try_lock() {
            Ok(mut stdin) => drop(stdin.take()),
            Err(TryLockError::Poisoned(stdin)) => drop(stdin.into_inner().take()),
            Err(TryLockError::WouldBlock) => {}
        }
        let Some(mut child) = lock(&self.child).take() else {
            return Err(io::Error::other("exec: Wait was already called"));
        };
        let _ = child.kill();
        let status = child.wait();
        if let Ok(status) = &status {
            // ProcessState.ExitCode(): -1 for a process killed by a signal.
            let _ = self.exit_code.set(status.code().unwrap_or(-1));
        }
        // cmd.Wait waits for the copy of the child's stderr, at most WaitDelay after the child has exited.
        let copied = lock(&self.stderr_copied).take();
        if let Some(copied) = copied {
            let _ = copied.recv_timeout(WAIT_DELAY);
        }
        // Go returns nil for an *exec.ExitError (the process was killed, or had failed) and for exec.ErrWaitDelay.
        status.map(|_| ())
    }

    // sys.go:101
    fn exit_code(&self) -> Option<i32> {
        self.exit_code.get().copied()
    }
}

// The child's stdin, which Go writes through cmd.StdinPipe's *os.File and which Close closes.
struct SharedChildStdin(Arc<Mutex<Option<ChildStdin>>>);

impl Write for SharedChildStdin {
    fn write(&mut self, data: &[u8]) -> io::Result<usize> {
        match lock(&self.0).as_mut() {
            Some(stdin) => stdin.write(data),
            None => Err(file_already_closed()),
        }
    }

    fn flush(&mut self) -> io::Result<()> {
        match lock(&self.0).as_mut() {
            Some(stdin) => stdin.flush(),
            None => Err(file_already_closed()),
        }
    }
}

// os.ErrClosed.
fn file_already_closed() -> io::Error {
    io::Error::new(io::ErrorKind::BrokenPipe, "file already closed")
}

// exec.LookPath for a bare name, with its error texts (`exec: "name": <reason>`, exec.Error).
fn look_path(file: &str) -> Result<String, String> {
    let not_found = if cfg!(windows) {
        "executable file not found in %PATH%"
    } else {
        "executable file not found in $PATH"
    };
    // validateLookPath
    if matches!(file, "" | "." | "..") {
        return Err(format!("exec: {}: {not_found}", quote(file)));
    }
    #[cfg(windows)]
    let exts = path_ext(&std::env::var("PATHEXT").unwrap_or_default());
    let path = std::env::var_os("PATH").unwrap_or_default();
    // filepath.SplitList returns no elements for an empty list.
    if !path.is_empty()
        && let Some(result) = std::env::split_paths(&path).into_iter().find_map(|dir| {
            // Windows skips empty PATH entries (os/exec/lp_windows.go lookPathExts).
            if cfg!(windows) && dir.as_os_str().is_empty() {
                return None;
            }
            // Unix shell semantics: path element "" means "."
            let dir = if dir.as_os_str().is_empty() { PathBuf::from(".") } else { dir };
            let path = dir.join(file);
            #[cfg(windows)]
            let path = find_executable(&path, &exts)?;
            #[cfg(not(windows))]
            if !is_executable(&path) {
                return None;
            }
            if !path.is_absolute() {
                return Some(Err(format!(
                    "exec: {}: cannot run executable found relative to current directory",
                    quote(file)
                )));
            }
            Some(Ok(path.to_string_lossy().into_owned()))
        })
    {
        return result;
    }
    Err(format!("exec: {}: {not_found}", quote(file)))
}

// os/exec/lp_windows.go pathExt. Take the environment value separately so the extension rules can be checked on
// every host without changing process-wide state.
pub fn path_ext(value: &str) -> Vec<String> {
    if value.is_empty() {
        return [".com", ".exe", ".bat", ".cmd"].map(str::to_string).to_vec();
    }
    value
        .to_lowercase()
        .split(';')
        .filter(|ext| !ext.is_empty())
        .map(|ext| if ext.starts_with('.') { ext.to_string() } else { format!(".{ext}") })
        .collect()
}

// os/exec/lp_windows.go findExecutable. PATH search discards candidate errors and only needs the successful path.
pub fn find_executable(file: &Path, exts: &[String]) -> Option<PathBuf> {
    let chk_stat = |path: &Path| std::fs::metadata(path).is_ok_and(|metadata| !metadata.is_dir());
    if exts.is_empty() {
        return chk_stat(file).then(|| file.to_path_buf());
    }
    // hasExt: a dot must follow the final drive or directory separator, including for dot-prefixed filenames.
    let name = file.to_string_lossy();
    let has_ext = name
        .rfind('.')
        .is_some_and(|dot| name.rfind([':', '\\', '/']).is_none_or(|separator| separator < dot));
    if has_ext && chk_stat(file) {
        return Some(file.to_path_buf());
    }
    // Keep checking extensions so names such as "foo.bat.exe" resolve when asked for "foo.bat".
    for ext in exts {
        let mut candidate = file.as_os_str().to_os_string();
        candidate.push(ext);
        let candidate = PathBuf::from(candidate);
        if chk_stat(&candidate) {
            return Some(candidate);
        }
    }
    None
}

// os/exec/lp_unix.go findExecutable: an existing file that is not a directory and has an execute bit (Go asks
// eaccess(2) first and falls back to the bits).
#[cfg(not(windows))]
fn is_executable(file: &Path) -> bool {
    let Ok(metadata) = std::fs::metadata(file) else {
        return false;
    };
    if metadata.is_dir() {
        return false;
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        metadata.permissions().mode() & 0o111 != 0
    }
    #[cfg(not(unix))]
    {
        true
    }
}

// The text of Go's syscall.Errno for an OS error: the C library's message, which Go's table holds with a lowercase
// first letter ("no such file or directory").
fn errno_text(err: &io::Error) -> String {
    let text = err.to_string();
    let text = match err.raw_os_error() {
        Some(code) => {
            text.strip_suffix(&format!(" (os error {code})")).unwrap_or(&text).to_string()
        }
        None => text,
    };
    let mut chars = text.chars();
    match chars.next() {
        Some(first) => first.to_lowercase().chain(chars).collect(),
        None => text,
    }
}

// Go's sync.Mutex has no poisoning.
fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(PoisonError::into_inner)
}
