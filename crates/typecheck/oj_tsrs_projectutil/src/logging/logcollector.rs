use std::fmt;
use std::io::Write;
use std::sync::{Arc, Mutex};
use std::time::{Duration, UNIX_EPOCH};

use super::logger::{Logger, LoggerImpl, format_time};

// Go `LogCollector`: a Logger whose output can be read back (`String()`).
pub trait LogCollector: Logger {
    fn string(&self) -> String;
}

pub struct LogCollectorImpl {
    logger: LoggerImpl,
    builder: Arc<Mutex<String>>,
}

struct SharedBuilder(Arc<Mutex<String>>);

impl Write for SharedBuilder {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.0.lock().unwrap().push_str(&tsrs_core::utf8::from_utf8_lossy(buf));
        Ok(buf.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

impl LogCollector for LogCollectorImpl {
    // logcollector.go:19
    fn string(&self) -> String {
        self.builder.lock().unwrap().clone()
    }
}

impl Logger for LogCollectorImpl {
    fn error(&self, msg: &str) {
        self.logger.error(msg);
    }
    fn errorf(&self, args: fmt::Arguments<'_>) {
        self.logger.errorf(args);
    }
    fn warn(&self, msg: &str) {
        self.logger.warn(msg);
    }
    fn warnf(&self, args: fmt::Arguments<'_>) {
        self.logger.warnf(args);
    }
    fn info(&self, msg: &str) {
        self.logger.info(msg);
    }
    fn infof(&self, args: fmt::Arguments<'_>) {
        self.logger.infof(args);
    }
    fn log(&self, msg: &str) {
        self.logger.log(msg);
    }
    fn logf(&self, args: fmt::Arguments<'_>) {
        self.logger.logf(args);
    }
    fn verbose(&self) -> Option<&dyn Logger> {
        self.logger.verbose()?;
        Some(self)
    }
    fn is_verbose(&self) -> bool {
        self.logger.is_verbose()
    }
    fn set_verbose(&self, verbose: bool) {
        self.logger.set_verbose(verbose);
    }
}

// logcollector.go:23
pub fn new_test_logger() -> Arc<dyn LogCollector> {
    let builder = Arc::new(Mutex::new(String::new()));
    Arc::new(LogCollectorImpl {
        logger: LoggerImpl::new(
            Box::new(SharedBuilder(Arc::clone(&builder))),
            Box::new(|| format_time(UNIX_EPOCH + Duration::from_secs(1_349_085_672))),
        ),
        builder,
    })
}
