//! Demo-mode session root validation and fixed path derivation.
//!
//! A demo session runs against a generated synthetic database inside one fresh
//! private root under `/tmp`. The launcher passes that root in
//! [`DEMO_ROOT_ENV`]; every entry point validates it before use so a malformed
//! or hostile root fails startup instead of being trusted. Normal mode never
//! consults this module.
//!
//! The derived child paths are fixed and are not independently overrideable.
//! "Unexpected" is measured against the top-level layout only, not
//! recursively: session data under `cache/attachments/`, `home/`, and the
//! state logs is legitimate.

use std::{
    fmt, io,
    path::{Component, Path, PathBuf},
};

/// Environment variable carrying the demo session root.
pub const DEMO_ROOT_ENV: &str = "BLOOM_DEMO_ROOT";

/// Maximum bytes in a Unix-domain socket path, excluding the terminating NUL.
///
/// Linux stores at most 108 bytes including the NUL in `sockaddr_un.sun_path`;
/// a path of exactly 108 bytes would be truncated, so the limit is 107.
pub const MAX_SOCKET_PATH_LEN: usize = 107;

/// Top-level entries a demo root may contain.
const ALLOWED_TOP_LEVEL: &[&str] = &[
    "demo-session.json",
    "runtime",
    "data",
    "config",
    "state",
    "cache",
    "home",
];

/// Validated demo session root plus the fixed paths derived from it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DemoPaths {
    pub root: PathBuf,
    /// Synthetic-only identity marker.
    pub session_file: PathBuf,
    pub socket_path: PathBuf,
    pub store_path: PathBuf,
    pub tui_config_path: PathBuf,
    pub daemon_log_path: PathBuf,
    pub tui_log_path: PathBuf,
    pub attachments_dir: PathBuf,
    pub home_dir: PathBuf,
}

impl DemoPaths {
    /// Validate the root named by [`DEMO_ROOT_ENV`].
    ///
    /// Returns [`DemoRootError::Unset`] when the variable is absent or blank, so
    /// callers can distinguish "not in demo mode" from a malformed root.
    pub fn from_env() -> Result<Self, DemoRootError> {
        match std::env::var_os(DEMO_ROOT_ENV) {
            Some(value) if !value.to_string_lossy().trim().is_empty() => {
                Self::validate(Path::new(&value))
            }
            _ => Err(DemoRootError::Unset),
        }
    }

    /// Validate an absolute, owned, private root and derive its fixed children.
    pub fn validate(root: &Path) -> Result<Self, DemoRootError> {
        #[cfg(unix)]
        {
            // SAFETY: `geteuid` has no preconditions and cannot fail.
            Self::validate_with_owner(root, unsafe { libc::geteuid() })
        }
        #[cfg(not(unix))]
        {
            Self::validate_with_owner(root, 0)
        }
    }

    /// Validation with an explicit expected owner, so ownership can be tested
    /// without changing the process identity.
    fn validate_with_owner(root: &Path, expected_uid: u32) -> Result<Self, DemoRootError> {
        if root.as_os_str().is_empty() || !root.is_absolute() {
            return Err(DemoRootError::NotAbsolute);
        }
        // Reject lexical traversal before touching the filesystem. A path with
        // `..` could otherwise resolve outside the root the launcher created.
        for component in root.components() {
            if matches!(component, Component::ParentDir | Component::CurDir) {
                return Err(DemoRootError::Traversal);
            }
        }

        let metadata = std::fs::symlink_metadata(root).map_err(DemoRootError::io)?;
        let file_type = metadata.file_type();
        if file_type.is_symlink() {
            return Err(DemoRootError::Symlink);
        }
        if !file_type.is_dir() {
            return Err(DemoRootError::NotADirectory);
        }

        #[cfg(unix)]
        {
            use std::os::unix::fs::MetadataExt;
            if metadata.uid() != expected_uid {
                return Err(DemoRootError::NotOwned {
                    expected: expected_uid,
                    found: metadata.uid(),
                });
            }
            let mode = metadata.mode() & 0o777;
            if mode != 0o700 {
                return Err(DemoRootError::NotPrivate { mode });
            }
        }
        #[cfg(not(unix))]
        {
            let _ = expected_uid;
        }

        let entries = std::fs::read_dir(root).map_err(DemoRootError::io)?;
        for entry in entries {
            let entry = entry.map_err(DemoRootError::io)?;
            let name = entry.file_name();
            let name = name.to_string_lossy();
            if !ALLOWED_TOP_LEVEL.contains(&name.as_ref()) {
                return Err(DemoRootError::UnexpectedEntry {
                    name: name.into_owned(),
                });
            }
            let entry_type = entry.file_type().map_err(DemoRootError::io)?;
            if entry_type.is_symlink() {
                return Err(DemoRootError::UnexpectedSymlink {
                    name: name.into_owned(),
                });
            }
        }

        let paths = Self::derive(root);
        let socket_len = socket_path_len(&paths.socket_path);
        if socket_len > MAX_SOCKET_PATH_LEN {
            return Err(DemoRootError::SocketPathTooLong {
                len: socket_len,
                max: MAX_SOCKET_PATH_LEN,
            });
        }
        Ok(paths)
    }

    fn derive(root: &Path) -> Self {
        Self {
            root: root.to_path_buf(),
            session_file: root.join("demo-session.json"),
            socket_path: root.join("runtime/bloomd.sock"),
            store_path: root.join("data/store.db"),
            tui_config_path: root.join("config/bloom/tui.json"),
            daemon_log_path: root.join("state/bloom/bloomd.log"),
            tui_log_path: root.join("state/bloom/tui.log"),
            attachments_dir: root.join("cache/attachments"),
            home_dir: root.join("home"),
        }
    }
}

#[cfg(unix)]
fn socket_path_len(path: &Path) -> usize {
    use std::os::unix::ffi::OsStrExt;
    path.as_os_str().as_bytes().len()
}

#[cfg(not(unix))]
fn socket_path_len(path: &Path) -> usize {
    path.as_os_str().to_string_lossy().len()
}

/// Why a demo session root was refused.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DemoRootError {
    /// `BLOOM_DEMO_ROOT` is absent or blank.
    Unset,
    /// The root is empty or relative.
    NotAbsolute,
    /// The root contains `.` or `..` components.
    Traversal,
    /// The root exists but is not a directory.
    NotADirectory,
    /// The root itself is a symlink.
    Symlink,
    /// The root is owned by a different user.
    NotOwned { expected: u32, found: u32 },
    /// The root's permission bits are not exactly `0700`.
    NotPrivate { mode: u32 },
    /// A top-level entry is not part of the demo layout.
    UnexpectedEntry { name: String },
    /// A top-level entry is a symlink, even if its name is expected.
    UnexpectedSymlink { name: String },
    /// The derived socket path would not fit in a Unix socket address.
    SocketPathTooLong { len: usize, max: usize },
    /// The root could not be inspected.
    Io(String),
}

impl DemoRootError {
    fn io(error: io::Error) -> Self {
        Self::Io(error.to_string())
    }
}

impl fmt::Display for DemoRootError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Unset => write!(f, "{DEMO_ROOT_ENV} is not set"),
            Self::NotAbsolute => write!(f, "demo root must be an absolute path"),
            Self::Traversal => write!(f, "demo root must not contain traversal components"),
            Self::NotADirectory => write!(f, "demo root is not a directory"),
            Self::Symlink => write!(f, "demo root must not be a symlink"),
            Self::NotOwned { expected, found } => write!(
                f,
                "demo root must be owned by uid {expected}, but is owned by uid {found}"
            ),
            Self::NotPrivate { mode } => {
                write!(f, "demo root must have mode 0700, but has mode {mode:04o}")
            }
            Self::UnexpectedEntry { name } => {
                write!(f, "demo root contains unexpected top-level entry {name:?}")
            }
            Self::UnexpectedSymlink { name } => {
                write!(f, "demo root top-level entry {name:?} is a symlink")
            }
            Self::SocketPathTooLong { len, max } => write!(
                f,
                "demo socket path is {len} bytes, exceeding the {max}-byte limit"
            ),
            Self::Io(error) => write!(f, "cannot inspect demo root: {error}"),
        }
    }
}

impl std::error::Error for DemoRootError {}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    fn layout(root: &Path) {
        for name in ["runtime", "data", "config", "state", "cache", "home"] {
            std::fs::create_dir(root.join(name)).unwrap();
        }
        std::fs::write(root.join("demo-session.json"), b"{}").unwrap();
    }

    fn private_root() -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        std::fs::set_permissions(dir.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
        layout(dir.path());
        dir
    }

    #[test]
    fn valid_root_derives_fixed_paths() {
        let dir = private_root();
        let paths = DemoPaths::validate(dir.path()).unwrap();
        assert_eq!(paths.root, dir.path());
        assert_eq!(paths.session_file, dir.path().join("demo-session.json"));
        assert_eq!(paths.socket_path, dir.path().join("runtime/bloomd.sock"));
        assert_eq!(paths.store_path, dir.path().join("data/store.db"));
        assert_eq!(
            paths.tui_config_path,
            dir.path().join("config/bloom/tui.json")
        );
        assert_eq!(
            paths.daemon_log_path,
            dir.path().join("state/bloom/bloomd.log")
        );
        assert_eq!(paths.tui_log_path, dir.path().join("state/bloom/tui.log"));
        assert_eq!(paths.attachments_dir, dir.path().join("cache/attachments"));
        assert_eq!(paths.home_dir, dir.path().join("home"));
    }

    #[test]
    fn symlink_root_is_rejected() {
        let real = private_root();
        let holder = tempfile::tempdir().unwrap();
        let link = holder.path().join("demo-root");
        std::os::unix::fs::symlink(real.path(), &link).unwrap();
        assert_eq!(DemoPaths::validate(&link), Err(DemoRootError::Symlink));
    }

    #[test]
    fn traversal_is_rejected() {
        assert_eq!(
            DemoPaths::validate(Path::new("/tmp/../etc")),
            Err(DemoRootError::Traversal)
        );
        assert_eq!(
            DemoPaths::validate(Path::new("relative/root")),
            Err(DemoRootError::NotAbsolute)
        );
    }

    #[test]
    fn unexpected_top_level_file_is_rejected() {
        let dir = private_root();
        std::fs::write(dir.path().join("notes.txt"), b"x").unwrap();
        assert_eq!(
            DemoPaths::validate(dir.path()),
            Err(DemoRootError::UnexpectedEntry {
                name: "notes.txt".to_owned()
            })
        );
    }

    #[test]
    fn unexpected_top_level_symlink_is_rejected() {
        let dir = private_root();
        let target = tempfile::tempdir().unwrap();
        std::fs::remove_dir(dir.path().join("runtime")).unwrap();
        std::os::unix::fs::symlink(target.path(), dir.path().join("runtime")).unwrap();
        assert_eq!(
            DemoPaths::validate(dir.path()),
            Err(DemoRootError::UnexpectedSymlink {
                name: "runtime".to_owned()
            })
        );
    }

    #[test]
    fn over_long_socket_path_is_rejected() {
        let holder = tempfile::tempdir().unwrap();
        let root = holder
            .path()
            .join("a".repeat(70))
            .join("b".repeat(40))
            .join("root");
        std::fs::create_dir_all(&root).unwrap();
        std::fs::set_permissions(&root, std::fs::Permissions::from_mode(0o700)).unwrap();
        layout(&root);
        assert!(socket_path_len(&root.join("runtime/bloomd.sock")) > MAX_SOCKET_PATH_LEN);
        assert!(matches!(
            DemoPaths::validate(&root),
            Err(DemoRootError::SocketPathTooLong { .. })
        ));
    }

    #[test]
    fn non_private_root_is_rejected() {
        let dir = private_root();
        std::fs::set_permissions(dir.path(), std::fs::Permissions::from_mode(0o755)).unwrap();
        assert_eq!(
            DemoPaths::validate(dir.path()),
            Err(DemoRootError::NotPrivate { mode: 0o755 })
        );
    }

    #[test]
    fn non_owned_root_is_rejected() {
        let dir = private_root();
        // SAFETY: `geteuid` has no preconditions and cannot fail.
        let expected = unsafe { libc::geteuid() }.wrapping_add(1);
        assert_eq!(
            DemoPaths::validate_with_owner(dir.path(), expected),
            Err(DemoRootError::NotOwned {
                expected,
                found: unsafe { libc::geteuid() },
            })
        );
    }

    #[test]
    fn missing_root_is_io_error() {
        let dir = tempfile::tempdir().unwrap();
        let missing = dir.path().join("absent");
        assert!(matches!(
            DemoPaths::validate(&missing),
            Err(DemoRootError::Io(_))
        ));
    }
}
