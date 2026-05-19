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

use std::sync::Arc;

use common::apis::{Event, EventOwner, SessionID};
use common::FlameError;

mod fs;
mod memory;

pub use fs::FsEventManager;
pub use memory::MemoryEventManager;

pub trait EventManager: Send + Sync {
    fn record_event(&self, owner: EventOwner, event: Event) -> Result<(), FlameError>;
    fn find_events(&self, owner: EventOwner) -> Result<Vec<Event>, FlameError>;
    fn remove_events(&self, session_id: SessionID) -> Result<(), FlameError>;
    fn clear(&self) -> Result<(), FlameError>;
}

pub type EventManagerPtr = Arc<dyn EventManager>;

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Utc;

    fn test_event_manager_impl(manager: &dyn EventManager) {
        manager
            .record_event(
                EventOwner {
                    session_id: String::from("1"),
                    task_id: 1,
                },
                Event {
                    code: 1,
                    message: Some("test".to_string()),
                    creation_time: Utc::now(),
                },
            )
            .unwrap();

        let events = manager
            .find_events(EventOwner {
                session_id: String::from("1"),
                task_id: 1,
            })
            .unwrap();

        assert_eq!(events.len(), 1);
        assert_eq!(events[0].code, 1);
        assert_eq!(events[0].message, Some("test".to_string()));

        manager.clear().unwrap();
    }

    #[test]
    fn test_memory_event_manager() {
        let manager = MemoryEventManager::new();
        test_event_manager_impl(&manager);
    }

    #[test]
    fn test_fs_event_manager() {
        let temp_dir = tempfile::tempdir().unwrap();
        let manager = FsEventManager::new(temp_dir.path().to_str().unwrap()).unwrap();
        test_event_manager_impl(&manager);
    }

    #[test]
    fn test_fs_event_manager_multiple_events_same_task() {
        let temp_dir = tempfile::tempdir().unwrap();
        let manager = FsEventManager::new(temp_dir.path().to_str().unwrap()).unwrap();

        let owner = EventOwner {
            session_id: "session-1".to_string(),
            task_id: 1,
        };

        for i in 0..3 {
            manager
                .record_event(
                    owner.clone(),
                    Event {
                        code: i,
                        message: Some(format!("event-{}", i)),
                        creation_time: Utc::now(),
                    },
                )
                .unwrap();
        }

        let events = manager.find_events(owner).unwrap();
        assert_eq!(events.len(), 3);
        assert_eq!(events[0].code, 0);
        assert_eq!(events[1].code, 1);
        assert_eq!(events[2].code, 2);
    }

    #[test]
    fn test_fs_event_manager_multiple_tasks() {
        let temp_dir = tempfile::tempdir().unwrap();
        let manager = FsEventManager::new(temp_dir.path().to_str().unwrap()).unwrap();

        let session_id = "session-1".to_string();

        for task_id in 1..=3 {
            manager
                .record_event(
                    EventOwner {
                        session_id: session_id.clone(),
                        task_id,
                    },
                    Event {
                        code: task_id as i32,
                        message: Some(format!("task-{}", task_id)),
                        creation_time: Utc::now(),
                    },
                )
                .unwrap();
        }

        for task_id in 1..=3 {
            let events = manager
                .find_events(EventOwner {
                    session_id: session_id.clone(),
                    task_id,
                })
                .unwrap();
            assert_eq!(events.len(), 1);
            assert_eq!(events[0].code, task_id as i32);
        }
    }

    #[test]
    fn test_fs_event_manager_multiple_sessions() {
        let temp_dir = tempfile::tempdir().unwrap();
        let manager = FsEventManager::new(temp_dir.path().to_str().unwrap()).unwrap();

        for i in 1..=3 {
            manager
                .record_event(
                    EventOwner {
                        session_id: format!("session-{}", i),
                        task_id: 1,
                    },
                    Event {
                        code: i,
                        message: Some(format!("session-{}-event", i)),
                        creation_time: Utc::now(),
                    },
                )
                .unwrap();
        }

        for i in 1..=3 {
            let events = manager
                .find_events(EventOwner {
                    session_id: format!("session-{}", i),
                    task_id: 1,
                })
                .unwrap();
            assert_eq!(events.len(), 1);
            assert_eq!(events[0].code, i);
        }
    }

    #[test]
    fn test_fs_event_manager_remove_events() {
        let temp_dir = tempfile::tempdir().unwrap();
        let manager = FsEventManager::new(temp_dir.path().to_str().unwrap()).unwrap();

        let session_id = "session-to-remove".to_string();
        let owner = EventOwner {
            session_id: session_id.clone(),
            task_id: 1,
        };

        manager
            .record_event(
                owner.clone(),
                Event {
                    code: 1,
                    message: Some("test".to_string()),
                    creation_time: Utc::now(),
                },
            )
            .unwrap();

        let events = manager.find_events(owner.clone()).unwrap();
        assert_eq!(events.len(), 1);

        manager.remove_events(session_id.clone()).unwrap();

        let result = manager.find_events(owner);
        assert!(result.unwrap().is_empty());
    }

    #[test]
    fn test_fs_event_manager_find_nonexistent_session() {
        let temp_dir = tempfile::tempdir().unwrap();
        let manager = FsEventManager::new(temp_dir.path().to_str().unwrap()).unwrap();

        let events = manager
            .find_events(EventOwner {
                session_id: "missing-session".to_string(),
                task_id: 0,
            })
            .unwrap();
        assert!(events.is_empty());
    }

    #[test]
    fn test_fs_event_manager_find_nonexistent_task() {
        let temp_dir = tempfile::tempdir().unwrap();
        let manager = FsEventManager::new(temp_dir.path().to_str().unwrap()).unwrap();

        manager
            .record_event(
                EventOwner {
                    session_id: "session-1".to_string(),
                    task_id: 1,
                },
                Event {
                    code: 1,
                    message: Some("test".to_string()),
                    creation_time: Utc::now(),
                },
            )
            .unwrap();

        let events = manager
            .find_events(EventOwner {
                session_id: "session-1".to_string(),
                task_id: 999,
            })
            .unwrap();
        assert!(events.is_empty());
    }

    #[test]
    fn test_fs_event_manager_persistence() {
        let temp_dir = tempfile::tempdir().unwrap();
        let path = temp_dir.path().to_str().unwrap().to_string();

        let owner = EventOwner {
            session_id: "persistent-session".to_string(),
            task_id: 1,
        };

        {
            let manager = FsEventManager::new(&path).unwrap();
            manager
                .record_event(
                    owner.clone(),
                    Event {
                        code: 42,
                        message: Some("persistent event".to_string()),
                        creation_time: Utc::now(),
                    },
                )
                .unwrap();
        }

        {
            let manager = FsEventManager::new(&path).unwrap();
            let events = manager.find_events(owner).unwrap();
            assert_eq!(events.len(), 1);
            assert_eq!(events[0].code, 42);
            assert_eq!(events[0].message, Some("persistent event".to_string()));
        }
    }

    #[test]
    fn test_fs_event_manager_with_flame_test_dir() {
        let temp_dir = tempfile::tempdir().unwrap();
        let temp_path = temp_dir.path().to_str().unwrap();

        std::env::set_var("FLAME_TEST_DIR", temp_path);

        let events_path = format!("{}/events", temp_path);
        let manager = FsEventManager::new(&events_path).unwrap();

        let owner = EventOwner {
            session_id: "test-session".to_string(),
            task_id: 1,
        };

        manager
            .record_event(
                owner.clone(),
                Event {
                    code: 1,
                    message: Some("test with FLAME_TEST_DIR".to_string()),
                    creation_time: Utc::now(),
                },
            )
            .unwrap();

        let events = manager.find_events(owner).unwrap();
        assert_eq!(events.len(), 1);

        std::env::remove_var("FLAME_TEST_DIR");
    }

    #[test]
    fn test_fs_event_manager_empty_message() {
        let temp_dir = tempfile::tempdir().unwrap();
        let manager = FsEventManager::new(temp_dir.path().to_str().unwrap()).unwrap();

        let owner = EventOwner {
            session_id: "session-1".to_string(),
            task_id: 1,
        };

        manager
            .record_event(
                owner.clone(),
                Event {
                    code: 1,
                    message: None,
                    creation_time: Utc::now(),
                },
            )
            .unwrap();

        let events = manager.find_events(owner).unwrap();
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].message, Some("".to_string()));
    }
}
