use bloom_ipc::client::ClientError;
use std::{
    fs::{self, OpenOptions},
    io::Write,
    path::PathBuf,
    time::Duration,
};

const MAX_BYTES: u64 = 256 * 1024;
const FILE: &str = "tui.log";

pub fn path() -> Option<PathBuf> {
    #[cfg(target_os = "macos")]
    let base = std::env::var_os("HOME")
        .map(PathBuf::from)?
        .join("Library/Logs");
    #[cfg(target_os = "windows")]
    let base = std::env::var_os("LOCALAPPDATA").map(PathBuf::from)?;
    #[cfg(not(any(target_os = "macos", target_os = "windows")))]
    let base = std::env::var_os("XDG_STATE_HOME")
        .map(PathBuf::from)
        .or_else(|| {
            std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".local/state"))
        })?;
    Some(base.join("bloom").join(FILE))
}

#[derive(Clone, Copy, Debug)]
pub enum Stage {
    Send,
    StartConversation,
}
impl Stage {
    fn name(self) -> &'static str {
        match self {
            Self::Send => "send",
            Self::StartConversation => "start_conversation",
        }
    }
}

pub fn safe_reason(error: &ClientError) -> &'static str {
    match error {
        ClientError::Connect => "Could not connect to Bloom. The message was not sent.",
        ClientError::Io | ClientError::Codec(_) | ClientError::WrongRequestId | ClientError::UnexpectedResponse => "Bloom communication failed. Delivery is uncertain; check the conversation before retrying.",
        ClientError::Timeout => "Bloom did not respond in time. Delivery is uncertain; check the conversation before retrying.",
        ClientError::WrongPeer => "Bloom rejected the daemon connection. The message was not sent.",
        ClientError::Server(bloom_ipc::ProtocolError::AccountThrottled) => "Bloom is rate limited. The message was not sent.",
        ClientError::Server(bloom_ipc::ProtocolError::RecipientUnreachable) => "Recipient is unreachable. The message was not sent.",
        ClientError::Server(bloom_ipc::ProtocolError::NotRegistered) => "Bloom account is not registered. The message was not sent.",
        ClientError::Server(bloom_ipc::ProtocolError::SessionExpired) => "Bloom session expired. The message was not sent.",
        ClientError::Server(_) => "Bloom rejected the request. The message was not sent.",
    }
}

pub fn category(error: &ClientError) -> &'static str {
    match error {
        ClientError::Connect => "connect",
        ClientError::Io => "io",
        ClientError::Timeout => "timeout",
        ClientError::WrongPeer => "wrong_peer",
        ClientError::Codec(_) => "codec",
        ClientError::WrongRequestId => "wrong_request_id",
        ClientError::UnexpectedResponse => "unexpected_response",
        ClientError::Server(bloom_ipc::ProtocolError::AccountThrottled) => "account_throttled",
        ClientError::Server(bloom_ipc::ProtocolError::RecipientUnreachable) => {
            "recipient_unreachable"
        }
        ClientError::Server(bloom_ipc::ProtocolError::NotRegistered) => "not_registered",
        ClientError::Server(bloom_ipc::ProtocolError::SessionExpired) => "session_expired",
        ClientError::Server(bloom_ipc::ProtocolError::InvalidRequest) => "invalid_request",
        ClientError::Server(bloom_ipc::ProtocolError::NotFound) => "not_found",
        ClientError::Server(bloom_ipc::ProtocolError::TransportUnavailable) => {
            "transport_unavailable"
        }
        ClientError::Server(bloom_ipc::ProtocolError::Unsupported) => "unsupported",
        ClientError::Server(bloom_ipc::ProtocolError::Protocol) => "protocol",
        ClientError::Server(bloom_ipc::ProtocolError::Internal) => "internal",
    }
}

pub fn record(line: &str) {
    let Some(path) = path() else { return };
    let _ = append(&path, line);
}

fn append(path: &PathBuf, line: &str) -> std::io::Result<()> {
    let dir = path.parent().ok_or(std::io::ErrorKind::InvalidInput)?;
    fs::create_dir_all(dir)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        if fs::symlink_metadata(dir)?.file_type().is_symlink() {
            return Err(std::io::ErrorKind::PermissionDenied.into());
        }
        fs::set_permissions(dir, fs::Permissions::from_mode(0o700))?;
        if fs::symlink_metadata(path).is_ok_and(|m| m.file_type().is_symlink()) {
            return Err(std::io::ErrorKind::PermissionDenied.into());
        }
        let metadata = fs::symlink_metadata(dir)?;
        if metadata.permissions().mode() & 0o077 != 0 {
            return Err(std::io::ErrorKind::PermissionDenied.into());
        }
    }
    if fs::metadata(path)
        .map(|m| m.len() >= MAX_BYTES)
        .unwrap_or(false)
    {
        let rotated = path.with_extension("log.1");
        let _ = fs::remove_file(&rotated);
        fs::rename(path, rotated)?;
    }
    let mut options = OpenOptions::new();
    options.create(true).append(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let mut file = options.open(path)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        file.set_permissions(fs::Permissions::from_mode(0o600))?;
    }
    writeln!(file, "{}", line.chars().take(512).collect::<String>())
}

pub fn request_result(stage: Stage, attempt: u64, elapsed: Duration, error: Option<&ClientError>) {
    match error {
        Some(error) => record(&format!(
            "request result attempt={attempt} stage={} elapsed_ms={} result=error category={}",
            stage.name(),
            elapsed.as_millis(),
            category(error)
        )),
        None => record(&format!(
            "request result attempt={attempt} stage={} elapsed_ms={} result=ok",
            stage.name(),
            elapsed.as_millis()
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn typed_reasons_do_not_expose_error_payload() {
        let err = ClientError::Server(bloom_ipc::ProtocolError::RecipientUnreachable);
        let reason = safe_reason(&err);
        assert_eq!(
            reason,
            "Recipient is unreachable. The message was not sent."
        );
        assert!(!reason.contains("payload"));
    }
    #[test]
    fn writes_private_file_and_rotates() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("tui.log");
        append(&path, "safe event").unwrap();
        assert!(fs::read_to_string(&path).unwrap().contains("safe event"));
        // `append` truncates each line to 512 chars, so grow the file directly
        // instead of relying on one oversized line to reach the rotation cap.
        fs::write(&path, "x".repeat(MAX_BYTES as usize)).unwrap();
        append(&path, "rotate now").unwrap();
        assert!(path.with_extension("log.1").exists());
        assert!(fs::read_to_string(&path).unwrap().contains("rotate now"));
        assert!(!fs::read_to_string(&path).unwrap().contains("safe event"));
    }
    #[test]
    fn logging_failure_is_nonfatal() {
        record("safe");
    }
}
