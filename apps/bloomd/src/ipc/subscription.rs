//! Bounded event fanout for dedicated subscription sockets.
use std::{
    sync::{Arc, Mutex},
    time::Duration,
};

use bloom_ipc::{Event, Response, ResponseKind};
use tokio::{io::AsyncWrite, sync::broadcast, time::timeout};

use super::listener::AuthenticatedStream;

pub const SUBSCRIPTION_WRITE_TIMEOUT: Duration = Duration::from_secs(30);
const DEFAULT_CAPACITY: usize = 128;

/// The latest session and readiness events, replayed to a late subscriber.
#[derive(Default)]
struct Replay {
    session: Option<Event>,
    readiness: Option<Event>,
}

impl Replay {
    fn events(&self) -> Vec<Event> {
        let mut events = Vec::with_capacity(2);
        if let Some(session) = &self.session {
            events.push(session.clone());
        }
        if let Some(readiness) = &self.readiness {
            events.push(readiness.clone());
        }
        events
    }
}

/// Bounded fanout for events that have already been persisted.
#[derive(Clone)]
pub struct SubscriptionHub {
    sender: broadcast::Sender<Event>,
    /// The most recent session and readiness state, replayed to clients that
    /// subscribe later. Without it a client that connects after startup never
    /// learns the engine's state or the current readiness revision.
    latest: Arc<Mutex<Replay>>,
    /// Serializes subscription capture with publication, so the replayed snapshot
    /// and the live receiver cannot lose, duplicate, or reorder an event.
    publish_lock: Arc<Mutex<()>>,
}

impl SubscriptionHub {
    pub fn new(capacity: usize) -> Self {
        assert!(capacity > 0, "subscription capacity must be nonzero");
        let (sender, _) = broadcast::channel(capacity);
        Self {
            sender,
            latest: Arc::new(Mutex::new(Replay::default())),
            publish_lock: Arc::new(Mutex::new(())),
        }
    }

    pub fn with_default_capacity() -> Self {
        Self::new(DEFAULT_CAPACITY)
    }

    /// Subscribe before acknowledging the request so subsequent events cannot fall in a gap.
    pub fn subscribe(&self) -> broadcast::Receiver<Event> {
        self.sender.subscribe()
    }

    /// Capture the receiver and replay together under the publication lock. The
    /// returned receiver sees only events published after this capture.
    pub fn subscribe_with_replay(&self) -> (broadcast::Receiver<Event>, Vec<Event>) {
        let _guard = self.publish_lock.lock().unwrap();
        let receiver = self.sender.subscribe();
        let replay = self.latest.lock().unwrap().events();
        (receiver, replay)
    }

    /// Publish a persisted event. No receivers is normal and is not a persistence error.
    pub fn publish(&self, event: Event) {
        let _guard = self.publish_lock.lock().unwrap();
        {
            let mut latest = self.latest.lock().unwrap();
            match event.event {
                bloom_ipc::EventKind::Session { .. } => latest.session = Some(event.clone()),
                bloom_ipc::EventKind::Readiness { .. } => latest.readiness = Some(event.clone()),
                _ => {}
            }
        }
        let _ = self.sender.send(event);
    }

    /// The last session and readiness events published, for replay on subscription.
    pub fn latest_replay(&self) -> Vec<Event> {
        let _guard = self.publish_lock.lock().unwrap();
        self.latest.lock().unwrap().events()
    }
}

async fn wait_for_client_close(stream: &mut tokio::net::UnixStream) -> std::io::Result<()> {
    let mut byte = [0u8; 1];
    use tokio::io::AsyncReadExt;
    match stream.read(&mut byte).await? {
        0 => Ok(()),
        _ => Err(std::io::Error::other("unexpected subscription client data")),
    }
}

/// Send one acknowledgement followed exclusively by event frames.
pub async fn serve_subscriber(
    mut connection: AuthenticatedStream,
    request_id: u64,
    mut receiver: broadcast::Receiver<Event>,
    initial: Vec<Event>,
) {
    let response = Response {
        request_id,
        response: ResponseKind::Subscribed,
    };
    if timeout(
        SUBSCRIPTION_WRITE_TIMEOUT,
        bloom_ipc::write_response(connection.stream_mut(), &response),
    )
    .await
    .map_or(true, |result| result.is_err())
    {
        return;
    }

    for event in initial {
        if write_event_bounded(connection.stream_mut(), &event, SUBSCRIPTION_WRITE_TIMEOUT)
            .await
            .is_err()
        {
            return;
        }
    }

    loop {
        let event = tokio::select! {
            result = receiver.recv() => match result {
                Ok(event) => event,
                Err(broadcast::error::RecvError::Lagged(_))
                | Err(broadcast::error::RecvError::Closed) => return,
            },
            result = wait_for_client_close(connection.stream_mut()) => {
                let _ = result;
                return;
            }
        };
        if write_event_bounded(connection.stream_mut(), &event, SUBSCRIPTION_WRITE_TIMEOUT)
            .await
            .is_err()
        {
            return;
        }
    }
}

async fn write_event_bounded<W: AsyncWrite + Unpin>(
    writer: &mut W,
    event: &Event,
    deadline: Duration,
) -> Result<(), ()> {
    timeout(deadline, bloom_ipc::write_event(writer, event))
        .await
        .map_err(|_| ())?
        .map_err(|_| ())
}

#[cfg(test)]
mod tests {
    use super::*;
    use bloom_ipc::{
        read_event, read_request, read_response, write_request, EventKind, Request, RequestKind,
    };
    use std::{fs, os::unix::fs::PermissionsExt};
    use tokio::{io::AsyncReadExt, net::UnixStream};

    async fn pair() -> (tempfile::TempDir, AuthenticatedStream, UnixStream) {
        let dir = tempfile::tempdir().unwrap();
        fs::set_permissions(dir.path(), fs::Permissions::from_mode(0o700)).unwrap();
        let path = dir.path().join("bloomd.sock");
        let listener = super::super::listener::Listener::bind(&path).unwrap();
        let (server, client) = tokio::join!(listener.accept(), UnixStream::connect(path));
        (dir, server.unwrap(), client.unwrap())
    }

    fn event() -> Event {
        Event {
            event: EventKind::Session {
                state: bloom_domain::SessionState::Disconnected,
            },
        }
    }

    fn readiness_event(revision: u64) -> Event {
        Event {
            event: EventKind::Readiness {
                snapshot: bloom_ipc::ReadinessSnapshot {
                    generation: 1,
                    operation: 1,
                    revision,
                    phase: bloom_ipc::ReadinessPhase::Ready,
                    outcome: Some(bloom_ipc::ReadinessOutcome::Complete),
                    observed_records: 0,
                    can_send: true,
                    engine_ready: true,
                    bounded_error: None,
                },
            },
        }
    }

    async fn subscribe_request(server: &mut AuthenticatedStream, client: &mut UnixStream) {
        write_request(
            client,
            &Request {
                request_id: 77,
                request: RequestKind::Subscribe,
            },
        )
        .await
        .unwrap();
        let request = read_request(server.stream_mut()).await.unwrap();
        assert_eq!(request.request_id, 77);
        assert!(matches!(request.request, RequestKind::Subscribe));
    }

    #[tokio::test]
    async fn acknowledgement_precedes_buffered_events_and_echoes_id() {
        let (_dir, mut server, mut client) = pair().await;
        subscribe_request(&mut server, &mut client).await;
        let hub = SubscriptionHub::new(4);
        let receiver = hub.subscribe();
        hub.publish(event());
        let task = tokio::spawn(serve_subscriber(server, 77, receiver, Vec::new()));
        let ack = read_response(&mut client).await.unwrap();
        assert_eq!(ack.request_id, 77);
        assert!(matches!(ack.response, ResponseKind::Subscribed));
        let received = read_event(&mut client).await.unwrap();
        assert!(received == event());
        drop(client);
        timeout(Duration::from_secs(1), task)
            .await
            .unwrap()
            .unwrap();
    }

    #[tokio::test]
    async fn idle_disconnect_releases_connection_without_an_event() {
        let (_dir, mut server, mut client) = pair().await;
        subscribe_request(&mut server, &mut client).await;
        let hub = SubscriptionHub::new(4);
        let task = tokio::spawn(serve_subscriber(server, 77, hub.subscribe(), Vec::new()));
        assert!(matches!(
            read_response(&mut client).await.unwrap().response,
            ResponseKind::Subscribed
        ));
        drop(client);
        timeout(Duration::from_secs(1), task)
            .await
            .unwrap()
            .unwrap();
    }

    #[tokio::test]
    async fn lagged_subscriber_is_disconnected_instead_of_missing_events() {
        let (_dir, mut server, mut client) = pair().await;
        subscribe_request(&mut server, &mut client).await;
        let hub = SubscriptionHub::new(1);
        let receiver = hub.subscribe();
        for _ in 0..3 {
            hub.publish(event());
        }
        let task = tokio::spawn(serve_subscriber(server, 77, receiver, Vec::new()));
        assert!(matches!(
            read_response(&mut client).await.unwrap().response,
            ResponseKind::Subscribed
        ));
        timeout(Duration::from_secs(1), task)
            .await
            .unwrap()
            .unwrap();
        let mut byte = [0];
        assert_eq!(client.read(&mut byte).await.unwrap(), 0);
    }

    #[tokio::test]
    async fn blocked_writer_has_deadline() {
        let (mut writer, _reader) = tokio::io::duplex(1);
        assert!(
            write_event_bounded(&mut writer, &event(), Duration::from_millis(20))
                .await
                .is_err()
        );
    }

    #[tokio::test]
    async fn a_late_subscriber_is_told_the_current_session_state() {
        let (_dir, mut server, mut client) = pair().await;
        subscribe_request(&mut server, &mut client).await;
        let hub = SubscriptionHub::new(4);

        // Published before anyone is listening, as happens at startup.
        hub.publish(Event {
            event: EventKind::Session {
                state: bloom_domain::SessionState::Active,
            },
        });
        assert!(!hub.latest_replay().is_empty());

        let (receiver, replay) = hub.subscribe_with_replay();
        let task = tokio::spawn(serve_subscriber(server, 77, receiver, replay));
        assert!(matches!(
            read_response(&mut client).await.unwrap().response,
            ResponseKind::Subscribed
        ));
        let replayed = read_event(&mut client).await.unwrap();
        assert!(matches!(
            replayed.event,
            EventKind::Session {
                state: bloom_domain::SessionState::Active
            }
        ));
        drop(client);
        timeout(Duration::from_secs(1), task)
            .await
            .unwrap()
            .unwrap();
    }

    #[tokio::test]
    async fn a_second_client_replays_session_and_readiness_in_order() {
        let (_dir, mut server, mut client) = pair().await;
        subscribe_request(&mut server, &mut client).await;
        let hub = SubscriptionHub::new(4);

        hub.publish(Event {
            event: EventKind::Session {
                state: bloom_domain::SessionState::Active,
            },
        });
        hub.publish(readiness_event(1));
        hub.publish(readiness_event(2));

        let (receiver, replay) = hub.subscribe_with_replay();
        assert_eq!(replay.len(), 2);
        let task = tokio::spawn(serve_subscriber(server, 77, receiver, replay));
        assert!(matches!(
            read_response(&mut client).await.unwrap().response,
            ResponseKind::Subscribed
        ));
        assert!(matches!(
            read_event(&mut client).await.unwrap().event,
            EventKind::Session {
                state: bloom_domain::SessionState::Active
            }
        ));
        match read_event(&mut client).await.unwrap().event {
            EventKind::Readiness { snapshot } => assert_eq!(snapshot.revision, 2),
            _ => panic!("expected readiness replay"),
        }
        drop(client);
        timeout(Duration::from_secs(1), task)
            .await
            .unwrap()
            .unwrap();
    }
}
