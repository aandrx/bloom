//! Encrypted-at-rest SQLite store.

mod conversations;
mod ingest;
mod messages;
pub use conversations::{get_conversation, list_conversations, upsert_conversation};
pub use ingest::{ingest_inbound, ingest_message, update_delivery};
pub use messages::{get_message, history, upsert_message, MessageUpsertOutcome};

use bloom_secrets::{SecretStore, SecretStoreError};
use rusqlite::{Connection, OpenFlags};
use std::{
    error::Error,
    fmt, fs,
    path::{Path, PathBuf},
};
use zeroize::Zeroize;

const KEY_BYTES: usize = 32;
const STORE_KEY_NAME: &str = "bloomd.sqlite.v1";

/// Fill a buffer from the OS CSPRNG. Used only to provision a new store key.
fn generate_key() -> Result<[u8; KEY_BYTES], StoreError> {
    let mut key = [0u8; KEY_BYTES];
    #[cfg(unix)]
    {
        let mut filled = 0usize;
        while filled < key.len() {
            let written = unsafe {
                libc::getrandom(key[filled..].as_mut_ptr().cast(), key.len() - filled, 0)
            };
            if written < 0 {
                let error = std::io::Error::last_os_error();
                if error.kind() == std::io::ErrorKind::Interrupted {
                    continue;
                }
                return Err(StoreError::KeySetup);
            }
            filled += written as usize;
        }
        Ok(key)
    }
    #[cfg(not(unix))]
    {
        Err(StoreError::UnsupportedPlatform)
    }
}

pub enum StoreError {
    Io(std::io::Error),
    Secret(SecretStoreError),
    InvalidKey,
    KeyUnavailable,
    Sqlite(rusqlite::Error),
    SqlCipherUnavailable,
    InvalidDatabase,
    UnsafeParent,
    UnsafePath,
    KeySetup,
    DecryptFailed,
    Schema,
    MessageConversationMismatch,
    UnsupportedSchemaVersion,
    UnsupportedPlatform,
}

impl fmt::Debug for StoreError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Io(_) => "StoreError::Io(<redacted>)",
            Self::Secret(_) => "StoreError::Secret(<redacted>)",
            Self::InvalidKey => "StoreError::InvalidKey",
            Self::KeyUnavailable => "StoreError::KeyUnavailable",
            Self::Sqlite(_) => "StoreError::Sqlite(<redacted>)",
            Self::SqlCipherUnavailable => "StoreError::SqlCipherUnavailable",
            Self::InvalidDatabase => "StoreError::InvalidDatabase",
            Self::UnsafeParent => "StoreError::UnsafeParent",
            Self::UnsafePath => "StoreError::UnsafePath",
            Self::KeySetup => "StoreError::KeySetup",
            Self::DecryptFailed => "StoreError::DecryptFailed",
            Self::Schema => "StoreError::Schema",
            Self::MessageConversationMismatch => "StoreError::MessageConversationMismatch",
            Self::UnsupportedSchemaVersion => "StoreError::UnsupportedSchemaVersion",
            Self::UnsupportedPlatform => "StoreError::UnsupportedPlatform",
        })
    }
}
impl fmt::Display for StoreError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Io(_) => f.write_str("store filesystem operation failed"),
            Self::Secret(_) => f.write_str("store key service failed"),
            Self::InvalidKey => f.write_str("store key must be 32 bytes"),
            Self::KeyUnavailable => f.write_str("store key unavailable"),
            Self::Sqlite(_) => f.write_str("encrypted store database operation failed"),
            Self::SqlCipherUnavailable => f.write_str("SQLCipher support unavailable"),
            Self::InvalidDatabase => f.write_str("store database is invalid or key is incorrect"),
            Self::UnsafeParent => f.write_str("store directory is unsafe"),
            Self::UnsafePath => f.write_str("store path is unsafe"),
            Self::KeySetup => f.write_str("encrypted store key setup failed"),
            Self::DecryptFailed => f.write_str("encrypted store authentication failed"),
            Self::Schema => f.write_str("encrypted store schema setup failed"),
            Self::MessageConversationMismatch => {
                f.write_str("message belongs to a different conversation")
            }
            Self::UnsupportedSchemaVersion => f.write_str("store schema version is unsupported"),
            Self::UnsupportedPlatform => f.write_str("encrypted store requires Unix permissions"),
        }
    }
}
impl Error for StoreError {}
impl From<std::io::Error> for StoreError {
    fn from(e: std::io::Error) -> Self {
        Self::Io(e)
    }
}
impl From<rusqlite::Error> for StoreError {
    fn from(e: rusqlite::Error) -> Self {
        Self::Sqlite(e)
    }
}

#[cfg(test)]
pub(crate) mod tests_support {
    use bloom_secrets::{SecretStore, SecretStoreError};
    use std::sync::Mutex;

    /// In-memory secret store so tests never touch the real keychain.
    #[derive(Default)]
    pub struct MemorySecrets(Mutex<Vec<(String, Vec<u8>)>>);

    impl MemorySecrets {
        pub fn is_empty(&self) -> bool {
            self.0.lock().unwrap().is_empty()
        }

        pub fn len(&self) -> usize {
            self.0.lock().unwrap().len()
        }
    }

    impl SecretStore for MemorySecrets {
        fn get(&self, key: &str) -> Result<Option<Vec<u8>>, SecretStoreError> {
            Ok(self
                .0
                .lock()
                .unwrap()
                .iter()
                .find(|(name, _)| name == key)
                .map(|(_, value)| value.clone()))
        }

        fn set(&self, key: &str, value: &[u8]) -> Result<(), SecretStoreError> {
            let mut guard = self.0.lock().unwrap();
            guard.retain(|(name, _)| name != key);
            guard.push((key.to_owned(), value.to_vec()));
            Ok(())
        }
    }
}

pub struct EncryptedStore {
    _connection: Connection,
    path: PathBuf,
}

impl EncryptedStore {
    /// Open the store, provisioning a keychain-backed key on first use.
    ///
    /// The key never leaves the OS keychain and there is no plaintext fallback: if
    /// the keychain refuses the write, startup fails rather than using a weaker key.
    pub fn open_or_create(
        path: impl AsRef<Path>,
        secrets: &dyn SecretStore,
    ) -> Result<Self, StoreError> {
        match secrets.get(STORE_KEY_NAME).map_err(StoreError::Secret)? {
            Some(mut key) => {
                let result = Self::open_with_key(path, &key);
                key.zeroize();
                result
            }
            None => {
                let mut key = generate_key()?;
                let stored = secrets
                    .set(STORE_KEY_NAME, &key)
                    .map_err(|_| StoreError::KeySetup);
                let result = stored.and_then(|()| Self::open_with_key(path, &key));
                key.zeroize();
                result
            }
        }
    }

    pub fn open(path: impl AsRef<Path>, secrets: &dyn SecretStore) -> Result<Self, StoreError> {
        let mut key = secrets
            .get(STORE_KEY_NAME)
            .map_err(StoreError::Secret)?
            .ok_or(StoreError::KeyUnavailable)?;
        let result = Self::open_with_key(path, &key);
        key.zeroize();
        result
    }

    pub fn open_with_key(path: impl AsRef<Path>, key: &[u8]) -> Result<Self, StoreError> {
        if key.len() != KEY_BYTES {
            return Err(StoreError::InvalidKey);
        }
        let path = path.as_ref().to_path_buf();
        let parent = path.parent().unwrap_or_else(|| Path::new("."));
        ensure_private_parent(parent)?;
        let parent = fs::canonicalize(parent)?;
        #[cfg(unix)]
        if parent == Path::new("/tmp") {
            return Err(StoreError::UnsafeParent);
        }
        let path = parent.join(path.file_name().ok_or(StoreError::UnsafePath)?);
        let meta = match fs::symlink_metadata(&path) {
            Ok(meta) => {
                if meta.file_type().is_symlink() || !meta.is_file() {
                    return Err(StoreError::UnsafePath);
                }
                Some(meta)
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => None,
            Err(e) => return Err(e.into()),
        };
        if meta.is_none() {
            create_private_file(&path)?;
        }
        let flags = OpenFlags::SQLITE_OPEN_READ_WRITE
            | OpenFlags::SQLITE_OPEN_CREATE
            | OpenFlags::SQLITE_OPEN_NOFOLLOW;
        let connection = Connection::open_with_flags(&path, flags)?;
        restrict_permissions(&path, false)?;
        let mut hex = hex_key(key);
        let mut pragma_key = format!("x'{hex}'");
        hex.zeroize();
        let key_result = connection.pragma_update(None, "key", &pragma_key);
        pragma_key.zeroize();
        key_result.map_err(|_| StoreError::KeySetup)?;
        let cipher: Result<String, _> =
            connection.query_row("PRAGMA cipher_version", [], |r| r.get(0));
        if !matches!(cipher, Ok(ref version) if !version.is_empty()) {
            return Err(StoreError::SqlCipherUnavailable);
        }
        connection.busy_timeout(std::time::Duration::from_secs(5))?;
        let check: Result<i64, _> =
            connection.query_row("SELECT count(*) FROM sqlite_master", [], |r| r.get(0));
        if check.is_err() {
            return Err(StoreError::DecryptFailed);
        }
        let version_table: bool = connection.query_row("SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type='table' AND name='schema_version')", [], |r| r.get(0))?;
        if version_table {
            let versions: Result<Vec<i64>, _> = connection
                .prepare("SELECT version FROM schema_version")?
                .query_map([], |r| r.get(0))?
                .collect();
            let versions = versions.map_err(|_| StoreError::InvalidDatabase)?;
            if versions.as_slice() != [1] {
                return Err(StoreError::UnsupportedSchemaVersion);
            }
        }
        connection.pragma_update(None, "foreign_keys", "ON")?;
        connection.pragma_update(None, "journal_mode", "WAL")?;
        connection.pragma_update(None, "synchronous", "FULL")?;
        connection.execute_batch(
            "BEGIN IMMEDIATE;
             CREATE TABLE IF NOT EXISTS schema_version (version INTEGER NOT NULL);
             INSERT INTO schema_version(version) SELECT 1 WHERE NOT EXISTS (SELECT 1 FROM schema_version);
             CREATE TABLE IF NOT EXISTS conversations (id TEXT PRIMARY KEY, created_at INTEGER NOT NULL, last_message_at INTEGER, data BLOB NOT NULL);
             CREATE INDEX IF NOT EXISTS conversations_last_message_order ON conversations(last_message_at DESC, id ASC);
             CREATE TABLE IF NOT EXISTS messages (id TEXT PRIMARY KEY, conversation_id TEXT NOT NULL REFERENCES conversations(id), created_at INTEGER NOT NULL, data BLOB NOT NULL);
             CREATE INDEX IF NOT EXISTS messages_conversation_history ON messages(conversation_id, created_at DESC, id DESC);
             COMMIT;"
        ).map_err(|_| StoreError::Schema)?;
        restrict_permissions(&path, false)?;
        restrict_sidecars(&path)?;
        Ok(Self {
            _connection: connection,
            path,
        })
    }

    pub fn path(&self) -> &Path {
        &self.path
    }
}

fn ensure_private_parent(parent: &Path) -> Result<(), StoreError> {
    match fs::symlink_metadata(parent) {
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            #[cfg(unix)]
            {
                use std::os::unix::fs::DirBuilderExt;
                fs::DirBuilder::new().mode(0o700).create(parent)?;
            }
            #[cfg(not(unix))]
            fs::create_dir(parent)?;
            restrict_permissions(parent, true)?;
        }
        Err(e) => return Err(e.into()),
        Ok(meta) => {
            if meta.file_type().is_symlink() || !meta.is_dir() {
                return Err(StoreError::UnsafeParent);
            }
            #[cfg(unix)]
            {
                use std::os::unix::fs::{MetadataExt, PermissionsExt};
                if meta.uid() != unsafe { libc::geteuid() }
                    || meta.permissions().mode() & 0o777 != 0o700
                {
                    return Err(StoreError::UnsafeParent);
                }
            }
        }
    }
    Ok(())
}

#[cfg(unix)]
fn restrict_permissions(path: &Path, directory: bool) -> Result<(), StoreError> {
    use std::os::unix::fs::PermissionsExt;
    let mode = if directory { 0o700 } else { 0o600 };
    fs::set_permissions(path, fs::Permissions::from_mode(mode))?;
    Ok(())
}
#[cfg(not(unix))]
fn restrict_permissions(_: &Path, _: bool) -> Result<(), StoreError> {
    Err(StoreError::UnsupportedPlatform)
}

#[cfg(unix)]
fn create_private_file(path: &Path) -> Result<(), StoreError> {
    use std::{fs::OpenOptions, os::unix::fs::OpenOptionsExt};
    OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(path)?;
    Ok(())
}
#[cfg(not(unix))]
fn create_private_file(_: &Path) -> Result<(), StoreError> {
    Err(StoreError::UnsupportedPlatform)
}

fn hex_key(key: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut out = String::with_capacity(key.len() * 2);
    for byte in key {
        out.push(HEX[(byte >> 4) as usize] as char);
        out.push(HEX[(byte & 15) as usize] as char);
    }
    out
}

fn restrict_sidecars(path: &Path) -> Result<(), StoreError> {
    for suffix in ["-wal", "-shm"] {
        let mut name = path.as_os_str().to_os_string();
        name.push(suffix);
        let sidecar = PathBuf::from(name);
        if sidecar.exists() {
            restrict_permissions(&sidecar, false)?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use bloom_secrets::SecretStoreError;
    use std::{collections::HashMap, sync::Mutex};

    #[derive(Default)]
    struct FakeSecrets(Mutex<HashMap<String, Vec<u8>>>);
    impl SecretStore for FakeSecrets {
        fn get(&self, key: &str) -> Result<Option<Vec<u8>>, SecretStoreError> {
            Ok(self.0.lock().unwrap().get(key).cloned())
        }
        fn set(&self, key: &str, value: &[u8]) -> Result<(), SecretStoreError> {
            self.0.lock().unwrap().insert(key.into(), value.into());
            Ok(())
        }
    }

    #[test]
    fn encrypted_roundtrip_wrong_key_and_permissions() {
        let dir = tempfile::tempdir().unwrap();
        let private_dir = dir.path().join("bloom");
        let path = private_dir.join("store.db");
        let key = [7u8; KEY_BYTES];
        let sentinel = "unique-secret-sentinel-bloomd-b2a";
        #[cfg(unix)]
        let original_parent_mode = {
            use std::os::unix::fs::PermissionsExt;
            fs::metadata(dir.path()).unwrap().permissions().mode() & 0o777
        };
        {
            let store = EncryptedStore::open_with_key(&path, &key).unwrap();
            store
                ._connection
                .execute(
                    "INSERT INTO conversations(id, created_at, data) VALUES (?1, 0, ?2)",
                    rusqlite::params![sentinel, sentinel.as_bytes()],
                )
                .unwrap();
        }
        for suffix in ["", "-wal", "-shm"] {
            let mut name = path.as_os_str().to_os_string();
            name.push(suffix);
            if let Ok(bytes) = fs::read(PathBuf::from(name)) {
                assert!(!bytes
                    .windows(sentinel.len())
                    .any(|w| w == sentinel.as_bytes()));
            }
        }
        EncryptedStore::open_with_key(&path, &key).unwrap();
        let wrong = match EncryptedStore::open_with_key(&path, &[8u8; KEY_BYTES]) {
            Ok(_) => panic!("wrong key unexpectedly opened database"),
            Err(error) => error,
        };
        assert!(format!("{wrong:?}").find("080808").is_none());
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(
                fs::metadata(&private_dir).unwrap().permissions().mode() & 0o777,
                0o700
            );
            assert_eq!(
                fs::metadata(dir.path()).unwrap().permissions().mode() & 0o777,
                original_parent_mode
            );
            assert_eq!(
                fs::metadata(&path).unwrap().permissions().mode() & 0o777,
                0o600
            );
        }
        let fake = FakeSecrets::default();
        assert!(matches!(
            EncryptedStore::open(dir.path().join("missing.db"), &fake),
            Err(StoreError::KeyUnavailable)
        ));
    }

    #[test]
    fn rejects_unsafe_parents_and_database_symlinks() {
        let outside = tempfile::tempdir().unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = outside.path().join("shared");
            fs::create_dir(&mode).unwrap();
            fs::set_permissions(&mode, fs::Permissions::from_mode(0o777)).unwrap();
            assert!(matches!(
                EncryptedStore::open_with_key(mode.join("x.db"), &[1; KEY_BYTES]),
                Err(StoreError::UnsafeParent)
            ));
            assert_eq!(
                fs::metadata(&mode).unwrap().permissions().mode() & 0o777,
                0o777
            );
            let target = outside.path().join("target");
            fs::write(&target, b"not db").unwrap();
            let private_dir = outside.path().join("bloom");
            fs::create_dir(&private_dir).unwrap();
            fs::set_permissions(&private_dir, fs::Permissions::from_mode(0o700)).unwrap();
            let link = private_dir.join("link.db");
            std::os::unix::fs::symlink(target, &link).unwrap();
            assert!(matches!(
                EncryptedStore::open_with_key(link, &[1; KEY_BYTES]),
                Err(StoreError::UnsafePath)
            ));
        }
    }

    #[test]
    fn open_or_create_provisions_a_key_once_and_reuses_it() {
        use crate::store::tests_support::MemorySecrets;

        let owner = tempfile::tempdir().unwrap();
        let private = owner.path().join("private");
        fs::create_dir(&private).unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(&private, fs::Permissions::from_mode(0o700)).unwrap();
        }
        let path = private.join("store.db");
        let secrets = MemorySecrets::default();

        assert!(secrets.is_empty(), "no key exists before first open");
        let store = EncryptedStore::open_or_create(&path, &secrets).unwrap();
        assert_eq!(secrets.len(), 1, "first open provisions exactly one key");
        drop(store);

        // Reopening reuses the same key, so existing data stays readable.
        EncryptedStore::open_or_create(&path, &secrets).unwrap();
        assert_eq!(
            secrets.len(),
            1,
            "reopening must not provision a second key"
        );
    }

    #[test]
    fn rejects_unsupported_schema_version() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("bloom/store.db");
        EncryptedStore::open_with_key(&path, &[3; KEY_BYTES])
            .unwrap()
            ._connection
            .execute("UPDATE schema_version SET version=2", [])
            .unwrap();
        assert!(matches!(
            EncryptedStore::open_with_key(path, &[3; KEY_BYTES]),
            Err(StoreError::UnsupportedSchemaVersion)
        ));
    }
}
