use anyhow::{anyhow, Context, Result};
use matrix_sdk::{
    authentication::matrix::MatrixSession,
    room::MessagesOptions,
    ruma::{events::room::message::RoomMessageEventContent, OwnedRoomId, OwnedUserId},
    Client, RoomState,
};
use rand::RngCore;
use serde::{Deserialize, Serialize};
use std::{path::Path, sync::Arc, time::Duration};
use tokio::sync::{broadcast, watch};

pub trait SecretStore: Send + Sync {
    fn get(&self, key: &str) -> Result<Option<Vec<u8>>>;
    fn set(&self, key: &str, value: &[u8]) -> Result<()>;
}
pub struct OsKeyring;
impl SecretStore for OsKeyring {
    fn get(&self, key: &str) -> Result<Option<Vec<u8>>> {
        match keyring::Entry::new("bloom", key)?.get_secret() {
            Ok(v) => Ok(Some(v)),
            Err(keyring::Error::NoEntry) => Ok(None),
            Err(e) => Err(anyhow!(e).context("OS keychain unavailable")),
        }
    }
    fn set(&self, key: &str, value: &[u8]) -> Result<()> {
        keyring::Entry::new("bloom", key)?
            .set_secret(value)
            .context("OS keychain unavailable")
    }
}
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub enum MembershipState {
    Invited,
    Joined,
    Left,
}
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub enum EncryptionStatus {
    Encrypted,
    NotEncrypted,
    Unknown,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RoomSummary {
    pub room_id: OwnedRoomId,
    pub display_name: String,
    pub membership: MembershipState,
    pub encryption: EncryptionStatus,
}
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub enum SendState {
    Sending,
    Sent,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Message {
    pub event_id: String,
    pub sender: String,
    pub body: Option<String>,
    pub timestamp_ms: u64,
    pub send_state: SendState,
}
#[derive(Debug, Clone)]
pub enum CoreEvent {
    Message {
        room_id: OwnedRoomId,
        message: Message,
    },
    RoomsChanged(Vec<RoomSummary>),
    Connection {
        connected: bool,
        error: Option<String>,
    },
}
struct SyncRunningGuard(Arc<std::sync::atomic::AtomicBool>);
impl Drop for SyncRunningGuard {
    fn drop(&mut self) {
        self.0.store(false, std::sync::atomic::Ordering::Release);
    }
}
pub struct SyncTask {
    cancel: watch::Sender<bool>,
    handle: Option<tokio::task::JoinHandle<()>>,
}
fn signal_sync_cancel(cancel: &watch::Sender<bool>) {
    cancel.send_replace(true);
}

impl SyncTask {
    pub fn cancel(&self) {
        signal_sync_cancel(&self.cancel);
    }

    pub async fn shutdown(mut self) -> Result<()> {
        self.cancel();
        if let Some(handle) = self.handle.take() {
            handle.await.context("sync task failed")?;
        }
        Ok(())
    }
}
impl Drop for SyncTask {
    fn drop(&mut self) {
        self.cancel();
    }
}
pub struct Bloom {
    client: Client,
    secrets: Arc<dyn SecretStore>,
    namespace: String,
    events: broadcast::Sender<CoreEvent>,
    sync_running: Arc<std::sync::atomic::AtomicBool>,
    sync_cancel: watch::Sender<bool>,
}
impl Drop for Bloom {
    fn drop(&mut self) {
        signal_sync_cancel(&self.sync_cancel);
    }
}
impl Bloom {
    pub async fn open(
        homeserver: &str,
        data_dir: &Path,
        account: &str,
        secrets: Arc<dyn SecretStore>,
    ) -> Result<Self> {
        std::fs::create_dir_all(data_dir)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(data_dir, std::fs::Permissions::from_mode(0o700))?;
        }
        let namespace = format!("{}:{}", homeserver.trim_end_matches('/'), account);
        let name = format!("{namespace}:store-key");
        let key = match secrets.get(&name)? {
            Some(k) => k,
            None => {
                let mut k = vec![0; 32];
                rand::thread_rng().fill_bytes(&mut k);
                secrets.set(&name, &k)?;
                k
            }
        };
        if key.len() != 32 {
            return Err(anyhow!("invalid encrypted-store key"));
        }
        let store = data_dir.join("matrix-store");
        std::fs::create_dir_all(&store)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&store, std::fs::Permissions::from_mode(0o700))?;
        }
        let config = matrix_sdk::SqliteStoreConfig::new(&store).key(Some(&key));
        let client = Client::builder()
            .homeserver_url(homeserver)
            .sqlite_store_with_config_and_cache_path(config, None::<&Path>)
            .build()
            .await?;
        let (events, _) = broadcast::channel(128);
        let (sync_cancel, _) = watch::channel(false);
        Ok(Self {
            client,
            secrets,
            namespace,
            events,
            sync_running: Arc::new(std::sync::atomic::AtomicBool::new(false)),
            sync_cancel,
        })
    }
    fn secret_key(&self, k: &str) -> String {
        format!("{}:{k}", self.namespace)
    }
    pub async fn login(&self, u: &str, p: &str) -> Result<()> {
        let r = self
            .client
            .matrix_auth()
            .login_username(u, p)
            .send()
            .await?;
        save_session(
            self.secrets.as_ref(),
            &self.namespace,
            &MatrixSession::from(&r),
        )?;
        Ok(())
    }
    pub async fn restore(&self) -> Result<bool> {
        let Some(b) = self.secrets.get(&self.secret_key("session"))? else {
            return Ok(false);
        };
        let s = decode_session(&b)?;
        self.client.restore_session(s).await?;
        Ok(true)
    }
    pub async fn rooms(&self) -> Vec<RoomSummary> {
        room_summaries(&self.client).await
    }
    pub async fn join(&self, id: &OwnedRoomId) -> Result<()> {
        let room = self
            .client
            .get_room(id)
            .ok_or_else(|| anyhow!("invited room not found locally"))?;
        if room.state() != RoomState::Invited {
            return Err(anyhow!("room is not locally known as invited"));
        }
        room.join().await?;
        Ok(())
    }
    pub async fn direct(&self, user: &str) -> Result<OwnedRoomId> {
        let mut q = matrix_sdk::ruma::api::client::room::create_room::v3::Request::new();
        q.invite = vec![OwnedUserId::try_from(user)?];
        q.preset =
            Some(matrix_sdk::ruma::api::client::room::create_room::v3::RoomPreset::PrivateChat);
        q.initial_state.push(matrix_sdk::ruma::serde::Raw::<
            matrix_sdk::ruma::events::AnyInitialStateEvent,
        >::from_json(serde_json::value::to_raw_value(
            &serde_json::json!({
                "type": "m.room.encryption",
                "state_key": "",
                "content": { "algorithm": "m.megolm.v1.aes-sha2" }
            }),
        )?));
        q.visibility = matrix_sdk::ruma::api::client::room::Visibility::Private;
        let r = self.client.create_room(q).await?;
        if !r.latest_encryption_state().await?.is_encrypted() {
            return Err(anyhow!("room encryption could not be confirmed"));
        }
        Ok(r.room_id().to_owned())
    }
    pub async fn send(&self, id: &OwnedRoomId, body: &str) -> Result<String> {
        let r = self
            .client
            .get_room(id)
            .ok_or_else(|| anyhow!("room not found"))?;
        if !matches!(encryption_status(&r).await, EncryptionStatus::Encrypted) {
            return Err(anyhow!(
                "refusing to send unless room encryption is confirmed"
            ));
        }
        Ok(r.send(RoomMessageEventContent::text_plain(body))
            .await?
            .response
            .event_id
            .to_string())
    }
    pub async fn history(&self, id: &OwnedRoomId, limit: u16) -> Result<Vec<Message>> {
        let r = self
            .client
            .get_room(id)
            .ok_or_else(|| anyhow!("room not found"))?;
        let mut o = MessagesOptions::backward();
        o.limit = limit.into();
        Ok(r.messages(o)
            .await?
            .chunk
            .into_iter()
            .filter_map(|e| message_from_timeline(e, SendState::Sent))
            .collect())
    }
    pub fn subscribe(&self, _n: usize) -> broadcast::Receiver<CoreEvent> {
        self.events.subscribe()
    }
    pub async fn start_sync(
        &self,
        _n: usize,
    ) -> Result<(broadcast::Receiver<CoreEvent>, SyncTask)> {
        if self
            .sync_running
            .swap(true, std::sync::atomic::Ordering::AcqRel)
        {
            return Err(anyhow!("sync task already running for this client"));
        }
        let rx = self.subscribe(128);
        let (cancel, mut stopped) = watch::channel(false);
        let lifetime = self.sync_cancel.clone();
        let running = SyncRunningGuard(self.sync_running.clone());
        let client = self.client.clone();
        let events = self.events.clone();
        let handle = tokio::spawn(async move {
            let mut backoff = Duration::from_secs(1);
            let mut lifetime_stopped = lifetime.subscribe();
            loop {
                if *stopped.borrow() || *lifetime_stopped.borrow() {
                    break;
                }
                let sync = client.sync_once(matrix_sdk::config::SyncSettings::default());
                let result = tokio::select! {
                    _ = lifetime_stopped.changed() => break,
                    result = sync => result,
                    changed = stopped.changed() => {
                        if changed.is_err() || *stopped.borrow() { break; }
                        continue;
                    }
                };
                match result {
                    Ok(resp) => {
                        backoff = Duration::from_secs(1);
                        for (id, r) in &resp.rooms.joined {
                            for raw in &r.timeline.events {
                                if let Some(message) =
                                    message_from_timeline(raw.clone(), SendState::Sent)
                                {
                                    let _ = events.send(CoreEvent::Message {
                                        room_id: id.to_owned(),
                                        message,
                                    });
                                }
                            }
                        }
                        let summaries = room_summaries(&client).await;
                        let _ = events.send(CoreEvent::RoomsChanged(summaries));
                        let _ = events.send(CoreEvent::Connection {
                            connected: true,
                            error: None,
                        });
                    }
                    Err(e) => {
                        let _ = events.send(CoreEvent::Connection {
                            connected: false,
                            error: Some(e.to_string()),
                        });
                        tokio::select! {
                            _ = tokio::time::sleep(backoff) => {},
                            _ = stopped.changed() => break,
                            _ = lifetime_stopped.changed() => break,
                        };
                        backoff = (backoff * 2).min(Duration::from_secs(30));
                    }
                }
            }
            drop(running);
        });
        Ok((
            rx,
            SyncTask {
                cancel,
                handle: Some(handle),
            },
        ))
    }
}
fn encryption_status_from_lookup(result: Result<bool>) -> EncryptionStatus {
    match result {
        Ok(true) => EncryptionStatus::Encrypted,
        Ok(false) => EncryptionStatus::NotEncrypted,
        Err(_) => EncryptionStatus::Unknown,
    }
}
async fn encryption_status(room: &matrix_sdk::Room) -> EncryptionStatus {
    encryption_status_from_lookup(
        room.latest_encryption_state()
            .await
            .map(|state| state.is_encrypted())
            .map_err(anyhow::Error::from),
    )
}
async fn room_summaries(client: &Client) -> Vec<RoomSummary> {
    let mut out = Vec::new();
    for room in client.rooms() {
        let membership = match room.state() {
            RoomState::Invited => MembershipState::Invited,
            RoomState::Joined => MembershipState::Joined,
            _ => MembershipState::Left,
        };
        let display_name = room
            .display_name()
            .await
            .ok()
            .map(|name| name.to_string())
            .unwrap_or_else(|| room.room_id().to_string());
        let encryption = encryption_status(&room).await;
        out.push(RoomSummary {
            room_id: room.room_id().to_owned(),
            display_name,
            membership,
            encryption,
        });
    }
    out
}
fn encode_session(session: &MatrixSession) -> Result<Vec<u8>> {
    Ok(serde_json::to_vec(session)?)
}
fn decode_session(bytes: &[u8]) -> Result<MatrixSession> {
    Ok(serde_json::from_slice(bytes)?)
}
fn save_session(secrets: &dyn SecretStore, namespace: &str, session: &MatrixSession) -> Result<()> {
    secrets.set(&format!("{namespace}:session"), &encode_session(session)?)
}
fn message_from_timeline(
    e: matrix_sdk::deserialized_responses::TimelineEvent,
    state: SendState,
) -> Option<Message> {
    use matrix_sdk::ruma::events::{AnySyncTimelineEvent, AnyTimelineEvent};
    let id = e.event_id()?.to_string();
    let sender = e.sender()?.to_string();
    let ts = e.timestamp.map(|t| u64::from(t.get())).unwrap_or_default();
    let body = match e.kind {
        matrix_sdk::deserialized_responses::TimelineEventKind::Decrypted(d) => {
            let parsed = d.event.deserialize().ok()?;
            let AnyTimelineEvent::MessageLike(
                matrix_sdk::ruma::events::AnyMessageLikeEvent::RoomMessage(m),
            ) = parsed
            else {
                return None;
            };
            m.as_original().map(|m| m.content.body().to_owned())
        }
        matrix_sdk::deserialized_responses::TimelineEventKind::PlainText { event } => {
            let parsed = event.deserialize().ok()?;
            let AnySyncTimelineEvent::MessageLike(
                matrix_sdk::ruma::events::AnySyncMessageLikeEvent::RoomMessage(m),
            ) = parsed
            else {
                return None;
            };
            m.as_original().map(|m| m.content.body().to_owned())
        }
        matrix_sdk::deserialized_responses::TimelineEventKind::UnableToDecrypt { .. } => None,
    };
    Some(Message {
        event_id: id,
        sender,
        body,
        timestamp_ms: ts,
        send_state: state,
    })
}
#[cfg(test)]
mod tests {
    use super::*;
    use std::{collections::HashMap, sync::Mutex};

    #[test]
    fn encryption_lookup_maps_errors_to_unknown() {
        assert_eq!(
            encryption_status_from_lookup(Ok(true)),
            EncryptionStatus::Encrypted
        );
        assert_eq!(
            encryption_status_from_lookup(Ok(false)),
            EncryptionStatus::NotEncrypted
        );
        assert_eq!(
            encryption_status_from_lookup(Err(anyhow!("unavailable"))),
            EncryptionStatus::Unknown
        );
    }

    #[test]
    fn sync_running_guard_resets_owner_flag_on_drop() {
        let running = Arc::new(std::sync::atomic::AtomicBool::new(true));
        drop(SyncRunningGuard(running.clone()));
        assert!(!running.load(std::sync::atomic::Ordering::Acquire));
    }
    #[derive(Default)]
    struct Fake {
        values: Mutex<HashMap<String, Vec<u8>>>,
        fail_get: bool,
        fail_set: bool,
    }
    impl SecretStore for Fake {
        fn get(&self, k: &str) -> Result<Option<Vec<u8>>> {
            if self.fail_get {
                anyhow::bail!("read failure")
            }
            Ok(self.values.lock().unwrap().get(k).cloned())
        }
        fn set(&self, k: &str, v: &[u8]) -> Result<()> {
            if self.fail_set {
                anyhow::bail!("write failure")
            }
            self.values.lock().unwrap().insert(k.into(), v.into());
            Ok(())
        }
    }
    #[tokio::test]
    async fn first_use_stores_32_byte_key() {
        let d = tempfile::tempdir().unwrap();
        let s = Arc::new(Fake::default());
        Bloom::open("http://localhost:8008", d.path(), "alice", s.clone())
            .await
            .unwrap();
        let len = s.values.lock().unwrap()["http://localhost:8008:alice:store-key"].len();
        assert_eq!(len, 32);
    }
    #[test]
    fn session_json_round_trips_without_password() {
        let session = MatrixSession {
            tokens: matrix_sdk::SessionTokens {
                access_token: "access-token".into(),
                refresh_token: None,
            },
            meta: matrix_sdk::SessionMeta {
                user_id: OwnedUserId::try_from("@alice:example.org").unwrap(),
                device_id: "DEVICE".into(),
            },
        };
        let encoded = encode_session(&session).unwrap();
        let json = String::from_utf8(encoded.clone()).unwrap();
        assert!(!json.contains("password"));
        assert!(!json.contains("secret-password"));
        let decoded = decode_session(&encoded).unwrap();
        assert_eq!(decoded.tokens.access_token, session.tokens.access_token);
        assert_eq!(decoded.meta.user_id, session.meta.user_id);
        assert_eq!(decoded.meta.device_id, session.meta.device_id);
    }
    #[tokio::test]
    async fn saved_session_uses_secret_store_and_restore_reaches_sdk() {
        let d = tempfile::tempdir().unwrap();
        let store = Arc::new(Fake::default());
        let bloom = Bloom::open("http://localhost:8008", d.path(), "alice", store.clone())
            .await
            .unwrap();
        let session = MatrixSession {
            tokens: matrix_sdk::SessionTokens {
                access_token: "access-token".into(),
                refresh_token: None,
            },
            meta: matrix_sdk::SessionMeta {
                user_id: OwnedUserId::try_from("@alice:example.org").unwrap(),
                device_id: "DEVICE".into(),
            },
        };
        save_session(store.as_ref(), "http://localhost:8008:alice", &session).unwrap();
        assert!(store
            .values
            .lock()
            .unwrap()
            .contains_key("http://localhost:8008:alice:session"));
        assert!(bloom.restore().await.unwrap());
        assert!(bloom.client.matrix_auth().logged_in());
    }
    #[tokio::test]
    async fn missing_session_is_false() {
        let d = tempfile::tempdir().unwrap();
        let b = Bloom::open(
            "http://localhost:8008",
            d.path(),
            "alice",
            Arc::new(Fake::default()),
        )
        .await
        .unwrap();
        assert!(!b.restore().await.unwrap())
    }
    #[tokio::test]
    async fn rejects_bad_keys() {
        for key in [b"not-json".to_vec(), vec![1; 31], vec![1; 33]] {
            let d = tempfile::tempdir().unwrap();
            let s = Arc::new(Fake::default());
            s.values
                .lock()
                .unwrap()
                .insert("http://localhost:8008:alice:store-key".into(), key);
            assert!(Bloom::open("http://localhost:8008", d.path(), "alice", s)
                .await
                .is_err())
        }
    }
    #[tokio::test]
    async fn keychain_errors_fail_closed() {
        let d = tempfile::tempdir().unwrap();
        assert!(Bloom::open(
            "http://localhost:8008",
            d.path(),
            "alice",
            Arc::new(Fake {
                fail_get: true,
                ..Default::default()
            })
        )
        .await
        .is_err());
        let d = tempfile::tempdir().unwrap();
        assert!(Bloom::open(
            "http://localhost:8008",
            d.path(),
            "alice",
            Arc::new(Fake {
                fail_set: true,
                ..Default::default()
            })
        )
        .await
        .is_err())
    }
    #[test]
    fn cancellation_is_sticky_without_receivers() {
        let (cancel, receiver) = watch::channel(false);
        drop(receiver);

        signal_sync_cancel(&cancel);

        let receiver = cancel.subscribe();
        assert!(*receiver.borrow());
    }

    #[test]
    fn timeline_event_converts() {
        let j = serde_json::json!({"type":"m.room.message","event_id":"$event:example.org","sender":"@alice:example.org","origin_server_ts":1234,"content":{"msgtype":"m.text","body":"hello"}});
        let raw=matrix_sdk::ruma::serde::Raw::<matrix_sdk::ruma::events::AnySyncTimelineEvent>::from_json(serde_json::value::to_raw_value(&j).unwrap());
        let e = matrix_sdk::deserialized_responses::TimelineEvent::from_plaintext(raw);
        let m = message_from_timeline(e, SendState::Sent).unwrap();
        assert_eq!(m.event_id, "$event:example.org");
        assert_eq!(m.body.as_deref(), Some("hello"));
        assert_eq!(m.timestamp_ms, 1234)
    }
}
