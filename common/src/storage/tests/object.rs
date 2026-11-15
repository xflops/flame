/*
Copyright 2025 The xflops Authors.
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

use super::*;
use chrono::Utc;
use serde_derive::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
struct TestObject {
    #[serde(with = "postcard::fixint::le")]
    id: ObjectId,
    #[serde(with = "postcard::fixint::le")]
    state: u64,
    #[serde(with = "postcard::fixint::le")]
    created_at: u64,
    #[serde(with = "postcard::fixint::le")]
    updated_at: u64,
    #[serde(with = "postcard::fixint::le")]
    owner: ObjectId,
}

impl Object for TestObject {
    fn id(&self) -> ObjectId {
        self.id
    }

    fn set_id(&mut self, id: ObjectId) {
        self.id = id;
    }

    fn owner(&self) -> ObjectId {
        self.owner
    }
}

#[test]
fn test_save_object() {
    let tmp_dir = tempfile::tempdir().unwrap();
    let mut storage =
        ObjectStorage::new(tmp_dir.path().to_string_lossy().as_ref(), "test").unwrap();

    let now = Utc::now().timestamp() as u64;

    let object = TestObject {
        id: 0,
        state: 0,
        created_at: now,
        updated_at: 0,
        owner: 1,
    };
    let obj = storage.save(&object).unwrap();

    assert_eq!(obj.id(), 1);
    assert_eq!(obj.state, 0);
    assert_eq!(obj.created_at, now);
    assert_eq!(obj.owner, 1);

    let obj = storage.load::<TestObject>(1).unwrap();
    assert_eq!(obj.id(), 1);
    assert_eq!(obj.state, 0);
    assert_eq!(obj.created_at, now);
    assert_eq!(obj.owner, 1);

    let obj = storage.save(&object).unwrap();

    assert_eq!(obj.id(), 2);
    assert_eq!(obj.state, 0);
    assert_eq!(obj.created_at, now);
    assert_eq!(obj.owner, 1);

    storage.clear().unwrap();
}

#[test]
fn test_update_object() {
    let tmp_dir = tempfile::tempdir().unwrap();
    let mut storage =
        ObjectStorage::new(tmp_dir.path().to_string_lossy().as_ref(), "test_update").unwrap();

    let now = Utc::now().timestamp() as u64;
    let updated_at = now + 1;
    let object = TestObject {
        id: 0,
        state: 0,
        created_at: now,
        updated_at: 0,
        owner: 1,
    };
    let obj = storage.save(&object).unwrap();

    assert_eq!(obj.id(), 1);
    assert_eq!(obj.state, 0);
    assert_eq!(obj.created_at, now);
    assert_eq!(obj.owner, 1);

    let mut obj = storage.load::<TestObject>(1).unwrap();
    assert_eq!(obj.id(), 1);
    assert_eq!(obj.state, 0);
    assert_eq!(obj.created_at, now);
    assert_eq!(obj.owner, 1);

    obj.state = 2;
    obj.updated_at = updated_at;
    let obj = storage.update(&obj).unwrap();
    assert_eq!(obj.id(), 1);
    assert_eq!(obj.state, 2);
    assert_eq!(obj.created_at, now);
    assert_eq!(obj.updated_at, updated_at);
    assert_eq!(obj.owner, 1);

    let obj = storage.load::<TestObject>(1).unwrap();
    assert_eq!(obj.id(), 1);
    assert_eq!(obj.state, 2);
    assert_eq!(obj.created_at, now);
    assert_eq!(obj.updated_at, updated_at);
    assert_eq!(obj.owner, 1);

    storage.clear().unwrap();
}

#[test]
fn test_list_object() {
    let tmp_dir = tempfile::tempdir().unwrap();
    let mut storage =
        ObjectStorage::new(tmp_dir.path().to_string_lossy().as_ref(), "test_list").unwrap();

    let now = Utc::now().timestamp() as u64;
    let updated_at = now + 1;

    let object1 = TestObject {
        id: 0,
        state: 0,
        created_at: now,
        updated_at: 0,
        owner: 1,
    };
    let obj1 = storage.save(&object1).unwrap();

    assert_eq!(obj1.id(), 1);
    assert_eq!(obj1.state, 0);
    assert_eq!(obj1.created_at, now);
    assert_eq!(obj1.owner, 1);

    let object2 = TestObject {
        id: 0,
        state: 1,
        created_at: now + 1,
        updated_at: 0,
        owner: 1,
    };
    let mut obj2 = storage.save(&object2).unwrap();

    assert_eq!(obj2.id(), 2);
    assert_eq!(obj2.state, 1);
    assert_eq!(obj2.created_at, now + 1);
    assert_eq!(obj2.owner, 1);

    let object3 = TestObject {
        id: 0,
        state: 2,
        created_at: now + 2,
        updated_at: 0,
        owner: 1,
    };
    let obj3 = storage.save(&object3).unwrap();

    assert_eq!(obj3.id(), 3);
    assert_eq!(obj3.state, 2);
    assert_eq!(obj3.created_at, now + 2);
    assert_eq!(obj3.owner, 1);

    obj2.state = 4;
    obj2.updated_at = updated_at + 1;
    let obj2 = storage.update(&obj2).unwrap();
    assert_eq!(obj2.id(), 2);
    assert_eq!(obj2.state, 4);
    assert_eq!(obj2.created_at, now + 1);
    assert_eq!(obj2.updated_at, updated_at + 1);
    assert_eq!(obj2.owner, 1);

    let objects: Vec<TestObject> = storage.list(Some(Filter { owner: 1 })).unwrap();
    assert_eq!(objects.len(), 3);
    assert_eq!(objects[0].id(), 1);
    assert_eq!(objects[0].state, 0);
    assert_eq!(objects[0].created_at, now);
    assert_eq!(objects[0].owner, 1);
    assert_eq!(objects[1].id(), 2);
    assert_eq!(objects[1].state, 4);
    assert_eq!(objects[1].created_at, now + 1);
    assert_eq!(objects[1].updated_at, updated_at + 1);
    assert_eq!(objects[1].owner, 1);
    assert_eq!(objects[2].id(), 3);
    assert_eq!(objects[2].state, 2);
    assert_eq!(objects[2].created_at, now + 2);
    assert_eq!(objects[2].updated_at, 0);
    assert_eq!(objects[2].owner, 1);

    storage.clear().unwrap();
}
