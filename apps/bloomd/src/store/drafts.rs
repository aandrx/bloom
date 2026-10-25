//! Durable composer drafts, stored only in the encrypted daemon database.
//!
//! Tombstones carry the latest revision so a late save cannot resurrect a draft
//! the user cleared. Retention is bounded: a tombstone protects against stale
//! writes only while it is retained, and expiry or the per-transport row cap may
//! eventually remove it.

use super::{EncryptedStore, StoreError};
use bloom_domain::{ConversationId, TransportId};
use rusqlite::{params, OptionalExtension, Transaction};
use serde_json::json;
use std::fmt;
use unicode_segmentation::UnicodeSegmentation;

/// Largest accepted draft body, measured in UTF-8 bytes.
pub const MAX_DRAFT_BYTES: usize = 65_536;
/// Drafts and tombstones older than this are pruned on load.
pub const MAX_DRAFT_AGE_MS: i64 = 30 * 24 * 60 * 60 * 1_000;
/// Maximum retained rows, active or tombstone, for one exact transport.
pub const MAX_DRAFT_ROWS_PER_TRANSPORT: usize = 100;

/// Exact owner of a draft. A new-chat draft without a usable address is invalid
/// and is never persisted.
#[derive(Clone, PartialEq, Eq)]
pub enum DraftKey {
    Conversation(ConversationId),
    NewChat {
        transport: TransportId,
        address: String,
    },
}

impl fmt::Debug for DraftKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Conversation(_) => f.write_str("DraftKey::Conversation([REDACTED])"),
            Self::NewChat { .. } => f.write_str("DraftKey::NewChat([REDACTED])"),
        }
    }
}

impl DraftKey {
    fn transport(&self) -> &str {
        match self {
            Self::Conversation(id) => id.transport().as_str(),
            Self::NewChat { transport, .. } => transport.as_str(),
        }
    }

    fn is_valid(&self) -> bool {
        match self {
            Self::Conversation(_) => true,
            Self::NewChat { transport, address } => {
                !transport.as_str().trim().is_empty() && !address.trim().is_empty()
            }
        }
    }
}

/// A restored draft body plus the composer flags needed to render it.
#[derive(Clone, PartialEq, Eq)]
pub struct DurableDraft {
    pub body: String,
    /// Grapheme index into `body`.
    pub cursor: usize,
    pub pasted_body: bool,
    pub uncertain: bool,
}

impl fmt::Debug for DurableDraft {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("DurableDraft")
            .field("body", &"[REDACTED]")
            .field("cursor", &self.cursor)
            .field("pasted_body", &self.pasted_body)
            .field("uncertain", &self.uncertain)
            .finish()
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DraftWriteOutcome {
    Applied,
    Stale,
}

#[derive(Debug, PartialEq, Eq)]
pub struct DraftLoad {
    pub draft: Option<DurableDraft>,
    pub revision_floor: u64,
    pub corrupt_deleted: bool,
}

struct StoredDraft {
    rowid: i64,
    conversation_id: Option<String>,
    new_chat_transport: Option<String>,
    new_chat_address: Option<String>,
    transport: String,
    revision: i64,
    deleted: i64,
    updated_at: i64,
    data: Option<Vec<u8>>,
}

fn valid_revision(revision: u64) -> Result<i64, StoreError> {
    if revision > i64::MAX as u64 {
        return Err(StoreError::InvalidDraft);
    }
    Ok(revision as i64)
}

fn valid_updated_at(updated_at: i64) -> Result<i64, StoreError> {
    if updated_at.checked_sub(MAX_DRAFT_AGE_MS).is_none() {
        return Err(StoreError::InvalidDraft);
    }
    Ok(updated_at)
}

fn draft_is_valid(draft: &DurableDraft) -> bool {
    draft.body.len() <= MAX_DRAFT_BYTES && draft.cursor <= draft.body.graphemes(true).count()
}

fn conversation_owner(id: &ConversationId) -> Result<String, StoreError> {
    serde_json::to_string(id).map_err(|_| StoreError::InvalidDraft)
}

fn encode_draft(draft: &DurableDraft) -> Result<Vec<u8>, StoreError> {
    serde_json::to_vec(&json!({
        "body": draft.body,
        "cursor": draft.cursor,
        "pasted_body": draft.pasted_body,
        "uncertain": draft.uncertain,
    }))
    .map_err(|_| StoreError::InvalidDraft)
}

fn decode_draft(data: &[u8]) -> Option<DurableDraft> {
    let value: serde_json::Value = serde_json::from_slice(data).ok()?;
    let draft = DurableDraft {
        body: value.get("body")?.as_str()?.to_owned(),
        cursor: usize::try_from(value.get("cursor")?.as_u64()?).ok()?,
        pasted_body: value.get("pasted_body")?.as_bool()?,
        uncertain: value.get("uncertain")?.as_bool()?,
    };
    draft_is_valid(&draft).then_some(draft)
}

fn clear_stale_new_chat_identity(tx: &Transaction<'_>, key: &DraftKey) -> Result<(), StoreError> {
    if let DraftKey::NewChat { transport, address } = key {
        tx.execute(
            "UPDATE drafts SET new_chat_transport=NULL, new_chat_address=NULL
             WHERE conversation_id IS NOT NULL
               AND new_chat_transport=?1 AND new_chat_address=?2",
            params![transport.as_str(), address],
        )?;
    }
    Ok(())
}

fn current_revision(tx: &Transaction<'_>, key: &DraftKey) -> Result<Option<i64>, StoreError> {
    let revision = match key {
        DraftKey::Conversation(id) => {
            let id = conversation_owner(id)?;
            tx.query_row(
                "SELECT revision FROM drafts WHERE conversation_id=?1 ORDER BY rowid LIMIT 1",
                params![id],
                |row| row.get(0),
            )
            .optional()?
        }
        DraftKey::NewChat { transport, address } => tx
            .query_row(
                "SELECT revision FROM drafts
                 WHERE conversation_id IS NULL
                   AND new_chat_transport=?1 AND new_chat_address=?2
                 ORDER BY rowid LIMIT 1",
                params![transport.as_str(), address],
                |row| row.get(0),
            )
            .optional()?,
    };
    Ok(revision)
}

fn apply_write(
    tx: &Transaction<'_>,
    key: &DraftKey,
    revision: i64,
    updated_at: i64,
    data: Option<&[u8]>,
    deleted: bool,
) -> Result<i64, StoreError> {
    let deleted = deleted as i64;
    let changed = match key {
        DraftKey::Conversation(id) => {
            let id = conversation_owner(id)?;
            tx.execute(
                "UPDATE drafts SET new_chat_transport=NULL, new_chat_address=NULL,
                        transport=?2, revision=?3, deleted=?4, updated_at=?5, data=?6
                 WHERE conversation_id=?1",
                params![id, key.transport(), revision, deleted, updated_at, data],
            )?
        }
        DraftKey::NewChat { transport, address } => tx.execute(
            "UPDATE drafts SET conversation_id=NULL,
                    transport=?3, revision=?4, deleted=?5, updated_at=?6, data=?7
             WHERE conversation_id IS NULL
               AND new_chat_transport=?1 AND new_chat_address=?2",
            params![
                transport.as_str(),
                address,
                key.transport(),
                revision,
                deleted,
                updated_at,
                data
            ],
        )?,
    };
    if changed != 0 {
        return exact_rowid(tx, key);
    }
    match key {
        DraftKey::Conversation(id) => {
            let id = conversation_owner(id)?;
            tx.execute(
                "INSERT INTO drafts(conversation_id, new_chat_transport, new_chat_address, transport, revision, deleted, updated_at, data)
                 VALUES (?1, NULL, NULL, ?2, ?3, ?4, ?5, ?6)",
                params![id, key.transport(), revision, deleted, updated_at, data],
            )?;
        }
        DraftKey::NewChat { transport, address } => {
            tx.execute(
                "INSERT INTO drafts(conversation_id, new_chat_transport, new_chat_address, transport, revision, deleted, updated_at, data)
                 VALUES (NULL, ?1, ?2, ?1, ?3, ?4, ?5, ?6)",
                params![
                    transport.as_str(),
                    address,
                    revision,
                    deleted,
                    updated_at,
                    data
                ],
            )?;
        }
    }
    exact_rowid(tx, key)
}

fn exact_rowid(tx: &Transaction<'_>, key: &DraftKey) -> Result<i64, StoreError> {
    match key {
        DraftKey::Conversation(id) => {
            let id = conversation_owner(id)?;
            tx.query_row(
                "SELECT rowid FROM drafts
                 WHERE conversation_id=?1
                   AND new_chat_transport IS NULL AND new_chat_address IS NULL",
                params![id],
                |row| row.get(0),
            )
            .map_err(StoreError::from)
        }
        DraftKey::NewChat { transport, address } => tx
            .query_row(
                "SELECT rowid FROM drafts
                 WHERE conversation_id IS NULL
                   AND new_chat_transport=?1 AND new_chat_address=?2",
                params![transport.as_str(), address],
                |row| row.get(0),
            )
            .map_err(StoreError::from),
    }
}

/// Save a draft only when `revision` is newer than the stored revision.
///
/// `updated_at` is daemon wall-clock time and also drives bounded retention.
/// Future IPC must not trust a client-supplied timestamp here.
pub fn save_draft(
    store: &mut EncryptedStore,
    key: &DraftKey,
    draft: &DurableDraft,
    revision: u64,
    updated_at: i64,
) -> Result<DraftWriteOutcome, StoreError> {
    if !key.is_valid() || !draft_is_valid(draft) {
        return Err(StoreError::InvalidDraft);
    }
    let revision = valid_revision(revision)?;
    let updated_at = valid_updated_at(updated_at)?;
    let data = encode_draft(draft)?;
    let tx = store._connection.transaction()?;
    clear_stale_new_chat_identity(&tx, key)?;
    if let Some(stored) = current_revision(&tx, key)? {
        if revision <= stored {
            tx.commit()?;
            return Ok(DraftWriteOutcome::Stale);
        }
    }
    let rowid = apply_write(&tx, key, revision, updated_at, Some(&data), false)?;
    prune_drafts(&tx, key.transport(), updated_at, Some(rowid))?;
    tx.commit()?;
    Ok(DraftWriteOutcome::Applied)
}

/// Delete a draft by writing a tombstone. A missing key can receive a tombstone.
///
/// `updated_at` is daemon wall-clock time and also drives bounded retention.
/// Future IPC must not trust a client-supplied timestamp here.
pub fn delete_draft(
    store: &mut EncryptedStore,
    key: &DraftKey,
    revision: u64,
    updated_at: i64,
) -> Result<DraftWriteOutcome, StoreError> {
    if !key.is_valid() {
        return Err(StoreError::InvalidDraft);
    }
    let revision = valid_revision(revision)?;
    let updated_at = valid_updated_at(updated_at)?;
    let tx = store._connection.transaction()?;
    clear_stale_new_chat_identity(&tx, key)?;
    if let Some(stored) = current_revision(&tx, key)? {
        if revision <= stored {
            tx.commit()?;
            return Ok(DraftWriteOutcome::Stale);
        }
    }
    let rowid = apply_write(&tx, key, revision, updated_at, None, true)?;
    prune_drafts(&tx, key.transport(), updated_at, Some(rowid))?;
    tx.commit()?;
    Ok(DraftWriteOutcome::Applied)
}

fn prune_drafts(
    tx: &Transaction<'_>,
    transport: &str,
    now: i64,
    protected_rowid: Option<i64>,
) -> Result<(), StoreError> {
    let cutoff = now.saturating_sub(MAX_DRAFT_AGE_MS);
    tx.execute(
        "DELETE FROM drafts
         WHERE transport=?1 AND updated_at < ?2
           AND (?3 IS NULL OR rowid != ?3)",
        params![transport, cutoff, protected_rowid],
    )?;
    let mut count: i64 = tx.query_row(
        "SELECT count(*) FROM drafts WHERE transport=?1",
        params![transport],
        |row| row.get(0),
    )?;
    while count > MAX_DRAFT_ROWS_PER_TRANSPORT as i64 {
        let removed = tx.execute(
            "DELETE FROM drafts WHERE rowid = (
                 SELECT d.rowid FROM drafts d
                 WHERE d.transport=?1 AND (?2 IS NULL OR d.rowid != ?2)
                 ORDER BY d.updated_at ASC, d.revision ASC,
                          COALESCE(d.conversation_id, '') ASC,
                          COALESCE(d.new_chat_transport, '') ASC,
                          COALESCE(d.new_chat_address, '') ASC,
                          d.rowid ASC
                 LIMIT 1
             )",
            params![transport, protected_rowid],
        )?;
        if removed == 0 {
            break;
        }
        count -= removed as i64;
    }
    Ok(())
}

fn tombstone(
    tx: &Transaction<'_>,
    key: &DraftKey,
    rowid: i64,
    revision: i64,
    updated_at: i64,
) -> Result<(), StoreError> {
    let revision = revision.max(0);
    match key {
        DraftKey::Conversation(id) => {
            let owner = conversation_owner(id)?;
            tx.execute(
                "UPDATE drafts SET conversation_id=?2, new_chat_transport=NULL,
                        new_chat_address=NULL, transport=?3, deleted=1, data=NULL,
                        revision=?4, updated_at=?5
                 WHERE rowid=?1",
                params![rowid, owner, key.transport(), revision, updated_at],
            )?;
        }
        DraftKey::NewChat { transport, address } => {
            tx.execute(
                "UPDATE drafts SET conversation_id=NULL, new_chat_transport=?2,
                        new_chat_address=?3, transport=?4, deleted=1, data=NULL,
                        revision=?5, updated_at=?6
                 WHERE rowid=?1",
                params![
                    rowid,
                    transport.as_str(),
                    address,
                    key.transport(),
                    revision,
                    updated_at
                ],
            )?;
        }
    }
    Ok(())
}

/// Structural corruption, including owner/transport mismatches and deleted/data
/// incoherence. Undecodable active data is checked separately after this.
fn stored_is_corrupt(
    key: &DraftKey,
    expected_conversation_owner: Option<&str>,
    stored: &StoredDraft,
) -> bool {
    if stored.revision < 0 || !matches!(stored.deleted, 0 | 1) {
        return true;
    }
    let owners_match = match key {
        DraftKey::Conversation(id) => {
            stored.conversation_id.as_deref() == expected_conversation_owner
                && stored.new_chat_transport.is_none()
                && stored.new_chat_address.is_none()
                && stored.transport == id.transport().as_str()
        }
        DraftKey::NewChat { transport, address } => {
            stored.conversation_id.is_none()
                && stored.new_chat_transport.as_deref() == Some(transport.as_str())
                && stored.new_chat_address.as_deref() == Some(address.as_str())
                && stored.transport == transport.as_str()
                && !address.trim().is_empty()
        }
    };
    if !owners_match {
        return true;
    }
    match stored.deleted {
        1 => stored.data.is_some(),
        0 => stored.data.is_none(),
        _ => true,
    }
}

/// Load a draft after pruning that exact transport's expired and excess rows.
///
/// The requested row and its revision floor are captured before pruning so an
/// expired or cap-evicted row still reports the floor it had when it existed.
/// Callers supply daemon wall-clock `updated_at`; a future IPC layer must not
/// trust a client timestamp for these semantics.
pub fn load_draft(
    store: &mut EncryptedStore,
    key: &DraftKey,
    now: i64,
) -> Result<DraftLoad, StoreError> {
    if !key.is_valid() {
        return Err(StoreError::InvalidDraft);
    }
    let transport = key.transport().to_owned();
    let expected_conversation_owner = match key {
        DraftKey::Conversation(id) => Some(conversation_owner(id)?),
        DraftKey::NewChat { .. } => None,
    };
    let tx = store._connection.transaction()?;
    // A malformed conversation row may carry stale new-chat columns. Clear those
    // before looking up the exact new-chat owner so it cannot hijack this key.
    clear_stale_new_chat_identity(&tx, key)?;
    let stored = match key {
        DraftKey::Conversation(id) => {
            let id = conversation_owner(id)?;
            tx.query_row(
                "SELECT rowid, conversation_id, new_chat_transport, new_chat_address, transport,
                        revision, deleted, updated_at, data
                 FROM drafts
                 WHERE conversation_id=?1
                 ORDER BY rowid LIMIT 1",
                params![id],
                map_stored,
            )
            .optional()?
        }
        DraftKey::NewChat { transport, address } => tx
            .query_row(
                "SELECT rowid, conversation_id, new_chat_transport, new_chat_address, transport,
                        revision, deleted, updated_at, data
                 FROM drafts
                 WHERE conversation_id IS NULL
                   AND new_chat_transport=?1 AND new_chat_address=?2
                 ORDER BY rowid LIMIT 1",
                params![transport.as_str(), address],
                map_stored,
            )
            .optional()?,
    };
    let captured_revision = stored.as_ref().map(|stored| stored.revision.max(0));
    prune_drafts(&tx, &transport, now, None)?;
    let Some(stored) = stored else {
        tx.commit()?;
        return Ok(DraftLoad {
            draft: None,
            revision_floor: 0,
            corrupt_deleted: false,
        });
    };
    let revision_floor_i64 = captured_revision.unwrap_or(0);
    let revision_floor = u64::try_from(revision_floor_i64).unwrap_or(0);
    let survived: bool = tx.query_row(
        "SELECT EXISTS(SELECT 1 FROM drafts WHERE rowid=?1)",
        params![stored.rowid],
        |row| row.get(0),
    )?;
    if !survived {
        tx.commit()?;
        return Ok(DraftLoad {
            draft: None,
            revision_floor,
            corrupt_deleted: false,
        });
    }
    let tombstone_updated_at = if stored.updated_at.checked_sub(MAX_DRAFT_AGE_MS).is_some() {
        stored.updated_at
    } else {
        now
    };
    if stored_is_corrupt(key, expected_conversation_owner.as_deref(), &stored) {
        tombstone(
            &tx,
            key,
            stored.rowid,
            revision_floor_i64,
            tombstone_updated_at,
        )?;
        tx.commit()?;
        return Ok(DraftLoad {
            draft: None,
            revision_floor,
            corrupt_deleted: true,
        });
    }
    if stored.deleted != 0 {
        tx.commit()?;
        return Ok(DraftLoad {
            draft: None,
            revision_floor,
            corrupt_deleted: false,
        });
    }
    let Some(data) = stored.data.as_deref() else {
        tombstone(
            &tx,
            key,
            stored.rowid,
            revision_floor_i64,
            tombstone_updated_at,
        )?;
        tx.commit()?;
        return Ok(DraftLoad {
            draft: None,
            revision_floor,
            corrupt_deleted: true,
        });
    };
    let Some(draft) = decode_draft(data) else {
        tombstone(
            &tx,
            key,
            stored.rowid,
            revision_floor_i64,
            tombstone_updated_at,
        )?;
        tx.commit()?;
        return Ok(DraftLoad {
            draft: None,
            revision_floor,
            corrupt_deleted: true,
        });
    };
    tx.commit()?;
    Ok(DraftLoad {
        draft: Some(draft),
        revision_floor,
        corrupt_deleted: false,
    })
}

#[cfg(test)]
pub(crate) fn corrupt_draft_data_for_test(
    store: &EncryptedStore,
    key: &DraftKey,
    data: &[u8],
) -> Result<(), StoreError> {
    let changed = match key {
        DraftKey::Conversation(id) => store._connection.execute(
            "UPDATE drafts SET data=?2 WHERE conversation_id=?1",
            params![conversation_owner(id)?, data],
        )?,
        DraftKey::NewChat { transport, address } => store._connection.execute(
            "UPDATE drafts SET data=?3
             WHERE conversation_id IS NULL
               AND new_chat_transport=?1 AND new_chat_address=?2",
            params![transport.as_str(), address, data],
        )?,
    };
    if changed == 1 {
        Ok(())
    } else {
        Err(StoreError::Schema)
    }
}

fn map_stored(row: &rusqlite::Row<'_>) -> rusqlite::Result<StoredDraft> {
    Ok(StoredDraft {
        rowid: row.get(0)?,
        conversation_id: row.get(1)?,
        new_chat_transport: row.get(2)?,
        new_chat_address: row.get(3)?,
        transport: row.get(4)?,
        revision: row.get(5)?,
        deleted: row.get(6)?,
        updated_at: row.get(7)?,
        data: row.get(8)?,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use bloom_domain::{ConversationId, TransportId};
    use rusqlite::params;

    use crate::store::KEY_BYTES;

    const KEY: [u8; 32] = [11; KEY_BYTES];

    fn setup() -> (tempfile::TempDir, EncryptedStore) {
        let temp = tempfile::tempdir().unwrap();
        let private = temp.path().join("bloom");
        let store = EncryptedStore::open_with_key(private.join("drafts.db"), &KEY).unwrap();
        (temp, store)
    }

    fn conversation(value: &str) -> ConversationId {
        ConversationId::new(TransportId::new("relay").unwrap(), value).unwrap()
    }

    fn new_chat(address: &str) -> DraftKey {
        DraftKey::NewChat {
            transport: TransportId::new("relay").unwrap(),
            address: address.to_owned(),
        }
    }

    fn draft(body: &str, cursor: usize) -> DurableDraft {
        DurableDraft {
            body: body.to_owned(),
            cursor,
            pasted_body: false,
            uncertain: false,
        }
    }

    fn row_count(store: &EncryptedStore) -> i64 {
        store
            ._connection
            .query_row("SELECT count(*) FROM drafts", [], |row| row.get(0))
            .unwrap()
    }

    #[test]
    fn conversation_and_new_chat_are_isolated_and_encrypted_on_reopen() {
        let (temp, mut store) = setup();
        let path = temp.path().join("bloom").join("drafts.db");
        let conv = DraftKey::Conversation(conversation("conversation-secret"));
        let chat = new_chat("alice@example.test");
        let other_chat = new_chat("bob@example.test");
        let conv_body = "conversation body sentinel b2a";
        let chat_body = "new chat body sentinel c7d";

        assert_eq!(
            save_draft(&mut store, &conv, &draft(conv_body, 3), 1, 1_000).unwrap(),
            DraftWriteOutcome::Applied
        );
        assert_eq!(
            save_draft(&mut store, &chat, &draft(chat_body, 0), 1, 1_000).unwrap(),
            DraftWriteOutcome::Applied
        );
        assert_eq!(
            save_draft(&mut store, &other_chat, &draft("other", 0), 1, 1_000).unwrap(),
            DraftWriteOutcome::Applied
        );
        assert_eq!(
            load_draft(&mut store, &conv, 1_100)
                .unwrap()
                .draft
                .unwrap()
                .body,
            conv_body
        );
        assert_eq!(
            load_draft(&mut store, &chat, 1_100)
                .unwrap()
                .draft
                .unwrap()
                .body,
            chat_body
        );
        assert_eq!(
            load_draft(&mut store, &other_chat, 1_100)
                .unwrap()
                .draft
                .unwrap()
                .body,
            "other"
        );
        drop(store);

        for suffix in ["", "-wal", "-shm"] {
            let mut name = path.as_os_str().to_os_string();
            name.push(suffix);
            if let Ok(bytes) = std::fs::read(std::path::PathBuf::from(name)) {
                for sentinel in [conv_body, chat_body] {
                    assert!(!bytes
                        .windows(sentinel.len())
                        .any(|w| w == sentinel.as_bytes()));
                }
            }
        }

        let mut reopened = EncryptedStore::open_with_key(&path, &KEY).unwrap();
        assert_eq!(
            load_draft(&mut reopened, &conv, 1_200)
                .unwrap()
                .draft
                .unwrap()
                .body,
            conv_body
        );
        assert_eq!(
            load_draft(&mut reopened, &chat, 1_200)
                .unwrap()
                .draft
                .unwrap()
                .body,
            chat_body
        );
    }

    #[test]
    fn unicode_cursor_and_byte_limits_are_enforced() {
        let (_temp, mut store) = setup();
        let key = DraftKey::Conversation(conversation("unicode"));
        let body = "a\u{1f469}\u{200d}\u{1f469}\u{200d}\u{1f467}\u{200d}\u{1f466}e\u{301}";
        let graphemes = body.graphemes(true).count();

        assert_eq!(
            save_draft(&mut store, &key, &draft(body, graphemes), 1, 1_000).unwrap(),
            DraftWriteOutcome::Applied
        );
        assert_eq!(
            load_draft(&mut store, &key, 1_100)
                .unwrap()
                .draft
                .unwrap()
                .cursor,
            graphemes
        );
        assert!(matches!(
            save_draft(&mut store, &key, &draft(body, graphemes + 1), 2, 1_000),
            Err(StoreError::InvalidDraft)
        ));

        let max_body = "a".repeat(MAX_DRAFT_BYTES);
        let max_key = DraftKey::Conversation(conversation("max-bytes"));
        assert_eq!(
            save_draft(&mut store, &max_key, &draft(&max_body, 1), 1, 1_000).unwrap(),
            DraftWriteOutcome::Applied
        );
        let too_large = "a".repeat(MAX_DRAFT_BYTES + 1);
        assert!(matches!(
            save_draft(
                &mut store,
                &DraftKey::Conversation(conversation("too-large")),
                &draft(&too_large, 0),
                1,
                1_000
            ),
            Err(StoreError::InvalidDraft)
        ));

        let blank = new_chat("   ");
        assert!(matches!(
            save_draft(&mut store, &blank, &draft("must not persist", 0), 1, 1_000),
            Err(StoreError::InvalidDraft)
        ));
        assert!(matches!(
            delete_draft(&mut store, &blank, 1, 1_000),
            Err(StoreError::InvalidDraft)
        ));
        assert_eq!(row_count(&store), 2);
    }

    #[test]
    fn stale_and_equal_saves_keep_the_newer_body() {
        let (_temp, mut store) = setup();
        let key = DraftKey::Conversation(conversation("stale"));
        assert_eq!(
            save_draft(&mut store, &key, &draft("first", 0), 1, 1_000).unwrap(),
            DraftWriteOutcome::Applied
        );
        assert_eq!(
            save_draft(&mut store, &key, &draft("stale", 0), 1, 1_100).unwrap(),
            DraftWriteOutcome::Stale
        );
        assert_eq!(
            save_draft(&mut store, &key, &draft("older", 0), 0, 1_100).unwrap(),
            DraftWriteOutcome::Stale
        );
        let load = load_draft(&mut store, &key, 1_200).unwrap();
        assert_eq!(load.draft.unwrap().body, "first");
        assert_eq!(load.revision_floor, 1);
        assert_eq!(
            save_draft(&mut store, &key, &draft("second", 0), 2, 1_300).unwrap(),
            DraftWriteOutcome::Applied
        );
        assert_eq!(
            load_draft(&mut store, &key, 1_400)
                .unwrap()
                .draft
                .unwrap()
                .body,
            "second"
        );
        assert!(matches!(
            save_draft(&mut store, &key, &draft("overflow", 0), u64::MAX, 1_500),
            Err(StoreError::InvalidDraft)
        ));
        assert!(matches!(
            save_draft(&mut store, &key, &draft("timestamp", 0), 3, i64::MIN),
            Err(StoreError::InvalidDraft)
        ));
    }

    #[test]
    fn tombstone_blocks_late_saves_and_missing_delete_writes_a_tombstone() {
        let (_temp, mut store) = setup();
        let key = DraftKey::Conversation(conversation("tombstone"));
        assert_eq!(
            save_draft(&mut store, &key, &draft("live", 0), 1, 1_000).unwrap(),
            DraftWriteOutcome::Applied
        );
        assert_eq!(
            delete_draft(&mut store, &key, 2, 1_100).unwrap(),
            DraftWriteOutcome::Applied
        );
        let deleted = load_draft(&mut store, &key, 1_200).unwrap();
        assert_eq!(deleted.draft, None);
        assert_eq!(deleted.revision_floor, 2);
        assert!(!deleted.corrupt_deleted);
        for revision in [1, 2] {
            assert_eq!(
                save_draft(&mut store, &key, &draft("late", 0), revision, 1_200).unwrap(),
                DraftWriteOutcome::Stale
            );
        }
        assert_eq!(
            load_draft(&mut store, &key, 1_300).unwrap().revision_floor,
            2
        );
        assert_eq!(
            save_draft(&mut store, &key, &draft("newer", 0), 3, 1_300).unwrap(),
            DraftWriteOutcome::Applied
        );

        let missing = DraftKey::Conversation(conversation("missing"));
        assert_eq!(
            delete_draft(&mut store, &missing, 5, 1_400).unwrap(),
            DraftWriteOutcome::Applied
        );
        let tombstone = load_draft(&mut store, &missing, 1_500).unwrap();
        assert_eq!(tombstone.draft, None);
        assert_eq!(tombstone.revision_floor, 5);
        assert_eq!(
            save_draft(&mut store, &missing, &draft("resurrect", 0), 5, 1_500).unwrap(),
            DraftWriteOutcome::Stale
        );
    }

    #[test]
    fn expiry_purges_rows_older_than_the_age_limit() {
        let (_temp, mut store) = setup();
        let now = 1_000_000_000_000;
        let old = DraftKey::Conversation(conversation("expired"));
        let recent = DraftKey::Conversation(conversation("recent"));
        assert_eq!(
            save_draft(
                &mut store,
                &old,
                &draft("expired", 0),
                1,
                now - MAX_DRAFT_AGE_MS - 1
            )
            .unwrap(),
            DraftWriteOutcome::Applied
        );
        // The requested row is captured before pruning, so its floor survives.
        let expired = load_draft(&mut store, &old, now).unwrap();
        assert_eq!(expired.draft, None);
        assert_eq!(expired.revision_floor, 1);
        assert_eq!(
            save_draft(&mut store, &recent, &draft("recent", 0), 1, now).unwrap(),
            DraftWriteOutcome::Applied
        );
        assert_eq!(
            load_draft(&mut store, &recent, now)
                .unwrap()
                .draft
                .unwrap()
                .body,
            "recent"
        );
        assert_eq!(row_count(&store), 1);
    }

    #[test]
    fn per_transport_cap_does_not_affect_another_transport() {
        let (_temp, mut store) = setup();
        for index in 0..MAX_DRAFT_ROWS_PER_TRANSPORT {
            let key = DraftKey::Conversation(conversation(&format!("c{index:03}")));
            assert_eq!(
                save_draft(&mut store, &key, &draft("body", 0), 1, 1_000 + index as i64).unwrap(),
                DraftWriteOutcome::Applied
            );
        }
        let other_transport = DraftKey::NewChat {
            transport: TransportId::new("direct").unwrap(),
            address: "keep@example.test".to_owned(),
        };
        assert_eq!(
            save_draft(
                &mut store,
                &other_transport,
                &draft("other transport", 0),
                1,
                1_000
            )
            .unwrap(),
            DraftWriteOutcome::Applied
        );
        let newest = DraftKey::Conversation(conversation("c100"));
        assert_eq!(
            save_draft(&mut store, &newest, &draft("newest", 0), 1, 2_000).unwrap(),
            DraftWriteOutcome::Applied
        );
        assert_eq!(
            load_draft(&mut store, &newest, 2_100)
                .unwrap()
                .draft
                .unwrap()
                .body,
            "newest"
        );
        assert_eq!(
            load_draft(
                &mut store,
                &DraftKey::Conversation(conversation("c000")),
                2_100
            )
            .unwrap()
            .draft,
            None
        );
        let relay_count: i64 = store
            ._connection
            .query_row(
                "SELECT count(*) FROM drafts WHERE transport='relay'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(relay_count, MAX_DRAFT_ROWS_PER_TRANSPORT as i64);
        assert_eq!(
            load_draft(&mut store, &other_transport, 2_100)
                .unwrap()
                .draft
                .unwrap()
                .body,
            "other transport"
        );
    }

    #[test]
    fn cap_eviction_is_deterministic_by_updated_at_revision_and_key() {
        let (_temp, mut store) = setup();
        for index in 0..MAX_DRAFT_ROWS_PER_TRANSPORT {
            let key = DraftKey::Conversation(conversation(&format!("d{index:03}")));
            save_draft(&mut store, &key, &draft("body", 0), 1, 1_000).unwrap();
        }
        let extra = DraftKey::Conversation(conversation("d999"));
        save_draft(&mut store, &extra, &draft("extra", 0), 1, 1_000).unwrap();
        // Saving the extra key prunes the oldest key at that timestamp.
        assert!(load_draft(&mut store, &extra, 1_100)
            .unwrap()
            .draft
            .is_some());
        assert!(load_draft(
            &mut store,
            &DraftKey::Conversation(conversation("d000")),
            1_100
        )
        .unwrap()
        .draft
        .is_none());
        assert!(load_draft(
            &mut store,
            &DraftKey::Conversation(conversation("d001")),
            1_100
        )
        .unwrap()
        .draft
        .is_some());
    }

    #[test]
    fn a_successful_write_is_protected_from_its_own_cap_prune() {
        let (_temp, mut store) = setup();
        for index in 0..MAX_DRAFT_ROWS_PER_TRANSPORT {
            let key = DraftKey::Conversation(conversation(&format!("z{index:03}")));
            save_draft(&mut store, &key, &draft("body", 0), 1, 1_000).unwrap();
        }
        let sorts_first = DraftKey::Conversation(conversation("a000"));
        assert_eq!(
            save_draft(&mut store, &sorts_first, &draft("protected", 0), 1, 1_000).unwrap(),
            DraftWriteOutcome::Applied
        );
        assert_eq!(row_count(&store), MAX_DRAFT_ROWS_PER_TRANSPORT as i64);
        assert_eq!(
            load_draft(&mut store, &sorts_first, 1_100)
                .unwrap()
                .draft
                .unwrap()
                .body,
            "protected"
        );

        let (_temp, mut store) = setup();
        for index in 0..MAX_DRAFT_ROWS_PER_TRANSPORT {
            let key = DraftKey::Conversation(conversation(&format!("z{index:03}")));
            save_draft(&mut store, &key, &draft("body", 0), 1, 1_000).unwrap();
        }
        let tombstone = DraftKey::Conversation(conversation("a-delete"));
        assert_eq!(
            delete_draft(&mut store, &tombstone, 7, 1_000).unwrap(),
            DraftWriteOutcome::Applied
        );
        let loaded = load_draft(&mut store, &tombstone, 1_100).unwrap();
        assert_eq!(loaded.draft, None);
        assert_eq!(loaded.revision_floor, 7);
    }

    #[test]
    fn writes_prune_without_a_load() {
        let (_temp, mut store) = setup();
        for index in 0..MAX_DRAFT_ROWS_PER_TRANSPORT {
            let key = DraftKey::Conversation(conversation(&format!("e{index:03}")));
            save_draft(&mut store, &key, &draft("body", 0), 1, 1_000 + index as i64).unwrap();
        }
        assert_eq!(row_count(&store), MAX_DRAFT_ROWS_PER_TRANSPORT as i64);
        let newest = DraftKey::Conversation(conversation("e100"));
        save_draft(&mut store, &newest, &draft("newest", 0), 1, 5_000).unwrap();
        assert_eq!(row_count(&store), MAX_DRAFT_ROWS_PER_TRANSPORT as i64);
        let oldest_owner = conversation_owner(&conversation("e000")).unwrap();
        let oldest_exists: bool = store
            ._connection
            .query_row(
                "SELECT EXISTS(SELECT 1 FROM drafts WHERE conversation_id=?1)",
                params![oldest_owner],
                |row| row.get(0),
            )
            .unwrap();
        assert!(!oldest_exists);

        let (_temp2, mut store2) = setup();
        let now = 1_000_000;
        let expired = DraftKey::Conversation(conversation("expired"));
        save_draft(
            &mut store2,
            &expired,
            &draft("expired", 0),
            1,
            now - MAX_DRAFT_AGE_MS - 1,
        )
        .unwrap();
        assert_eq!(row_count(&store2), 1);
        let marker = DraftKey::Conversation(conversation("marker"));
        delete_draft(&mut store2, &marker, 1, now).unwrap();
        assert_eq!(row_count(&store2), 1);
        let expired_owner = conversation_owner(&conversation("expired")).unwrap();
        let expired_exists: bool = store2
            ._connection
            .query_row(
                "SELECT EXISTS(SELECT 1 FROM drafts WHERE conversation_id=?1)",
                params![expired_owner],
                |row| row.get(0),
            )
            .unwrap();
        assert!(!expired_exists);
    }

    #[test]
    fn cap_evicted_request_returns_its_captured_floor() {
        let (_temp, mut store) = setup();
        for index in 0..MAX_DRAFT_ROWS_PER_TRANSPORT {
            let key = DraftKey::Conversation(conversation(&format!("f{index:03}")));
            save_draft(&mut store, &key, &draft("body", 0), 1, 1_000 + index as i64).unwrap();
        }
        let evicted_id = conversation("evicted");
        let owner = conversation_owner(&evicted_id).unwrap();
        let data = encode_draft(&draft("evict", 0)).unwrap();
        store
            ._connection
            .execute(
                "INSERT INTO drafts(conversation_id, new_chat_transport, new_chat_address,
                                    transport, revision, deleted, updated_at, data)
                 VALUES (?1, NULL, NULL, 'relay', 7, 0, 500, ?2)",
                params![owner, data],
            )
            .unwrap();
        assert_eq!(row_count(&store), MAX_DRAFT_ROWS_PER_TRANSPORT as i64 + 1);
        let load = load_draft(&mut store, &DraftKey::Conversation(evicted_id), 2_000).unwrap();
        assert_eq!(load.draft, None);
        assert_eq!(load.revision_floor, 7);
        assert!(!load.corrupt_deleted);
        assert_eq!(row_count(&store), MAX_DRAFT_ROWS_PER_TRANSPORT as i64);
    }

    #[test]
    fn malformed_opposite_owner_rows_are_tombstoned_then_repaired() {
        let (_temp, mut store) = setup();
        let conv = DraftKey::Conversation(conversation("malformed-conversation"));
        save_draft(&mut store, &conv, &draft("valid", 0), 3, 1_000).unwrap();
        let owner = conversation_owner(&conversation("malformed-conversation")).unwrap();
        store
            ._connection
            .pragma_update(None, "ignore_check_constraints", "ON")
            .unwrap();
        store
            ._connection
            .execute(
                "UPDATE drafts SET new_chat_transport='relay', new_chat_address='stale@example.test'
                 WHERE conversation_id=?1",
                params![owner],
            )
            .unwrap();
        store
            ._connection
            .execute(
                "UPDATE drafts SET conversation_id='malformed'
                 WHERE new_chat_transport='relay' AND new_chat_address=?1",
                params!["alice@example.test"],
            )
            .unwrap();
        store
            ._connection
            .pragma_update(None, "ignore_check_constraints", "OFF")
            .unwrap();

        let corrupt = load_draft(&mut store, &conv, 1_100).unwrap();
        assert_eq!(corrupt.draft, None);
        assert_eq!(corrupt.revision_floor, 3);
        assert!(corrupt.corrupt_deleted);
        let (deleted, new_chat_transport, new_chat_address): (i64, Option<String>, Option<String>) =
            store
                ._connection
                .query_row(
                    "SELECT deleted, new_chat_transport, new_chat_address FROM drafts
                 WHERE conversation_id=?1",
                    params![owner],
                    |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
                )
                .unwrap();
        assert_eq!(deleted, 1);
        assert_eq!(new_chat_transport, None);
        assert_eq!(new_chat_address, None);
        assert_eq!(
            save_draft(&mut store, &conv, &draft("repaired", 0), 4, 1_200).unwrap(),
            DraftWriteOutcome::Applied
        );
        assert_eq!(
            load_draft(&mut store, &conv, 1_300)
                .unwrap()
                .draft
                .unwrap()
                .body,
            "repaired"
        );

        let chat = new_chat("alice@example.test");
        save_draft(&mut store, &chat, &draft("chat", 0), 2, 1_000).unwrap();
        store
            ._connection
            .pragma_update(None, "ignore_check_constraints", "ON")
            .unwrap();
        store
            ._connection
            .execute(
                "UPDATE drafts SET conversation_id='malformed'
                 WHERE new_chat_transport='relay' AND new_chat_address=?1",
                params!["alice@example.test"],
            )
            .unwrap();
        store
            ._connection
            .pragma_update(None, "ignore_check_constraints", "OFF")
            .unwrap();
        // Saving the exact new-chat key must not treat the opposite-owner row as
        // its revision floor or overwrite that row. The stale new-chat columns are
        // cleared and a legitimate new-chat row is inserted.
        assert_eq!(
            save_draft(&mut store, &chat, &draft("chat repaired", 0), 3, 1_200).unwrap(),
            DraftWriteOutcome::Applied
        );
        assert_eq!(
            load_draft(&mut store, &chat, 1_300)
                .unwrap()
                .draft
                .unwrap()
                .body,
            "chat repaired"
        );
        let (legitimate, stale): (i64, i64) = store
            ._connection
            .query_row(
                "SELECT
                    sum(CASE WHEN conversation_id IS NULL
                              AND new_chat_transport='relay'
                              AND new_chat_address=?1 THEN 1 ELSE 0 END),
                    sum(CASE WHEN conversation_id='malformed'
                              AND new_chat_transport IS NULL
                              AND new_chat_address IS NULL THEN 1 ELSE 0 END)
                 FROM drafts",
                params!["alice@example.test"],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .unwrap();
        assert_eq!((legitimate, stale), (1, 1));
    }

    #[test]
    fn negative_revision_and_active_null_data_are_tombstoned_as_corruption() {
        let (_temp, mut store) = setup();
        let negative = DraftKey::Conversation(conversation("negative-revision"));
        let null_data = DraftKey::Conversation(conversation("null-data"));
        save_draft(&mut store, &negative, &draft("negative", 0), 5, 1_000).unwrap();
        save_draft(&mut store, &null_data, &draft("null", 0), 2, 1_000).unwrap();
        let negative_owner = conversation_owner(&conversation("negative-revision")).unwrap();
        let null_owner = conversation_owner(&conversation("null-data")).unwrap();
        store
            ._connection
            .pragma_update(None, "ignore_check_constraints", "ON")
            .unwrap();
        store
            ._connection
            .execute(
                "UPDATE drafts SET revision=-5 WHERE conversation_id=?1",
                params![negative_owner],
            )
            .unwrap();
        store
            ._connection
            .execute(
                "UPDATE drafts SET data=NULL WHERE conversation_id=?1",
                params![null_owner],
            )
            .unwrap();
        store
            ._connection
            .pragma_update(None, "ignore_check_constraints", "OFF")
            .unwrap();

        let load = load_draft(&mut store, &negative, 1_100).unwrap();
        assert_eq!(load.draft, None);
        assert_eq!(load.revision_floor, 0);
        assert!(load.corrupt_deleted);
        let (revision, deleted, updated_at): (i64, i64, i64) = store
            ._connection
            .query_row(
                "SELECT revision, deleted, updated_at FROM drafts WHERE conversation_id=?1",
                params![negative_owner],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )
            .unwrap();
        assert_eq!((revision, deleted, updated_at), (0, 1, 1_000));

        let load = load_draft(&mut store, &null_data, 1_100).unwrap();
        assert_eq!(load.draft, None);
        assert_eq!(load.revision_floor, 2);
        assert!(load.corrupt_deleted);
        let (revision, deleted, data): (i64, i64, Option<Vec<u8>>) = store
            ._connection
            .query_row(
                "SELECT revision, deleted, data FROM drafts WHERE conversation_id=?1",
                params![null_owner],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )
            .unwrap();
        assert_eq!((revision, deleted, data), (2, 1, None));
    }

    #[test]
    fn corrupt_rows_are_tombstoned_and_a_newer_save_succeeds() {
        let (_temp, mut store) = setup();
        let key = DraftKey::Conversation(conversation("corrupt"));
        assert_eq!(
            save_draft(&mut store, &key, &draft("valid", 0), 4, 1_000).unwrap(),
            DraftWriteOutcome::Applied
        );
        let owner = conversation_owner(&conversation("corrupt")).unwrap();
        store
            ._connection
            .execute(
                "UPDATE drafts SET data=?2 WHERE conversation_id=?1",
                params![owner, b"not json".as_slice()],
            )
            .unwrap();
        let load = load_draft(&mut store, &key, 1_100).unwrap();
        assert_eq!(load.draft, None);
        assert_eq!(load.revision_floor, 4);
        assert!(load.corrupt_deleted);
        assert_eq!(
            save_draft(&mut store, &key, &draft("late", 0), 4, 1_100).unwrap(),
            DraftWriteOutcome::Stale
        );
        assert_eq!(
            save_draft(&mut store, &key, &draft("recovered", 0), 5, 1_200).unwrap(),
            DraftWriteOutcome::Applied
        );
        assert_eq!(
            load_draft(&mut store, &key, 1_300)
                .unwrap()
                .draft
                .unwrap()
                .body,
            "recovered"
        );
    }

    #[test]
    fn invalid_stored_owner_is_tombstoned() {
        let (_temp, mut store) = setup();
        let key = DraftKey::Conversation(conversation("bad-owner"));
        assert_eq!(
            save_draft(&mut store, &key, &draft("valid", 0), 2, 1_000).unwrap(),
            DraftWriteOutcome::Applied
        );
        store
            ._connection
            .execute("UPDATE drafts SET transport='wrong'", [])
            .unwrap();
        let load = load_draft(&mut store, &key, 1_100).unwrap();
        assert_eq!(load.draft, None);
        assert_eq!(load.revision_floor, 2);
        assert!(load.corrupt_deleted);
    }

    #[test]
    fn existing_v1_database_gains_drafts_without_data_loss() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("bloom").join("legacy.db");
        let existing = "legacy conversation sentinel";
        {
            let store = EncryptedStore::open_with_key(&path, &KEY).unwrap();
            store
                ._connection
                .execute(
                    "INSERT INTO conversations(id, created_at, data) VALUES ('legacy', 1, ?1)",
                    params![existing.as_bytes()],
                )
                .unwrap();
            store
                ._connection
                .execute_batch(
                    "DROP INDEX IF EXISTS drafts_conversation_key;
                     DROP INDEX IF EXISTS drafts_new_chat_key;
                     DROP INDEX IF EXISTS drafts_transport_retention;
                     DROP TABLE drafts;",
                )
                .unwrap();
        }
        let mut reopened = EncryptedStore::open_with_key(&path, &KEY).unwrap();
        let stored: Vec<u8> = reopened
            ._connection
            .query_row(
                "SELECT data FROM conversations WHERE id='legacy'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(stored, existing.as_bytes());
        let version: i64 = reopened
            ._connection
            .query_row("SELECT version FROM schema_version", [], |row| row.get(0))
            .unwrap();
        assert_eq!(version, 1);
        let key = DraftKey::Conversation(conversation("after-upgrade"));
        assert_eq!(
            save_draft(&mut reopened, &key, &draft("works", 0), 1, 1_000).unwrap(),
            DraftWriteOutcome::Applied
        );
        assert_eq!(
            load_draft(&mut reopened, &key, 1_100)
                .unwrap()
                .draft
                .unwrap()
                .body,
            "works"
        );
    }
}
