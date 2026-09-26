use super::{EncryptedStore, StoreError};
use bloom_domain::{Conversation, ConversationId};
use rusqlite::{params, OptionalExtension};

fn stable_id(id: &ConversationId) -> Result<String, StoreError> {
    serde_json::to_string(id).map_err(|_| StoreError::Schema)
}

pub fn upsert_conversation(
    store: &mut EncryptedStore,
    conversation: &Conversation,
) -> Result<(), StoreError> {
    let id = stable_id(&conversation.id)?;
    let data = serde_json::to_vec(conversation).map_err(|_| StoreError::Schema)?;
    let tx = store._connection.transaction()?;
    tx.execute(
        "INSERT INTO conversations(id, created_at, last_message_at, data) VALUES (?1, ?2, ?3, ?4)
         ON CONFLICT(id) DO UPDATE SET created_at=excluded.created_at, last_message_at=excluded.last_message_at, data=excluded.data",
        params![id, conversation.created_at, conversation.last_message_at, data],
    )?;
    tx.commit()?;
    Ok(())
}

pub fn list_conversations(store: &EncryptedStore) -> Result<Vec<Conversation>, StoreError> {
    let mut statement = store
        ._connection
        .prepare("SELECT data FROM conversations ORDER BY last_message_at DESC, id ASC")?;
    let records = statement.query_map([], |row| row.get::<_, Vec<u8>>(0))?;
    let mut conversations = Vec::new();
    for record in records {
        let data = record?;
        conversations.push(serde_json::from_slice(&data).map_err(|_| StoreError::Schema)?);
    }
    Ok(conversations)
}

pub fn get_conversation(
    store: &EncryptedStore,
    id: &ConversationId,
) -> Result<Option<Conversation>, StoreError> {
    let key = stable_id(id)?;
    let data: Option<Vec<u8>> = store
        ._connection
        .query_row(
            "SELECT data FROM conversations WHERE id=?1",
            params![key],
            |row| row.get(0),
        )
        .optional()?;
    data.map(|data| serde_json::from_slice(&data).map_err(|_| StoreError::Schema))
        .transpose()
}

#[cfg(test)]
mod tests {
    use super::*;
    use bloom_domain::{ConversationKind, TransportId};
    use std::{fs, path::PathBuf};

    fn conversation(
        transport: &str,
        value: &str,
        title: &str,
        last_message_at: Option<i64>,
    ) -> Conversation {
        Conversation {
            id: ConversationId::new(TransportId::new(transport).unwrap(), value).unwrap(),
            remote_id: format!("remote-{transport}-{value}"),
            kind: ConversationKind::Direct,
            title: Some(title.to_owned()),
            participants: vec![],
            created_at: 1,
            last_message_at,
        }
    }

    fn database_path() -> (tempfile::TempDir, PathBuf) {
        let owner = tempfile::tempdir().unwrap();
        let private = owner.path().join("private-child");
        fs::create_dir(&private).unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(&private, fs::Permissions::from_mode(0o700)).unwrap();
        }
        let path = private.join("store.db");
        (owner, path)
    }

    #[test]
    fn conversation_crud_scopes_ids_orders_and_encrypts() {
        let (_owner, path) = database_path();
        let mut store = EncryptedStore::open_with_key(&path, &[41; 32]).unwrap();
        let alpha = conversation("alpha", "same-local-id", "synthetic-alpha-title", Some(20));
        let beta = conversation("beta", "same-local-id", "synthetic-beta-title", Some(20));
        let older = conversation("alpha", "older", "synthetic-older-title", Some(10));
        let missing_time = conversation("alpha", "missing-time", "synthetic-missing-title", None);

        upsert_conversation(&mut store, &alpha).unwrap();
        upsert_conversation(&mut store, &beta).unwrap();
        upsert_conversation(&mut store, &older).unwrap();
        upsert_conversation(&mut store, &missing_time).unwrap();
        assert_eq!(
            get_conversation(&store, &alpha.id).unwrap(),
            Some(alpha.clone())
        );
        assert_eq!(
            get_conversation(&store, &beta.id).unwrap(),
            Some(beta.clone())
        );
        let nonexistent = conversation("alpha", "does-not-exist", "synthetic-unused", None);
        assert_eq!(get_conversation(&store, &nonexistent.id).unwrap(), None);

        let updated = conversation(
            "alpha",
            "same-local-id",
            "synthetic-updated-title",
            Some(30),
        );
        upsert_conversation(&mut store, &updated).unwrap();
        upsert_conversation(&mut store, &updated).unwrap();
        assert_eq!(
            get_conversation(&store, &updated.id).unwrap(),
            Some(updated.clone())
        );
        assert_eq!(
            list_conversations(&store).unwrap(),
            vec![updated, beta, older, missing_time]
        );

        let title = b"synthetic-updated-title";
        for suffix in ["", "-wal"] {
            let mut filename = path.as_os_str().to_os_string();
            filename.push(suffix);
            if let Ok(bytes) = fs::read(PathBuf::from(filename)) {
                assert!(!bytes.windows(title.len()).any(|window| window == title));
            }
        }
    }
}
