//! Daemon configuration.
//!
//! Every value can be overridden from the environment, but the local single-machine
//! setup is discovered automatically so the daemon normally needs no configuration:
//! the engine library and state come from the installed OpenBubbles Flatpak, the
//! sidecar script sits beside the binary or in the repository, and Dart is taken
//! from `PATH` or a known local SDK. Explicit configuration still wins.

use crate::engine::EngineConfig;
use std::{fmt, path::PathBuf};

/// Flatpak application id of the engine provider.
const FLATPAK_APP: &str = "app.openbubbles.OpenBubbles";
/// Sidecar entry point inside the repository, used for development builds.
const REPO_SCRIPT: &str = "../../ext/bloom-engine-sidecar/dart/bin/bloom_engine.dart";
/// Pinned Dart SDK, relative to the checkout root. Kept in the repository so a
/// cache clean cannot remove the engine's interpreter.
const REPO_DART: &str = ".dart-sdk/bin/dart";

#[derive(Clone, Debug)]
pub struct DaemonConfig {
    pub socket_path: PathBuf,
    pub store_path: PathBuf,
    pub engine: EngineConfig,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ConfigError {
    /// A required value was neither set nor discoverable.
    Missing {
        variable: &'static str,
        hint: String,
    },
}

impl fmt::Display for ConfigError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Missing { variable, hint } => write!(f, "{variable} is required: {hint}"),
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

fn home() -> Option<PathBuf> {
    env("HOME").map(PathBuf::from)
}

/// Where OpenBubbles keeps its registration state on Linux.
pub fn default_engine_state() -> Option<PathBuf> {
    let candidate = home()?
        .join(".var/app")
        .join(FLATPAK_APP)
        .join("data/bluebubbles");
    candidate.is_dir().then_some(candidate)
}

/// The engine library inside the installed Flatpak.
pub fn default_engine_library() -> Option<PathBuf> {
    let output = std::process::Command::new("flatpak")
        .args(["info", "--show-location", FLATPAK_APP])
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let root = String::from_utf8(output.stdout).ok()?;
    let candidate =
        PathBuf::from(root.trim()).join("files/bluebubbles/lib/librust_lib_bluebubbles.so");
    candidate.is_file().then_some(candidate)
}

/// The sidecar entry point: environment, installed prefix, legacy sibling, else
/// the repository layout. Shared with the launcher's doctor.
pub fn default_script() -> Option<PathBuf> {
    let executable = std::env::current_exe().unwrap_or_default();
    let checkout = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join(REPO_SCRIPT);
    let script_env = std::env::var_os("BLOOM_ENGINE_SCRIPT");
    bloom_ipc::sidecar_script(&executable, script_env.as_deref(), &checkout)
}

/// Dart from `PATH`, else the checkout's pinned SDK, else a user-local SDK, so
/// no export is normally needed.
pub fn default_dart() -> PathBuf {
    if let Ok(path) = std::env::var("PATH") {
        for directory in std::env::split_paths(&path) {
            let candidate = directory.join("dart");
            if candidate.is_file() {
                return candidate;
            }
        }
    }
    let checkout = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../..")
        .join(REPO_DART);
    if checkout.is_file() {
        return checkout;
    }
    if let Some(home) = home() {
        for relative in [
            ".cache/dart-sdk/dart-sdk/bin/dart",
            ".local/share/bloom/dart/bin/dart",
        ] {
            let candidate = home.join(relative);
            if candidate.is_file() {
                return candidate;
            }
        }
    }
    PathBuf::from("dart")
}

impl DaemonConfig {
    /// Resolve configuration from the environment plus local discovery.
    pub fn from_env() -> Result<Self, ConfigError> {
        let state_dir = env("BLOOM_ENGINE_STATE")
            .map(PathBuf::from)
            .or_else(default_engine_state)
            .ok_or_else(|| ConfigError::Missing {
                variable: "BLOOM_ENGINE_STATE",
                hint: "set it to the OpenBubbles state directory; the default \
                       ~/.var/app/app.openbubbles.OpenBubbles/data/bluebubbles was not found"
                    .to_owned(),
            })?;

        let library = env("BLOOM_ENGINE_LIBRARY")
            .map(PathBuf::from)
            .or_else(default_engine_library)
            .ok_or_else(|| ConfigError::Missing {
                variable: "BLOOM_ENGINE_LIBRARY",
                hint: format!(
                    "install {FLATPAK_APP} with Flatpak, or point this at \
                     librust_lib_bluebubbles.so"
                ),
            })?;

        let script = default_script().ok_or_else(|| ConfigError::Missing {
            variable: "BLOOM_ENGINE_SCRIPT",
            hint: "set it to ext/bloom-engine-sidecar/dart/bin/bloom_engine.dart".to_owned(),
        })?;

        let dart_executable = env("BLOOM_DART_EXECUTABLE")
            .map(PathBuf::from)
            .unwrap_or_else(default_dart);

        let data_dir = env("XDG_DATA_HOME")
            .or_else(|| home().map(|home| format!("{}/.local/share", home.display())))
            .ok_or_else(|| ConfigError::Missing {
                variable: "HOME",
                hint: "neither XDG_DATA_HOME nor HOME is set".to_owned(),
            })?;

        let socket_path = bloom_ipc::socket_path_from_env();
        let store_path = env("BLOOM_STORE_PATH")
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from(format!("{data_dir}/bloom/store.db")));

        Ok(Self {
            socket_path,
            store_path,
            engine: EngineConfig {
                // The daemon sets this when it finds its store empty.
                sync_chats: false,
                dart_executable,
                script,
                state_dir,
                library,
            },
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn missing_values_are_reported_with_a_hint() {
        let error = ConfigError::Missing {
            variable: "BLOOM_ENGINE_STATE",
            hint: "not found".to_owned(),
        };
        let text = error.to_string();
        assert!(text.contains("BLOOM_ENGINE_STATE"));
        assert!(text.contains("not found"));
    }

    #[test]
    fn discovery_returns_none_for_paths_that_do_not_exist() {
        // Discovery must never invent a path; it either finds a real one or defers
        // to the caller's error handling.
        let absent = home().unwrap_or_default().join("bloom-does-not-exist");
        assert!(!absent.is_dir());
        if let Some(found) = default_engine_state() {
            assert!(found.is_dir(), "a discovered state directory must exist");
        }
        if let Some(found) = default_engine_library() {
            assert!(found.is_file(), "a discovered library must exist");
        }
    }

    #[test]
    fn dart_discovery_found_something_usable_or_defers_to_path() {
        let found = default_dart();
        assert!(
            found.file_name().is_some(),
            "dart path must at least name an executable"
        );
    }
}
