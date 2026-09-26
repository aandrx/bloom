use bloom_core::{CoreEvent, EncryptionStatus, MembershipState, Message, RoomSummary};
use std::collections::{HashMap, HashSet};

#[derive(Default)]
pub struct App {
    pub rooms: Vec<RoomSummary>,
    pub selected: Option<String>,
    pub messages: HashMap<String, Vec<Message>>,
    pub connection: String,
    pub status: String,
    pub error: Option<String>,
    pub joining: HashSet<String>,
    loading: HashSet<String>,
    loaded: HashSet<String>,
}

impl App {
    pub fn set_rooms(&mut self, rooms: Vec<RoomSummary>) -> Vec<String> {
        let selected = self.selected.clone();
        self.rooms = rooms;
        self.selected = selected
            .filter(|id| self.rooms.iter().any(|room| room.room_id.as_str() == id))
            .or_else(|| self.rooms.first().map(|room| room.room_id.to_string()));
        self.take_history_requests()
    }

    pub fn handle(&mut self, event: CoreEvent) -> Vec<String> {
        match event {
            CoreEvent::RoomsChanged(rooms) => return self.set_rooms(rooms),
            CoreEvent::Message { room_id, message } => {
                let id = room_id.to_string();
                let messages = self.messages.entry(id).or_default();
                if !messages.iter().any(|old| old.event_id == message.event_id) {
                    messages.push(message);
                    messages.sort_by_key(|message| message.timestamp_ms);
                }
            }
            CoreEvent::Connection { connected, error } => {
                self.connection = if connected {
                    "Connected"
                } else {
                    "Disconnected"
                }
                .into();
                self.error = error;
            }
        }
        Vec::new()
    }

    pub fn take_history_requests(&mut self) -> Vec<String> {
        let mut requests = Vec::new();
        for room in &self.rooms {
            let id = room.room_id.to_string();
            if room.membership == MembershipState::Joined
                && !self.loading.contains(&id)
                && !self.loaded.contains(&id)
            {
                self.loading.insert(id.clone());
                requests.push(id);
            }
        }
        requests
    }

    pub fn history_result(&mut self, id: String, result: anyhow::Result<Vec<Message>>) {
        self.loading.remove(&id);
        match result {
            Ok(messages) => {
                self.loaded.insert(id.clone());
                let current = self.messages.entry(id).or_default();
                for message in messages {
                    if !current.iter().any(|old| old.event_id == message.event_id) {
                        current.push(message);
                    }
                }
                current.sort_by_key(|message| message.timestamp_ms);
            }
            Err(error) => self.error = Some(error.to_string()),
        }
    }

    pub fn select_step(&mut self, delta: isize) {
        if self.rooms.is_empty() {
            self.selected = None;
            return;
        }
        let current = self
            .selected
            .as_ref()
            .and_then(|id| {
                self.rooms
                    .iter()
                    .position(|room| room.room_id.as_str() == id)
            })
            .unwrap_or(0);
        let next = (current as isize + delta).clamp(0, self.rooms.len() as isize - 1) as usize;
        self.selected = Some(self.rooms[next].room_id.to_string());
    }

    pub fn selected_room(&self) -> Option<&RoomSummary> {
        self.rooms
            .iter()
            .find(|room| Some(room.room_id.to_string()) == self.selected)
    }

    pub fn request_join(&mut self) -> Option<String> {
        let room = self.selected_room()?;
        if room.membership != MembershipState::Invited {
            return None;
        }
        let id = room.room_id.to_string();
        let name = room.display_name.clone();
        if self.joining.insert(id.clone()) {
            self.status = format!("Joining {name}");
            Some(id)
        } else {
            None
        }
    }

    pub fn join_result(&mut self, id: String, result: anyhow::Result<()>) {
        self.joining.remove(&id);
        match result {
            Ok(()) => self.status = "Joined room".into(),
            Err(error) => self.error = Some(error.to_string()),
        }
    }
}

pub fn encryption_label(status: &EncryptionStatus) -> &'static str {
    match status {
        EncryptionStatus::Encrypted => "Encrypted",
        EncryptionStatus::NotEncrypted => "Not encrypted",
        EncryptionStatus::Unknown => "Unknown",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use bloom_core::SendState;
    use matrix_sdk::ruma::room_id;

    fn room(id: &str, membership: MembershipState) -> RoomSummary {
        RoomSummary {
            room_id: id.parse().unwrap(),
            display_name: id.into(),
            membership,
            encryption: EncryptionStatus::Unknown,
        }
    }
    fn message(event_id: &str, timestamp_ms: u64) -> Message {
        Message {
            event_id: event_id.into(),
            sender: "@a:example.org".into(),
            body: Some(event_id.into()),
            timestamp_ms,
            send_state: SendState::Sent,
        }
    }

    #[test]
    fn joined_room_added_by_sync_requests_history_once_and_merges_events() {
        let mut app = App::default();
        let requests = app.handle(CoreEvent::RoomsChanged(vec![room(
            "!a:example.org",
            MembershipState::Joined,
        )]));
        assert_eq!(requests, vec!["!a:example.org"]);
        assert!(app.take_history_requests().is_empty());
        let id = room_id!("!a:example.org").to_owned();
        app.handle(CoreEvent::Message {
            room_id: id,
            message: message("$live", 2),
        });
        app.history_result(
            "!a:example.org".into(),
            Ok(vec![message("$old", 1), message("$live", 2)]),
        );
        let messages = &app.messages["!a:example.org"];
        assert_eq!(
            messages
                .iter()
                .map(|m| m.event_id.as_str())
                .collect::<Vec<_>>(),
            vec!["$old", "$live"]
        );
        assert!(app.take_history_requests().is_empty());
    }

    #[test]
    fn selection_is_preserved_by_room_id_on_summary_update() {
        let mut app = App::default();
        app.set_rooms(vec![
            room("!a:example.org", MembershipState::Invited),
            room("!b:example.org", MembershipState::Invited),
        ]);
        app.select_step(1);
        app.set_rooms(vec![
            room("!b:example.org", MembershipState::Invited),
            room("!a:example.org", MembershipState::Invited),
        ]);
        assert_eq!(app.selected.as_deref(), Some("!b:example.org"));
    }
}
