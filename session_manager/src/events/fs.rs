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

use std::collections::{hash_map::Entry, HashMap};
use std::fs;

use bincode::{Decode, Encode};
use chrono::{DateTime, Utc};
use stdng::{lock_ptr, new_ptr, MutexPtr};

use common::apis::{Event, EventOwner, SessionID, TaskID};
use common::storage::{DataStorage, Index, Object, ObjectId, ObjectStorage};
use common::FlameError;

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

pub struct FsEventManager {
    storage_path: String,
    event_storage: MutexPtr<HashMap<SessionID, EventStorage>>,
    events: MutexPtr<HashMap<SessionID, HashMap<TaskID, Vec<EventDao>>>>,
}

impl FsEventManager {
    pub fn new(path: &str) -> Result<Self, FlameError> {
        fs::create_dir_all(path)?;

        let mut manager = Self {
            storage_path: path.to_string(),
            event_storage: new_ptr(HashMap::new()),
            events: new_ptr(HashMap::new()),
        };

        let sessions = manager.list_sessions()?;
        for session_id in &sessions {
            manager.setup_event_storage(session_id.clone())?;
        }

        manager.load_events()?;

        Ok(manager)
    }

    fn load_events(&self) -> Result<(), FlameError> {
        let mut event_storage = lock_ptr!(self.event_storage)?;
        let mut events = lock_ptr!(self.events)?;
        let sessions = event_storage.keys().cloned().collect::<Vec<SessionID>>();

        for session_id in &sessions {
            let event_daos: Vec<EventDao> = event_storage
                .get_mut(session_id)
                .ok_or(FlameError::Internal(format!(
                    "Event storage not found: {}",
                    session_id
                )))?
                .object_storage
                .list(None)?;

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

    fn setup_event_storage(&self, session_id: SessionID) -> Result<(), FlameError> {
        let base_path = format!("{}/{}", self.storage_path, session_id);
        let mut event_storage = lock_ptr!(self.event_storage)?;

        if let Entry::Vacant(e) = event_storage.entry(session_id) {
            fs::create_dir_all(&base_path)?;
            let storage = EventStorage {
                object_storage: ObjectStorage::new(&base_path, "events")?,
                data_storage: DataStorage::new(&base_path, "event_messages")?,
            };
            e.insert(storage);
        }
        Ok(())
    }
}

impl EventManager for FsEventManager {
    fn record_event(&self, owner: EventOwner, event: Event) -> Result<(), FlameError> {
        self.setup_event_storage(owner.session_id.clone())?;

        let mut event_storage = lock_ptr!(self.event_storage)?;
        let storage = event_storage
            .get_mut(&owner.session_id)
            .ok_or(FlameError::Internal("Event storage not found".to_string()))?;

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
        let mut event_storage = lock_ptr!(self.event_storage)?;
        let storage = event_storage
            .get_mut(&owner.session_id)
            .ok_or(FlameError::Internal("Event storage not found".to_string()))?;

        let events = lock_ptr!(self.events)?;
        let event_daos = events
            .get(&owner.session_id)
            .and_then(|s| s.get(&owner.task_id));

        let Some(event_daos) = event_daos else {
            return Ok(vec![]);
        };

        let mut event_list = vec![];
        for event_dao in event_daos {
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
        {
            let mut event_storage = lock_ptr!(self.event_storage)?;
            if let Some(storage) = event_storage.get_mut(&session_id) {
                storage.object_storage.clear()?;
                storage.data_storage.clear()?;
            }
        }

        {
            let mut events = lock_ptr!(self.events)?;
            events.remove(&session_id);
        }

        let dir_path = format!("{}/{}", self.storage_path, session_id);
        if std::path::Path::new(&dir_path).exists() {
            fs::remove_dir_all(&dir_path).map_err(|e| {
                FlameError::Storage(format!("Failed to remove event storage directory: {}", e))
            })?;
        }

        Ok(())
    }

    fn clear(&self) -> Result<(), FlameError> {
        let mut event_storage = lock_ptr!(self.event_storage)?;
        for storage in event_storage.values_mut() {
            storage.object_storage.clear()?;
            storage.data_storage.clear()?;
        }

        let mut events = lock_ptr!(self.events)?;
        events.clear();

        Ok(())
    }
}
