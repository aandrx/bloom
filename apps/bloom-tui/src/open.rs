//! Explicit external opening for downloaded attachments.
//!
//! Bloom never opens downloaded content automatically. The shell calls this only
//! after the user runs `/open` and the daemon returns a path for a token that
//! this session requested. The file is passed as a single argv element, so a
//! reported filename can never become shell syntax.

use std::path::{Component, Path, PathBuf};
use std::process::{Command, Stdio};

/// Optional override for the desktop opener. The value is a program name or
/// path with no arguments; Bloom never runs it through a shell.
pub const OPENER_ENV: &str = "BLOOM_OPENER";
const DEFAULT_OPENER: &str = "xdg-open";

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum OpenError {
    /// The daemon reported a path that is not absolute or contains `..`.
    InvalidPath,
    /// The file changed type, vanished or became empty since it was fetched.
    Changed,
    /// The path is not a regular file any more.
    Unavailable,
    /// The configured opener could not be started.
    OpenerMissing,
}

impl OpenError {
    /// User-facing text. It deliberately omits the path: the save notice
    /// already showed it, and failures do not repeat attacker-influenced input.
    pub fn message(&self) -> String {
        match self {
            Self::InvalidPath => "Refusing to open an attachment outside an absolute path.".into(),
            Self::Changed => {
                "Attachment file changed on disk; download it again before opening.".into()
            }
            Self::Unavailable => {
                "Attachment file is no longer available; download it again.".into()
            }
            Self::OpenerMissing => {
                "No file opener is available. Install xdg-open or set BLOOM_OPENER.".into()
            }
        }
    }
}

fn resolve_opener(value: Option<&str>) -> String {
    value
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .unwrap_or(DEFAULT_OPENER)
        .to_owned()
}

fn checked_path(path: &str) -> Result<PathBuf, OpenError> {
    let path = Path::new(path);
    if !path.is_absolute()
        || path
            .components()
            .any(|component| matches!(component, Component::ParentDir))
    {
        return Err(OpenError::InvalidPath);
    }
    match std::fs::symlink_metadata(path) {
        Ok(metadata) if metadata.file_type().is_symlink() => Err(OpenError::Changed),
        Ok(metadata) if !metadata.is_file() => Err(OpenError::Unavailable),
        Ok(metadata) if metadata.len() == 0 => Err(OpenError::Changed),
        Ok(_) => Ok(path.to_path_buf()),
        Err(_) => Err(OpenError::Unavailable),
    }
}

/// Opens `path` with the configured desktop opener.
///
/// Returns as soon as the opener process starts; it is reaped on a detached
/// thread so a long-lived viewer cannot block the UI. Success means the opener
/// was launched, not that it displayed anything.
pub fn open_external(path: &str) -> Result<(), OpenError> {
    let value = std::env::var(OPENER_ENV).ok();
    open_external_with(path, value.as_deref())
}

fn open_external_with(path: &str, opener: Option<&str>) -> Result<(), OpenError> {
    let path = checked_path(path)?;
    let program = resolve_opener(opener);
    let mut child = Command::new(&program)
        .arg(&path)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|_| OpenError::OpenerMissing)?;
    std::thread::spawn(move || {
        let _ = child.wait();
    });
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use std::os::unix::fs::symlink;

    fn temp_file(contents: &[u8]) -> (tempfile::TempDir, String) {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("attachment.bin");
        let mut file = std::fs::File::create(&path).unwrap();
        file.write_all(contents).unwrap();
        let path = path.to_string_lossy().into_owned();
        (directory, path)
    }

    #[test]
    fn opener_defaults_and_honours_only_a_non_blank_override() {
        assert_eq!(resolve_opener(None), "xdg-open");
        assert_eq!(resolve_opener(Some("   ")), "xdg-open");
        assert_eq!(resolve_opener(Some(" /tmp/recorder ")), "/tmp/recorder");
    }

    #[test]
    fn accepts_an_absolute_regular_nonempty_file() {
        let (_directory, path) = temp_file(b"payload");
        assert_eq!(checked_path(&path), Ok(PathBuf::from(&path)));
    }

    #[test]
    fn rejects_relative_and_parent_paths() {
        assert_eq!(checked_path("photo.jpg"), Err(OpenError::InvalidPath));
        assert_eq!(
            checked_path("/tmp/../etc/passwd"),
            Err(OpenError::InvalidPath)
        );
    }

    #[test]
    fn rejects_missing_directories_symlinks_and_empty_files() {
        let (directory, path) = temp_file(b"payload");
        let link = directory.path().join("link.bin");
        symlink(&path, &link).unwrap();
        assert_eq!(
            checked_path(&link.to_string_lossy()),
            Err(OpenError::Changed)
        );
        assert_eq!(
            checked_path(&directory.path().to_string_lossy()),
            Err(OpenError::Unavailable)
        );
        assert_eq!(
            checked_path(&directory.path().join("missing").to_string_lossy()),
            Err(OpenError::Unavailable)
        );

        std::fs::File::create(&path).unwrap();
        assert_eq!(checked_path(&path), Err(OpenError::Changed));
    }

    #[test]
    fn reports_a_missing_opener_without_leaking_the_path() {
        let (_directory, path) = temp_file(b"payload");
        let error = open_external_with(&path, Some("/nonexistent/bloom-opener")).unwrap_err();
        assert_eq!(error, OpenError::OpenerMissing);
        assert!(!error.message().contains("nonexistent"));
        assert!(!error.message().contains("attachment.bin"));
    }

    #[test]
    fn launches_the_opener_with_exactly_one_argument() {
        use std::os::unix::fs::PermissionsExt;
        use std::time::{Duration, Instant};

        let (directory, path) = temp_file(b"payload");
        let record = directory.path().join("argv.txt");
        let script = directory.path().join("recorder.sh");
        std::fs::write(
            &script,
            format!(
                "#!/bin/sh\nprintf '%s|%s' \"$#\" \"$1\" > '{}'\n",
                record.display()
            ),
        )
        .unwrap();
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o700)).unwrap();

        open_external_with(&path, Some(&script.to_string_lossy())).unwrap();

        let expected = format!("1|{path}");
        let deadline = Instant::now() + Duration::from_secs(5);
        while Instant::now() < deadline {
            if std::fs::read_to_string(&record).ok().as_deref() == Some(&expected) {
                break;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        assert_eq!(std::fs::read_to_string(&record).unwrap(), expected);
    }

    #[test]
    fn failure_messages_name_no_paths() {
        for error in [
            OpenError::InvalidPath,
            OpenError::Changed,
            OpenError::Unavailable,
            OpenError::OpenerMissing,
        ] {
            assert!(!error.message().contains('/'));
        }
    }
}
