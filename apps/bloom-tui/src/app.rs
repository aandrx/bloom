use bloom_core::{CoreEvent, EncryptionStatus, MembershipState, Message, RoomSummary, SendState};
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
    pub composer_open: bool,
    pub draft: String,
    pub sending: bool,
    loading: HashSet<String>,
    loaded: HashSet<String>,
    submitted: Option<(String, String, u64)>,
    draft_generation: u64,
}

impl App {
    pub fn can_send(&self) -> bool {
        self.selected_room().is_some_and(|room| {
            room.membership == MembershipState::Joined
                && room.encryption == EncryptionStatus::Encrypted
        })
    }

    pub fn send_reason(&self) -> &'static str {
        match self.selected_room() {
            None => "No room selected",
            Some(room) if room.membership != MembershipState::Joined => "Join the room to send",
            Some(room) if room.encryption != EncryptionStatus::Encrypted => {
                "Sending requires confirmed encryption"
            }
            Some(_) => "Enter to send, Esc to cancel",
        }
    }

    pub fn open_composer(&mut self) {
        self.composer_open = self.can_send();
        self.error = None;
    }
    pub fn cancel_composer(&mut self) {
        self.composer_open = false;
        self.draft.clear();
        self.draft_generation = self.draft_generation.wrapping_add(1);
    }
    pub fn edit_char(&mut self, c: char) {
        self.draft.push(c);
        self.draft_generation = self.draft_generation.wrapping_add(1);
    }
    pub fn backspace(&mut self) {
        if self.draft.pop().is_some() {
            self.draft_generation = self.draft_generation.wrapping_add(1);
        }
    }
    pub fn begin_send(&mut self) -> Option<(String, String, u64)> {
        if self.sending || !self.can_send() || self.draft.trim().is_empty() {
            return None;
        }
        let room = self.selected.clone()?;
        let body = self.draft.clone();
        self.sending = true;
        let generation = self.draft_generation;
        self.submitted = Some((room.clone(), body.clone(), generation));
        Some((room, body, generation))
    }
    pub fn send_result(
        &mut self,
        room: String,
        body: String,
        generation: u64,
        result: anyhow::Result<String>,
    ) {
        if self.submitted.as_ref() != Some(&(room.clone(), body.clone(), generation)) {
            return;
        }
        self.sending = false;
        self.submitted = None;
        match result {
            Ok(event_id) => {
                if self.selected.as_deref() == Some(&room) && self.draft_generation == generation {
                    self.draft.clear();
                    self.composer_open = false;
                }
                let message = Message {
                    event_id,
                    sender: "You".into(),
                    body: Some(body),
                    timestamp_ms: now_ms(),
                    send_state: SendState::Sent,
                };
                self.insert_message(room, message);
                self.error = None;
            }
            Err(error) => self.error = Some(error.to_string()),
        }
    }
    fn insert_message(&mut self, room: String, message: Message) {
        let list = self.messages.entry(room).or_default();
        if !list.iter().any(|old| old.event_id == message.event_id) {
            list.push(message);
        }
        list.sort_by_key(|message| message.timestamp_ms);
    }
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
                self.insert_message(room_id.to_string(), message)
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
                for message in messages {
                    self.insert_message(id.clone(), message);
                }
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
fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
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
    use matrix_sdk::ruma::room_id;
    fn room(id: &str, membership: MembershipState, encryption: EncryptionStatus) -> RoomSummary {
        RoomSummary {
            room_id: id.parse().unwrap(),
            display_name: id.into(),
            membership,
            encryption,
        }
    }
    #[test]
    fn send_requires_joined_confirmed_encryption() {
        let mut no_room = App {
            draft: "attempt".into(),
            ..Default::default()
        };
        assert!(!no_room.can_send());
        assert!(no_room.begin_send().is_none());

        for (membership, encryption, expected) in [
            (MembershipState::Joined, EncryptionStatus::Encrypted, true),
            (MembershipState::Joined, EncryptionStatus::Unknown, false),
            (
                MembershipState::Joined,
                EncryptionStatus::NotEncrypted,
                false,
            ),
            (MembershipState::Invited, EncryptionStatus::Encrypted, false),
            (MembershipState::Left, EncryptionStatus::Encrypted, false),
        ] {
            let mut app = App {
                rooms: vec![room("!a:example.org", membership, encryption)],
                selected: Some("!a:example.org".into()),
                draft: "attempt".into(),
                ..Default::default()
            };
            assert_eq!(app.can_send(), expected);
            if expected {
                assert!(app.begin_send().is_some());
            } else {
                assert!(app.begin_send().is_none());
            }
        }
    }
    #[test]
    fn composer_edits_unicode_and_retains_failure_then_clears_success() {
        let mut app = App {
            rooms: vec![room(
                "!a:example.org",
                MembershipState::Joined,
                EncryptionStatus::Encrypted,
            )],
            selected: Some("!a:example.org".into()),
            ..Default::default()
        };
        app.open_composer();
        app.edit_char('🦀');
        app.edit_char('x');
        app.backspace();
        assert_eq!(app.draft, "🦀");
        let (id, body, generation) = app.begin_send().unwrap();
        app.send_result(
            id.clone(),
            body.clone(),
            generation,
            Err(anyhow::anyhow!("failed")),
        );
        assert_eq!(app.draft, "🦀");
        let (id, body, generation) = app.begin_send().unwrap();
        app.send_result(id, body, generation, Ok("$event".into()));
        assert!(app.draft.is_empty());
        assert_eq!(app.messages["!a:example.org"].len(), 1);
    }
    #[test]
    fn edited_identical_draft_survives_old_success() {
        let mut app = App {
            rooms: vec![room(
                "!a:example.org",
                MembershipState::Joined,
                EncryptionStatus::Encrypted,
            )],
            selected: Some("!a:example.org".into()),
            ..Default::default()
        };
        app.open_composer();
        for c in "same".chars() {
            app.edit_char(c);
        }
        let (id, body, generation) = app.begin_send().unwrap();
        app.backspace();
        app.edit_char('e');
        app.send_result(id, body, generation, Ok("$event".into()));
        assert_eq!(app.draft, "same");
        assert!(app.composer_open);
    }
    #[test]
    fn canceled_and_retyped_identical_draft_survives_old_success() {
        let mut app = App {
            rooms: vec![room(
                "!a:example.org",
                MembershipState::Joined,
                EncryptionStatus::Encrypted,
            )],
            selected: Some("!a:example.org".into()),
            ..Default::default()
        };
        app.open_composer();
        for c in "same".chars() {
            app.edit_char(c);
        }
        let (id, body, generation) = app.begin_send().unwrap();
        app.cancel_composer();
        app.open_composer();
        for c in body.chars() {
            app.edit_char(c);
        }
        app.send_result(id, body, generation, Ok("$event".into()));
        assert_eq!(app.draft, "same");
        assert!(app.composer_open);
    }
    #[test]
    fn unrelated_draft_survives_inflight_result() {
        let mut app = App {
            rooms: vec![room(
                "!a:example.org",
                MembershipState::Joined,
                EncryptionStatus::Encrypted,
            )],
            selected: Some("!a:example.org".into()),
            draft: "one".into(),
            ..Default::default()
        };
        let (id, body, generation) = app.begin_send().unwrap();
        app.cancel_composer();
        app.open_composer();
        app.draft = "two".into();
        app.send_result(id, body, generation, Ok("$event".into()));
        assert_eq!(app.draft, "two");
    }
    #[test]
    fn room_id_type_remains_supported() {
        let _ = room_id!("!a:example.org");
    }
}
