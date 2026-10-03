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
use std::fs::{self, OpenOptions};
use std::io::{BufRead, BufReader, Write};
use std::path::PathBuf;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use stdng::{lock_ptr, new_ptr, MutexPtr};

use crate::apis::{Event, EventOwner, SessionGID};
use crate::FlameError;

use super::EventManager;

#[derive(Clone, Serialize, Deserialize)]
struct EventRecord {
    task: Option<String>,
    code: i32,
    message: Option<String>,
    creation_time: i64,
}

type OwnerEvents = HashMap<Option<String>, Vec<EventRecord>>;
type Events = HashMap<SessionGID, OwnerEvents>;

pub struct FsEventManager {
    storage_path: PathBuf,
    events: MutexPtr<Events>,
}

impl FsEventManager {
    pub fn new(path: &str) -> Result<Self, FlameError> {
        let storage_path = PathBuf::from(path);
        fs::create_dir_all(&storage_path)?;
        let mut events = HashMap::new();
        for workspace_entry in fs::read_dir(&storage_path)? {
            let workspace_entry = workspace_entry?;
            if !workspace_entry.file_type()?.is_dir() {
                return Err(FlameError::Storage(format!(
                    "unexpected file in event storage: {}",
                    workspace_entry.path().display()
                )));
            }
            let workspace = workspace_entry.file_name().to_string_lossy().to_string();
            for session_entry in fs::read_dir(workspace_entry.path())? {
                let session_entry = session_entry?;
                if !session_entry.file_type()?.is_dir() {
                    return Err(FlameError::Storage(format!(
                        "invalid event storage layout: {}",
                        session_entry.path().display()
                    )));
                }
                let session = session_entry.file_name().to_string_lossy().to_string();
                let log_path = session_entry.path().join("events.jsonl");
                let mut owners = OwnerEvents::new();
                if log_path.exists() {
                    for line in BufReader::new(fs::File::open(log_path)?).lines() {
                        let record: EventRecord = serde_json::from_str(&line?).map_err(|e| {
                            FlameError::Storage(format!("invalid event record: {e}"))
                        })?;
                        owners.entry(record.task.clone()).or_default().push(record);
                    }
                }
                events.insert(SessionGID::new(workspace.clone(), session), owners);
            }
        }
        Ok(Self {
            storage_path,
            events: new_ptr(events),
        })
    }

    fn event_path(&self, session: &SessionGID) -> PathBuf {
        self.storage_path
            .join(&session.workspace)
            .join(&session.session)
            .join("events.jsonl")
    }
}

impl EventManager for FsEventManager {
    fn record_event(&self, owner: EventOwner, event: Event) -> Result<(), FlameError> {
        let session = SessionGID::new(&owner.workspace, &owner.session);
        let path = self.event_path(&session);
        let record = EventRecord {
            task: owner.task.clone(),
            code: event.code,
            message: event.message,
            creation_time: event.creation_time.timestamp_millis(),
        };
        // Serialize appends and removals for this manager.
        let mut events = lock_ptr!(self.events)?;
        fs::create_dir_all(path.parent().expect("event path has parent"))?;
        let mut file = OpenOptions::new().append(true).create(true).open(path)?;
        serde_json::to_writer(&mut file, &record)
            .map_err(|e| FlameError::Storage(format!("failed to serialize event: {e}")))?;
        file.write_all(b"\n")?;
        file.sync_data()?;
        events
            .entry(SessionGID::new(owner.workspace, owner.session))
            .or_default()
            .entry(owner.task)
            .or_default()
            .push(record);
        Ok(())
    }

    fn find_events(&self, owner: EventOwner) -> Result<Vec<Event>, FlameError> {
        lock_ptr!(self.events)?
            .get(&SessionGID::new(owner.workspace, owner.session))
            .and_then(|owners| owners.get(&owner.task))
            .into_iter()
            .flatten()
            .map(|record| {
                Ok(Event {
                    code: record.code,
                    message: record.message.clone(),
                    creation_time: DateTime::<Utc>::from_timestamp_millis(record.creation_time)
                        .ok_or_else(|| FlameError::Storage("invalid event timestamp".into()))?,
                })
            })
            .collect()
    }

    fn remove_events(&self, session: &SessionGID) -> Result<(), FlameError> {
        let mut events = lock_ptr!(self.events)?;
        let path = self
            .storage_path
            .join(&session.workspace)
            .join(&session.session);
        if path.exists() {
            fs::remove_dir_all(path)?;
        }
        events.remove(session);
        Ok(())
    }

    fn clear(&self) -> Result<(), FlameError> {
        let mut events = lock_ptr!(self.events)?;
        for entry in fs::read_dir(&self.storage_path)? {
            let entry = entry?;
            if entry.file_type()?.is_dir() {
                fs::remove_dir_all(entry.path())?;
            }
        }
        events.clear();
        Ok(())
    }
}
