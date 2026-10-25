//! Pure durable-draft orchestration. This module performs no IO; callers take one
//! operation at a time, perform it, and feed the correlated result back in.

use bloom_ipc::client::ClientError;
use bloom_ipc::{DraftKey, DraftLoad, DraftWriteOutcome, DurableDraft};
use std::collections::{HashMap, HashSet};
use std::hash::{Hash, Hasher};
use std::time::{Duration, Instant};

/// How long local edits settle before a save or delete may be taken.
pub const DEBOUNCE: Duration = Duration::from_millis(300);

/// Bounded backoff before an identical delete intent is retried after failure.
pub const DELETE_RETRY: Duration = Duration::from_secs(2);

/// How many stale reload-and-retry cycles a single key may consume before its
/// write is suppressed until the desired content changes.
pub const STALE_RETRY_LIMIT: u32 = 3;

/// Generic, body-free status text for any failed draft operation.
pub const SYNC_ERROR: &str = "Draft sync failed; changes are kept locally.";

/// Exact draft keys are serialized without `Hash`/`Ord`, so this small ordered
/// map keeps the public coordinator API keyed by `DraftKey`.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct DraftMap {
    entries: Vec<(DraftKey, DurableDraft)>,
}

impl DraftMap {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn insert(&mut self, key: DraftKey, draft: DurableDraft) -> Option<DurableDraft> {
        if let Some(entry) = self
            .entries
            .iter_mut()
            .find(|(existing, _)| existing == &key)
        {
            return Some(std::mem::replace(&mut entry.1, draft));
        }
        self.entries.push((key, draft));
        None
    }

    pub fn remove(&mut self, key: &DraftKey) -> Option<DurableDraft> {
        let index = self
            .entries
            .iter()
            .position(|(existing, _)| existing == key)?;
        Some(self.entries.remove(index).1)
    }

    pub fn get(&self, key: &DraftKey) -> Option<&DurableDraft> {
        self.entries
            .iter()
            .find(|(existing, _)| existing == key)
            .map(|(_, draft)| draft)
    }

    pub fn contains_key(&self, key: &DraftKey) -> bool {
        self.get(key).is_some()
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    pub fn len(&self) -> usize {
        self.entries.len()
    }

    pub fn iter(&self) -> impl Iterator<Item = (&DraftKey, &DurableDraft)> {
        self.entries.iter().map(|(key, draft)| (key, draft))
    }
}

impl FromIterator<(DraftKey, DurableDraft)> for DraftMap {
    fn from_iter<T: IntoIterator<Item = (DraftKey, DurableDraft)>>(iter: T) -> Self {
        let mut map = Self::new();
        for (key, draft) in iter {
            map.insert(key, draft);
        }
        map
    }
}

impl IntoIterator for DraftMap {
    type Item = (DraftKey, DurableDraft);
    type IntoIter = std::vec::IntoIter<(DraftKey, DurableDraft)>;

    fn into_iter(self) -> Self::IntoIter {
        self.entries.into_iter()
    }
}

#[derive(Clone, PartialEq, Eq)]
struct KeyId(DraftKey);

impl From<&DraftKey> for KeyId {
    fn from(key: &DraftKey) -> Self {
        Self(key.clone())
    }
}

impl std::hash::Hash for KeyId {
    fn hash<H: Hasher>(&self, state: &mut H) {
        match &self.0 {
            DraftKey::Conversation { owner } => {
                0_u8.hash(state);
                owner.transport().as_str().hash(state);
                owner.value().hash(state);
            }
            DraftKey::NewChat { transport, address } => {
                1_u8.hash(state);
                transport.as_str().hash(state);
                address.hash(state);
            }
        }
    }
}

impl std::fmt::Debug for KeyId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("DraftKey([REDACTED])")
    }
}

#[derive(Clone)]
pub enum DraftOperation {
    Load {
        request: u64,
        key: DraftKey,
    },
    Save {
        request: u64,
        key: DraftKey,
        revision: u64,
        draft: DurableDraft,
    },
    Delete {
        request: u64,
        key: DraftKey,
        revision: u64,
    },
}

impl std::fmt::Debug for DraftOperation {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Load { request, key } => f
                .debug_struct("DraftOperation::Load")
                .field("request", request)
                .field("key", key)
                .finish(),
            Self::Save {
                request,
                key,
                revision,
                draft: _,
            } => f
                .debug_struct("DraftOperation::Save")
                .field("request", request)
                .field("key", key)
                .field("revision", revision)
                .field("draft", &"[REDACTED]")
                .finish(),
            Self::Delete {
                request,
                key,
                revision,
            } => f
                .debug_struct("DraftOperation::Delete")
                .field("request", request)
                .field("key", key)
                .field("revision", revision)
                .finish(),
        }
    }
}

impl DraftOperation {
    pub fn request(&self) -> u64 {
        match self {
            Self::Load { request, .. }
            | Self::Save { request, .. }
            | Self::Delete { request, .. } => *request,
        }
    }

    pub fn key(&self) -> &DraftKey {
        match self {
            Self::Load { key, .. } | Self::Save { key, .. } | Self::Delete { key, .. } => key,
        }
    }

    fn is_save(&self) -> bool {
        matches!(self, Self::Save { .. })
    }

    #[cfg(test)]
    fn revision(&self) -> u64 {
        match self {
            Self::Load { .. } => 0,
            Self::Save { revision, .. } | Self::Delete { revision, .. } => *revision,
        }
    }
}

#[derive(Debug)]
pub enum DraftResult {
    Load {
        request: u64,
        key: DraftKey,
        result: Result<DraftLoad, ClientError>,
    },
    Save {
        request: u64,
        key: DraftKey,
        revision: u64,
        result: Result<DraftWriteOutcome, ClientError>,
    },
    Delete {
        request: u64,
        key: DraftKey,
        revision: u64,
        result: Result<DraftWriteOutcome, ClientError>,
    },
}

impl DraftResult {
    fn request(&self) -> u64 {
        match self {
            Self::Load { request, .. }
            | Self::Save { request, .. }
            | Self::Delete { request, .. } => *request,
        }
    }

    fn key(&self) -> &DraftKey {
        match self {
            Self::Load { key, .. } | Self::Save { key, .. } | Self::Delete { key, .. } => key,
        }
    }

    fn is_save(&self) -> bool {
        matches!(self, Self::Save { .. })
    }
}

/// A stored draft that the app may restore into an empty active composer.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RestoreCandidate {
    pub key: DraftKey,
    pub draft: DurableDraft,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum LoadCause {
    Activate,
    Stale,
}

#[derive(Clone, Copy, Debug)]
struct PendingLoad {
    deadline: Instant,
    cause: LoadCause,
}

#[derive(Clone, Copy, Debug)]
struct PendingWrite {
    deadline: Instant,
}

#[derive(Debug)]
struct InFlight {
    operation: DraftOperation,
    generation: u64,
    load_cause: Option<LoadCause>,
}

#[derive(Debug, Default)]
pub struct DraftCoordinator {
    desired: HashMap<KeyId, DurableDraft>,
    /// Last content confirmed present in the store for a key; `None` means the
    /// store is known to have no draft for that key.
    baselines: HashMap<KeyId, Option<DurableDraft>>,
    loaded: HashSet<KeyId>,
    floors: HashMap<KeyId, u64>,
    generations: HashMap<KeyId, u64>,
    pending_loads: HashMap<KeyId, PendingLoad>,
    pending_writes: HashMap<KeyId, PendingWrite>,
    dirty: HashSet<KeyId>,
    failed: HashMap<KeyId, u64>,
    exhausted: HashSet<KeyId>,
    stale_retries: HashMap<KeyId, u32>,
    stale_suppressed: HashSet<KeyId>,
    active: Option<KeyId>,
    in_flight: Option<InFlight>,
    next_request: u64,
    last_now: Option<Instant>,
    error: Option<String>,
}

impl DraftCoordinator {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn error(&self) -> Option<&str> {
        self.error.as_deref()
    }

    pub fn take_error(&mut self) -> Option<String> {
        self.error.take()
    }

    pub fn is_revision_exhausted(&self, key: &DraftKey) -> bool {
        self.exhausted.contains(&KeyId::from(key))
    }

    /// Whether repeated stale writes suspended this key until its content changes.
    pub fn is_stale_suppressed(&self, key: &DraftKey) -> bool {
        self.stale_suppressed.contains(&KeyId::from(key))
    }

    pub fn known_floor(&self, key: &DraftKey) -> Option<u64> {
        self.floors.get(&KeyId::from(key)).copied()
    }

    pub fn is_loaded(&self, key: &DraftKey) -> bool {
        self.loaded.contains(&KeyId::from(key))
    }

    /// Update the active owner and the full desired map. A newly active owner gets
    /// one immediate load; changed local content is saved or deleted after the
    /// debounce. Local desired state always outranks a load response.
    pub fn observe(
        &mut self,
        current_owner: Option<DraftKey>,
        desired_map: DraftMap,
        now: Instant,
    ) {
        self.last_now = Some(now);
        // Invalid local content is never persisted. A key whose new content is
        // invalid keeps its previous desired value so it is not mistaken for a
        // delete intent.
        let mut invalid = HashSet::new();
        let mut filtered: HashMap<KeyId, DurableDraft> = HashMap::new();
        for (key, draft) in desired_map {
            let key = KeyId(key);
            if draft.is_valid() {
                filtered.insert(key, draft);
            } else {
                invalid.insert(key);
            }
        }
        for key in &invalid {
            if let Some(old) = self.desired.get(key) {
                filtered.insert(key.clone(), old.clone());
            }
        }
        let desired_map = filtered;
        let current_owner = current_owner.map(KeyId);
        if current_owner != self.active {
            self.active = current_owner.clone();
            self.pending_loads.retain(|key, pending| {
                pending.cause == LoadCause::Stale || Some(key) == self.active.as_ref()
            });
            if let Some(key) = &self.active {
                self.pending_loads.insert(
                    key.clone(),
                    PendingLoad {
                        deadline: now,
                        cause: LoadCause::Activate,
                    },
                );
            }
        }

        let mut keys: HashSet<KeyId> = self.desired.keys().cloned().collect();
        keys.extend(desired_map.keys().cloned());
        keys.extend(self.baselines.keys().cloned());
        for key in keys {
            let old = self.desired.get(&key);
            let new = desired_map.get(&key);
            // Clearing a draft that exists remotely is a delete intent even when
            // the local desired map was already empty (for example after load).
            let clears_baseline =
                new.is_none() && self.baselines.get(&key).is_some_and(|base| base.is_some());
            let already_scheduled =
                self.dirty.contains(&key) || self.pending_writes.contains_key(&key);
            if (old == new && (!clears_baseline || already_scheduled)) || invalid.contains(&key) {
                continue;
            }
            if self.matches_baseline(&key, new) {
                self.dirty.remove(&key);
                self.pending_writes.remove(&key);
                continue;
            }
            let generation = self.generations.entry(key.clone()).or_insert(0);
            *generation = generation.saturating_add(1);
            self.failed.remove(&key);
            self.stale_retries.remove(&key);
            self.stale_suppressed.remove(&key);
            self.dirty.insert(key.clone());
            if !self.exhausted.contains(&key) {
                self.pending_writes.insert(
                    key,
                    PendingWrite {
                        deadline: now + DEBOUNCE,
                    },
                );
            }
        }
        self.desired = desired_map;
    }

    /// Take at most one operation. Loads run first, saving before deleting so an
    /// alias can write its new key before the old key disappears.
    pub fn take_ready(&mut self, now: Instant) -> Option<DraftOperation> {
        self.last_now = Some(now);
        if self.in_flight.is_some() {
            return None;
        }

        let load_key = self
            .active
            .as_ref()
            .filter(|key| {
                self.pending_loads
                    .get(*key)
                    .is_some_and(|pending| pending.deadline <= now)
            })
            .cloned()
            .or_else(|| {
                self.pending_loads
                    .iter()
                    .find(|(_, pending)| pending.deadline <= now)
                    .map(|(key, _)| key.clone())
            });
        if let Some(key) = load_key {
            let pending = self
                .pending_loads
                .remove(&key)
                .expect("selected pending load");
            let request = self.next_request();
            let operation = DraftOperation::Load {
                request,
                key: key.0.clone(),
            };
            self.in_flight = Some(InFlight {
                operation: operation.clone(),
                generation: self.generation(&key),
                load_cause: Some(pending.cause),
            });
            return Some(operation);
        }

        loop {
            let mut save_key = None;
            let mut delete_key = None;
            for (key, pending) in &self.pending_writes {
                if pending.deadline > now
                    || self.exhausted.contains(key)
                    || self.stale_suppressed.contains(key)
                {
                    continue;
                }
                if self.desired.contains_key(key) {
                    if save_key.is_none() {
                        save_key = Some(key.clone());
                    }
                } else if delete_key.is_none() {
                    delete_key = Some(key.clone());
                }
            }
            let key = save_key.or(delete_key)?;
            let is_save = self.desired.contains_key(&key);
            let floor = self.floors.get(&key).copied().unwrap_or(0);
            if floor >= i64::MAX as u64 {
                self.pending_writes.remove(&key);
                self.exhausted.insert(key);
                self.error = Some(SYNC_ERROR.into());
                continue;
            }
            let revision = floor + 1;
            self.pending_writes.remove(&key);
            let request = self.next_request();
            let operation = if is_save {
                DraftOperation::Save {
                    request,
                    key: key.0.clone(),
                    revision,
                    draft: self.desired.get(&key).expect("save has desired").clone(),
                }
            } else {
                DraftOperation::Delete {
                    request,
                    key: key.0.clone(),
                    revision,
                }
            };
            self.in_flight = Some(InFlight {
                operation: operation.clone(),
                generation: self.generation(&key),
                load_cause: None,
            });
            return Some(operation);
        }
    }

    /// Correlate and apply one operation result. Obsolete requests, late owner
    /// loads, and results from an older content generation are ignored.
    pub fn apply_result(&mut self, result: DraftResult) -> Option<RestoreCandidate> {
        let matches = self.in_flight.as_ref().is_some_and(|in_flight| {
            in_flight.operation.request() == result.request()
                && in_flight.operation.key() == result.key()
                && in_flight.operation.is_save() == result.is_save()
        });
        if !matches {
            return None;
        }
        let in_flight = self.in_flight.take().expect("matched in-flight operation");
        let key = KeyId(result.key().clone());
        let generation = self.generation(&key);
        let current_generation = in_flight.generation == generation;

        match result {
            DraftResult::Load { result, .. } => {
                if in_flight.load_cause == Some(LoadCause::Activate)
                    && self.active.as_ref() != Some(&key)
                {
                    return None;
                }
                match result {
                    Ok(load) => {
                        let floor = self.floors.entry(key.clone()).or_insert(0);
                        *floor = (*floor).max(load.revision_floor);
                        self.loaded.insert(key.clone());
                        self.failed.remove(&key);
                        self.error = None;
                        // Only an acceptable draft may become the baseline or be
                        // restored; oversized content is treated as absent.
                        let loaded = load.draft.filter(DurableDraft::is_valid);
                        self.baselines.insert(key.clone(), loaded.clone());
                        let local = self.desired.contains_key(&key);
                        // A cleared draft is a dirty delete intent even though it is
                        // absent locally; a stale reload must not hand it back.
                        let cleared = self.dirty.contains(&key) && !local;
                        let restore = if !local
                            && !cleared
                            && current_generation
                            && self.active.as_ref() == Some(&key)
                        {
                            loaded.map(|draft| RestoreCandidate {
                                key: key.0.clone(),
                                draft,
                            })
                        } else {
                            None
                        };
                        if restore.is_none() && self.dirty.contains(&key) {
                            self.schedule_after_load(&key, in_flight.load_cause);
                        }
                        restore
                    }
                    Err(_) => {
                        self.error = Some(SYNC_ERROR.into());
                        self.pending_writes.remove(&key);
                        if self.dirty.contains(&key) {
                            let fingerprint = self.fingerprint(self.desired.get(&key));
                            self.failed.insert(key, fingerprint);
                        }
                        None
                    }
                }
            }
            DraftResult::Save {
                revision, result, ..
            }
            | DraftResult::Delete {
                revision, result, ..
            } => {
                if !current_generation {
                    return None;
                }
                let sent = match &in_flight.operation {
                    DraftOperation::Save { draft, .. } => Some(draft.clone()),
                    _ => None,
                };
                let is_delete = matches!(in_flight.operation, DraftOperation::Delete { .. });
                match result {
                    Ok(DraftWriteOutcome::Applied) => {
                        self.floors.insert(key.clone(), revision);
                        self.baselines.insert(key.clone(), sent);
                        self.dirty.remove(&key);
                        self.failed.remove(&key);
                        self.stale_retries.remove(&key);
                        self.stale_suppressed.remove(&key);
                        self.pending_writes.remove(&key);
                        self.error = None;
                    }
                    Ok(DraftWriteOutcome::Stale) => {
                        let attempts = self.stale_retries.entry(key.clone()).or_insert(0);
                        *attempts = attempts.saturating_add(1);
                        let now = self.last_now.unwrap_or_else(Instant::now);
                        if *attempts > STALE_RETRY_LIMIT {
                            self.stale_suppressed.insert(key.clone());
                            self.pending_writes.remove(&key);
                            self.error = Some(SYNC_ERROR.into());
                        } else {
                            self.pending_loads.insert(
                                key,
                                PendingLoad {
                                    deadline: now,
                                    cause: LoadCause::Stale,
                                },
                            );
                        }
                    }
                    Err(_) => {
                        self.pending_writes.remove(&key);
                        self.error = Some(SYNC_ERROR.into());
                        if is_delete {
                            // A transient failure must not lose a clear. Keep the
                            // absent intent and retry the identical delete after
                            // a bounded backoff; one operation is in flight.
                            let now = self.last_now.unwrap_or_else(Instant::now);
                            self.pending_writes.insert(
                                key,
                                PendingWrite {
                                    deadline: now + DELETE_RETRY,
                                },
                            );
                        } else {
                            self.failed
                                .insert(key.clone(), self.fingerprint(self.desired.get(&key)));
                        }
                    }
                }
                None
            }
        }
    }

    fn schedule_after_load(&mut self, key: &KeyId, cause: Option<LoadCause>) {
        if !self.dirty.contains(key)
            || self.exhausted.contains(key)
            || self.stale_suppressed.contains(key)
            || self.pending_writes.contains_key(key)
            || self.failed.get(key) == Some(&self.fingerprint(self.desired.get(key)))
            || self.matches_baseline(key, self.desired.get(key))
        {
            return;
        }
        let now = self.last_now.unwrap_or_else(Instant::now);
        let deadline = match cause {
            Some(LoadCause::Stale) => now,
            _ => now + DEBOUNCE,
        };
        self.pending_writes
            .insert(key.clone(), PendingWrite { deadline });
    }

    fn next_request(&mut self) -> u64 {
        self.next_request = self.next_request.wrapping_add(1);
        self.next_request
    }

    fn generation(&self, key: &KeyId) -> u64 {
        self.generations.get(key).copied().unwrap_or(0)
    }

    /// Whether the candidate content already matches the last content confirmed
    /// in the store, so a restore must not be written back.
    fn matches_baseline(&self, key: &KeyId, candidate: Option<&DurableDraft>) -> bool {
        self.baselines
            .get(key)
            .is_some_and(|baseline| baseline.as_ref() == candidate)
    }

    fn fingerprint(&self, desired: Option<&DurableDraft>) -> u64 {
        let mut hasher = std::collections::hash_map::DefaultHasher::new();
        match desired {
            Some(draft) => {
                1_u8.hash(&mut hasher);
                draft.body.hash(&mut hasher);
                draft.cursor.hash(&mut hasher);
                draft.pasted_body.hash(&mut hasher);
                draft.uncertain.hash(&mut hasher);
            }
            None => 0_u8.hash(&mut hasher),
        }
        hasher.finish()
    }
}

#[cfg(test)]
impl DraftCoordinator {
    fn is_dirty_for_test(&self, key: &DraftKey) -> bool {
        self.dirty.contains(&KeyId::from(key))
    }

    fn matches_baseline_for_test(&self, key: &DraftKey, draft: Option<&DurableDraft>) -> bool {
        self.baselines.get(&KeyId::from(key)) == Some(&draft.cloned())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use bloom_domain::{ConversationId, TransportId};
    use unicode_segmentation::UnicodeSegmentation;

    fn conversation(value: &str) -> ConversationId {
        ConversationId::new(TransportId::new("openbubbles").unwrap(), value).unwrap()
    }

    fn key(value: &str) -> DraftKey {
        DraftKey::Conversation {
            owner: conversation(value),
        }
    }

    fn draft(body: &str) -> DurableDraft {
        DurableDraft {
            body: body.into(),
            cursor: body.graphemes(true).count(),
            pasted_body: false,
            uncertain: false,
        }
    }

    fn desired(entries: &[(&str, &str)]) -> DraftMap {
        entries
            .iter()
            .map(|(owner, body)| (key(owner), draft(body)))
            .collect()
    }

    #[test]
    fn debug_redacts_keys_and_draft_bodies() {
        let operation = DraftOperation::Save {
            request: 7,
            key: key("private@example.test"),
            revision: 3,
            draft: draft("secret body"),
        };
        let rendered = format!("{operation:?}");
        assert!(!rendered.contains("private@example.test"));
        assert!(!rendered.contains("secret body"));
        assert!(rendered.contains("[REDACTED]"));

        let result = DraftResult::Load {
            request: 7,
            key: key("private@example.test"),
            result: Ok(DraftLoad {
                draft: Some(draft("secret body")),
                revision_floor: 1,
                corrupt_deleted: false,
            }),
        };
        let rendered = format!("{result:?}");
        assert!(!rendered.contains("private@example.test"));
        assert!(!rendered.contains("secret body"));
    }

    #[test]
    fn debounce_coalesces_changes_to_the_latest_desired_content() {
        let start = Instant::now();
        let mut coordinator = DraftCoordinator::new();
        coordinator.observe(None, desired(&[("a", "first")]), start);
        assert!(coordinator
            .take_ready(start + Duration::from_millis(299))
            .is_none());
        coordinator.observe(
            None,
            desired(&[("a", "second")]),
            start + Duration::from_millis(100),
        );
        assert!(coordinator
            .take_ready(start + Duration::from_millis(399))
            .is_none());
        match coordinator.take_ready(start + Duration::from_millis(400)) {
            Some(DraftOperation::Save {
                draft, revision, ..
            }) => {
                assert_eq!(draft.body, "second");
                assert_eq!(revision, 1);
            }
            other => panic!("expected save, got {other:?}"),
        }
    }

    #[test]
    fn only_one_operation_is_in_flight_globally() {
        let start = Instant::now();
        let mut coordinator = DraftCoordinator::new();
        coordinator.observe(None, desired(&[("a", "a"), ("b", "b")]), start);
        assert!(coordinator.take_ready(start + DEBOUNCE).is_some());
        assert!(coordinator.take_ready(start + DEBOUNCE).is_none());
    }

    #[test]
    fn alias_saves_the_new_key_before_deleting_the_old_key() {
        let start = Instant::now();
        let mut coordinator = DraftCoordinator::new();
        coordinator.observe(None, desired(&[("old", "same")]), start);
        let save = coordinator
            .take_ready(start + DEBOUNCE)
            .expect("initial save");
        let (request, revision) = match save {
            DraftOperation::Save {
                request, revision, ..
            } => (request, revision),
            other => panic!("expected save, got {other:?}"),
        };
        coordinator.apply_result(DraftResult::Save {
            request,
            key: key("old"),
            revision,
            result: Ok(DraftWriteOutcome::Applied),
        });

        let changed_at = start + Duration::from_secs(1);
        coordinator.observe(None, desired(&[("new", "same")]), changed_at);
        let first = coordinator
            .take_ready(changed_at + DEBOUNCE)
            .expect("new save");
        match first {
            DraftOperation::Save { key: ref saved, .. } => {
                assert_eq!(saved, &key("new"));
            }
            other => panic!("expected new save first, got {other:?}"),
        }
        let save_request = first.request();
        coordinator.apply_result(DraftResult::Save {
            request: save_request,
            key: key("new"),
            revision: 1,
            result: Ok(DraftWriteOutcome::Applied),
        });
        match coordinator.take_ready(changed_at + DEBOUNCE) {
            Some(DraftOperation::Delete { key: deleted, .. }) => {
                assert_eq!(deleted, key("old"));
            }
            other => panic!("expected old delete second, got {other:?}"),
        }
    }

    #[test]
    fn stale_write_reloads_the_floor_then_saves_latest_desired_at_plus_one() {
        let start = Instant::now();
        let mut coordinator = DraftCoordinator::new();
        coordinator.observe(None, desired(&[("a", "local")]), start);
        let operation = coordinator.take_ready(start + DEBOUNCE).expect("save");
        let (request, revision) = match &operation {
            DraftOperation::Save {
                request, revision, ..
            } => (*request, *revision),
            _ => unreachable!(),
        };
        assert_eq!(revision, 1);
        coordinator.apply_result(DraftResult::Save {
            request,
            key: key("a"),
            revision,
            result: Ok(DraftWriteOutcome::Stale),
        });
        let load = coordinator.take_ready(start + DEBOUNCE).expect("reload");
        let load_request = load.request();
        assert!(matches!(load, DraftOperation::Load { .. }));
        let restore = coordinator.apply_result(DraftResult::Load {
            request: load_request,
            key: key("a"),
            result: Ok(DraftLoad {
                draft: Some(draft("remote")),
                revision_floor: 9,
                corrupt_deleted: false,
            }),
        });
        assert!(restore.is_none(), "local desired always wins over load");
        assert_eq!(coordinator.known_floor(&key("a")), Some(9));
        match coordinator
            .take_ready(start + DEBOUNCE)
            .expect("latest save")
        {
            DraftOperation::Save {
                revision, draft, ..
            } => {
                assert_eq!(revision, 10);
                assert_eq!(draft.body, "local");
            }
            other => panic!("expected save, got {other:?}"),
        }
    }

    #[test]
    fn failure_suppresses_identical_desired_until_content_changes() {
        let start = Instant::now();
        let mut coordinator = DraftCoordinator::new();
        coordinator.observe(None, desired(&[("a", "first")]), start);
        let operation = coordinator.take_ready(start + DEBOUNCE).expect("save");
        let request = operation.request();
        coordinator.apply_result(DraftResult::Save {
            request,
            key: key("a"),
            revision: 1,
            result: Err(ClientError::Io),
        });
        assert_eq!(coordinator.error(), Some(SYNC_ERROR));
        coordinator.observe(
            None,
            desired(&[("a", "first")]),
            start + Duration::from_secs(1),
        );
        assert!(coordinator
            .take_ready(start + Duration::from_secs(2))
            .is_none());
        coordinator.observe(
            None,
            desired(&[("a", "second")]),
            start + Duration::from_secs(3),
        );
        match coordinator
            .take_ready(start + Duration::from_secs(3) + DEBOUNCE)
            .expect("changed content retries")
        {
            DraftOperation::Save {
                draft, revision, ..
            } => {
                assert_eq!(draft.body, "second");
                assert_eq!(revision, 1);
            }
            other => panic!("expected save, got {other:?}"),
        }
    }

    #[test]
    fn stale_generation_and_request_results_are_ignored() {
        let start = Instant::now();
        let mut coordinator = DraftCoordinator::new();
        coordinator.observe(None, desired(&[("a", "first")]), start);
        let operation = coordinator.take_ready(start + DEBOUNCE).expect("save");
        let request = operation.request();
        coordinator.observe(
            None,
            desired(&[("a", "changed while in flight")]),
            start + DEBOUNCE,
        );
        assert!(coordinator
            .apply_result(DraftResult::Save {
                request,
                key: key("a"),
                revision: 1,
                result: Ok(DraftWriteOutcome::Applied),
            })
            .is_none());
        assert_eq!(coordinator.known_floor(&key("a")), None);
        let next = coordinator
            .take_ready(start + Duration::from_secs(1))
            .expect("latest save");
        let next_request = next.request();
        assert_ne!(next_request, request);
        assert!(coordinator
            .apply_result(DraftResult::Save {
                request: next_request + 1,
                key: key("a"),
                revision: 1,
                result: Ok(DraftWriteOutcome::Applied),
            })
            .is_none());
        assert_eq!(coordinator.known_floor(&key("a")), None);
    }

    #[test]
    fn revision_exhaustion_is_exposed_without_wrapping() {
        let start = Instant::now();
        let mut coordinator = DraftCoordinator::new();
        coordinator.observe(Some(key("a")), desired(&[("a", "local")]), start);
        let load = coordinator.take_ready(start).expect("activation load");
        coordinator.apply_result(DraftResult::Load {
            request: load.request(),
            key: key("a"),
            result: Ok(DraftLoad {
                draft: None,
                revision_floor: i64::MAX as u64,
                corrupt_deleted: false,
            }),
        });
        assert!(coordinator.take_ready(start + DEBOUNCE).is_none());
        assert!(coordinator.is_revision_exhausted(&key("a")));
        assert_eq!(coordinator.known_floor(&key("a")), Some(i64::MAX as u64));
        assert_eq!(coordinator.error(), Some(SYNC_ERROR));
    }

    #[test]
    fn no_save_after_load_records_the_loaded_draft_as_baseline() {
        let start = Instant::now();
        let mut coordinator = DraftCoordinator::new();
        coordinator.observe(Some(key("a")), DraftMap::new(), start);
        let load = coordinator.take_ready(start).expect("load a");
        let loaded = DurableDraft {
            body: "stored".into(),
            cursor: 6,
            pasted_body: true,
            uncertain: true,
        };
        let restore = coordinator
            .apply_result(DraftResult::Load {
                request: load.request(),
                key: key("a"),
                result: Ok(DraftLoad {
                    draft: Some(loaded.clone()),
                    revision_floor: 4,
                    corrupt_deleted: false,
                }),
            })
            .expect("restore candidate");
        assert_eq!(restore.key, key("a"));
        assert_eq!(restore.draft, loaded);
        assert!(restore.draft.pasted_body);
        assert!(restore.draft.uncertain);

        let after_restore = start + Duration::from_secs(1);
        coordinator.observe(
            Some(key("a")),
            DraftMap::from_iter([(key("a"), loaded)]),
            after_restore,
        );
        assert!(
            coordinator.take_ready(after_restore + DEBOUNCE).is_none(),
            "restored content already matches the baseline"
        );
    }

    #[test]
    fn failed_delete_is_retried_after_bounded_backoff() {
        let start = Instant::now();
        let mut coordinator = DraftCoordinator::new();
        coordinator.observe(None, desired(&[("a", "local")]), start);
        let save = coordinator.take_ready(start + DEBOUNCE).expect("save");
        coordinator.apply_result(DraftResult::Save {
            request: save.request(),
            key: key("a"),
            revision: 1,
            result: Ok(DraftWriteOutcome::Applied),
        });

        let cleared_at = start + Duration::from_secs(1);
        coordinator.observe(None, DraftMap::new(), cleared_at);
        let delete_at = cleared_at + DEBOUNCE;
        let delete = coordinator.take_ready(delete_at).expect("delete");
        let revision = match &delete {
            DraftOperation::Delete { revision, .. } => *revision,
            other => panic!("expected delete, got {other:?}"),
        };
        coordinator.apply_result(DraftResult::Delete {
            request: delete.request(),
            key: key("a"),
            revision,
            result: Err(ClientError::Io),
        });
        assert_eq!(coordinator.error(), Some(SYNC_ERROR));
        assert!(coordinator
            .take_ready(delete_at + DELETE_RETRY - Duration::from_millis(1))
            .is_none());
        let retry = coordinator
            .take_ready(delete_at + DELETE_RETRY)
            .expect("delete retry");
        match &retry {
            DraftOperation::Delete {
                revision: retried, ..
            } => assert_eq!(*retried, revision),
            other => panic!("expected delete retry, got {other:?}"),
        }
        coordinator.apply_result(DraftResult::Delete {
            request: retry.request(),
            key: key("a"),
            revision,
            result: Ok(DraftWriteOutcome::Applied),
        });
        assert!(coordinator
            .take_ready(delete_at + Duration::from_secs(10))
            .is_none());
    }

    #[test]
    fn stale_retries_are_capped_until_desired_content_changes() {
        let start = Instant::now();
        let mut coordinator = DraftCoordinator::new();
        coordinator.observe(None, desired(&[("a", "local")]), start);
        let now = start + DEBOUNCE;
        for attempt in 0..=STALE_RETRY_LIMIT {
            let save = coordinator.take_ready(now).expect("save");
            let revision = match &save {
                DraftOperation::Save { revision, .. } => *revision,
                other => panic!("expected save, got {other:?}"),
            };
            coordinator.apply_result(DraftResult::Save {
                request: save.request(),
                key: key("a"),
                revision,
                result: Ok(DraftWriteOutcome::Stale),
            });
            if attempt < STALE_RETRY_LIMIT {
                let load = coordinator.take_ready(now).expect("stale reload");
                coordinator.apply_result(DraftResult::Load {
                    request: load.request(),
                    key: key("a"),
                    result: Ok(DraftLoad {
                        draft: None,
                        revision_floor: 10 + u64::from(attempt),
                        corrupt_deleted: false,
                    }),
                });
            }
        }
        assert!(coordinator.is_stale_suppressed(&key("a")));
        assert_eq!(coordinator.error(), Some(SYNC_ERROR));
        assert!(coordinator
            .take_ready(now + Duration::from_secs(30))
            .is_none());

        let changed_at = now + Duration::from_secs(31);
        coordinator.observe(None, desired(&[("a", "changed")]), changed_at);
        assert!(!coordinator.is_stale_suppressed(&key("a")));
        match coordinator
            .take_ready(changed_at + DEBOUNCE)
            .expect("content change resets the cap")
        {
            DraftOperation::Save { draft, .. } => assert_eq!(draft.body, "changed"),
            other => panic!("expected save, got {other:?}"),
        }
    }

    #[test]
    fn invalid_local_body_is_never_persisted() {
        let start = Instant::now();
        let mut coordinator = DraftCoordinator::new();
        let oversized = draft(&"x".repeat(65_537));
        assert!(!oversized.is_valid());
        coordinator.observe(None, DraftMap::from_iter([(key("a"), oversized)]), start);
        assert!(coordinator
            .take_ready(start + Duration::from_secs(10))
            .is_none());
    }

    #[test]
    fn invalid_content_holds_the_previous_draft_instead_of_deleting_it() {
        let start = Instant::now();
        let mut coordinator = DraftCoordinator::new();
        let saved = draft("saved");
        coordinator.observe(
            Some(key("a")),
            DraftMap::from_iter([(key("a"), saved.clone())]),
            start,
        );
        let load = coordinator.take_ready(start).expect("load a");
        coordinator.apply_result(DraftResult::Load {
            request: load.request(),
            key: key("a"),
            result: Ok(DraftLoad {
                draft: None,
                revision_floor: 0,
                corrupt_deleted: false,
            }),
        });
        let write = coordinator
            .take_ready(start + Duration::from_secs(10))
            .expect("save a");
        coordinator.apply_result(DraftResult::Save {
            request: write.request(),
            key: key("a"),
            revision: write.revision(),
            result: Ok(DraftWriteOutcome::Applied),
        });
        assert!(coordinator.matches_baseline_for_test(&key("a"), Some(&saved)));

        coordinator.observe(
            Some(key("a")),
            DraftMap::from_iter([(key("a"), draft(&"x".repeat(65_537)))]),
            start + Duration::from_secs(20),
        );
        assert!(!coordinator.is_dirty_for_test(&key("a")));
        assert!(coordinator
            .take_ready(start + Duration::from_secs(30))
            .is_none());
    }

    #[test]
    fn stale_delete_does_not_restore_remote_content() {
        let start = Instant::now();
        let mut coordinator = DraftCoordinator::new();
        coordinator.observe(Some(key("a")), DraftMap::new(), start);
        let load = coordinator.take_ready(start).expect("load a");
        coordinator.apply_result(DraftResult::Load {
            request: load.request(),
            key: key("a"),
            result: Ok(DraftLoad {
                draft: Some(draft("stored")),
                revision_floor: 4,
                corrupt_deleted: false,
            }),
        });
        coordinator.observe(
            Some(key("a")),
            DraftMap::new(),
            start + Duration::from_secs(1),
        );
        let delete = coordinator
            .take_ready(start + Duration::from_secs(10))
            .expect("delete a");
        assert_eq!(delete.revision(), 5);
        assert!(coordinator
            .apply_result(DraftResult::Delete {
                request: delete.request(),
                key: key("a"),
                revision: delete.revision(),
                result: Ok(DraftWriteOutcome::Stale),
            })
            .is_none());
        let reload = coordinator
            .take_ready(start + Duration::from_secs(20))
            .expect("reload floor");
        assert!(
            coordinator
                .apply_result(DraftResult::Load {
                    request: reload.request(),
                    key: key("a"),
                    result: Ok(DraftLoad {
                        draft: Some(draft("stored")),
                        revision_floor: 9,
                        corrupt_deleted: false,
                    }),
                })
                .is_none(),
            "a cleared draft must not be restored while the delete is pending"
        );
    }

    #[test]
    fn oversized_loaded_draft_is_not_accepted() {
        let start = Instant::now();
        let mut coordinator = DraftCoordinator::new();
        coordinator.observe(Some(key("a")), DraftMap::new(), start);
        let load = coordinator.take_ready(start).expect("load a");
        let oversized = draft(&"x".repeat(65_537));
        assert!(coordinator
            .apply_result(DraftResult::Load {
                request: load.request(),
                key: key("a"),
                result: Ok(DraftLoad {
                    draft: Some(oversized),
                    revision_floor: 2,
                    corrupt_deleted: false,
                }),
            })
            .is_none());
        assert_eq!(coordinator.known_floor(&key("a")), Some(2));
        assert!(coordinator
            .take_ready(start + Duration::from_secs(10))
            .is_none());
    }

    #[test]
    fn in_flight_activation_load_is_dropped_when_the_owner_changes() {
        let start = Instant::now();
        let mut coordinator = DraftCoordinator::new();
        coordinator.observe(Some(key("a")), DraftMap::new(), start);
        let load = coordinator.take_ready(start).expect("load a");
        assert_eq!(load.key(), &key("a"));
        let load_request = load.request();
        assert!(matches!(load, DraftOperation::Load { .. }));

        coordinator.observe(
            Some(key("b")),
            DraftMap::new(),
            start + Duration::from_secs(1),
        );
        assert!(
            coordinator
                .apply_result(DraftResult::Load {
                    request: load_request,
                    key: key("a"),
                    result: Ok(DraftLoad {
                        draft: Some(draft("late")),
                        revision_floor: 1,
                        corrupt_deleted: false,
                    }),
                })
                .is_none(),
            "the active-owner guard drops a load whose owner is no longer active"
        );
        assert_eq!(coordinator.known_floor(&key("a")), None);
    }
}
