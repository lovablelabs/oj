// Port of execute/incremental/host.go.

use std::sync::Arc;
use std::time::SystemTime;

use tsrs_compiler::CompilerHost;
use tsrs_vfs::FS;

pub trait Host {
    fn fs(&self) -> &dyn FS;
    fn get_m_time(&self, file_name: &str) -> Option<SystemTime>;
    fn set_m_time(&self, file_name: &str, m_time: Option<SystemTime>) -> Result<(), String>;
}

struct HostImpl {
    host: Arc<dyn CompilerHost>,
}

impl Host for HostImpl {
    fn fs(&self) -> &dyn FS {
        self.host.fs()
    }

    fn get_m_time(&self, file_name: &str) -> Option<SystemTime> {
        get_m_time(&*self.host, file_name)
    }

    fn set_m_time(&self, file_name: &str, m_time: Option<SystemTime>) -> Result<(), String> {
        // Go passes the zero time.Time as the access time, which Chtimes leaves unchanged.
        let m_time = m_time.unwrap_or(SystemTime::UNIX_EPOCH);
        self.host.fs().chtimes(file_name, m_time, m_time)
    }
}

pub fn create_host(compiler_host: Arc<dyn CompilerHost>) -> Box<dyn Host + Send + Sync> {
    Box::new(HostImpl { host: compiler_host })
}

// host.go:37
// Go returns the zero time.Time when the file does not exist; Rust returns None.
pub fn get_m_time(host: &dyn CompilerHost, file_name: &str) -> Option<SystemTime> {
    host.fs().stat(file_name)?.mod_time()
}
