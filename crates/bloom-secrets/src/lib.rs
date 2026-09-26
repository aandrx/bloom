//! Secure storage abstraction for Bloom credentials.
//!
//! The OS keychain protects secrets at rest according to the host's security
//! guarantees. Callers must still protect values in memory and avoid exposing
//! them through logs, serialization, or diagnostics. Key names are metadata and
//! should not themselves contain sensitive information. Keychain access fails
//! closed: errors are returned rather than silently falling back to weaker
//! storage.

use std::{error::Error as StdError, fmt};

// Match bloom-core's existing keychain service so migration retains stored keys.
const SERVICE_NAME: &str = "bloom";

/// Errors returned by secret storage backends. Error values never include
/// secret bytes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SecretStoreError {
    /// The operating system keychain could not be accessed or updated.
    BackendUnavailable,
}

impl fmt::Display for SecretStoreError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::BackendUnavailable => f.write_str("OS keychain unavailable"),
        }
    }
}

impl StdError for SecretStoreError {}

/// Store and retrieve opaque binary secrets under non-secret keys.
pub trait SecretStore: Send + Sync {
    /// Return `None` only when the key does not exist.
    fn get(&self, key: &str) -> Result<Option<Vec<u8>>, SecretStoreError>;

    /// Store or replace a secret.
    fn set(&self, key: &str, value: &[u8]) -> Result<(), SecretStoreError>;

    /// Delete a secret if present. Backends may report that deletion is
    /// unsupported; they must not claim success without deleting it.
    fn delete(&self, _key: &str) -> Result<(), SecretStoreError> {
        Err(SecretStoreError::BackendUnavailable)
    }
}

/// OS keychain-backed implementation using the existing `bloom` service namespace.
pub struct OsKeyring;

impl SecretStore for OsKeyring {
    fn get(&self, key: &str) -> Result<Option<Vec<u8>>, SecretStoreError> {
        match keyring::Entry::new(SERVICE_NAME, key)
            .map_err(|_| SecretStoreError::BackendUnavailable)?
            .get_secret()
        {
            Ok(value) => Ok(Some(value)),
            Err(keyring::Error::NoEntry) => Ok(None),
            Err(_) => Err(SecretStoreError::BackendUnavailable),
        }
    }

    fn set(&self, key: &str, value: &[u8]) -> Result<(), SecretStoreError> {
        keyring::Entry::new(SERVICE_NAME, key)
            .map_err(|_| SecretStoreError::BackendUnavailable)?
            .set_secret(value)
            .map_err(|_| SecretStoreError::BackendUnavailable)
    }

    fn delete(&self, key: &str) -> Result<(), SecretStoreError> {
        match keyring::Entry::new(SERVICE_NAME, key)
            .map_err(|_| SecretStoreError::BackendUnavailable)?
            .delete_credential()
        {
            Ok(()) | Err(keyring::Error::NoEntry) => Ok(()),
            Err(_) => Err(SecretStoreError::BackendUnavailable),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{collections::HashMap, sync::Mutex};

    #[derive(Default)]
    struct MemoryStore(Mutex<HashMap<String, Vec<u8>>>);

    impl SecretStore for MemoryStore {
        fn get(&self, key: &str) -> Result<Option<Vec<u8>>, SecretStoreError> {
            Ok(self.0.lock().unwrap().get(key).cloned())
        }

        fn set(&self, key: &str, value: &[u8]) -> Result<(), SecretStoreError> {
            self.0
                .lock()
                .unwrap()
                .insert(key.to_owned(), value.to_vec());
            Ok(())
        }

        fn delete(&self, key: &str) -> Result<(), SecretStoreError> {
            self.0.lock().unwrap().remove(key);
            Ok(())
        }
    }

    #[test]
    fn stores_arbitrary_binary_values_and_deletes_them() {
        let store = MemoryStore::default();
        let value = [0, 255, 1, 0, 128];
        store.set("session", &value).unwrap();
        assert_eq!(store.get("session").unwrap(), Some(value.to_vec()));
        store.delete("session").unwrap();
        assert_eq!(store.get("session").unwrap(), None);
    }

    #[test]
    fn default_delete_fails_closed_when_unsupported() {
        struct ReadWriteOnly;
        impl SecretStore for ReadWriteOnly {
            fn get(&self, _: &str) -> Result<Option<Vec<u8>>, SecretStoreError> {
                Ok(None)
            }
            fn set(&self, _: &str, _: &[u8]) -> Result<(), SecretStoreError> {
                Ok(())
            }
        }
        assert_eq!(
            ReadWriteOnly.delete("session"),
            Err(SecretStoreError::BackendUnavailable)
        );
        assert_eq!(SERVICE_NAME, "bloom");
    }
}
