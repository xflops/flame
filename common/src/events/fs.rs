/*
Copyright 2025 The Flame Authors.
Licensed under the Apache License, Version 2.0 (the "License");
you may not use this file except in compliance with the License.
You may obtain a copy of the License at
    http://www.apache.org/licenses/LICENSE-2.0
Unless required by applicable law or agreed to in writing, software
distributed under the License is distributed on an "AS IS" BASIS,
WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
See the License for the specific language governing permissions and
limitations under the License.
*/

use std::collections::HashMap;
use std::fs;

use bincode::{Decode, Encode};
use chrono::{DateTime, Utc};
use stdng::{lock_ptr, new_ptr, MutexPtr};

use crate::apis::{Event, EventOwner, SessionID, TaskID};
use crate::storage::{DataStorage, Index, Object, ObjectId, ObjectStorage};
use crate::FlameError;

use super::EventManager;

struct EventStorage {
    object_storage: ObjectStorage,
    data_storage: DataStorage,
}

#[derive(Clone, Debug, Encode, Decode)]
struct EventDao {
    id: Option<u64>,
    owner: TaskID,
    code: i32,
    message: Index,
    creation_time: i64,
}

impl Object for EventDao {
    fn id(&self) -> ObjectId {
        self.id.unwrap_or(0)
    }

    fn owner(&self) -> ObjectId {
        self.owner as ObjectId
    }

    fn set_id(&mut self, id: ObjectId) {
        self.id = Some(id);
    }
}

/// File-backed event records. Runtime callers coordinate recording and cleanup
/// through Storage's session lifecycle; this manager owns event files only.
pub struct FsEventManager {
    storage_path: String,
    /// Registry of per-session entries. Held only to look up, insert or remove an
    /// entry, never across file I/O, so one session's I/O cannot block another's.
    event_storage: MutexPtr<HashMap<SessionID, MutexPtr<EventStorage>>>,
    /// Committed event records. Held briefly to clone or publish, never across I/O.
    events: MutexPtr<HashMap<SessionID, HashMap<TaskID, Vec<EventDao>>>>,
}

impl FsEventManager {
    pub fn new(path: &str) -> Result<Self, FlameError> {
        fs::create_dir_all(path)?;

        let manager = Self {
            storage_path: path.to_string(),
            event_storage: new_ptr(HashMap::new()),
            events: new_ptr(HashMap::new()),
        };

        let sessions = manager.list_sessions()?;
        for session_id in &sessions {
            manager.setup_event_storage(session_id)?;
        }

        manager.load_events()?;

        Ok(manager)
    }

    fn load_events(&self) -> Result<(), FlameError> {
        let sessions = {
            let registry = lock_ptr!(self.event_storage)?;
            registry
                .iter()
                .map(|(id, entry)| (id.clone(), entry.clone()))
                .collect::<Vec<_>>()
        };

        for (session_id, entry) in sessions {
            let mut storage = lock_ptr!(entry)?;
            let event_daos: Vec<EventDao> = storage.object_storage.list(None)?;

            let mut events = lock_ptr!(self.events)?;
            for event_dao in event_daos {
                events
                    .entry(session_id.clone())
                    .or_default()
                    .entry(event_dao.owner as TaskID)
                    .or_default()
                    .push(event_dao);
            }
        }
        Ok(())
    }

    fn list_sessions(&self) -> Result<Vec<SessionID>, FlameError> {
        let mut sessions = vec![];
        let entries = fs::read_dir(&self.storage_path)?;
        for entry in entries {
            let file_name = entry?.file_name();
            let session_id = file_name.to_string_lossy().to_string();
            sessions.push(session_id);
        }
        Ok(sessions)
    }

    /// The session's entry; the registry lock is released before it is returned.
    fn session_entry(&self, session_id: &SessionID) -> Result<MutexPtr<EventStorage>, FlameError> {
        lock_ptr!(self.event_storage)?
            .get(session_id)
            .cloned()
            .ok_or_else(|| FlameError::NotFound(format!("Event storage not found: {session_id}")))
    }

    fn setup_event_storage(&self, session_id: &SessionID) -> Result<(), FlameError> {
        if lock_ptr!(self.event_storage)?.contains_key(session_id) {
            return Ok(());
        }
        // Open outside the registry lock; creation of one session is serialized
        // by Storage, so a concurrent duplicate open is not expected.
        let base_path = format!("{}/{}", self.storage_path, session_id);
        fs::create_dir_all(&base_path)?;
        let storage = EventStorage {
            object_storage: ObjectStorage::new(&base_path, "events")?,
            data_storage: DataStorage::new(&base_path, "event_messages")?,
        };
        lock_ptr!(self.event_storage)?
            .entry(session_id.clone())
            .or_insert_with(|| new_ptr(storage));
        Ok(())
    }
}

impl EventManager for FsEventManager {
    fn setup_events(&self, session_id: SessionID) -> Result<(), FlameError> {
        self.setup_event_storage(&session_id)
    }

    fn record_event(&self, owner: EventOwner, event: Event) -> Result<(), FlameError> {
        let entry = self.session_entry(&owner.session_id)?;
        let mut storage = lock_ptr!(entry)?;

        let message = event.message.unwrap_or_default();
        let msg_index = storage.data_storage.save(message.as_bytes())?;

        let event_dao = EventDao {
            id: None,
            owner: owner.task_id,
            code: event.code,
            message: msg_index,
            creation_time: event.creation_time.timestamp_millis(),
        };

        storage.object_storage.save(&event_dao)?;

        let mut events = lock_ptr!(self.events)?;
        events
            .entry(owner.session_id)
            .or_default()
            .entry(owner.task_id)
            .or_default()
            .push(event_dao);

        Ok(())
    }

    fn find_events(&self, owner: EventOwner) -> Result<Vec<Event>, FlameError> {
        let entry = {
            let registry = lock_ptr!(self.event_storage)?;
            let Some(entry) = registry.get(&owner.session_id) else {
                return Ok(vec![]);
            };
            entry.clone()
        };
        let mut storage = lock_ptr!(entry)?;

        let event_daos = {
            let events = lock_ptr!(self.events)?;
            events
                .get(&owner.session_id)
                .and_then(|s| s.get(&owner.task_id))
                .cloned()
                .unwrap_or_default()
        };

        let mut event_list = vec![];
        for event_dao in &event_daos {
            let message = storage.data_storage.load(&event_dao.message)?;
            event_list.push(Event {
                code: event_dao.code,
                message: Some(String::from_utf8(message)?),
                creation_time: DateTime::<Utc>::from_timestamp_millis(event_dao.creation_time)
                    .ok_or(FlameError::Internal("Invalid creation time".to_string()))?,
            });
        }

        Ok(event_list)
    }

    fn remove_events(&self, session_id: SessionID) -> Result<(), FlameError> {
        // The entry owns files only. Storage serializes event recording and
        // cleanup with its cached session state during close and deletion.
        let entry = lock_ptr!(self.event_storage)?.get(&session_id).cloned();
        {
            // Hold the entry so an in-flight writer finishes before removal. Its
            // file handles stay open until the entry is dropped with the session.
            let _storage = match entry.as_ref() {
                Some(entry) => Some(lock_ptr!(entry)?),
                None => None,
            };

            let dir_path = format!("{}/{}", self.storage_path, session_id);
            if std::path::Path::new(&dir_path).exists() {
                fs::remove_dir_all(&dir_path).map_err(|e| {
                    FlameError::Storage(format!("Failed to remove event storage directory: {}", e))
                })?;
            }

            lock_ptr!(self.events)?.remove(&session_id);
        }
        if let Some(entry) = entry {
            let mut registry = lock_ptr!(self.event_storage)?;
            if registry
                .get(&session_id)
                .is_some_and(|current| std::sync::Arc::ptr_eq(current, &entry))
            {
                registry.remove(&session_id);
            }
        }
        Ok(())
    }

    fn clear(&self) -> Result<(), FlameError> {
        let sessions = {
            let registry = lock_ptr!(self.event_storage)?;
            registry.values().cloned().collect::<Vec<_>>()
        };
        for entry in sessions {
            let mut storage = lock_ptr!(entry)?;
            storage.object_storage.clear()?;
            storage.data_storage.clear()?;
        }

        let mut events = lock_ptr!(self.events)?;
        events.clear();

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{mpsc, Arc};
    use std::time::Duration;

    fn owner(session: &str, task: TaskID) -> EventOwner {
        EventOwner {
            session_id: session.to_string(),
            task_id: task,
        }
    }

    fn event(message: &str) -> Event {
        Event {
            code: 1,
            message: Some(message.to_string()),
            creation_time: Utc::now(),
        }
    }

    #[test]
    fn one_sessions_entry_does_not_block_another_session() {
        let dir = tempfile::tempdir().unwrap();
        let manager = Arc::new(FsEventManager::new(dir.path().to_str().unwrap()).unwrap());

        manager.setup_events("a".to_string()).unwrap();
        manager.setup_events("b".to_string()).unwrap();

        // Hold session "a" as a long event I/O would.
        let held = manager.session_entry(&"a".to_string()).unwrap();
        let _guard = lock_ptr!(held).unwrap();

        let (done, finished) = mpsc::channel();
        let other = manager.clone();
        std::thread::spawn(move || {
            other.record_event(owner("b", 1), event("other")).unwrap();
            let found = other.find_events(owner("b", 1)).unwrap();
            done.send(found.len()).unwrap();
        });

        assert_eq!(
            finished
                .recv_timeout(Duration::from_secs(2))
                .expect("session b waited for session a"),
            1
        );
    }

    #[test]
    fn removed_session_rejects_later_events() {
        let dir = tempfile::tempdir().unwrap();
        let manager = FsEventManager::new(dir.path().to_str().unwrap()).unwrap();
        manager.setup_events("s".to_string()).unwrap();
        manager.record_event(owner("s", 1), event("first")).unwrap();
        manager.remove_events("s".to_string()).unwrap();
        assert!(!lock_ptr!(manager.event_storage).unwrap().contains_key("s"));
        assert!(!dir.path().join("s").exists());
        assert!(manager.find_events(owner("s", 1)).unwrap().is_empty());

        // Storage lives until removal; a late writer must not recreate it.
        let error = manager
            .record_event(owner("s", 1), event("late"))
            .unwrap_err();
        assert!(matches!(error, FlameError::NotFound(_)));
        assert!(!dir.path().join("s").exists());
    }

    #[test]
    fn setup_events_is_idempotent() {
        let dir = tempfile::tempdir().unwrap();
        let manager = FsEventManager::new(dir.path().to_str().unwrap()).unwrap();
        manager.setup_events("s".to_string()).unwrap();
        manager.record_event(owner("s", 1), event("first")).unwrap();
        manager.setup_events("s".to_string()).unwrap();
        assert_eq!(manager.find_events(owner("s", 1)).unwrap().len(), 1);
    }

    #[test]
    fn removal_without_events_does_not_retain_a_registry_entry() {
        let dir = tempfile::tempdir().unwrap();
        let manager = FsEventManager::new(dir.path().to_str().unwrap()).unwrap();
        manager.remove_events("no-events".to_string()).unwrap();
        assert!(lock_ptr!(manager.event_storage).unwrap().is_empty());
    }

    #[test]
    fn removal_tolerates_already_deleted_event_files() {
        let dir = tempfile::tempdir().unwrap();
        let manager = FsEventManager::new(dir.path().to_str().unwrap()).unwrap();
        manager.setup_events("s".to_string()).unwrap();
        manager.record_event(owner("s", 1), event("first")).unwrap();
        fs::remove_file(dir.path().join("s/events.dat")).unwrap();

        manager.remove_events("s".to_string()).unwrap();
        manager.remove_events("s".to_string()).unwrap();
        assert!(!dir.path().join("s").exists());
        assert!(manager.find_events(owner("s", 1)).unwrap().is_empty());
    }

    #[test]
    fn clear_keeps_storage_open_for_the_next_event() {
        let dir = tempfile::tempdir().unwrap();
        let manager = FsEventManager::new(dir.path().to_str().unwrap()).unwrap();
        manager.setup_events("s".to_string()).unwrap();
        manager
            .record_event(owner("s", 1), event("before"))
            .unwrap();
        manager.clear().unwrap();
        assert!(manager.find_events(owner("s", 1)).unwrap().is_empty());

        manager.record_event(owner("s", 1), event("after")).unwrap();
        let found = manager.find_events(owner("s", 1)).unwrap();
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].message.as_deref(), Some("after"));
    }

    #[test]
    fn events_survive_reopen() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().to_str().unwrap();
        {
            let manager = FsEventManager::new(path).unwrap();
            manager.setup_events("s".to_string()).unwrap();
            manager.record_event(owner("s", 7), event("one")).unwrap();
            manager.record_event(owner("s", 7), event("two")).unwrap();
        }
        let reopened = FsEventManager::new(path).unwrap();
        let found = reopened.find_events(owner("s", 7)).unwrap();
        let messages: Vec<_> = found.iter().map(|e| e.message.clone().unwrap()).collect();
        assert_eq!(messages, vec!["one", "two"]);
    }
}
