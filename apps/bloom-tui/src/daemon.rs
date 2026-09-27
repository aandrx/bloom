//! Daemon session handling: snapshotting, live events, and resynchronisation.
//!
//! The store is authoritative. A snapshot replaces cached state for the visible
//! conversation rather than merging, and after any subscription loss the client
//! re-snapshots before trusting new events. There is no durable event cursor, so
//! resync is always a fresh read.

use bloom_domain::{Conversation, ConversationId, Message};
use bloom_ipc::{client::BloomClient, client::ClientError, EventKind};
use tokio::sync::{mpsc, watch};

/// How much thread history a snapshot requests.
pub const HISTORY_LIMIT: u32 = 200;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConnectionState {
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
    mut visible: watch::Receiver<Option<ConversationId>>,
    events: &mpsc::UnboundedSender<EventKind>,
    snapshots: &mpsc::UnboundedSender<Snapshot>,
    status: &mpsc::UnboundedSender<ConnectionState>,
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

    loop {
        tokio::select! {
            event = stream.next() => match event {
                Ok(event) => {
                    if events.send(event.event).is_err() {
                        return Ok(());
                    }
                }
                Err(error) => return Err(error),
            },
            changed = visible.changed() => {
                if changed.is_err() {
                    return Ok(());
                }
                // Read and drop the borrow inside a block: holding a watch guard
                // across an await point would make this future non-Send.
                let requested = {
                    let guard = visible.borrow_and_update();
                    guard.clone()
                };
                if requested != current {
                    // Re-read from the store; a selection change is not an event.
                    let refreshed = snapshot(client, requested).await?;
                    current = refreshed.visible.clone();
                    let _ = snapshots.send(refreshed);
                }
            }
        }
    }
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
