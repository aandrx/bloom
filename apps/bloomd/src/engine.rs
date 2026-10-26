//! Child-process adapter for the Bloom engine sidecar.
//!
//! The engine (OpenBubbles' library, which embeds SSPL and closed components) runs
//! in a separate Dart process and is never linked into this crate. This module
//! spawns that process, speaks its newline-delimited JSON protocol, and maps its
//! payloads into `bloom_domain` values.
//!
//! Lifecycle is owned by `supervisor::Supervisor`, which takes the sole bounded
//! event receiver before `start` and forwards events to its sink.

use async_trait::async_trait;
use bloom_domain::{
    AttachmentRef, Capabilities, Conversation, ConversationId, ConversationKind, DeliveryState,
    Handle, HandleKind, Message, MessageId, MessagePart, OutgoingBody, Participant, SessionState,
    TransportId,
};
use bloom_transport::{SyncCompletion, Transport, TransportError, TransportEvent, TransportResult};
use serde_json::Value;
use std::{
    collections::HashMap,
    path::PathBuf,
    sync::{
        atomic::{AtomicBool, AtomicU64, Ordering},
        Arc, Mutex,
    },
    time::Duration,
};
use tokio::{
    io::{AsyncBufReadExt, AsyncWriteExt, BufReader},
    process::{Child, ChildStdin, Command},
    sync::{mpsc, oneshot},
};

/// Engine frames are line-delimited JSON; refuse absurd lines instead of buffering.
pub const MAX_FRAME_BYTES: usize = 1 << 20;
pub const EVENT_CAPACITY: usize = 256;
/// How long a send may wait for its confirmation before the result is uncertain.
pub const SEND_TIMEOUT: Duration = Duration::from_secs(30);
/// Grace period for final sidecar frames; also bounds individual shutdown I/O.
pub const STOP_TIMEOUT: Duration = Duration::from_secs(10);
const READER_STOP_TIMEOUT: Duration = Duration::from_secs(2);
/// The sidecar bounds one attachment download to 120 seconds, so the daemon
/// waits at least as long without retrying automatically.
pub const ATTACHMENT_FETCH_TIMEOUT: Duration = Duration::from_secs(150);

/// A fetched attachment's private local cache file.
#[derive(Clone, PartialEq, Eq)]
pub struct AttachmentFile {
    pub path: String,
    pub bytes: u64,
}
impl std::fmt::Debug for AttachmentFile {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AttachmentFile")
            .field("path", &"[REDACTED]")
            .field("bytes", &self.bytes)
            .finish()
    }
}

pub fn transport_id() -> TransportId {
    TransportId::new("openbubbles").expect("static transport id is valid")
}

/// The engine currently serving requests.
///
/// The lock is held only long enough to clone the current engine, never across an
/// await. A future generation replaces the contents so existing callers observe
/// the new engine on their next request without owning the swap themselves.
pub type EngineSlot = std::sync::Arc<std::sync::RwLock<std::sync::Arc<Engine>>>;

/// Wrap an engine in a fresh slot.
pub fn engine_slot(engine: std::sync::Arc<Engine>) -> EngineSlot {
    std::sync::Arc::new(std::sync::RwLock::new(engine))
}

#[derive(Clone, Debug)]
pub struct EngineConfig {
    pub dart_executable: PathBuf,
    pub script: PathBuf,
    pub state_dir: PathBuf,
    pub library: PathBuf,
    /// Ask the engine to list the account's existing conversations. Set when the
    /// store is empty, so a fresh install is usable without paying for a full
    /// account listing on every subsequent start.
    pub sync_chats: bool,
}

impl EngineConfig {
    fn command(&self) -> Command {
        let mut command = Command::new(&self.dart_executable);
        command
            .arg("run")
            .arg(&self.script)
            .arg("--state")
            .arg(&self.state_dir)
            .arg("--library")
            .arg(&self.library);
        if self.sync_chats {
            command.arg("--sync-chats");
        }
        #[cfg(target_os = "linux")]
        {
            use libc;
            use std::io;

            // dart run launches another process; PDEATHSIG covers only that direct
            // child, while stdin EOF remains the sidecar's actual shutdown path.
            unsafe {
                command.pre_exec(|| {
                    if libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGTERM) == -1 {
                        return Err(io::Error::last_os_error());
                    }
                    Ok(())
                });
            }
        }
        command
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::inherit())
            .kill_on_drop(true);
        command
    }
}

struct Pending {
    waiters: Mutex<HashMap<u64, oneshot::Sender<TransportResult<MessageId>>>>,
}

type SyncResult = TransportResult<(bool, u32)>;
type SyncWaiters = Arc<Mutex<HashMap<u64, oneshot::Sender<SyncResult>>>>;
type AttachmentWaiters = Arc<Mutex<HashMap<u64, oneshot::Sender<TransportResult<AttachmentFile>>>>>;

#[derive(Clone)]
struct ReadFrameContext {
    outgoing: mpsc::Sender<TransportEvent>,
    pending: Arc<Pending>,
    sync_waiters: SyncWaiters,
    attachment_waiters: AttachmentWaiters,
    sync_events: Arc<tokio::sync::Mutex<()>>,
    id: TransportId,
    my_handles: Arc<Mutex<Vec<String>>>,
    recipient_lists_supported: Arc<AtomicBool>,
    started: Arc<AtomicBool>,
    stdin: Arc<tokio::sync::Mutex<Option<ChildStdin>>>,
    child: Arc<tokio::sync::Mutex<Option<Child>>>,
    reader_stop: Arc<tokio::sync::Notify>,
    reader_done: tokio::sync::watch::Sender<bool>,
}

pub struct Engine {
    config: EngineConfig,
    id: TransportId,
    incoming: Mutex<Option<mpsc::Receiver<TransportEvent>>>,
    outgoing: Mutex<Option<mpsc::Sender<TransportEvent>>>,
    stdin: Arc<tokio::sync::Mutex<Option<ChildStdin>>>,
    child: Arc<tokio::sync::Mutex<Option<Child>>>,
    pending: Arc<Pending>,
    next_request: AtomicU64,
    started: Arc<AtomicBool>,
    /// Addresses the account can send from, learned from the sidecar's ready frame.
    my_handles: Arc<Mutex<Vec<String>>>,
    /// Whether the sidecar advertises multi-recipient send frames.
    recipient_lists_supported: Arc<AtomicBool>,
    sync_waiters: SyncWaiters,
    sync_events: Arc<tokio::sync::Mutex<()>>,
    sync_lock: tokio::sync::Mutex<()>,
    attachment_waiters: AttachmentWaiters,
    reader_stop: Arc<tokio::sync::Notify>,
    reader_done: tokio::sync::watch::Sender<bool>,
    lifecycle_lock: tokio::sync::Mutex<()>,
}

impl Engine {
    pub fn new(config: EngineConfig) -> Self {
        let (outgoing, incoming) = mpsc::channel(EVENT_CAPACITY);
        let (reader_done, _) = tokio::sync::watch::channel(true);
        Self {
            config,
            id: transport_id(),
            incoming: Mutex::new(Some(incoming)),
            outgoing: Mutex::new(Some(outgoing)),
            stdin: Arc::new(tokio::sync::Mutex::new(None)),
            child: Arc::new(tokio::sync::Mutex::new(None)),
            pending: Arc::new(Pending {
                waiters: Mutex::new(HashMap::new()),
            }),
            next_request: AtomicU64::new(1),
            started: Arc::new(AtomicBool::new(false)),
            my_handles: Arc::new(Mutex::new(Vec::new())),
            recipient_lists_supported: Arc::new(AtomicBool::new(false)),
            sync_waiters: Arc::new(Mutex::new(HashMap::new())),
            sync_events: Arc::new(tokio::sync::Mutex::new(())),
            sync_lock: tokio::sync::Mutex::new(()),
            attachment_waiters: Arc::new(Mutex::new(HashMap::new())),
            reader_stop: Arc::new(tokio::sync::Notify::new()),
            reader_done,
            lifecycle_lock: tokio::sync::Mutex::new(()),
        }
    }

    /// Addresses this account can send from, as reported by the engine.
    pub fn my_handles(&self) -> Vec<String> {
        self.my_handles.lock().unwrap().clone()
    }

    /// The local participant's handle, used as the sender of outbound messages.
    pub fn primary_handle(&self) -> Option<Handle> {
        let handles = self.my_handles();
        handles.first().and_then(|raw| handle_from(raw))
    }

    /// Send a text message into a conversation identified by its engine chat guid.
    ///
    /// Sends are resolved against the store, so they do not go through
    /// `Transport::send`: the transport trait is conversation-id based, while the
    /// engine needs a recipient address and the existing chat guid.
    pub async fn send_text(
        &self,
        recipients: &[String],
        chat_guid: Option<&str>,
        text: &str,
    ) -> TransportResult<MessageId> {
        if recipients.is_empty()
            || recipients
                .iter()
                .any(|recipient| recipient.trim().is_empty())
        {
            return Err(TransportError::Protocol);
        }
        if !self.started.load(Ordering::SeqCst) {
            return Err(TransportError::TransportUnavailable);
        }
        if recipients.len() > 1 && !self.recipient_lists_supported.load(Ordering::SeqCst) {
            return Err(TransportError::Unsupported);
        }
        let request_id = self.next_request.fetch_add(1, Ordering::Relaxed);
        let (sender, receiver) = oneshot::channel();
        self.pending
            .waiters
            .lock()
            .unwrap()
            .insert(request_id, sender);

        let mut frame = serde_json::json!({
            "version": 1,
            "type": "send",
            "request_id": request_id,
            "recipients": recipients,
            "text": text,
        });
        // Older sidecars only understand a scalar recipient. Preserve direct sends
        // while multi-recipient sends require an explicit ready-frame capability.
        if let [recipient] = recipients {
            frame["recipient"] = Value::String(recipient.clone());
        }
        if let Some(guid) = chat_guid {
            frame["chat_guid"] = Value::String(guid.to_owned());
        }

        if self.write_frame(&frame).await.is_err() {
            self.pending.waiters.lock().unwrap().remove(&request_id);
            return Err(TransportError::TransportUnavailable);
        }

        match tokio::time::timeout(SEND_TIMEOUT, receiver).await {
            Ok(Ok(result)) => result,
            Ok(Err(_)) => Err(TransportError::Internal),
            Err(_) => {
                self.pending.waiters.lock().unwrap().remove(&request_id);
                // The engine may still have delivered the message.
                Err(TransportError::Protocol)
            }
        }
    }

    /// Request bounded remote conversation-list metadata sync. Does not fetch messages.
    pub async fn sync_conversations(&self) -> TransportResult<(bool, u32)> {
        let _serial = self.sync_lock.lock().await;
        if !self.started.load(Ordering::SeqCst) {
            return Err(TransportError::TransportUnavailable);
        }
        let request_id = self.next_request.fetch_add(1, Ordering::Relaxed);
        let (tx, rx) = oneshot::channel();
        self.sync_waiters.lock().unwrap().insert(request_id, tx);
        if self
            .write_frame(&serde_json::json!({
                "version": 1,
                "type": "sync_conversations",
                "request_id": request_id
            }))
            .await
            .is_err()
        {
            self.sync_waiters.lock().unwrap().remove(&request_id);
            return Err(TransportError::TransportUnavailable);
        }
        match tokio::time::timeout(Duration::from_secs(120), rx).await {
            Ok(Ok(result)) => result,
            _ => {
                self.sync_waiters.lock().unwrap().remove(&request_id);
                Err(TransportError::Protocol)
            }
        }
    }

    /// Fetch one opaque attachment token through the sidecar. The token is
    /// validated locally; no path, key or content crosses this call in logs.
    /// A failure is returned once and is never retried automatically.
    pub async fn fetch_attachment(&self, token: &str) -> TransportResult<AttachmentFile> {
        if !AttachmentRef::is_valid_fetch_token(token) {
            return Err(TransportError::Protocol);
        }
        if !self.started.load(Ordering::SeqCst) {
            return Err(TransportError::TransportUnavailable);
        }
        let request_id = self.next_request.fetch_add(1, Ordering::Relaxed);
        let (tx, rx) = oneshot::channel();
        self.attachment_waiters
            .lock()
            .unwrap()
            .insert(request_id, tx);
        if self
            .write_frame(&serde_json::json!({
                "version": 1,
                "type": "attachment_fetch",
                "request_id": request_id,
                "token": token,
            }))
            .await
            .is_err()
        {
            self.attachment_waiters.lock().unwrap().remove(&request_id);
            return Err(TransportError::TransportUnavailable);
        }
        match tokio::time::timeout(ATTACHMENT_FETCH_TIMEOUT, rx).await {
            Ok(Ok(result)) => result,
            Ok(Err(_)) => Err(TransportError::Internal),
            Err(_) => {
                self.attachment_waiters.lock().unwrap().remove(&request_id);
                Err(TransportError::Protocol)
            }
        }
    }

    async fn write_frame(&self, frame: &Value) -> std::io::Result<()> {
        let mut guard = self.stdin.lock().await;
        let stdin = guard.as_mut().ok_or_else(|| {
            std::io::Error::new(std::io::ErrorKind::NotConnected, "engine stdin closed")
        })?;
        let mut line = serde_json::to_vec(frame)
            .map_err(|_| std::io::Error::new(std::io::ErrorKind::InvalidData, "encode"))?;
        line.push(b'\n');
        stdin.write_all(&line).await?;
        stdin.flush().await
    }
}

#[async_trait]
impl Transport for Engine {
    fn id(&self) -> TransportId {
        self.id.clone()
    }

    /// Generic transport sends remain unsupported because they lack recipient resolution.
    /// The daemon's store-backed send path separately supports plain-text groups.
    fn capabilities(&self) -> Capabilities {
        Capabilities {
            // Inbound reaction decoding/display does not imply an outbound operation.
            reactions: false,
            edits: false,
            unsend: false,
            attachments: false,
            groups: false,
            typing: false,
            read_receipts: false,
            sms: false,
        }
    }

    async fn take_events(&self) -> TransportResult<mpsc::Receiver<TransportEvent>> {
        self.incoming
            .lock()
            .unwrap()
            .take()
            .ok_or(TransportError::Internal)
    }

    async fn start(&self) -> TransportResult<()> {
        self.start_with(self.config.command()).await
    }

    async fn stop(&self) -> TransportResult<()> {
        let _lifecycle = self.lifecycle_lock.lock().await;
        self.started.store(false, Ordering::SeqCst);
        let mut reader_done = self.reader_done.subscribe();
        let _ = tokio::time::timeout(
            STOP_TIMEOUT,
            self.write_frame(&serde_json::json!({
                "version": 1,
                "type": "shutdown",
                "request_id": self.next_request.fetch_add(1, Ordering::Relaxed),
            })),
        )
        .await;
        self.stdin.lock().await.take();
        // The reader owns the drain deadline, so cancellation of this stop future
        // cannot leave a backpressured reader and its child running indefinitely.
        self.reader_stop.notify_one();
        let _ = tokio::time::timeout(
            STOP_TIMEOUT + READER_STOP_TIMEOUT * 3,
            reader_done.wait_for(|done| *done),
        )
        .await;
        if let Some(mut child) = self.child.lock().await.take() {
            if tokio::time::timeout(STOP_TIMEOUT, child.wait())
                .await
                .is_err()
            {
                let _ = child.kill().await;
                let _ = child.wait().await;
            }
        }
        self.started.store(false, Ordering::SeqCst);
        self.recipient_lists_supported
            .store(false, Ordering::SeqCst);
        self.my_handles.lock().unwrap().clear();
        Ok(())
    }

    async fn conversations(&self) -> TransportResult<Vec<Conversation>> {
        // The daemon's own store is authoritative for conversation listing.
        Err(TransportError::Unsupported)
    }

    async fn history(
        &self,
        _conversation: &ConversationId,
        _limit: u16,
    ) -> TransportResult<Vec<Message>> {
        Err(TransportError::Unsupported)
    }

    async fn send(
        &self,
        _conversation: &ConversationId,
        _body: OutgoingBody,
    ) -> TransportResult<MessageId> {
        // Outbound needs a store-resolved recipient address; use `send_text`.
        Err(TransportError::Unsupported)
    }
}

impl Engine {
    async fn start_with(&self, mut command: Command) -> TransportResult<()> {
        let _lifecycle = self.lifecycle_lock.lock().await;
        if self.started.swap(true, Ordering::SeqCst) {
            return Err(TransportError::TransportUnavailable);
        }
        let outgoing = match self.outgoing.lock().unwrap().take() {
            Some(outgoing) => outgoing,
            None => {
                self.started.store(false, Ordering::SeqCst);
                return Err(TransportError::TransportUnavailable);
            }
        };
        let mut child = match command.spawn() {
            Ok(child) => child,
            Err(_) => {
                *self.outgoing.lock().unwrap() = Some(outgoing);
                self.started.store(false, Ordering::SeqCst);
                return Err(TransportError::TransportUnavailable);
            }
        };
        eprintln!("bloomd: engine child started pid={:?}", child.id());
        let stdout = match child.stdout.take() {
            Some(stdout) => stdout,
            None => return Err(self.abort_start(child, outgoing).await),
        };
        let stdin = match child.stdin.take() {
            Some(stdin) => stdin,
            None => return Err(self.abort_start(child, outgoing).await),
        };
        *self.stdin.lock().await = Some(stdin);
        *self.child.lock().await = Some(child);

        // Hand the only sender to the reader task. If the engine held one, the
        // supervisor's forwarder could never observe the channel closing and
        // shutdown would always report an incomplete drain.
        self.recipient_lists_supported
            .store(false, Ordering::SeqCst);
        let my_handles = self.my_handles.clone();
        let recipient_lists_supported = self.recipient_lists_supported.clone();
        let pending = self.pending.clone();
        let sync_waiters = self.sync_waiters.clone();
        let attachment_waiters = self.attachment_waiters.clone();
        let sync_events = self.sync_events.clone();
        let started = self.started.clone();
        let stdin = self.stdin.clone();
        let child = self.child.clone();
        let reader_stop = self.reader_stop.clone();
        let reader_done = self.reader_done.clone();
        reader_done.send_replace(false);
        let id = self.id.clone();
        tokio::spawn(async move {
            read_frames(
                stdout,
                ReadFrameContext {
                    outgoing,
                    pending,
                    sync_waiters,
                    attachment_waiters,
                    sync_events,
                    id,
                    my_handles,
                    recipient_lists_supported,
                    started,
                    stdin,
                    child,
                    reader_stop,
                    reader_done,
                },
            )
            .await;
        });
        Ok(())
    }

    /// Terminates a child whose pipes could not be taken and restores the state
    /// `start_with` changed, so a later start attempt is not blocked.
    async fn abort_start(
        &self,
        mut child: Child,
        outgoing: mpsc::Sender<TransportEvent>,
    ) -> TransportError {
        let _ = child.kill().await;
        let _ = tokio::time::timeout(STOP_TIMEOUT, child.wait()).await;
        *self.stdin.lock().await = None;
        *self.child.lock().await = None;
        *self.outgoing.lock().unwrap() = Some(outgoing);
        self.started.store(false, Ordering::SeqCst);
        TransportError::Internal
    }
}

/// Why the frame reader stopped, so a dead engine is distinguishable from a quiet
/// stream end and neither is mistaken for the other.
enum FrameRead {
    /// One frame's bytes, without its terminator.
    Frame(Vec<u8>),
    /// The engine closed the stream between frames.
    Eof,
    /// The frame exceeded `MAX_FRAME_BYTES` and the rest was not buffered.
    TooLarge,
    /// The underlying read failed.
    Failed(std::io::Error),
}

/// Read one newline-terminated frame, never buffering more than the frame limit.
///
/// `AsyncBufRead::fill_buf` exposes only the bytes already in the buffer, so an
/// oversized line is rejected without being accumulated first.
/// Structural description of a rejected frame. Never includes payload text.
fn unparsable_frame_summary(line: &str) -> String {
    let bytes = line.as_bytes();
    let printable = bytes
        .iter()
        .filter(|byte| matches!(byte, 0x20..=0x7e))
        .count();
    let ratio = if bytes.is_empty() {
        0
    } else {
        printable * 100 / bytes.len()
    };
    let digits = bytes.iter().filter(|byte| byte.is_ascii_digit()).count();
    let letters = bytes
        .iter()
        .filter(|byte| byte.is_ascii_alphabetic())
        .count();
    let spaces = bytes.iter().filter(|byte| **byte == b' ').count();
    format!(
        "len={} first={:#04x} printable={ratio}% nul={} brace={} digits={digits} letters={letters} spaces={spaces} all_digits={}",
        bytes.len(),
        bytes.first().copied().unwrap_or(0),
        line.contains('\0'),
        line.starts_with('{'),
        !bytes.is_empty() && digits == bytes.len()
    )
}

async fn read_frame<R: tokio::io::AsyncBufRead + Unpin>(reader: &mut R) -> FrameRead {
    let mut bytes: Vec<u8> = Vec::with_capacity(4096);
    loop {
        let buf = match reader.fill_buf().await {
            Ok(buf) => buf,
            Err(error) => return FrameRead::Failed(error),
        };
        if buf.is_empty() {
            // The stream ended; a final frame without a terminator is still used.
            return strip_terminator(bytes);
        }
        let take = buf
            .iter()
            .position(|byte| *byte == b'\n')
            .map_or(buf.len(), |index| index + 1);
        if bytes.len().saturating_add(take) > MAX_FRAME_BYTES + 1 {
            reader.consume(take);
            return FrameRead::TooLarge;
        }
        let ended = buf[take - 1] == b'\n';
        bytes.extend_from_slice(&buf[..take]);
        reader.consume(take);
        if ended {
            return strip_terminator(bytes);
        }
    }
}

fn strip_terminator(mut bytes: Vec<u8>) -> FrameRead {
    if bytes.is_empty() {
        return FrameRead::Eof;
    }
    if bytes.last() == Some(&b'\n') {
        bytes.pop();
    }
    if bytes.last() == Some(&b'\r') {
        bytes.pop();
    }
    // A blank line is left as an empty frame so the parser reports it as
    // unparsable, which is what closing the engine connection on it means.
    FrameRead::Frame(bytes)
}

async fn read_frames(stdout: tokio::process::ChildStdout, context: ReadFrameContext) {
    {
        let processing = process_frames(stdout, context.clone());
        tokio::pin!(processing);
        tokio::select! {
            _ = context.reader_stop.notified() => {
                // Preserve frames emitted during cooperative shutdown. A stalled
                // consumer still cannot prevent bounded reader/child cleanup.
                tokio::select! {
                    _ = tokio::time::timeout(STOP_TIMEOUT, &mut processing) => {},
                    _ = context.outgoing.closed() => {},
                }
            },
            _ = context.outgoing.closed() => {},
            _ = &mut processing => {},
        }
    }
    cleanup_reader(&context).await;
    context.reader_done.send_replace(true);
}

async fn cleanup_reader(context: &ReadFrameContext) {
    context.started.store(false, Ordering::SeqCst);
    context
        .recipient_lists_supported
        .store(false, Ordering::SeqCst);
    context.my_handles.lock().unwrap().clear();
    let waiters = std::mem::take(&mut *context.sync_waiters.lock().unwrap());
    for (_, waiter) in waiters {
        let _ = waiter.send(Err(TransportError::TransportUnavailable));
    }
    let attachment_waiters = std::mem::take(&mut *context.attachment_waiters.lock().unwrap());
    for (_, waiter) in attachment_waiters {
        let _ = waiter.send(Err(TransportError::TransportUnavailable));
    }
    // Written-but-unacknowledged sends retain their timeout outcome: failure of
    // the stream cannot prove that the remote service did not accept them.
    context.stdin.lock().await.take();
    if let Some(mut child) = context.child.lock().await.take() {
        if tokio::time::timeout(READER_STOP_TIMEOUT, child.wait())
            .await
            .is_err()
        {
            let _ = child.kill().await;
            let _ = child.wait().await;
        }
    }
    // A stalled event consumer must not keep a dead engine's reader alive.
    let _ = tokio::time::timeout(
        READER_STOP_TIMEOUT,
        context.outgoing.send(TransportEvent::Session {
            state: SessionState::Disconnected,
            reason: None,
        }),
    )
    .await;
}

async fn process_frames(stdout: tokio::process::ChildStdout, context: ReadFrameContext) {
    let ReadFrameContext {
        outgoing,
        pending,
        sync_waiters,
        attachment_waiters,
        sync_events,
        id,
        my_handles: my_handles_store,
        recipient_lists_supported,
        started: _,
        stdin: _,
        child: _,
        reader_stop: _,
        reader_done: _,
    } = context;
    let mut reader = BufReader::new(stdout);
    let mut my_handles: Vec<String> = Vec::new();
    let mut foreign_stdout_lines: u64 = 0;
    let mut skipped_conversations = 0usize;
    let mut skipped: HashMap<SkipReason, u64> = HashMap::new();
    loop {
        let bytes = match read_frame(&mut reader).await {
            FrameRead::Frame(bytes) => bytes,
            FrameRead::Eof => {
                eprintln!("engine frame reader stopped: engine closed the stream");
                break;
            }
            FrameRead::TooLarge => {
                eprintln!("engine frame too large; closing engine connection");
                break;
            }
            FrameRead::Failed(error) => {
                eprintln!("engine frame reader stopped with error: {error}");
                break;
            }
        };
        let frame_started = std::time::Instant::now();
        let line = match String::from_utf8(bytes) {
            Ok(line) => line,
            Err(error) => {
                eprintln!("engine frame reader stopped with invalid UTF-8: {error}");
                break;
            }
        };
        // The linked engine can write diagnostics to the same stdout. A marked
        // line is protocol; unmarked JSON remains accepted for older sidecars.
        // Anything else is foreign output: log only its structure and skip it,
        // bounded so repeated corruption still closes the connection.
        let protocol_line = if let Some(marked) = line.strip_prefix('\u{1e}') {
            marked
        } else if line.trim_start().starts_with('{') {
            line.as_str()
        } else {
            foreign_stdout_lines += 1;
            eprintln!(
                "engine stdout carried an unmarked non-JSON line; skipping ({})",
                unparsable_frame_summary(&line)
            );
            if foreign_stdout_lines > MAX_FOREIGN_STDOUT_LINES {
                eprintln!(
                    "engine produced too many foreign stdout lines; closing engine connection"
                );
                break;
            }
            continue;
        };
        let frame: Value = match serde_json::from_str(protocol_line) {
            Ok(frame) => frame,
            Err(_) => {
                eprintln!(
                    "engine sent an unparsable frame; closing engine connection ({})",
                    unparsable_frame_summary(&line)
                );
                break;
            }
        };
        if crate::diagnostics::enabled() {
            if let Some(sequence) = frame
                .get("sequence")
                .and_then(Value::as_u64)
                .filter(|_| frame.get("type").and_then(Value::as_str) == Some("inbound_message"))
            {
                eprintln!(
                    "bloom timing daemon frame_decode_us={} sequence={sequence}",
                    crate::diagnostics::elapsed_micros(frame_started)
                );
            }
        }
        match frame.get("type").and_then(Value::as_str) {
            Some("ready") => {
                if let Some(handles) = frame.get("handles").and_then(Value::as_array) {
                    my_handles = handles
                        .iter()
                        .filter_map(Value::as_str)
                        .map(str::to_owned)
                        .collect();
                    *my_handles_store.lock().unwrap() = my_handles.clone();
                }
                let supports_recipient_lists = frame
                    .get("capabilities")
                    .and_then(Value::as_object)
                    .and_then(|capabilities| capabilities.get("recipient_lists"))
                    .and_then(Value::as_bool)
                    .unwrap_or(false);
                recipient_lists_supported.store(supports_recipient_lists, Ordering::SeqCst);
                if outgoing
                    .send(TransportEvent::Session {
                        state: SessionState::Active,
                        reason: None,
                    })
                    .await
                    .is_err()
                {
                    return;
                }
            }
            Some("inbound_message") | Some("history_message") => {
                let payload = frame
                    .get("payload")
                    .or_else(|| frame.get("message"))
                    .cloned()
                    .unwrap_or(Value::Null);
                let sequence = frame.get("sequence").and_then(Value::as_u64);
                let trace_sequence = crate::diagnostics::enabled().then_some(sequence).flatten();
                match map_inbound_with_sequence(&payload, &my_handles, &id, trace_sequence) {
                    Ok(event) => {
                        if outgoing.send(event).await.is_err() {
                            return;
                        }
                    }
                    Err(reason) => *skipped.entry(reason).or_default() += 1,
                }
            }
            Some("send_confirmation") => {
                let request_id = frame.get("request_id").and_then(Value::as_u64);
                let accepted = frame.get("accepted").and_then(Value::as_bool) == Some(true);
                let message_id = frame.get("message_id").and_then(Value::as_str);
                if let Some(request_id) = request_id {
                    if let Some(waiter) = pending.waiters.lock().unwrap().remove(&request_id) {
                        let result = match (accepted, message_id) {
                            (true, Some(message_id)) => MessageId::new(id.clone(), message_id)
                                .map_err(|_| TransportError::Protocol),
                            _ => Err(TransportError::RecipientUnreachable),
                        };
                        let _ = waiter.send(result);
                    }
                }
            }
            Some("session_state") => {
                let state = frame
                    .get("state")
                    .and_then(Value::as_str)
                    .map(map_session_state)
                    .unwrap_or(SessionState::Disconnected);
                if outgoing
                    .send(TransportEvent::Session {
                        state,
                        reason: None,
                    })
                    .await
                    .is_err()
                {
                    return;
                }
            }
            Some("error") => {
                // Bounded code only; the frame never carries payload or identifiers.
                let code = frame
                    .get("code")
                    .and_then(Value::as_str)
                    .unwrap_or("unknown");
                let request_id = frame.get("request_id").and_then(Value::as_u64);
                if let Some(request_id) = request_id {
                    if let Some(waiter) = attachment_waiters.lock().unwrap().remove(&request_id) {
                        let _ = waiter.send(Err(map_attachment_error(code)));
                        continue;
                    }
                }
                eprintln!("engine reported error: {code}");
            }
            Some("attachment_fetched") => {
                let request_id = frame.get("request_id").and_then(Value::as_u64);
                let path = frame
                    .get("path")
                    .and_then(Value::as_str)
                    .filter(|path| !path.is_empty());
                let bytes = frame.get("bytes").and_then(Value::as_u64);
                let result = match (path, bytes) {
                    (Some(path), Some(bytes)) if safe_attachment_path(path) => Ok(AttachmentFile {
                        path: path.to_owned(),
                        bytes,
                    }),
                    _ => Err(TransportError::Protocol),
                };
                if let Some(waiter) =
                    request_id.and_then(|id| attachment_waiters.lock().unwrap().remove(&id))
                {
                    let _ = waiter.send(result);
                }
            }
            Some("shutdown_ack") => {}
            Some("sync_result") => {
                let _guard = sync_events.lock().await;
                let request_id = frame.get("request_id").and_then(Value::as_u64);
                let status = frame.get("status").and_then(Value::as_str);
                let count = frame
                    .get("conversations")
                    .and_then(Value::as_u64)
                    .filter(|count| *count <= 20_000);
                let result = match (status, count) {
                    (Some("complete"), Some(count)) => Ok((false, count as u32)),
                    (Some("limited"), Some(count)) => Ok((true, count as u32)),
                    (Some("failed"), _) => Err(TransportError::TransportUnavailable),
                    _ => Err(TransportError::Protocol),
                };
                let waiter = request_id.and_then(|id| sync_waiters.lock().unwrap().remove(&id));
                let completion = Arc::new(SyncCompletion::default());
                let wait_for_completion = completion.clone();
                if outgoing
                    .send(TransportEvent::ConversationSyncComplete { completion })
                    .await
                    .is_err()
                {
                    if let Some(waiter) = waiter {
                        let _ = waiter.send(Err(TransportError::TransportUnavailable));
                    }
                    return;
                }
                wait_for_completion.wait().await;
                let result = if wait_for_completion.persistence_failed() {
                    Err(TransportError::Internal)
                } else {
                    result
                };
                if let Some(waiter) = waiter {
                    let _ = waiter.send(result);
                }
            }
            Some("history_complete") => {
                let conversations = frame
                    .get("conversations")
                    .and_then(Value::as_u64)
                    .unwrap_or(0);
                let limited = frame
                    .get("limited")
                    .and_then(Value::as_bool)
                    .unwrap_or(false);
                eprintln!(
                    "engine listing complete: conversations={conversations} limited={limited} skipped={skipped_conversations}"
                );
                // One refresh for the whole listing sync, not one per conversation.
                if outgoing
                    .send(TransportEvent::ConversationsChanged)
                    .await
                    .is_err()
                {
                    return;
                }
            }
            Some("conversation") => {
                let _guard = sync_events.lock().await;
                match map_conversation(frame.get("conversation"), &my_handles, &id) {
                    Some(event) => {
                        if outgoing.send(event).await.is_err() {
                            return;
                        }
                    }
                    None => skipped_conversations += 1,
                }
            }
            _ => eprintln!("engine sent an unknown frame type"),
        }
    }
    if !skipped.is_empty() {
        let summary = skipped
            .iter()
            .map(|(reason, count)| format!("{reason}={count}"))
            .collect::<Vec<_>>()
            .join(", ");
        eprintln!("engine skipped payloads: {summary}");
    }
}

fn safe_attachment_path(path: &str) -> bool {
    let path = std::path::Path::new(path);
    path.is_absolute()
        && path
            .components()
            .all(|component| !matches!(component, std::path::Component::ParentDir))
}

fn map_attachment_error(code: &str) -> TransportError {
    match code {
        "invalid_token" => TransportError::Protocol,
        "unavailable" => TransportError::TransportUnavailable,
        "too_large" | "timeout" => TransportError::Protocol,
        _ => TransportError::Internal,
    }
}

fn map_session_state(state: &str) -> SessionState {
    match state {
        "unregistered" => SessionState::Unregistered,
        "activating" => SessionState::Activating,
        "registered" => SessionState::Registered,
        "active" => SessionState::Active,
        "renewal_due" => SessionState::RenewalDue,
        "renewing" => SessionState::Renewing,
        "throttled" => SessionState::Throttled,
        "blocked" => SessionState::Blocked,
        "failed" => SessionState::Failed,
        _ => SessionState::Disconnected,
    }
}

/// Why a payload could not be turned into a domain message. Reported, never silent.
#[derive(Debug, PartialEq, Eq, Hash)]
pub enum SkipReason {
    NotAnInboundMessage,
    NotANormalMessage,
    MissingChatGuid,
    MissingMessageId,
    MissingSender,
    MissingReactionTarget,
    MissingMutationTarget,
    MissingMutationPart,
    MissingMutationTimestamp,
    UnsupportedEditContent,
    UnsupportedReaction,
    InvalidIdentity,
    NoContent,
}

impl std::fmt::Display for SkipReason {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::NotAnInboundMessage => "payload is not an inbound iMessage",
            Self::NotANormalMessage => "payload is not a normal message variant",
            Self::MissingChatGuid => "payload has no chat guid",
            Self::MissingMessageId => "payload has no message id",
            Self::MissingSender => "payload has no sender",
            Self::MissingReactionTarget => "reaction payload has no usable target message id",
            Self::MissingMutationTarget => "edit or unsend payload has no usable target message id",
            Self::MissingMutationPart => "edit or unsend payload has no usable part index",
            Self::MissingMutationTimestamp => "edit or unsend payload has no timestamp",
            Self::UnsupportedEditContent => {
                "edit payload carried replacement content outside plain text"
            }
            Self::UnsupportedReaction => "reaction payload carried an unsupported kind",
            Self::InvalidIdentity => "payload identity was rejected by the domain model",
            Self::NoContent => "payload carried no representable content",
        })
    }
}

pub fn handle_from(raw: &str) -> Option<Handle> {
    let (kind, value) = if let Some(rest) = raw.strip_prefix("mailto:") {
        (HandleKind::Email, rest)
    } else if let Some(rest) = raw.strip_prefix("tel:") {
        (HandleKind::Tel, rest)
    } else {
        (HandleKind::Opaque, raw)
    };
    Handle::new(kind, value).ok()
}

fn text_of(parts: &[MessagePart]) -> Option<String> {
    let mut buffer = String::new();
    for part in parts {
        match part {
            MessagePart::Text(text) => buffer.push_str(text),
            // Anything else cannot be carried by the text-only send path. Returning
            // what we can would send a truncated message that looks successful, so
            // reject the whole body instead.
            _ => return None,
        }
    }
    if buffer.is_empty() {
        None
    } else {
        Some(buffer)
    }
}

/// Keep the descriptive fields an inbound attachment actually carries.
///
/// Only `name`, `mime`, and a length whose `attachment_type.kind` names the
/// field's meaning are copied. Everything else, including engine numbering,
/// `uti_type`, `iris`, and remote access material, is ignored. The reference is
/// an opaque token for this decoded message revision at this physical position;
/// it is not a stored index and promises nothing across edits or reordering.
fn attachment_part(
    part: &Value,
    transport: &TransportId,
    raw_message_id: &str,
    position: usize,
) -> MessagePart {
    const MAX_TOKEN_BYTES: usize = 256;
    // Bound the shared context before encoding it once per attachment.
    if transport
        .as_str()
        .len()
        .saturating_add(raw_message_id.len())
        > MAX_TOKEN_BYTES
    {
        return MessagePart::Unsupported {
            kind: "attachment".to_owned(),
        };
    }

    fn nonempty_string(value: Option<&Value>) -> Option<String> {
        value
            .and_then(Value::as_str)
            .filter(|text| !text.is_empty())
            .map(str::to_owned)
    }

    let file_name = nonempty_string(part.get("name"));
    let mime_type = nonempty_string(part.get("mime"));
    let attachment_type = part.get("attachment_type");
    let byte_len = match attachment_type.and_then(|kind| kind.get("kind")) {
        Some(kind) if kind.as_str() == Some("inline") => attachment_type
            .and_then(|kind| kind.get("byte_length"))
            .and_then(Value::as_u64),
        Some(kind) if kind.as_str() == Some("mmcs") => attachment_type
            .and_then(|kind| kind.get("size"))
            .and_then(Value::as_u64),
        _ => None,
    };

    if file_name.is_none() && mime_type.is_none() && byte_len.is_none() {
        return MessagePart::Unsupported {
            kind: "attachment".to_owned(),
        };
    }

    let Ok(numbering) = serde_json::to_string(&(transport.as_str(), raw_message_id, position))
    else {
        return MessagePart::Unsupported {
            kind: "attachment".to_owned(),
        };
    };
    let token = format!("local-metadata:{numbering}");
    // JSON escaping can expand even a short raw context beyond the budget.
    if token.len() > MAX_TOKEN_BYTES {
        return MessagePart::Unsupported {
            kind: "attachment".to_owned(),
        };
    }
    let fetch_token = part
        .get("fetch_token")
        .and_then(Value::as_str)
        .filter(|token| AttachmentRef::is_valid_fetch_token(token))
        .map(str::to_owned);
    match AttachmentRef::new(token, mime_type, file_name, byte_len)
        .and_then(|reference| reference.with_fetch_token(fetch_token))
    {
        Ok(reference) => MessagePart::Attachment(reference),
        Err(_) => MessagePart::Unsupported {
            kind: "attachment".to_owned(),
        },
    }
}

/// Map an engine payload into a domain conversation and message pair.
///
/// Only normal inbound iMessages are mapped. Reactions, edits, unsends, delivery
/// receipts and unmodelled variants are skipped with an explicit reason rather than
/// guessed at, and content is never invented.
pub fn map_inbound(
    payload: &Value,
    my_handles: &[String],
    id: &TransportId,
) -> Result<TransportEvent, SkipReason> {
    map_inbound_with_sequence(payload, my_handles, id, None)
}

fn envelope_sender(inst: &Value) -> Result<Handle, SkipReason> {
    let sender = inst
        .get("sender")
        .and_then(Value::as_str)
        .ok_or(SkipReason::MissingSender)?;
    handle_from(sender).ok_or(SkipReason::InvalidIdentity)
}

/// Reaction kinds this mapper has evidence for. Anything else, including a
/// free-form string or an emoji object, is skipped rather than guessed.
const KNOWN_REACTIONS: [&str; 6] = ["heart", "like", "dislike", "laugh", "emphasize", "question"];

/// A bounded number of foreign stdout lines is tolerated so one engine logger
/// cannot permanently kill the session; repeated corruption still fails closed.
const MAX_FOREIGN_STDOUT_LINES: u64 = 16;

fn map_reaction(
    inst: &Value,
    message_value: &Value,
    id: &TransportId,
) -> Result<TransportEvent, SkipReason> {
    let chat_guid = inst
        .get("conversation")
        .and_then(|conversation| conversation.get("sender_guid"))
        .and_then(Value::as_str)
        .ok_or(SkipReason::MissingChatGuid)?;
    let sender = envelope_sender(inst)?;
    let target = message_value
        .get("to_uuid")
        .and_then(Value::as_str)
        .ok_or(SkipReason::MissingReactionTarget)?;
    let conversation_id =
        ConversationId::new(id.clone(), chat_guid).map_err(|_| SkipReason::InvalidIdentity)?;
    let message_id =
        MessageId::new(id.clone(), target).map_err(|_| SkipReason::MissingReactionTarget)?;

    let wrapper = message_value
        .get("reaction")
        .ok_or(SkipReason::UnsupportedReaction)?;
    let active = wrapper
        .get("enable")
        .and_then(Value::as_bool)
        .ok_or(SkipReason::UnsupportedReaction)?;
    let kind = wrapper
        .get("reaction")
        .and_then(Value::as_str)
        .filter(|kind| KNOWN_REACTIONS.contains(kind))
        .ok_or(SkipReason::UnsupportedReaction)?
        .to_owned();
    let part = message_value
        .get("to_part")
        .and_then(Value::as_u64)
        .and_then(|part| u32::try_from(part).ok());
    let at = inst.get("sent_timestamp_ms").and_then(Value::as_i64);

    Ok(TransportEvent::ReactionChanged {
        conversation_id,
        message_id,
        sender,
        kind,
        active,
        part,
        at,
    })
}

/// Replacement content for an edit. Only a nonempty list of nonempty text parts
/// is accepted; anything else rejects the whole list rather than partially applying it.
fn replacement_text_parts(message_value: &Value) -> Result<Vec<MessagePart>, SkipReason> {
    let list = message_value
        .get("new_parts")
        .and_then(Value::as_array)
        .filter(|list| !list.is_empty())
        .ok_or(SkipReason::UnsupportedEditContent)?;
    let mut parts = Vec::with_capacity(list.len());
    for entry in list {
        let part = entry
            .get("part")
            .ok_or(SkipReason::UnsupportedEditContent)?;
        if part.get("type").and_then(Value::as_str) != Some("text") {
            return Err(SkipReason::UnsupportedEditContent);
        }
        let text = part
            .get("text")
            .and_then(Value::as_str)
            .filter(|text| !text.is_empty())
            .ok_or(SkipReason::UnsupportedEditContent)?;
        parts.push(MessagePart::Text(text.to_owned()));
    }
    Ok(parts)
}

/// Map an inbound edit or unsend payload. Both name one existing message part;
/// only an edit carries replacement content. The target is never created here.
fn map_mutation(
    inst: &Value,
    message_value: &Value,
    id: &TransportId,
    edit: bool,
) -> Result<TransportEvent, SkipReason> {
    let chat_guid = inst
        .get("conversation")
        .and_then(|conversation| conversation.get("sender_guid"))
        .and_then(Value::as_str)
        .filter(|guid| !guid.is_empty())
        .ok_or(SkipReason::MissingChatGuid)?;
    let sender = envelope_sender(inst)?;
    let at = inst
        .get("sent_timestamp_ms")
        .and_then(Value::as_i64)
        .ok_or(SkipReason::MissingMutationTimestamp)?;
    let target = message_value
        .get("tuuid")
        .and_then(Value::as_str)
        .filter(|target| !target.is_empty())
        .ok_or(SkipReason::MissingMutationTarget)?;
    let part = message_value
        .get("edit_part")
        .and_then(Value::as_u64)
        .ok_or(SkipReason::MissingMutationPart)?;
    let replacement_parts = if edit {
        Some(replacement_text_parts(message_value)?)
    } else {
        None
    };
    let conversation_id =
        ConversationId::new(id.clone(), chat_guid).map_err(|_| SkipReason::InvalidIdentity)?;
    let message_id =
        MessageId::new(id.clone(), target).map_err(|_| SkipReason::MissingMutationTarget)?;
    Ok(match replacement_parts {
        Some(replacement_parts) => TransportEvent::MessageEdited {
            conversation_id,
            message_id,
            sender,
            part,
            replacement_parts,
            at,
        },
        None => TransportEvent::MessageUnsent {
            conversation_id,
            message_id,
            sender,
            part,
            at,
        },
    })
}

fn map_inbound_with_sequence(
    payload: &Value,
    my_handles: &[String],
    id: &TransportId,
    push_sequence: Option<u64>,
) -> Result<TransportEvent, SkipReason> {
    if payload.get("variant").and_then(Value::as_str) != Some("iMessage") {
        return Err(SkipReason::NotAnInboundMessage);
    }
    let inst = payload.get("inst").ok_or(SkipReason::NotAnInboundMessage)?;
    let message_value = inst.get("message").ok_or(SkipReason::NotANormalMessage)?;
    let message_type = message_value.get("type").and_then(Value::as_str);
    match message_type {
        Some("react") => return map_reaction(inst, message_value, id),
        Some("edit") => return map_mutation(inst, message_value, id, true),
        Some("unsend") => return map_mutation(inst, message_value, id, false),
        _ => {}
    }
    if message_type != Some("normal") {
        return Err(SkipReason::NotANormalMessage);
    }
    let normal = message_value
        .get("value")
        .ok_or(SkipReason::NotANormalMessage)?;

    let chat_guid = inst
        .get("conversation")
        .and_then(|conversation| conversation.get("sender_guid"))
        .and_then(Value::as_str)
        .ok_or(SkipReason::MissingChatGuid)?;
    let message_id = inst
        .get("id")
        .and_then(Value::as_str)
        .ok_or(SkipReason::MissingMessageId)?;
    let sender = inst
        .get("sender")
        .and_then(Value::as_str)
        .ok_or(SkipReason::MissingSender)?;
    let sent_at = inst
        .get("sent_timestamp_ms")
        .and_then(Value::as_i64)
        .unwrap_or_default();
    let from_me = my_handles.iter().any(|handle| handle == sender);

    let mut parts = Vec::new();
    let mut part_ids = Vec::new();
    if let Some(list) = normal.get("parts").and_then(Value::as_array) {
        for (position, entry) in list.iter().enumerate() {
            let part = match entry.get("part") {
                Some(part) => part,
                None => continue,
            };
            let idx = entry.get("idx").and_then(Value::as_u64);
            let mapped = match part.get("type").and_then(Value::as_str) {
                Some("text") => part
                    .get("text")
                    .and_then(Value::as_str)
                    .filter(|text| !text.is_empty())
                    .map(|text| MessagePart::Text(text.to_owned())),
                Some("attachment") => Some(attachment_part(part, id, message_id, position)),
                Some(kind) => Some(MessagePart::Unsupported {
                    kind: kind.to_owned(),
                }),
                None => None,
            };
            if let Some(mapped) = mapped {
                parts.push(mapped);
                part_ids.push(idx);
            }
        }
    }
    if parts.is_empty() {
        return Err(SkipReason::NoContent);
    }
    if matches!(parts.as_slice(), [MessagePart::Text(_)]) {
        if let [slot] = part_ids.as_mut_slice() {
            if slot.is_none() {
                *slot = Some(0);
            }
        }
    }

    let sender_handle = envelope_sender(inst)?;
    let conversation_id =
        ConversationId::new(id.clone(), chat_guid).map_err(|_| SkipReason::InvalidIdentity)?;
    let message_id =
        MessageId::new(id.clone(), message_id).map_err(|_| SkipReason::InvalidIdentity)?;

    let mut participants = Vec::new();
    let mut remote_participants = 0usize;
    if let Some(list) = inst
        .get("conversation")
        .and_then(|conversation| conversation.get("participants"))
        .and_then(Value::as_array)
    {
        for entry in list {
            let Some(raw) = entry.as_str() else { continue };
            let Some(handle) = handle_from(raw) else {
                continue;
            };
            let is_me = my_handles.iter().any(|mine| mine == raw);
            if !is_me {
                remote_participants += 1;
            }
            participants.push(Participant {
                handle,
                display_name: None,
                is_me,
            });
        }
    }
    if participants.is_empty() {
        participants.push(Participant {
            handle: sender_handle.clone(),
            display_name: None,
            is_me: from_me,
        });
        if !from_me {
            remote_participants = 1;
        }
    }

    let title = inst
        .get("conversation")
        .and_then(|conversation| conversation.get("cv_name"))
        .and_then(Value::as_str)
        .map(str::to_owned);

    let conversation = Conversation {
        id: conversation_id.clone(),
        remote_id: chat_guid.to_owned(),
        kind: if remote_participants > 1 {
            ConversationKind::Group
        } else {
            ConversationKind::Direct
        },
        title,
        participants,
        created_at: sent_at,
        last_message_at: Some(sent_at),
        aliases: Vec::new(),
        preview: None,
    };

    let message = Message::new(
        message_id,
        conversation_id.clone(),
        None,
        sender_handle,
        parts,
        sent_at,
        None,
        if from_me {
            DeliveryState::Sent
        } else {
            DeliveryState::Delivered
        },
        from_me,
        None,
        Vec::new(),
        None,
    )
    .map_err(|_| SkipReason::InvalidIdentity)?;

    Ok(TransportEvent::Message {
        conversation,
        message,
        part_ids,
        push_sequence,
    })
}

/// Map a `conversation` frame into a domain conversation. Returns None when the
/// frame is unusable, so one malformed entry cannot end the whole listing.
pub fn map_conversation(
    value: Option<&Value>,
    my_handles: &[String],
    id: &TransportId,
) -> Option<TransportEvent> {
    let value = value?;
    let guid = value.get("guid")?.as_str()?.trim();
    if guid.is_empty() {
        return None;
    }
    let title = value
        .get("title")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|title| !title.is_empty())
        .map(str::to_owned);
    let mut participants = Vec::new();
    if let Some(raw) = value.get("participants").and_then(Value::as_array) {
        for entry in raw {
            let Some(text) = entry.as_str() else { continue };
            let Some(handle) = handle_from(text) else {
                continue;
            };
            participants.push(Participant {
                handle,
                display_name: None,
                is_me: my_handles.iter().any(|mine| mine == text),
            });
        }
    }
    let is_group = value
        .get("is_group")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    Some(TransportEvent::Conversation {
        conversation: Conversation {
            id: ConversationId::new(id.clone(), guid).ok()?,
            remote_id: guid.to_owned(),
            kind: if is_group {
                ConversationKind::Group
            } else {
                ConversationKind::Direct
            },
            title,
            participants,
            created_at: 0,
            last_message_at: None,
            aliases: Vec::new(),
            preview: None,
        },
    })
}

/// Extract the text to send from an outgoing body. Only plain text is supported.
pub fn outgoing_text(body: &OutgoingBody) -> TransportResult<String> {
    text_of(&body.parts).ok_or(TransportError::Unsupported)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn transport() -> TransportId {
        transport_id()
    }

    #[test]
    fn engine_does_not_advertise_outbound_reactions() {
        let engine = Engine::new(EngineConfig {
            dart_executable: PathBuf::new(),
            script: PathBuf::new(),
            state_dir: PathBuf::new(),
            library: PathBuf::new(),
            sync_chats: false,
        });
        assert!(!engine.capabilities().reactions);
    }

    #[test]
    fn engine_slot_starts_with_the_given_engine() {
        let engine = Arc::new(Engine::new(EngineConfig {
            dart_executable: PathBuf::new(),
            script: PathBuf::new(),
            state_dir: PathBuf::new(),
            library: PathBuf::new(),
            sync_chats: false,
        }));
        let slot = engine_slot(engine.clone());
        assert!(Arc::ptr_eq(&engine, &slot.read().unwrap()));
    }

    #[test]
    fn replacing_slot_contents_is_visible_to_a_later_read() {
        let first = Arc::new(Engine::new(EngineConfig {
            dart_executable: PathBuf::new(),
            script: PathBuf::new(),
            state_dir: PathBuf::new(),
            library: PathBuf::new(),
            sync_chats: false,
        }));
        let second = Arc::new(Engine::new(EngineConfig {
            dart_executable: PathBuf::new(),
            script: PathBuf::new(),
            state_dir: PathBuf::new(),
            library: PathBuf::new(),
            sync_chats: false,
        }));
        let slot = engine_slot(first.clone());
        assert!(Arc::ptr_eq(&first, &slot.read().unwrap()));
        *slot.write().unwrap() = second.clone();
        assert!(Arc::ptr_eq(&second, &slot.read().unwrap()));
    }

    #[tokio::test]
    async fn multi_recipient_send_requires_sidecar_capability() {
        let engine = Engine::new(EngineConfig {
            dart_executable: PathBuf::new(),
            script: PathBuf::new(),
            state_dir: PathBuf::new(),
            library: PathBuf::new(),
            sync_chats: false,
        });
        engine.started.store(true, Ordering::SeqCst);

        let result = engine
            .send_text(
                &[
                    "mailto:one@example.invalid".to_owned(),
                    "tel:+15550000002".to_owned(),
                ],
                Some("iMessage;-;group"),
                "group message",
            )
            .await;
        assert!(matches!(result, Err(TransportError::Unsupported)));
        assert!(engine.pending.waiters.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn failed_start_releases_child_and_allows_retry() {
        let engine = Engine::new(EngineConfig {
            dart_executable: PathBuf::new(),
            script: PathBuf::new(),
            state_dir: PathBuf::new(),
            library: PathBuf::new(),
            sync_chats: false,
        });

        let mut command = Command::new("sh");
        command
            .args(["-c", "exit 0"])
            .stdout(std::process::Stdio::null());
        assert!(matches!(
            engine.start_with(command).await,
            Err(TransportError::Internal)
        ));
        assert!(!engine.started.load(Ordering::SeqCst));
        assert!(engine.outgoing.lock().unwrap().is_some());
        assert!(engine.child.lock().await.is_none());
        assert!(engine.stdin.lock().await.is_none());

        let mut retry = Command::new("sh");
        retry
            .args(["-c", "exit 0"])
            .stdout(std::process::Stdio::null());
        assert!(matches!(
            engine.start_with(retry).await,
            Err(TransportError::Internal)
        ));
    }

    fn payload(sender: &str, text: &str) -> Value {
        json!({
            "variant": "iMessage",
            "complete": true,
            "inst": {
                "id": "11111111-2222-3333-4444-555555555555",
                "sender": sender,
                "conversation": {
                    "participants": [sender, "mailto:me@example.invalid"],
                    "cv_name": null,
                    "sender_guid": "iMessage;-;someone@example.com",
                    "after_guid": null
                },
                "message": {
                    "type": "normal",
                    "complete": true,
                    "value": {
                        "parts": [{"idx": 0, "part": {"type": "text", "text": text}}],
                        "service": {"type": "iMessage", "complete": true}
                    }
                },
                "sent_timestamp_ms": 1700000000000i64,
                "send_delivered": false,
                "verification_failed": false
            }
        })
    }

    #[test]
    fn maps_an_inbound_message_with_participants_and_text() {
        let event = map_inbound(
            &payload("tel:+15550000001", "hello"),
            &["mailto:me@example.invalid".to_owned()],
            &transport(),
        )
        .expect("payload maps");

        let TransportEvent::Message {
            conversation,
            message,
            ..
        } = event
        else {
            panic!("expected a message event");
        };
        assert_eq!(conversation.id.value(), "iMessage;-;someone@example.com");
        assert_eq!(message.conversation_id(), &conversation.id);
        assert!(!message.from_me);
        assert_eq!(message.parts(), &[MessagePart::Text("hello".to_owned())]);
        assert_eq!(message.created_at, 1_700_000_000_000);
    }

    #[test]
    fn carries_optional_push_sequence_with_inbound_message() {
        let event = map_inbound_with_sequence(
            &payload("tel:+15550000001", "hello"),
            &[],
            &transport(),
            Some(17),
        )
        .expect("payload maps");
        assert!(matches!(
            event,
            TransportEvent::Message {
                push_sequence: Some(17),
                ..
            }
        ));
    }

    #[test]
    fn marks_our_own_handle_and_echoes_as_from_me() {
        let event = map_inbound(
            &payload("mailto:me@example.invalid", "mine"),
            &["mailto:me@example.invalid".to_owned()],
            &transport(),
        )
        .unwrap();
        let TransportEvent::Message { message, .. } = event else {
            panic!("expected a message event");
        };
        assert!(message.from_me);
    }

    fn attachment_payload(parts: Value) -> Value {
        let mut value = payload("tel:+15550000001", "ignored");
        value["inst"]["message"]["value"]["parts"] = parts;
        value
    }

    fn mapped_message(value: &Value) -> Message {
        let TransportEvent::Message { message, .. } =
            map_inbound(value, &[], &transport()).expect("payload maps")
        else {
            panic!("expected a message event");
        };
        message
    }

    fn mapped_part_ids(value: &Value) -> (Vec<Option<u64>>, usize) {
        let TransportEvent::Message {
            message, part_ids, ..
        } = map_inbound(value, &[], &transport()).expect("payload maps")
        else {
            panic!("expected a message event");
        };
        (part_ids, message.parts().len())
    }

    #[test]
    fn preserves_explicit_engine_part_ids() {
        let value = attachment_payload(json!([
            {"idx": 4, "part": {"type": "text", "text": "a"}},
            {"idx": 9, "part": {"type": "text", "text": "b"}}
        ]));
        let (part_ids, part_count) = mapped_part_ids(&value);
        assert_eq!(part_ids, vec![Some(4), Some(9)]);
        assert_eq!(part_ids.len(), part_count);
    }

    #[test]
    fn skipped_entries_do_not_misalign_part_ids() {
        let value = attachment_payload(json!([
            {"idx": 1, "part": {"type": "text", "text": "a"}},
            {"idx": 2, "part": {"type": "text", "text": ""}},
            {"idx": 3},
            {"idx": 5, "part": {"type": "text", "text": "b"}}
        ]));
        let (part_ids, part_count) = mapped_part_ids(&value);
        assert_eq!(part_count, 2, "empty and malformed entries are skipped");
        assert_eq!(part_ids, vec![Some(1), Some(5)]);
        assert_eq!(part_ids.len(), part_count);
    }

    #[test]
    fn normalizes_a_single_plain_part_to_zero() {
        let value = attachment_payload(json!([
            {"part": {"type": "text", "text": "plain"}}
        ]));
        let (part_ids, part_count) = mapped_part_ids(&value);
        assert_eq!(part_count, 1);
        assert_eq!(part_ids, vec![Some(0)]);
    }

    #[test]
    fn does_not_invent_an_id_for_a_single_attachment() {
        let value = attachment_payload(json!([
            {"part": {"type": "attachment", "name": "one.bin"}}
        ]));
        let (part_ids, part_count) = mapped_part_ids(&value);
        assert_eq!(part_count, 1);
        assert_eq!(part_ids, vec![None]);
    }

    fn first_attachment(message: &Message) -> &AttachmentRef {
        message
            .parts()
            .iter()
            .find_map(|part| match part {
                MessagePart::Attachment(reference) => Some(reference),
                _ => None,
            })
            .expect("message has an attachment")
    }

    fn expected_token(message_id: &str, position: usize) -> String {
        format!(
            "local-metadata:{}",
            serde_json::to_string(&("openbubbles", message_id, position)).unwrap()
        )
    }

    const MESSAGE_ID: &str = "11111111-2222-3333-4444-555555555555";

    fn attachment_field(part: &MessagePart, field: &str) -> Value {
        serde_json::to_value(part).unwrap()["value"][field].clone()
    }

    #[test]
    fn attachment_metadata_is_preserved_for_named_parts() {
        let value = attachment_payload(json!([
            {"idx": 0, "part": {"type": "attachment", "complete": true, "name": "a.jpg"}}
        ]));
        let message = mapped_message(&value);
        let reference = first_attachment(&message);
        assert_eq!(reference.id(), expected_token(MESSAGE_ID, 0));
        assert_eq!(
            message.parts(),
            &[MessagePart::Attachment(
                AttachmentRef::new(
                    expected_token(MESSAGE_ID, 0),
                    None,
                    Some("a.jpg".to_owned()),
                    None
                )
                .unwrap()
            )]
        );
    }

    #[test]
    fn attachment_without_metadata_falls_back_to_unsupported() {
        let value = attachment_payload(json!([
            {"idx": 0, "part": {"type": "attachment", "complete": true}}
        ]));
        let message = mapped_message(&value);
        assert_eq!(
            message.parts(),
            &[MessagePart::Unsupported {
                kind: "attachment".to_owned()
            }]
        );
    }

    #[test]
    fn attachment_metadata_keeps_a_valid_fetch_token_and_ignores_invalid() {
        const TOKEN: &str = "0123456789abcdef0123456789abcdef";
        let value = attachment_payload(json!([
            {"part": {"type": "attachment", "name": "a.jpg", "fetch_token": TOKEN}},
            {"part": {"type": "attachment", "name": "b.jpg", "fetch_token": "not-a-token"}}
        ]));
        let message = mapped_message(&value);
        assert_eq!(first_attachment(&message).fetch_token(), Some(TOKEN));
        assert_eq!(
            first_attachment(&message).fetch_token(),
            Some(TOKEN),
            "only the first attachment is checked here"
        );
        let second = Message::new(
            message.id().clone(),
            message.conversation_id().clone(),
            None,
            message.sender.clone(),
            vec![message.parts()[1].clone()],
            message.created_at,
            None,
            message.delivery.clone(),
            message.from_me,
            None,
            Vec::new(),
            None,
        )
        .unwrap();
        assert_eq!(first_attachment(&second).fetch_token(), None);
    }

    #[test]
    fn attachment_metadata_preserves_exact_filename_and_mime() {
        let value = attachment_payload(json!([
            {"part": {"type": "attachment", "name": " Report  Final.PDF ", "mime": "application/x-Custom"}}
        ]));
        let reference = first_attachment(&mapped_message(&value)).clone();
        assert_eq!(
            serde_json::to_value(&reference).unwrap()["file_name"],
            json!(" Report  Final.PDF ")
        );
        assert_eq!(
            serde_json::to_value(&reference).unwrap()["mime_type"],
            json!("application/x-Custom")
        );
    }

    #[test]
    fn attachment_metadata_reads_inline_lengths() {
        let value = attachment_payload(json!([
            {"part": {"type": "attachment", "name": "zero", "attachment_type": {"kind": "inline", "byte_length": 0}}},
            {"part": {"type": "attachment", "name": "max", "attachment_type": {"kind": "inline", "byte_length": u64::MAX}}}
        ]));
        let message = mapped_message(&value);
        assert_eq!(attachment_field(&message.parts()[0], "byte_len"), json!(0));
        assert_eq!(
            attachment_field(&message.parts()[1], "byte_len"),
            json!(u64::MAX)
        );
    }

    #[test]
    fn attachment_metadata_reads_mmcs_size_only() {
        let value = attachment_payload(json!([
            {"part": {"type": "attachment", "name": "mmcs", "attachment_type": {"kind": "mmcs", "size": 12}}},
            {"part": {"type": "attachment", "name": "wrong-kind", "attachment_type": {"kind": "mmcs", "byte_length": 8, "size": 4}}},
            {"part": {"type": "attachment", "name": "maximum", "attachment_type": {"kind": "mmcs", "size": u64::MAX}}}
        ]));
        let message = mapped_message(&value);
        assert_eq!(attachment_field(&message.parts()[0], "byte_len"), json!(12));
        assert_eq!(attachment_field(&message.parts()[1], "byte_len"), json!(4));
        assert_eq!(
            attachment_field(&message.parts()[2], "byte_len"),
            json!(u64::MAX)
        );
    }

    #[test]
    fn attachment_metadata_rejects_malformed_lengths() {
        for (kind, field) in [("inline", "byte_length"), ("mmcs", "size")] {
            for invalid in [
                json!(-1),
                json!(1.5),
                json!(1.0),
                json!("7"),
                json!(true),
                Value::Null,
                json!({}),
            ] {
                let mut part = json!({"type": "attachment", "name": "invalid.bin",
                    "attachment_type": {"kind": kind}});
                part["attachment_type"][field] = invalid;
                let message = mapped_message(&attachment_payload(json!([{"part": part}])));
                assert_eq!(
                    attachment_field(&message.parts()[0], "byte_len"),
                    Value::Null
                );
            }
        }
    }

    #[test]
    fn attachment_metadata_does_not_use_uti_or_extensions_for_mime() {
        let value = attachment_payload(json!([
            {"part": {"type": "attachment", "name": "photo.HEIC", "uti_type": "public.jpeg"}}
        ]));
        let user_value = first_attachment(&mapped_message(&value)).clone();
        assert_eq!(
            serde_json::to_value(&user_value).unwrap()["mime_type"],
            Value::Null
        );
    }

    #[test]
    fn attachment_metadata_keeps_mixed_text_order_and_whitespace() {
        let value = attachment_payload(json!([
            {"idx": 0, "part": {"type": "text", "text": "  leading"}},
            {"idx": 1, "part": {"type": "attachment", "name": "one.bin", "attachment_type": {"kind": "mmcs", "size": 3}}},
            {"idx": 2, "part": {"type": "text", "text": "trailing\n"}},
            {"idx": 3, "part": {"type": "attachment", "name": "two.bin"}},
            {"idx": 4, "part": {"type": "poll"}}
        ]));
        let message = mapped_message(&value);
        assert_eq!(message.parts().len(), 5);
        assert_eq!(
            message.parts()[0],
            MessagePart::Text("  leading".to_owned())
        );
        assert_eq!(
            message.parts()[2],
            MessagePart::Text("trailing\n".to_owned())
        );
        assert_eq!(
            message.parts()[4],
            MessagePart::Unsupported {
                kind: "poll".to_owned()
            }
        );
        assert_eq!(attachment_field(&message.parts()[1], "byte_len"), json!(3));
        let ids: Vec<_> = message
            .parts()
            .iter()
            .filter_map(|part| match part {
                MessagePart::Attachment(reference) => Some(reference.id().to_owned()),
                _ => None,
            })
            .collect();
        assert_eq!(
            ids,
            vec![expected_token(MESSAGE_ID, 1), expected_token(MESSAGE_ID, 3)]
        );
    }

    #[test]
    fn attachment_metadata_skips_entries_without_a_part_container() {
        let value = attachment_payload(json!([
            {"idx": 0},
            {"part": {"type": "attachment", "name": "second.bin"}}
        ]));
        let message = mapped_message(&value);
        assert_eq!(
            first_attachment(&message).id(),
            expected_token(MESSAGE_ID, 1)
        );
    }

    #[test]
    fn attachment_metadata_token_is_exact_and_replay_deterministic() {
        let value = attachment_payload(json!([
            {"part": {"type": "attachment", "name": "x.bin"}}
        ]));
        let first = first_attachment(&mapped_message(&value)).id().to_owned();
        let second = first_attachment(&mapped_message(&value)).id().to_owned();
        assert_eq!(first, second);
        assert_eq!(
            first,
            format!("local-metadata:[\"openbubbles\",\"{MESSAGE_ID}\",0]")
        );
    }

    #[test]
    fn attachment_metadata_token_varies_by_message_transport_and_position() {
        let value = attachment_payload(json!([
            {"part": {"type": "attachment", "name": "x.bin"}}
        ]));
        let base = first_attachment(&mapped_message(&value)).id().to_owned();

        let mut other_message = value.clone();
        other_message["inst"]["id"] = json!("99999999-0000-0000-0000-000000000000");
        let other = first_attachment(&mapped_message(&other_message))
            .id()
            .to_owned();
        assert_ne!(base, other);

        let TransportEvent::Message { message, .. } =
            map_inbound(&value, &[], &TransportId::new("other-transport").unwrap()).unwrap()
        else {
            panic!("expected a message event");
        };
        assert_ne!(base, first_attachment(&message).id());

        let two = attachment_payload(json!([
            {"part": {"type": "attachment", "name": "x.bin"}},
            {"part": {"type": "attachment", "name": "x.bin"}}
        ]));
        let message = mapped_message(&two);
        assert_ne!(message.parts()[0], message.parts()[1]);
    }

    #[test]
    fn attachment_metadata_token_ignores_engine_numbering() {
        let numbered = attachment_payload(json!([
            {"idx": 7, "part": {"type": "attachment", "part": 99, "name": "x.bin"}}
        ]));
        let unnumbered = attachment_payload(json!([
            {"part": {"type": "attachment", "name": "x.bin"}}
        ]));
        assert_eq!(
            first_attachment(&mapped_message(&numbered)).id(),
            first_attachment(&mapped_message(&unnumbered)).id()
        );
    }

    #[test]
    fn attachment_metadata_accepts_exact_token_budget() {
        let overhead = expected_token("", 0).len();
        for extra in [0, 1] {
            let mut value = attachment_payload(json!([
                {"part": {"type": "attachment", "name": "boundary.bin"}}
            ]));
            value["inst"]["id"] = json!("m".repeat(256 - overhead + extra));
            let message = mapped_message(&value);
            if extra == 0 {
                assert_eq!(first_attachment(&message).id().len(), 256);
            } else {
                assert_eq!(
                    message.parts(),
                    &[MessagePart::Unsupported {
                        kind: "attachment".into()
                    }]
                );
            }
        }
    }

    #[test]
    fn attachment_metadata_rejects_oversized_tokens_without_losing_text() {
        for (transport_name, raw_id) in [
            ("openbubbles".to_owned(), "m".repeat(300)),
            ("t".repeat(200), "m".repeat(100)),
            ("openbubbles".to_owned(), "\\".repeat(120)),
        ] {
            let mut value = attachment_payload(json!([
                {"part": {"type": "text", "text": " before\n"}},
                {"part": {"type": "attachment", "name": "bounded.bin"}},
                {"part": {"type": "text", "text": "after "}}
            ]));
            value["inst"]["id"] = json!(raw_id);
            let transport = TransportId::new(transport_name).unwrap();
            let TransportEvent::Message { message, .. } =
                map_inbound(&value, &[], &transport).unwrap()
            else {
                panic!("expected a message event");
            };
            assert_eq!(
                message.parts(),
                &[
                    MessagePart::Text(" before\n".into()),
                    MessagePart::Unsupported {
                        kind: "attachment".into()
                    },
                    MessagePart::Text("after ".into()),
                ]
            );
        }
    }

    #[test]
    fn attachment_metadata_validates_optional_fields_independently() {
        let value = attachment_payload(json!([
            {"part": {"type": "attachment", "name": 12, "mime": "image/png"}},
            {"part": {"type": "attachment", "name": "", "mime": false,
                "attachment_type": {"kind": "inline", "byte_length": 0}}},
            {"part": {"type": "attachment", "name": false, "mime": {},
                "attachment_type": {"kind": "mmcs", "size": "2"}}},
            {"part": {"type": "attachment", "uti_type": "public.jpeg",
                "attachment_type": {"kind": "unknown", "size": 2, "byte_length": 2}}},
            {"part": {"type": "attachment", "name": "", "mime": "",
                "attachment_type": {"size": 2, "byte_length": 2}}}
        ]));
        let message = mapped_message(&value);
        assert_eq!(
            attachment_field(&message.parts()[0], "file_name"),
            Value::Null
        );
        assert_eq!(
            attachment_field(&message.parts()[0], "mime_type"),
            json!("image/png")
        );
        assert_eq!(
            attachment_field(&message.parts()[0], "byte_len"),
            Value::Null
        );
        assert_eq!(
            attachment_field(&message.parts()[1], "file_name"),
            Value::Null
        );
        assert_eq!(
            attachment_field(&message.parts()[1], "mime_type"),
            Value::Null
        );
        assert_eq!(attachment_field(&message.parts()[1], "byte_len"), json!(0));
        for part in &message.parts()[2..] {
            assert_eq!(
                part,
                &MessagePart::Unsupported {
                    kind: "attachment".into()
                }
            );
        }
    }

    #[test]
    fn attachment_metadata_ignores_remote_access_canaries() {
        let value = attachment_payload(json!([
            {"part": {
                "type": "attachment",
                "name": "safe.bin",
                "mime": "application/octet-stream",
                "attachment_type": {
                    "kind": "mmcs", "size": 5,
                    "object": "CANARY_OBJECT", "url": "https://CANARY_URL.invalid/x",
                    "key_base64": {"ignored": "CANARY_KEY"},
                    "signature_base64": ["CANARY_SIGNATURE"]
                },
                "iris": "CANARY_IRIS",
                "uti_type": "CANARY_UTI",
                "weird": {"nested": ["CANARY_NESTED", 3]},
                "wrong_bool": "CANARY_WRONG_BOOL",
                "wrong_number": "CANARY_WRONG_NUMBER"
            }}
        ]));
        let message = mapped_message(&value);
        let serialized = serde_json::to_string(&message).unwrap();
        for canary in [
            "CANARY_OBJECT",
            "CANARY_URL",
            "CANARY_KEY",
            "CANARY_SIGNATURE",
            "CANARY_IRIS",
            "CANARY_UTI",
            "CANARY_NESTED",
            "CANARY_WRONG_BOOL",
            "CANARY_WRONG_NUMBER",
        ] {
            assert!(!serialized.contains(canary), "leaked {canary}");
        }
        let reference = first_attachment(&message);
        let serialized_reference = serde_json::to_value(reference).unwrap();
        assert_eq!(serialized_reference["file_name"], json!("safe.bin"));
        assert_eq!(
            serialized_reference["mime_type"],
            json!("application/octet-stream")
        );
        assert_eq!(serialized_reference["byte_len"], json!(5));
    }

    #[test]
    fn attachment_metadata_survives_encrypted_store_roundtrip() {
        use crate::store::{
            get_message, upsert_conversation, upsert_message, EncryptedStore, MessageUpsertOutcome,
        };

        let value = attachment_payload(json!([
            {"part": {
                "type": "attachment",
                "name": "roundtrip.bin",
                "mime": "application/octet-stream",
                "attachment_type": {
                    "kind": "mmcs", "size": 0,
                    "object": "CANARY_OBJECT", "url": "CANARY_URL",
                    "key_base64": {"ignored": "CANARY_KEY"},
                    "signature_base64": ["CANARY_SIGNATURE"]
                },
                "part": "CANARY_NUMBER", "uti_type": "CANARY_UTI", "iris": "CANARY_IRIS"
            }}
        ]));
        let TransportEvent::Message {
            conversation,
            message,
            ..
        } = map_inbound(&value, &[], &transport()).unwrap()
        else {
            panic!("expected a message event");
        };

        let temp = tempfile::tempdir().unwrap();
        let private = temp.path().join("private");
        std::fs::create_dir(&private).unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&private, std::fs::Permissions::from_mode(0o700)).unwrap();
        }
        let database = private.join("messages.db");
        let key = [42u8; 32];

        let mut store = EncryptedStore::open_with_key(&database, &key).unwrap();
        upsert_conversation(&mut store, &conversation).unwrap();
        assert_eq!(
            upsert_message(&mut store, &message).unwrap(),
            MessageUpsertOutcome::Inserted
        );
        assert_eq!(
            upsert_message(&mut store, &mapped_message(&value)).unwrap(),
            MessageUpsertOutcome::Unchanged
        );
        drop(store);

        let store = EncryptedStore::open_with_key(&database, &key).unwrap();
        let stored = get_message(&store, message.id()).unwrap().expect("stored");
        assert_eq!(stored, message);
        let reference = serde_json::to_value(first_attachment(&stored)).unwrap();
        assert_eq!(reference["file_name"], json!("roundtrip.bin"));
        assert_eq!(reference["mime_type"], json!("application/octet-stream"));
        assert_eq!(reference["byte_len"], json!(0));
        let serialized = serde_json::to_string(&stored).unwrap();
        for canary in [
            "CANARY_OBJECT",
            "CANARY_URL",
            "CANARY_KEY",
            "CANARY_SIGNATURE",
            "CANARY_NUMBER",
            "CANARY_UTI",
            "CANARY_IRIS",
        ] {
            assert!(
                !serialized.contains(canary),
                "leaked {canary} after reopening"
            );
        }
    }

    #[test]
    fn non_message_variants_and_receipts_are_skipped_with_a_reason() {
        let mut receipt = payload("tel:+15550000001", "x");
        receipt["inst"]["message"] = json!({"type": "delivered", "complete": true});
        assert!(matches!(
            map_inbound(&receipt, &[], &transport()),
            Err(SkipReason::NotANormalMessage)
        ));

        let confirm = json!({"variant": "sendConfirm", "complete": true, "uuid": "u"});
        assert!(matches!(
            map_inbound(&confirm, &[], &transport()),
            Err(SkipReason::NotAnInboundMessage)
        ));
    }

    #[test]
    fn missing_identity_or_content_is_rejected_rather_than_guessed() {
        let mut no_guid = payload("tel:+15550000001", "hi");
        no_guid["inst"]["conversation"]["sender_guid"] = Value::Null;
        assert!(matches!(
            map_inbound(&no_guid, &[], &transport()),
            Err(SkipReason::MissingChatGuid)
        ));

        let mut empty = payload("tel:+15550000001", "hi");
        empty["inst"]["message"]["value"]["parts"] = json!([]);
        assert!(matches!(
            map_inbound(&empty, &[], &transport()),
            Err(SkipReason::NoContent)
        ));
    }

    fn react_envelope(sender: &str) -> Value {
        json!({
            "variant": "iMessage",
            "complete": true,
            "inst": {
                "id": "11111111-2222-3333-4444-555555555555",
                "sender": sender,
                "conversation": {
                    "participants": [sender],
                    "sender_guid": "iMessage;-;someone@example.com"
                },
                "message": {
                    "type": "react",
                    "complete": true,
                    "to_uuid": "aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeeeee",
                    "to_part": 0,
                    "reaction": {"kind": "react", "enable": true, "reaction": "like"},
                    "to_text": "hello"
                }
            }
        })
    }

    #[test]
    fn react_envelope_sender_is_parsed() {
        let react = react_envelope("tel:+15550000001");
        assert_eq!(
            envelope_sender(&react["inst"]).unwrap(),
            handle_from("tel:+15550000001").unwrap()
        );
    }

    #[test]
    fn react_envelope_without_sender_is_missing_sender() {
        let mut react = react_envelope("tel:+15550000001");
        react["inst"].as_object_mut().unwrap().remove("sender");
        assert!(matches!(
            envelope_sender(&react["inst"]),
            Err(SkipReason::MissingSender)
        ));
    }

    #[test]
    fn react_envelope_with_malformed_sender_matches_normal_path() {
        let react = react_envelope("");
        assert!(matches!(
            envelope_sender(&react["inst"]),
            Err(SkipReason::InvalidIdentity)
        ));
        assert!(matches!(
            map_inbound(&payload("", "hello"), &[], &transport()),
            Err(SkipReason::InvalidIdentity)
        ));
    }

    #[test]
    fn react_maps_to_a_reaction_event() {
        let mut react = react_envelope("tel:+15550000001");
        react["inst"]["sent_timestamp_ms"] = json!(1_700_000_000_000i64);
        let event = map_inbound(&react, &[], &transport()).expect("react maps");
        let TransportEvent::ReactionChanged {
            conversation_id,
            message_id,
            sender,
            kind,
            active,
            part,
            at,
        } = event
        else {
            panic!("expected a reaction event");
        };
        assert_eq!(
            conversation_id,
            ConversationId::new(transport(), "iMessage;-;someone@example.com").unwrap()
        );
        assert_eq!(
            message_id,
            MessageId::new(transport(), "aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeeeee").unwrap()
        );
        assert_eq!(sender, handle_from("tel:+15550000001").unwrap());
        assert_eq!(kind, "like");
        assert!(active);
        assert_eq!(part, Some(0));
        assert_eq!(at, Some(1_700_000_000_000));
    }

    #[test]
    fn emoji_object_reaction_is_unsupported() {
        let mut react = react_envelope("tel:+15550000001");
        react["inst"]["message"]["reaction"]["reaction"] =
            json!({"emoji": "\u{1f600}", "payload": "x"});
        assert!(matches!(
            map_inbound(&react, &[], &transport()),
            Err(SkipReason::UnsupportedReaction)
        ));
        // A bare emoji string is not one of the mapper's evidence-backed kinds.
        let mut bare = react_envelope("tel:+15550000001");
        bare["inst"]["message"]["reaction"]["reaction"] = json!("\u{1f600}");
        assert!(matches!(
            map_inbound(&bare, &[], &transport()),
            Err(SkipReason::UnsupportedReaction)
        ));

        for unsupported in [json!(""), json!("unknown-kind"), json!("😀")] {
            let mut react = react_envelope("tel:+15550000001");
            react["inst"]["message"]["reaction"]["reaction"] = unsupported;
            assert!(matches!(
                map_inbound(&react, &[], &transport()),
                Err(SkipReason::UnsupportedReaction)
            ));
        }
    }

    #[test]
    fn react_without_a_target_is_missing_reaction_target() {
        let mut react = react_envelope("tel:+15550000001");
        react["inst"]["message"]
            .as_object_mut()
            .unwrap()
            .remove("to_uuid");
        assert!(matches!(
            map_inbound(&react, &[], &transport()),
            Err(SkipReason::MissingReactionTarget)
        ));
    }

    fn mutation_envelope(kind: &str) -> Value {
        let mut value = json!({
            "variant": "iMessage",
            "complete": true,
            "inst": {
                "id": "11111111-2222-3333-4444-555555555555",
                "sender": "tel:+15550000001",
                "sent_timestamp_ms": 1_700_000_000_000i64,
                "conversation": {
                    "participants": ["tel:+15550000001"],
                    "sender_guid": "iMessage;-;someone@example.com"
                },
                "message": {
                    "type": kind,
                    "complete": true,
                    "tuuid": "aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeeeee",
                    "edit_part": 1
                }
            }
        });
        if kind == "edit" {
            value["inst"]["message"]["new_parts"] = json!([
                {"idx": 0, "part": {"type": "text", "text": "first"}},
                {"idx": 5, "part": {"type": "text", "text": "second"}}
            ]);
        }
        value
    }

    #[test]
    fn edit_maps_to_a_typed_mutation_event_in_order() {
        let event = map_inbound(&mutation_envelope("edit"), &[], &transport()).expect("edit maps");
        let TransportEvent::MessageEdited {
            conversation_id,
            message_id,
            sender,
            part,
            replacement_parts,
            at,
        } = event
        else {
            panic!("expected an edit event");
        };
        assert_eq!(
            conversation_id,
            ConversationId::new(transport(), "iMessage;-;someone@example.com").unwrap()
        );
        assert_eq!(
            message_id,
            MessageId::new(transport(), "aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeeeee").unwrap()
        );
        assert_eq!(sender, handle_from("tel:+15550000001").unwrap());
        assert_eq!(part, 1);
        assert_eq!(
            replacement_parts,
            vec![
                MessagePart::Text("first".into()),
                MessagePart::Text("second".into())
            ]
        );
        assert_eq!(at, 1_700_000_000_000);
    }

    #[test]
    fn unsend_maps_to_a_typed_mutation_event() {
        let event =
            map_inbound(&mutation_envelope("unsend"), &[], &transport()).expect("unsend maps");
        let TransportEvent::MessageUnsent {
            conversation_id,
            message_id,
            sender,
            part,
            at,
        } = event
        else {
            panic!("expected an unsend event");
        };
        assert_eq!(
            conversation_id,
            ConversationId::new(transport(), "iMessage;-;someone@example.com").unwrap()
        );
        assert_eq!(
            message_id,
            MessageId::new(transport(), "aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeeeee").unwrap()
        );
        assert_eq!(sender, handle_from("tel:+15550000001").unwrap());
        assert_eq!(part, 1);
        assert_eq!(at, 1_700_000_000_000);
    }

    #[test]
    fn mutation_identity_fields_are_required() {
        for kind in ["edit", "unsend"] {
            let mut missing_target = mutation_envelope(kind);
            missing_target["inst"]["message"]
                .as_object_mut()
                .unwrap()
                .remove("tuuid");
            assert!(matches!(
                map_inbound(&missing_target, &[], &transport()),
                Err(SkipReason::MissingMutationTarget)
            ));

            let mut empty_target = mutation_envelope(kind);
            empty_target["inst"]["message"]["tuuid"] = json!("");
            assert!(matches!(
                map_inbound(&empty_target, &[], &transport()),
                Err(SkipReason::MissingMutationTarget)
            ));

            let mut missing_part = mutation_envelope(kind);
            missing_part["inst"]["message"]
                .as_object_mut()
                .unwrap()
                .remove("edit_part");
            assert!(matches!(
                map_inbound(&missing_part, &[], &transport()),
                Err(SkipReason::MissingMutationPart)
            ));

            let mut missing_timestamp = mutation_envelope(kind);
            missing_timestamp["inst"]
                .as_object_mut()
                .unwrap()
                .remove("sent_timestamp_ms");
            assert!(matches!(
                map_inbound(&missing_timestamp, &[], &transport()),
                Err(SkipReason::MissingMutationTimestamp)
            ));

            let mut malformed_sender = mutation_envelope(kind);
            malformed_sender["inst"]["sender"] = json!("");
            assert!(matches!(
                map_inbound(&malformed_sender, &[], &transport()),
                Err(SkipReason::InvalidIdentity)
            ));

            let mut empty_guid = mutation_envelope(kind);
            empty_guid["inst"]["conversation"]["sender_guid"] = json!("");
            assert!(matches!(
                map_inbound(&empty_guid, &[], &transport()),
                Err(SkipReason::MissingChatGuid)
            ));
        }
    }

    #[test]
    fn edit_replacement_content_must_be_nonempty_text_only() {
        let unsupported = [
            json!([]),
            json!([{"idx": 0, "part": {"type": "text", "text": ""}}]),
            json!([{"idx": 0, "part": {"type": "attachment", "name": "x"}}]),
            json!([{"idx": 0, "part": {"type": "text", "text": "ok"}},
                   {"idx": 1, "part": {"type": "attachment", "name": "x"}}]),
            json!([{"idx": 0, "part": {"type": "text", "text": "ok"}},
                   {"idx": 1, "part": {"type": "mention"}}]),
            json!([{"idx": 0}]),
            json!("text"),
        ];
        for new_parts in unsupported {
            let mut value = mutation_envelope("edit");
            value["inst"]["message"]["new_parts"] = new_parts.clone();
            assert!(
                matches!(
                    map_inbound(&value, &[], &transport()),
                    Err(SkipReason::UnsupportedEditContent)
                ),
                "unexpected result for {new_parts}"
            );
        }

        let mut absent = mutation_envelope("edit");
        absent["inst"]["message"]
            .as_object_mut()
            .unwrap()
            .remove("new_parts");
        assert!(matches!(
            map_inbound(&absent, &[], &transport()),
            Err(SkipReason::UnsupportedEditContent)
        ));
    }

    #[test]
    fn mutation_debug_output_is_redacted() {
        let edited = map_inbound(&mutation_envelope("edit"), &[], &transport()).unwrap();
        assert_eq!(
            format!("{edited:?}"),
            "TransportEvent::MessageEdited([REDACTED])"
        );
        let unsent = map_inbound(&mutation_envelope("unsend"), &[], &transport()).unwrap();
        assert_eq!(
            format!("{unsent:?}"),
            "TransportEvent::MessageUnsent([REDACTED])"
        );
    }

    #[test]
    fn normal_message_is_unaffected_by_the_react_arm() {
        let event = map_inbound(&payload("tel:+15550000001", "hello"), &[], &transport())
            .expect("normal payload maps");
        assert!(matches!(event, TransportEvent::Message { .. }));
    }

    #[test]
    fn group_conversations_are_recognised() {
        let mut value = payload("tel:+15550000001", "hi");
        value["inst"]["conversation"]["participants"] = json!([
            "tel:+15550000001",
            "tel:+15550000002",
            "mailto:me@example.invalid"
        ]);
        value["inst"]["conversation"]["cv_name"] = json!("Two people");
        value["inst"]["conversation"]["sender_guid"] = json!("iMessage;-;group");
        let TransportEvent::Message {
            conversation,
            message,
            ..
        } = map_inbound(
            &value,
            &["mailto:me@example.invalid".to_owned()],
            &transport(),
        )
        .unwrap()
        else {
            panic!("expected a message event");
        };
        assert_eq!(conversation.id.value(), "iMessage;-;group");
        assert_eq!(conversation.remote_id, "iMessage;-;group");
        assert_eq!(conversation.kind, ConversationKind::Group);
        assert_eq!(conversation.title.as_deref(), Some("Two people"));
        assert_eq!(conversation.participants.len(), 3);
        assert_eq!(
            conversation
                .participants
                .iter()
                .filter(|participant| participant.is_me)
                .count(),
            1
        );
        assert!(conversation
            .participants
            .iter()
            .any(|participant| participant.handle.value() == "+15550000001" && !participant.is_me));
        assert!(conversation
            .participants
            .iter()
            .any(|participant| participant.handle.value() == "+15550000002" && !participant.is_me));
        assert!(conversation
            .participants
            .iter()
            .any(
                |participant| participant.handle.value() == "me@example.invalid"
                    && participant.is_me
            ));
        assert_eq!(message.conversation_id(), &conversation.id);
        assert_eq!(message.parts(), &[MessagePart::Text("hi".to_owned())]);
    }

    #[test]
    fn maps_a_conversation_frame_including_our_own_participant() {
        let value = json!({
            "guid": "iMessage;-;group",
            "title": "Weekend crew",
            "participants": [
                "mailto:a@example.invalid",
                "mailto:me@example.invalid",
                "tel:+15550000001"
            ],
            "is_group": true
        });
        let event = map_conversation(
            Some(&value),
            &["mailto:me@example.invalid".to_owned()],
            &transport(),
        )
        .expect("conversation maps");
        let TransportEvent::Conversation { conversation } = event else {
            panic!("expected a conversation event");
        };
        assert_eq!(conversation.id.value(), "iMessage;-;group");
        assert_eq!(conversation.title.as_deref(), Some("Weekend crew"));
        assert_eq!(conversation.kind, ConversationKind::Group);
        assert_eq!(conversation.participants.len(), 3);
        assert_eq!(
            conversation
                .participants
                .iter()
                .filter(|participant| participant.is_me)
                .count(),
            1
        );
        assert!(conversation.last_message_at.is_none());
    }

    #[test]
    fn unusable_conversation_frames_are_skipped_not_fatal() {
        let no_guid = json!({"guid": "  ", "participants": []});
        assert!(map_conversation(Some(&no_guid), &[], &transport()).is_none());
        assert!(map_conversation(None, &[], &transport()).is_none());

        // A conversation with no usable participants still lists, so the user can
        // still select it.
        let empty = json!({"guid": "g", "title": null, "participants": [], "is_group": false});
        let event = map_conversation(Some(&empty), &[], &transport()).expect("maps");
        let TransportEvent::Conversation { conversation } = event else {
            panic!("expected a conversation event");
        };
        assert!(conversation.participants.is_empty());
        assert!(conversation.title.is_none());
        assert_eq!(conversation.kind, ConversationKind::Direct);
    }

    #[test]
    fn handles_map_to_typed_kinds() {
        assert!(matches!(
            handle_from("mailto:a@b.c").unwrap().kind(),
            HandleKind::Email
        ));
        assert!(matches!(
            handle_from("tel:+15550000001").unwrap().kind(),
            HandleKind::Tel
        ));
        assert!(matches!(
            handle_from("weird").unwrap().kind(),
            HandleKind::Opaque
        ));
    }

    #[test]
    fn session_states_map_and_unknown_states_disconnect() {
        assert_eq!(map_session_state("active"), SessionState::Active);
        assert_eq!(map_session_state("throttled"), SessionState::Throttled);
        assert_eq!(map_session_state("nonsense"), SessionState::Disconnected);
    }

    /// A reader that fails once, so the error path can be exercised without a
    /// real broken pipe.
    struct BrokenReader;

    impl tokio::io::AsyncRead for BrokenReader {
        fn poll_read(
            self: std::pin::Pin<&mut Self>,
            _cx: &mut std::task::Context<'_>,
            _buf: &mut tokio::io::ReadBuf<'_>,
        ) -> std::task::Poll<std::io::Result<()>> {
            std::task::Poll::Ready(Err(std::io::Error::other("synthetic read failure")))
        }
    }

    #[tokio::test]
    async fn frames_are_read_one_at_a_time_without_their_terminator() {
        let input = b"{\"a\":1}\n{\"b\":2}\r\n";
        let mut reader = tokio::io::BufReader::new(&input[..]);
        match read_frame(&mut reader).await {
            FrameRead::Frame(bytes) => assert_eq!(bytes, b"{\"a\":1}"),
            _ => panic!("expected the first frame"),
        }
        match read_frame(&mut reader).await {
            FrameRead::Frame(bytes) => assert_eq!(bytes, b"{\"b\":2}"),
            _ => panic!("expected the second frame"),
        }
        assert!(matches!(read_frame(&mut reader).await, FrameRead::Eof));
    }

    #[tokio::test]
    async fn a_final_frame_without_a_terminator_is_still_used() {
        let input = b"{\"last\":true}";
        let mut reader = tokio::io::BufReader::new(&input[..]);
        match read_frame(&mut reader).await {
            FrameRead::Frame(bytes) => assert_eq!(bytes, b"{\"last\":true}"),
            _ => panic!("a trailing frame without a newline must still be read"),
        }
    }

    /// An endless source that counts how many bytes were actually pulled, so the
    /// bound can be measured instead of inferred from the returned variant.
    struct CountingReader {
        remaining: usize,
        pulled: Arc<std::sync::atomic::AtomicUsize>,
    }

    impl tokio::io::AsyncRead for CountingReader {
        fn poll_read(
            self: std::pin::Pin<&mut Self>,
            _cx: &mut std::task::Context<'_>,
            buf: &mut tokio::io::ReadBuf<'_>,
        ) -> std::task::Poll<std::io::Result<()>> {
            let this = self.get_mut();
            let count = this.remaining.min(buf.remaining()).min(4096);
            let filler = vec![b'z'; count];
            buf.put_slice(&filler);
            this.remaining -= count;
            this.pulled
                .fetch_add(count, std::sync::atomic::Ordering::SeqCst);
            std::task::Poll::Ready(Ok(()))
        }
    }

    #[tokio::test]
    async fn an_oversized_frame_neither_returns_nor_drains_the_stream() {
        // The frame never ends, so a reader that accumulates first and checks the
        // limit afterwards would pull the entire source before rejecting it.
        let pulled = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let source = CountingReader {
            remaining: 4 * MAX_FRAME_BYTES,
            pulled: pulled.clone(),
        };
        let mut reader = tokio::io::BufReader::with_capacity(8192, source);
        assert!(matches!(read_frame(&mut reader).await, FrameRead::TooLarge));
        let pulled = pulled.load(std::sync::atomic::Ordering::SeqCst);
        assert!(
            pulled <= MAX_FRAME_BYTES + 64 * 1024,
            "a bounded read must stop near the limit, but it pulled {pulled} bytes"
        );
    }

    #[tokio::test]
    async fn a_frame_at_the_limit_is_accepted() {
        let mut input = vec![b'y'; MAX_FRAME_BYTES];
        input.push(b'\n');
        let mut reader = tokio::io::BufReader::new(&input[..]);
        match read_frame(&mut reader).await {
            FrameRead::Frame(bytes) => assert_eq!(bytes.len(), MAX_FRAME_BYTES),
            _ => panic!("a frame at the limit must be accepted"),
        }
    }

    #[tokio::test]
    async fn a_read_error_is_reported_rather_than_looking_like_a_clean_end() {
        let mut reader = tokio::io::BufReader::new(BrokenReader);
        assert!(matches!(
            read_frame(&mut reader).await,
            FrameRead::Failed(_)
        ));
    }

    #[test]
    fn unparsable_frame_summary_is_structural_only() {
        let summary = unparsable_frame_summary("not json with private text");
        assert!(summary.contains("len=26"), "{summary}");
        assert!(summary.contains("first=0x6e"), "{summary}");
        assert!(summary.contains("printable=100%"), "{summary}");
        assert!(summary.contains("nul=false"), "{summary}");
        assert!(summary.contains("brace=false"), "{summary}");
        assert!(summary.contains("digits=0"), "{summary}");
        assert!(summary.contains("all_digits=false"), "{summary}");
        for leaked in ["not json", "private", "text"] {
            assert!(!summary.contains(leaked), "leaked payload: {summary}");
        }

        let brace = unparsable_frame_summary("{\"type\": ");
        assert!(brace.contains("brace=true"), "{brace}");
        assert!(!brace.contains("type"), "{brace}");
    }

    #[tokio::test]
    async fn a_blank_line_is_an_empty_frame_not_an_end_of_stream() {
        let input = b"\n";
        let mut reader = tokio::io::BufReader::new(&input[..]);
        match read_frame(&mut reader).await {
            FrameRead::Frame(bytes) => assert!(bytes.is_empty()),
            _ => panic!("a blank line must be reported as an unparsable frame"),
        }
    }

    #[test]
    fn outgoing_bodies_must_carry_text() {
        assert_eq!(
            outgoing_text(&OutgoingBody { parts: vec![] }),
            Err(TransportError::Unsupported)
        );
        assert_eq!(
            outgoing_text(&OutgoingBody {
                parts: vec![MessagePart::Text("hi".to_owned())]
            }),
            Ok("hi".to_owned())
        );
        assert_eq!(
            outgoing_text(&OutgoingBody {
                parts: vec![
                    MessagePart::Text("hi".to_owned()),
                    MessagePart::Text(" there".to_owned())
                ]
            }),
            Ok("hi there".to_owned())
        );
    }

    #[test]
    fn a_body_with_an_unsupported_part_is_rejected_rather_than_truncated() {
        // Sending only the text of a mixed body would drop the attachment while
        // reporting success, so the whole body must be refused.
        let body = OutgoingBody {
            parts: vec![
                MessagePart::Text("look at this".to_owned()),
                MessagePart::Unsupported {
                    kind: "attachment".to_owned(),
                },
            ],
        };
        assert_eq!(outgoing_text(&body), Err(TransportError::Unsupported));
    }
}
