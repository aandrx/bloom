//! Daemon configuration, taken from the environment only.

use crate::engine::EngineConfig;
use std::{fmt, path::PathBuf};

#[derive(Clone, Debug)]
pub struct DaemonConfig {
    pub socket_path: PathBuf,
    pub store_path: PathBuf,
    pub engine: EngineConfig,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ConfigError {
    Missing(&'static str),
}

impl fmt::Display for ConfigError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Missing(name) => write!(f, "{name} is required"),
        }
    }
}

impl std::error::Error for ConfigError {}

fn env(name: &str) -> Option<String> {
    match std::env::var(name) {
        Ok(value) if !value.trim().is_empty() => Some(value),
        _ => None,
    }
}

impl DaemonConfig {
    /// `BLOOM_ENGINE_STATE`, `BLOOM_ENGINE_LIBRARY` and `BLOOM_ENGINE_SCRIPT` are
    /// required; there are no built-in paths. Socket and store locations follow the
    /// XDG directories unless overridden.
    pub fn from_env() -> Result<Self, ConfigError> {
        let state_dir =
            env("BLOOM_ENGINE_STATE").ok_or(ConfigError::Missing("BLOOM_ENGINE_STATE"))?;
        let library =
            env("BLOOM_ENGINE_LIBRARY").ok_or(ConfigError::Missing("BLOOM_ENGINE_LIBRARY"))?;
        let script =
            env("BLOOM_ENGINE_SCRIPT").ok_or(ConfigError::Missing("BLOOM_ENGINE_SCRIPT"))?;
        let dart_executable = env("BLOOM_DART_EXECUTABLE").unwrap_or_else(|| "dart".to_owned());

        let runtime_dir = env("XDG_RUNTIME_DIR").unwrap_or_else(|| "/tmp".to_owned());
        let data_dir = env("XDG_DATA_HOME")
            .or_else(|| env("HOME").map(|home| format!("{home}/.local/share")))
            .ok_or(ConfigError::Missing("HOME"))?;

        let socket_path = env("BLOOM_SOCKET_PATH")
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from(format!("{runtime_dir}/bloom/bloomd.sock")));
        let store_path = env("BLOOM_STORE_PATH")
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from(format!("{data_dir}/bloom/store.db")));

        Ok(Self {
            socket_path,
            store_path,
            engine: EngineConfig {
                dart_executable: PathBuf::from(dart_executable),
                script: PathBuf::from(script),
                state_dir: PathBuf::from(state_dir),
                library: PathBuf::from(library),
            },
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn missing_engine_variables_are_reported_by_name() {
        // This test must not depend on the ambient environment, so it only asserts
        // the error type carries a variable name when one is missing.
        let error = ConfigError::Missing("BLOOM_ENGINE_STATE");
        assert_eq!(error.to_string(), "BLOOM_ENGINE_STATE is required");
    }
}
