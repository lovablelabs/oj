//! Unix socket listener for the API transport.
//!
//! The directory is private from its creation, so the socket cannot be reached
//! during the short interval before its final permissions are set.

use std::fs::{self, DirBuilder, File, Metadata, Permissions};
use std::io::{self, Read};
use std::net::Shutdown;
use std::os::unix::fs::{DirBuilderExt, MetadataExt, PermissionsExt};
use std::os::unix::net::UnixListener;
use std::path::{Path, PathBuf};

#[derive(Clone, Copy, PartialEq, Eq)]
struct FileIdentity {
    device: u64,
    inode: u64,
}

impl From<&Metadata> for FileIdentity {
    fn from(metadata: &Metadata) -> Self {
        Self { device: metadata.dev(), inode: metadata.ino() }
    }
}

pub struct PipeListener {
    listener: UnixListener,
    directory: PathBuf,
    socket_path: PathBuf,
    directory_identity: FileIdentity,
    socket_identity: FileIdentity,
}

impl PipeListener {
    /// `path` must not exist. It becomes a private directory containing `socket`.
    pub fn bind(path: &Path) -> io::Result<Self> {
        let mut builder = DirBuilder::new();
        builder.mode(0o700);
        builder.create(path).map_err(|error| {
            io::Error::new(error.kind(), format!("create private socket directory: {error}"))
        })?;

        let socket_path = path.join("socket");
        let mut socket_created = false;
        let result = (|| {
            fs::set_permissions(path, Permissions::from_mode(0o700)).map_err(|error| {
                io::Error::new(
                    error.kind(),
                    format!("set private socket directory permissions: {error}"),
                )
            })?;
            let directory_identity = FileIdentity::from(&fs::metadata(path)?);
            let listener = UnixListener::bind(&socket_path).map_err(|error| {
                io::Error::new(error.kind(), format!("listen on private socket: {error}"))
            })?;
            socket_created = true;
            fs::set_permissions(&socket_path, Permissions::from_mode(0o600)).map_err(|error| {
                io::Error::new(error.kind(), format!("set private socket permissions: {error}"))
            })?;
            let socket_identity = FileIdentity::from(&fs::metadata(&socket_path)?);
            Ok(Self {
                listener,
                directory: path.to_path_buf(),
                socket_path: socket_path.clone(),
                directory_identity,
                socket_identity,
            })
        })();

        if result.is_err() {
            if socket_created {
                let _ = fs::remove_file(&socket_path);
            }
            let _ = fs::remove_dir(path);
        }
        result
    }

    /// The supplied name is ignored on Unix, matching upstream's private path.
    pub fn bind_private(_name: &str) -> io::Result<Self> {
        let mut entropy = File::open("/dev/urandom")?;
        for _ in 0..32 {
            let mut random = [0_u8; 16];
            entropy.read_exact(&mut random)?;
            let directory =
                std::env::temp_dir().join(format!("tsgo-api-{:032x}", u128::from_ne_bytes(random)));
            match Self::bind(&directory) {
                Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {}
                result => return result,
            }
        }
        Err(io::Error::new(
            io::ErrorKind::AlreadyExists,
            "could not create a unique private socket directory",
        ))
    }

    pub fn path(&self) -> &Path {
        &self.socket_path
    }

    pub(super) fn accept(&self) -> io::Result<super::Stream> {
        let (stream, _) = self.listener.accept()?;
        let reader = stream.try_clone()?;
        let closer = stream.try_clone()?;
        Ok(super::Stream {
            reader: Box::new(reader),
            writer: Box::new(stream),
            closer: Some(Box::new(move || {
                let _ = closer.shutdown(Shutdown::Both);
            })),
        })
    }
}

impl Drop for PipeListener {
    fn drop(&mut self) {
        // The path may have been replaced while this listener was alive. Only
        // remove the directory and socket that this instance actually created.
        if fs::symlink_metadata(&self.directory)
            .is_ok_and(|metadata| FileIdentity::from(&metadata) == self.directory_identity)
        {
            if fs::symlink_metadata(&self.socket_path)
                .is_ok_and(|metadata| FileIdentity::from(&metadata) == self.socket_identity)
            {
                let _ = fs::remove_file(&self.socket_path);
            }
            let _ = fs::remove_dir(&self.directory);
        }
    }
}
