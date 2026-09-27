use serde::{Deserialize, Serialize};
use std::{
    collections::HashMap,
    fs::{self, File, OpenOptions},
    io::{self, Write},
    path::{Path, PathBuf},
    sync::atomic::{AtomicU64, Ordering},
};

static TEMP_FILE_SEQUENCE: AtomicU64 = AtomicU64::new(0);

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(default)]
pub struct Config {
    pub version: u32,
    /// `None` follows the detected terminal background.
    pub theme: Option<String>,
    pub compact_sidebar: bool,
    /// Local display labels keyed by canonical participant identity.
    pub nicknames: HashMap<String, String>,
    /// Local conversation titles keyed by canonical conversation identity. Used by
    /// the sidebar's Rename action for group chats, which have no participant to
    /// nickname.
    pub titles: HashMap<String, String>,
    /// Conversations closed locally, keyed by canonical conversation identity. The
    /// messages are untouched; the chat is only hidden from the list.
    pub hidden: Vec<String>,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            version: 1,
            theme: None,
            compact_sidebar: false,
            nicknames: HashMap::new(),
            titles: HashMap::new(),
            hidden: Vec::new(),
        }
    }
}

impl Config {
    pub fn path() -> PathBuf {
        if let Some(directory) = std::env::var_os("XDG_CONFIG_HOME") {
            return PathBuf::from(directory).join("bloom/tui.json");
        }
        #[cfg(windows)]
        if let Some(directory) = std::env::var_os("APPDATA") {
            return PathBuf::from(directory).join("Bloom/tui.json");
        }
        std::env::var_os("HOME")
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from("."))
            .join(".config/bloom/tui.json")
    }

    /// Missing or invalid preferences never prevent the chat client from starting.
    /// A warning is returned for invalid files so the UI can explain the fallback.
    pub fn load(path: &Path) -> (Self, Option<String>) {
        let bytes = match fs::read(path) {
            Ok(bytes) => bytes,
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                return (Self::default(), None);
            }
            Err(error) => {
                return (
                    Self::default(),
                    Some(format!(
                        "Could not read local settings; defaults loaded: {error}"
                    )),
                );
            }
        };
        let mut config = match serde_json::from_slice::<Self>(&bytes) {
            Ok(config) => config,
            Err(error) => {
                return (
                    Self::default(),
                    Some(format!("Invalid local settings; defaults loaded: {error}")),
                );
            }
        };
        if config.version != 1 {
            return (
                Self::default(),
                Some(format!(
                    "Unsupported local settings version {}; defaults loaded.",
                    config.version
                )),
            );
        }
        if config
            .theme
            .as_deref()
            .is_some_and(|theme| !matches!(theme, "dark" | "light"))
        {
            config.theme = None;
            return (
                config,
                Some("Invalid local theme preference; using automatic theme.".into()),
            );
        }
        let original_count = config.nicknames.len();
        config
            .nicknames
            .retain(|_, nickname| valid_nickname(nickname));
        let warning = (config.nicknames.len() != original_count)
            .then(|| "Invalid local nicknames were ignored.".to_owned());
        (config, warning)
    }

    /// Write through an exclusive, private temporary file, then atomically replace
    /// the target. A stale temp file cannot weaken permissions or block future saves.
    pub fn save(&self, path: &Path) -> io::Result<()> {
        let directory = path.parent().unwrap_or_else(|| Path::new("."));
        fs::create_dir_all(directory)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(directory, fs::Permissions::from_mode(0o700))?;
        }

        let file_name = path
            .file_name()
            .and_then(|name| name.to_str())
            .unwrap_or("tui.json");
        let mut temporary_path = None;
        let mut temporary_file = None;
        for _ in 0..16 {
            let sequence = TEMP_FILE_SEQUENCE.fetch_add(1, Ordering::Relaxed);
            let candidate = directory.join(format!(
                ".{file_name}.{}.{}.tmp",
                std::process::id(),
                sequence
            ));
            let mut options = OpenOptions::new();
            options.write(true).create_new(true);
            #[cfg(unix)]
            {
                use std::os::unix::fs::OpenOptionsExt;
                options.mode(0o600);
            }
            match options.open(&candidate) {
                Ok(file) => {
                    temporary_path = Some(candidate);
                    temporary_file = Some(file);
                    break;
                }
                Err(error) if error.kind() == io::ErrorKind::AlreadyExists => continue,
                Err(error) => return Err(error),
            }
        }
        let temporary_path = temporary_path.ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::AlreadyExists,
                "could not allocate a unique settings temp file",
            )
        })?;
        let mut temporary_file = temporary_file.expect("temp path and file are created together");

        let result = (|| {
            serde_json::to_writer_pretty(&mut temporary_file, self).map_err(io::Error::other)?;
            temporary_file.write_all(b"\n")?;
            temporary_file.sync_all()?;
            drop(temporary_file);
            fs::rename(&temporary_path, path)?;
            if let Ok(directory) = File::open(directory) {
                let _ = directory.sync_all();
            }
            Ok(())
        })();
        if result.is_err() {
            let _ = fs::remove_file(temporary_path);
        }
        result
    }
}

pub fn valid_nickname(value: &str) -> bool {
    let value = value.trim();
    !value.is_empty() && value.chars().count() <= 80 && !value.chars().any(char::is_control)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_roundtrip_and_invalid_files_fall_back() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("settings.json");
        assert_eq!(Config::load(&path), (Config::default(), None));

        let mut config = Config {
            theme: Some("light".into()),
            compact_sidebar: true,
            ..Config::default()
        };
        config
            .nicknames
            .insert("email:person@example.test".into(), "Maya".into());
        config.save(&path).unwrap();
        assert_eq!(Config::load(&path), (config, None));

        fs::write(&path, b"{").unwrap();
        let (fallback, warning) = Config::load(&path);
        assert_eq!(fallback, Config::default());
        assert!(warning.is_some());
    }

    #[test]
    fn unsupported_version_and_invalid_theme_are_handled_safely() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("settings.json");
        fs::write(&path, r#"{"version":99}"#).unwrap();
        let (fallback, warning) = Config::load(&path);
        assert_eq!(fallback, Config::default());
        assert!(warning.unwrap().contains("Unsupported"));

        fs::write(&path, r#"{"version":1,"theme":"solarized"}"#).unwrap();
        let (config, warning) = Config::load(&path);
        assert_eq!(config.theme, None);
        assert!(warning.unwrap().contains("theme"));
    }

    #[test]
    fn nickname_validation_rejects_empty_controls_and_overlong_values() {
        assert!(valid_nickname("Maya"));
        assert!(!valid_nickname("  "));
        assert!(!valid_nickname("line\nbreak"));
        assert!(!valid_nickname(&"x".repeat(81)));
    }

    #[cfg(unix)]
    #[test]
    fn saves_private_config_files() {
        use std::os::unix::fs::PermissionsExt;
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("settings.json");
        Config::default().save(&path).unwrap();
        assert_eq!(
            fs::metadata(path).unwrap().permissions().mode() & 0o777,
            0o600
        );
    }
}
