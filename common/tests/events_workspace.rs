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

use chrono::Utc;
use common::apis::{Event, EventOwner, SessionGID};
use common::events::{EventManager, FsEventManager, MemoryEventManager};
use std::sync::Arc;

fn event(code: i32) -> Event {
    Event {
        code,
        message: Some(format!("event {code}")),
        creation_time: Utc::now(),
    }
}

#[test]
fn memory_scopes_by_workspace_session_and_task() {
    let manager = MemoryEventManager::new();
    let first = EventOwner::task("alpha".into(), "session".into(), "1".into());
    let other_session = EventOwner::task("alpha".into(), "other".into(), "1".into());
    let other_workspace = EventOwner::task("beta".into(), "session".into(), "1".into());
    let session_event = EventOwner::session("alpha".into(), "session".into());
    manager.record_event(first.clone(), event(1)).unwrap();
    manager
        .record_event(other_session.clone(), event(4))
        .unwrap();
    manager
        .record_event(other_workspace.clone(), event(2))
        .unwrap();
    manager
        .record_event(session_event.clone(), event(3))
        .unwrap();
    assert_eq!(manager.find_events(first.clone()).unwrap()[0].code, 1);
    assert_eq!(
        manager.find_events(other_session.clone()).unwrap()[0].code,
        4
    );
    assert_eq!(
        manager.find_events(other_workspace.clone()).unwrap()[0].code,
        2
    );
    assert_eq!(
        manager.find_events(session_event.clone()).unwrap()[0].code,
        3
    );
    manager
        .remove_events(&SessionGID::new("alpha", "session"))
        .unwrap();
    assert!(manager.find_events(first).unwrap().is_empty());
    assert!(manager.find_events(session_event).unwrap().is_empty());
    assert_eq!(manager.find_events(other_session).unwrap()[0].code, 4);
    assert_eq!(manager.find_events(other_workspace).unwrap()[0].code, 2);
}

#[test]
fn filesystem_recovers_workspace_scoped_events() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().to_string_lossy().to_string();
    let owner = EventOwner::task("alpha".into(), "session".into(), "1".into());
    let other_session = EventOwner::task("alpha".into(), "other".into(), "1".into());
    let other_workspace = EventOwner::task("beta".into(), "session".into(), "1".into());
    let manager = FsEventManager::new(&path).unwrap();
    manager.record_event(owner.clone(), event(7)).unwrap();
    manager
        .record_event(other_session.clone(), event(8))
        .unwrap();
    manager
        .record_event(other_workspace.clone(), event(9))
        .unwrap();
    let recovered = FsEventManager::new(&path).unwrap();
    assert_eq!(recovered.find_events(owner.clone()).unwrap()[0].code, 7);
    recovered
        .remove_events(&SessionGID::new("alpha", "session"))
        .unwrap();
    assert!(recovered.find_events(owner).unwrap().is_empty());
    assert_eq!(recovered.find_events(other_session).unwrap()[0].code, 8);
    assert_eq!(recovered.find_events(other_workspace).unwrap()[0].code, 9);
}

#[test]
fn filesystem_recovers_empty_event_directory() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(dir.path().join("alpha").join("session")).unwrap();
    let manager = FsEventManager::new(&dir.path().to_string_lossy()).unwrap();
    let owner = EventOwner::session("alpha".into(), "session".into());
    assert!(manager.find_events(owner).unwrap().is_empty());
}

#[test]
fn filesystem_serializes_concurrent_event_appends() {
    let dir = tempfile::tempdir().unwrap();
    let manager = Arc::new(FsEventManager::new(&dir.path().to_string_lossy()).unwrap());
    let owner = EventOwner::task("alpha".into(), "session".into(), "1".into());
    let workers: Vec<_> = (0..12)
        .map(|code| {
            let manager = manager.clone();
            let owner = owner.clone();
            std::thread::spawn(move || manager.record_event(owner, event(code)).unwrap())
        })
        .collect();
    for worker in workers {
        worker.join().unwrap();
    }
    let recovered = FsEventManager::new(&dir.path().to_string_lossy()).unwrap();
    assert_eq!(recovered.find_events(owner).unwrap().len(), 12);
}
