use std::{
    fs, io,
    os::unix::{
        fs::{FileTypeExt, MetadataExt, PermissionsExt},
        net::UnixListener as StdUnixListener,
    },
    path::{Path, PathBuf},
    time::Duration,
};

use tokio::{
    net::{UnixListener, UnixStream},
    sync::{OwnedSemaphorePermit, Semaphore},
};

pub const MAX_ACCEPTED: usize = 32;
pub const MAX_HANDSHAKE_IDLE: Duration = Duration::from_secs(10);

/// A peer-authenticated stream, ready for a future protocol handler.
/// Holds an accept slot for its full lifetime. B3b must apply `MAX_HANDSHAKE_IDLE`
/// as the deadline for reading its first complete framed request.
pub struct AuthenticatedStream {
    stream: UnixStream,
    peer_uid: u32,
    _permit: OwnedSemaphorePermit,
}

impl AuthenticatedStream {
    pub fn stream_mut(&mut self) -> &mut UnixStream {
        &mut self.stream
    }
    pub fn peer_uid(&self) -> u32 {
        self.peer_uid
    }
}

/// Secure listener. Acquiring a stream also reserves one bounded handler slot.
pub struct Listener {
    listener: UnixListener,
    slots: std::sync::Arc<Semaphore>,
    path: PathBuf,
    socket_dev: u64,
    socket_ino: u64,
}

#[derive(Debug)]
pub enum ListenerError {
    Io(io::Error),
    UnsafePath(&'static str),
    PeerUidMismatch { expected: u32, actual: u32 },
    HandshakeTimeout,
}

impl From<io::Error> for ListenerError {
    fn from(value: io::Error) -> Self {
        Self::Io(value)
    }
}

impl std::fmt::Display for ListenerError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Io(e) => write!(f, "IPC listener: {e}"),
            Self::UnsafePath(reason) => write!(f, "unsafe IPC socket path: {reason}"),
            Self::PeerUidMismatch { expected, actual } => {
                write!(f, "peer uid {actual} does not match {expected}")
            }
            Self::HandshakeTimeout => write!(f, "IPC handshake idle timeout"),
        }
    }
}
impl std::error::Error for ListenerError {}

/// Pure policy check, exposed so wrong-UID behavior can be tested without another OS user.
pub fn credential_matches(expected_uid: u32, peer_uid: u32) -> bool {
    expected_uid == peer_uid
}

impl Listener {
    pub fn bind(path: impl AsRef<Path>) -> Result<Self, ListenerError> {
        let path = path.as_ref().to_path_buf();
        let parent = path
            .parent()
            .ok_or(ListenerError::UnsafePath("missing parent directory"))?;
        let dir_meta = fs::symlink_metadata(parent)?;
        if !dir_meta.file_type().is_dir()
            || dir_meta.uid() != unsafe { libc::geteuid() }
            || dir_meta.mode() & 0o777 != 0o700
        {
            return Err(ListenerError::UnsafePath(
                "parent must be an owned 0700 directory",
            ));
        }
        match fs::symlink_metadata(&path) {
            Ok(meta) => {
                // A previous daemon may have crashed and left its socket behind,
                // which would otherwise make restart impossible. Only reclaim a path
                // that is a socket, owned by us, in our own 0700 directory, and that
                // nothing is answering on.
                if !meta.file_type().is_socket() || meta.uid() != unsafe { libc::geteuid() } {
                    return Err(ListenerError::UnsafePath(
                        "socket path exists and is not our socket",
                    ));
                }
                match std::os::unix::net::UnixStream::connect(&path) {
                    Ok(_) => {
                        return Err(ListenerError::UnsafePath(
                            "another daemon is already listening on this socket",
                        ));
                    }
                    Err(_) => {
                        fs::remove_file(&path)?;
                    }
                }
            }
            Err(e) if e.kind() == io::ErrorKind::NotFound => (),
            Err(e) => return Err(e.into()),
        }
        // Bind atomically without following/replacing a preexisting filesystem entry.
        let std_listener = StdUnixListener::bind(&path)?;
        let identity = fs::symlink_metadata(&path).ok().map(|m| (m.dev(), m.ino()));
        let setup = (|| {
            fs::set_permissions(&path, fs::Permissions::from_mode(0o600))?;
            std_listener.set_nonblocking(true)?;
            let metadata = fs::symlink_metadata(&path)?;
            let (socket_dev, socket_ino) = (metadata.dev(), metadata.ino());
            Ok(Self {
                listener: UnixListener::from_std(std_listener)?,
                slots: std::sync::Arc::new(Semaphore::new(MAX_ACCEPTED)),
                path: path.clone(),
                socket_dev,
                socket_ino,
            })
        })();
        if setup.is_err() {
            remove_matching_socket(&path, identity);
        }
        setup
    }

    /// Accepts one authenticated peer. The handler must enforce the first-request
    /// deadline; this method only authenticates and bounds concurrent clients.
    pub async fn accept(&self) -> Result<AuthenticatedStream, ListenerError> {
        let permit = self
            .slots
            .clone()
            .acquire_owned()
            .await
            .expect("listener semaphore is never closed");
        let (stream, _) = self.listener.accept().await?;
        let cred = stream.peer_cred()?;
        let uid = cred.uid();
        let expected = unsafe { libc::geteuid() };
        if !credential_matches(expected, uid) {
            drop(stream);
            return Err(ListenerError::PeerUidMismatch {
                expected,
                actual: uid,
            });
        }
        Ok(AuthenticatedStream {
            stream,
            peer_uid: uid,
            _permit: permit,
        })
    }
}

fn remove_matching_socket(path: &Path, identity: Option<(u64, u64)>) {
    if let Ok(meta) = fs::symlink_metadata(path) {
        if meta.file_type().is_socket()
            && meta.uid() == unsafe { libc::geteuid() }
            && identity.is_none_or(|(dev, ino)| meta.dev() == dev && meta.ino() == ino)
        {
            let _ = fs::remove_file(path);
        }
    }
}

impl Drop for Listener {
    fn drop(&mut self) {
        remove_matching_socket(&self.path, Some((self.socket_dev, self.socket_ino)));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::FileTypeExt;

    #[test]
    fn uid_policy() {
        assert!(credential_matches(1000, 1000));
        assert!(!credential_matches(1000, 1001));
    }

    #[tokio::test]
    async fn a_stale_socket_is_reclaimed_but_a_live_one_is_respected() {
        let dir = tempfile::tempdir().unwrap();
        fs::set_permissions(dir.path(), fs::Permissions::from_mode(0o700)).unwrap();
        let path = dir.path().join("bloomd.sock");

        // A crashed daemon leaves the socket file behind with nothing listening.
        {
            let stale = std::os::unix::net::UnixListener::bind(&path).unwrap();
            drop(stale);
        }
        assert!(
            path.exists(),
            "the stale socket file should still be present"
        );
        let listener = Listener::bind(&path).expect("stale socket must be reclaimed");
        assert!(path.exists());
        drop(listener);
        let _ = fs::remove_file(&path);

        // A live daemon must not be displaced.
        let live = std::os::unix::net::UnixListener::bind(&path).unwrap();
        let error = match Listener::bind(&path) {
            Ok(_) => panic!("a live socket must not be taken over"),
            Err(error) => error,
        };
        assert!(
            matches!(error, ListenerError::UnsafePath(_)),
            "unexpected error: {error:?}"
        );
        drop(live);
    }

    #[tokio::test]
    async fn mode_and_same_uid_acceptance_and_duplicate_refusal() {
        let dir = tempfile::tempdir().unwrap();
        fs::set_permissions(dir.path(), fs::Permissions::from_mode(0o700)).unwrap();
        let path = dir.path().join("bloomd.sock");
        let listener = Listener::bind(&path).unwrap();
        let meta = fs::symlink_metadata(&path).unwrap();
        assert!(meta.file_type().is_socket());
        assert_eq!(meta.mode() & 0o777, 0o600);
        assert!(Listener::bind(&path).is_err());
        let client = UnixStream::connect(&path);
        let (accepted, connected) = tokio::join!(listener.accept(), client);
        let accepted = accepted.unwrap();
        connected.unwrap();
        assert_eq!(accepted.peer_uid(), unsafe { libc::geteuid() });
        assert_eq!(listener.slots.available_permits(), MAX_ACCEPTED - 1);
        drop(accepted);
        assert_eq!(listener.slots.available_permits(), MAX_ACCEPTED);
        remove_matching_socket(&path, Some((listener.socket_dev, listener.socket_ino + 1)));
        assert!(path.exists(), "a replaced socket must not be unlinked");
        drop(listener);
        assert!(!path.exists(), "dropping the owner must remove its socket");
    }
}
