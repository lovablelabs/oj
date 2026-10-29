use std::fmt;
use std::io::Write;
use std::sync::{Arc, Mutex};
use std::time::{SystemTime, UNIX_EPOCH};

// Go's variadic `Log(msg ...any)` / `Logf(format, args...)` become `log(&str)` / `logf(format_args!(...))`.
pub trait Logger: Send + Sync {
    // Error logs an error message.
    fn error(&self, msg: &str);
    // Errorf logs a formatted error message.
    fn errorf(&self, args: fmt::Arguments<'_>);
    // Warn logs a warning message.
    fn warn(&self, msg: &str);
    // Warnf logs a formatted warning message.
    fn warnf(&self, args: fmt::Arguments<'_>);
    // Info logs an info message.
    fn info(&self, msg: &str);
    // Infof logs a formatted info message.
    fn infof(&self, args: fmt::Arguments<'_>);
    // Log prints a line to the output writer with a header.
    fn log(&self, msg: &str);
    // Logf prints a formatted line to the output writer with a header.
    fn logf(&self, args: fmt::Arguments<'_>);

    // Verbose returns the logger instance if verbose logging is enabled, and otherwise returns nil.
    // A nil logger created with `logging.NewLogger` is safe to call methods on.
    fn verbose(&self) -> Option<&dyn Logger>;
    // IsVerbose returns true if verbose logging is enabled, and false otherwise.
    fn is_verbose(&self) -> bool;
    // SetVerbose sets the verbose logging flag.
    fn set_verbose(&self, verbose: bool);
}

pub type Writer = Box<dyn Write + Send>;

// Go `*logger`; `inner == None` is the nil logger (`NewNopLogger`).
pub struct LoggerImpl {
    inner: Option<LoggerInner>,
}

struct LoggerInner {
    mu: Mutex<LoggerState>,
    prefix: Box<dyn Fn() -> String + Send + Sync>,
}

struct LoggerState {
    verbose: bool,
    writer: Writer,
}

impl LoggerImpl {
    pub(crate) fn new(writer: Writer, prefix: Box<dyn Fn() -> String + Send + Sync>) -> LoggerImpl {
        LoggerImpl {
            inner: Some(LoggerInner {
                mu: Mutex::new(LoggerState { verbose: false, writer }),
                prefix,
            }),
        }
    }
}

impl Logger for LoggerImpl {
    // logger.go:46
    fn log(&self, msg: &str) {
        let Some(l) = &self.inner else {
            return;
        };
        let mut st = l.mu.lock().unwrap();
        let _ = writeln!(st.writer, "{} {}", (l.prefix)(), msg);
    }

    // logger.go:55
    fn logf(&self, args: fmt::Arguments<'_>) {
        let Some(l) = &self.inner else {
            return;
        };
        let mut st = l.mu.lock().unwrap();
        let _ = writeln!(st.writer, "{} {}", (l.prefix)(), args);
    }

    // logger.go:64
    fn verbose(&self) -> Option<&dyn Logger> {
        let l = self.inner.as_ref()?;
        if !l.mu.lock().unwrap().verbose {
            return None;
        }
        Some(self)
    }

    // logger.go:76
    fn is_verbose(&self) -> bool {
        match &self.inner {
            None => false,
            Some(l) => l.mu.lock().unwrap().verbose,
        }
    }

    // logger.go:85
    fn set_verbose(&self, verbose: bool) {
        if let Some(l) = &self.inner {
            l.mu.lock().unwrap().verbose = verbose;
        }
    }

    // logger.go:94
    fn error(&self, msg: &str) {
        self.log(msg);
    }

    // logger.go:98
    fn errorf(&self, args: fmt::Arguments<'_>) {
        self.logf(args);
    }

    // logger.go:102
    fn warn(&self, msg: &str) {
        self.log(msg);
    }

    // logger.go:106
    fn warnf(&self, args: fmt::Arguments<'_>) {
        self.logf(args);
    }

    // logger.go:110
    fn info(&self, msg: &str) {
        self.log(msg);
    }

    // logger.go:114
    fn infof(&self, args: fmt::Arguments<'_>) {
        self.logf(args);
    }
}

// logger.go:118
pub fn new_logger(output: Writer) -> Arc<dyn Logger> {
    Arc::new(LoggerImpl::new(output, Box::new(|| format_time(SystemTime::now()))))
}

// logger.go:129
// NewNopLogger returns a no-op Logger that discards all log messages.
// It is safe to call any method on the returned Logger.
pub fn new_nop_logger() -> Arc<dyn Logger> {
    Arc::new(LoggerImpl { inner: None })
}

// logger.go:133
// Go formats `15:04:05.000` in the local time zone; std has no time zone database, so this is UTC.
pub(crate) fn format_time(t: SystemTime) -> String {
    let d = t.duration_since(UNIX_EPOCH).unwrap_or_default();
    let secs = d.as_secs();
    let ms = d.subsec_millis();
    format!("[{:02}:{:02}:{:02}.{:03}]", (secs / 3600) % 24, (secs / 60) % 60, secs % 60, ms)
}
