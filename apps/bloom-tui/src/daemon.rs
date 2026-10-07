//! Daemon session handling: snapshotting, live events, and resynchronisation.
//!
//! The store is authoritative. A snapshot replaces cached state for the visible
//! conversation rather than merging, and after any subscription loss the client
//! re-snapshots before trusting new events. There is no durable event cursor, so
//! resync is always a fresh read.

use bloom_domain::{Conversation, ConversationId, Message};
use bloom_ipc::{client::BloomClient, client::ClientError, EventKind};
use std::sync::atomic::{AtomicU64, Ordering};
use tokio::sync::{mpsc, watch};

/// How much thread history a snapshot requests.
pub const HISTORY_LIMIT: u32 = 200;

static NEXT_REVISION: AtomicU64 = AtomicU64::new(1);

struct AbortOnDrop(tokio::task::JoinHandle<()>);

impl Drop for AbortOnDrop {
    fn drop(&mut self) {
        self.0.abort();
    }
}

/// Order reads and live events by when they began, not when their IPC finishes.
pub(crate) fn next_revision() -> u64 {
    NEXT_REVISION.fetch_add(1, Ordering::SeqCst)
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum ConnectionState {
    #[default]
    Connecting,
    Connected,
    Disconnected,
}

impl ConnectionState {
    pub fn label(self) -> &'static str {
        match self {
            Self::Connecting => "Connecting",
            Self::Connected => "Connected",
            Self::Disconnected => "Disconnected, retrying",
        }
    }
}

/// A consistent read of what the daemon currently holds.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Snapshot {
    /// Assigned before IPC starts, so an older read cannot replace newer state.
    pub revision: u64,
    /// The conversation the snapshot carries history for, if any.
    pub visible: Option<ConversationId>,
    pub conversations: Vec<Conversation>,
    pub history: Vec<Message>,
}

/// Read conversations and, when one is selected, that conversation's history.
///
/// If the requested conversation no longer exists the first conversation is chosen
/// instead, so the caller never shows an empty thread for a deleted conversation.
pub async fn snapshot(
    client: &BloomClient,
    requested: Option<ConversationId>,
) -> Result<Snapshot, ClientError> {
    let revision = next_revision();
    let conversations = client.list_conversations().await?;
    let visible = requested
        .filter(|id| {
            conversations
                .iter()
                .any(|conversation| &conversation.id == id)
        })
        .or_else(|| {
            conversations
                .first()
                .map(|conversation| conversation.id.clone())
        });
    let history = match &visible {
        Some(id) => client.history(id.clone(), HISTORY_LIMIT).await?,
        None => Vec::new(),
    };
    Ok(Snapshot {
        revision,
        visible,
        conversations,
        history,
    })
}

/// Read only the conversation list, for a refresh that must not disturb the thread
/// the user is reading.
pub async fn conversations(client: &BloomClient) -> Result<Vec<Conversation>, ClientError> {
    client.list_conversations().await
}

/// Run one connected session: snapshot, subscribe, forward events, and follow the
/// visible conversation when the user selects a different one.
///
/// Returns when the subscription ends or fails. The caller owns reconnect policy.
pub async fn pump_session(
    client: &BloomClient,
    visible: watch::Receiver<Option<ConversationId>>,
    events: &mpsc::UnboundedSender<EventKind>,
    snapshots: &mpsc::UnboundedSender<Snapshot>,
    status: &mpsc::UnboundedSender<ConnectionState>,
) -> Result<(), ClientError> {
    pump_session_with_interval(
        client,
        visible,
        events,
        snapshots,
        status,
        std::time::Duration::from_secs(5),
    )
    .await
}

pub async fn pump_session_with_interval(
    client: &BloomClient,
    mut visible: watch::Receiver<Option<ConversationId>>,
    events: &mpsc::UnboundedSender<EventKind>,
    snapshots: &mpsc::UnboundedSender<Snapshot>,
    status: &mpsc::UnboundedSender<ConnectionState>,
    interval: std::time::Duration,
) -> Result<(), ClientError> {
    let _ = status.send(ConnectionState::Connecting);
    // Read and drop the watch borrow before awaiting: a guard held across an await
    // point would make this future non-Send.
    let requested = {
        let guard = visible.borrow();
        guard.clone()
    };
    let initial = snapshot(client, requested).await?;
    let mut current = initial.visible.clone();
    let _ = snapshots.send(initial);

    let mut stream = client.subscribe().await?;
    let _ = status.send(ConnectionState::Connected);

    // History reconciliation must not block the live subscription reader while
    // IPC is waiting on the store. Run one bounded snapshot at a time separately.
    let polling_client = client.clone();
    let polling_visible = visible.clone();
    let polling_snapshots = snapshots.clone();
    let _poller = AbortOnDrop(tokio::spawn(async move {
        let mut ticker = tokio::time::interval(interval);
        ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        ticker.tick().await;
        loop {
            ticker.tick().await;
            let requested = { polling_visible.borrow().clone() };
            let Some(id) = requested else { continue };
            let Ok(refreshed) = snapshot(&polling_client, Some(id.clone())).await else {
                continue;
            };
            let still_visible = { polling_visible.borrow().as_ref() == Some(&id) };
            if still_visible
                && refreshed.visible.as_ref() == Some(&id)
                && polling_snapshots.send(refreshed).is_err()
            {
                break;
            }
        }
    }));

    let result = loop {
        tokio::select! {
            event = stream.next() => match event {
                Ok(event) => {
                    if events.send(event.event).is_err() {
                        break Ok(());
                    }
                }
                Err(error) => break Err(error),
            },
            changed = visible.changed() => {
                if changed.is_err() {
                    break Ok(());
                }
                // Read and drop the borrow inside a block: holding a watch guard
                // across an await point would make this future non-Send.
                let requested = {
                    let guard = visible.borrow_and_update();
                    guard.clone()
                };
                if requested != current {
                    // Re-read from the store; a selection change is not an event.
                    match snapshot(client, requested).await {
                        Ok(refreshed) => {
                            current = refreshed.visible.clone();
                            let _ = snapshots.send(refreshed);
                        }
                        Err(error) => break Err(error),
                    }
                }
            }
        }
    };
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn connection_labels_are_stable() {
        assert_eq!(ConnectionState::Connected.label(), "Connected");
        assert_eq!(
            ConnectionState::Disconnected.label(),
            "Disconnected, retrying"
        );
    }
}
