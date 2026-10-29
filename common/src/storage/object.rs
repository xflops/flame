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

use std::collections::HashMap;
use std::fs::{self, File, OpenOptions};
use std::io::prelude::*;
use std::io::SeekFrom;
use std::path::PathBuf;

use serde::de::DeserializeOwned;
use serde_derive::Deserialize;
use serde_derive::Serialize;
use stdng::trace_fn;

use crate::FlameError;

pub type ObjectId = u64;

pub trait Object {
    fn id(&self) -> ObjectId;
    fn set_id(&mut self, id: ObjectId);
    fn owner(&self) -> ObjectId;
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct Metadata {
    size: usize,
}

pub struct Filter {
    owner: ObjectId,
}

pub struct ObjectStorage {
    metadata: Metadata,
    metadata_file: File,
    metadata_file_path: PathBuf,

    owners: HashMap<ObjectId, Vec<ObjectId>>,
    owners_file: File,
    owners_file_path: PathBuf,

    data: File,
    data_file_path: PathBuf,
}

impl ObjectStorage {
    pub fn new(path: &str, name: &str) -> Result<Self, FlameError> {
        trace_fn!("ObjectStorage::new");

        let metadata_file_path = PathBuf::from(format!("{}/{}.json", path, name));
        let owners_file_path = PathBuf::from(format!("{}/{}.csv", path, name));
        let data_file_path = PathBuf::from(format!("{}/{}.dat", path, name));

        let data = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(&data_file_path)?;

        let owners_file = OpenOptions::new()
            .read(true)
            .append(true)
            .create(true)
            .truncate(false)
            .open(&owners_file_path)?;

        let metadata_file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(&metadata_file_path)?;

        let owners = HashMap::new();
        let mut storage = ObjectStorage {
            metadata: Metadata { size: 0 },
            metadata_file,
            metadata_file_path,
            owners,
            owners_file,
            owners_file_path,
            data,
            data_file_path,
        };

        storage.reload()?;

        Ok(storage)
    }

    pub fn save<T: Object + serde::Serialize + Clone>(
        &mut self,
        object: &T,
    ) -> Result<T, FlameError> {
        trace_fn!("ObjectStorage::save");

        let mut obj = object.clone();
        obj.set_id(self.next_id()?);

        let data = postcard::to_allocvec(&obj).map_err(|e| FlameError::Internal(e.to_string()))?;

        tracing::debug!("save data: {:?}", data);

        if self.metadata.size == 0 {
            self.metadata.size = data.len();
            let metadata_str = serde_json::to_string(&self.metadata)
                .map_err(|e| FlameError::Internal(e.to_string()))?;
            self.metadata_file.write_all(metadata_str.as_bytes())?;
            self.metadata_file.flush()?;
        }

        self.write_data(obj.id(), &data)?;

        self.update_owners(obj.id(), obj.owner())?;

        Ok(obj)
    }

    pub fn update<T: Object + serde::Serialize + Clone>(
        &mut self,
        object: &T,
    ) -> Result<T, FlameError> {
        trace_fn!("ObjectStorage::update");
        let data =
            postcard::to_allocvec(object).map_err(|e| FlameError::Internal(e.to_string()))?;

        tracing::debug!("update data: {:?}", data);

        self.write_data(object.id(), &data)?;

        Ok(object.clone())
    }

    pub fn load<T: Object + DeserializeOwned>(&mut self, id: ObjectId) -> Result<T, FlameError> {
        trace_fn!("ObjectStorage::load");

        self.seek(id)?;

        let mut object = vec![0u8; self.metadata.size];
        self.data.read_exact(&mut object)?;

        tracing::debug!("load data: {:?}", object);

        let (object, _) =
            postcard::take_from_bytes(&object).map_err(|e| FlameError::Internal(e.to_string()))?;

        Ok(object)
    }

    pub fn list<T: Object + DeserializeOwned>(
        &mut self,
        filter: Option<Filter>,
    ) -> Result<Vec<T>, FlameError> {
        let mut objects = vec![];
        let ids = if let Some(filter) = &filter {
            self.owners.get(&filter.owner).cloned().unwrap_or(vec![])
        } else {
            self.owners.values().flatten().cloned().collect()
        };

        for id in ids {
            let object = self.load::<T>(id)?;
            objects.push(object);
        }

        Ok(objects)
    }

    pub fn clear(&mut self) -> Result<(), FlameError> {
        fs::remove_file(&self.data_file_path).map_err(|e| {
            FlameError::Storage(format!(
                "failed to clear object storage data at <{}>: {}",
                self.data_file_path.to_string_lossy(),
                e
            ))
        })?;
        fs::remove_file(&self.owners_file_path).map_err(|e| {
            FlameError::Storage(format!(
                "failed to clear object storage owners at <{}>: {}",
                self.owners_file_path.to_string_lossy(),
                e
            ))
        })?;
        fs::remove_file(&self.metadata_file_path).map_err(|e| {
            FlameError::Storage(format!(
                "failed to clear object storage metadata at <{}>: {}",
                self.metadata_file_path.to_string_lossy(),
                e
            ))
        })?;

        Ok(())
    }

    fn update_owners(&mut self, id: ObjectId, owner: ObjectId) -> Result<(), FlameError> {
        writeln!(self.owners_file, "{id},{owner}")?;

        self.owners.entry(owner).or_default().push(id);

        Ok(())
    }

    fn load_owners(&mut self) -> Result<(), FlameError> {
        let mut owners = HashMap::new();
        for line in fs::read_to_string(&self.owners_file_path)?.lines() {
            let pairs = line.split(',').collect::<Vec<&str>>();

            let id = pairs[0]
                .parse::<ObjectId>()
                .map_err(|e| FlameError::Internal(e.to_string()))?;
            let owner = pairs[1]
                .parse::<ObjectId>()
                .map_err(|e| FlameError::Internal(e.to_string()))?;

            owners.entry(owner).or_insert(vec![]).push(id);
        }
        self.owners = owners;

        Ok(())
    }

    fn write_data(&mut self, id: ObjectId, data: &[u8]) -> Result<(), FlameError> {
        self.check_data_size(data.len())?;
        self.seek(id)?;

        tracing::debug!("write data to offset: {:?}", self.data.stream_position()?);

        self.data.write_all(data)?;
        self.data.flush()?;

        Ok(())
    }

    fn check_data_size(&self, data_size: usize) -> Result<(), FlameError> {
        if data_size != self.metadata.size {
            return Err(FlameError::InvalidState(format!(
                "data size mismatch: {} != {}",
                data_size, self.metadata.size
            )));
        }

        Ok(())
    }

    fn reload(&mut self) -> Result<(), FlameError> {
        self.load_metadata()?;
        self.load_owners()?;

        Ok(())
    }

    fn load_metadata(&mut self) -> Result<(), FlameError> {
        let metadata_str = fs::read_to_string(&self.metadata_file_path)?;
        if metadata_str.is_empty() {
            self.metadata.size = 0;
            return Ok(());
        }

        self.metadata =
            serde_json::from_str(&metadata_str).map_err(|e| FlameError::Internal(e.to_string()))?;
        Ok(())
    }

    fn next_id(&mut self) -> Result<ObjectId, FlameError> {
        if self.metadata.size == 0 {
            Ok(1)
        } else {
            let len = self.data.metadata()?.len();
            Ok(len / self.metadata.size as u64 + 1)
        }
    }

    fn seek(&mut self, id: ObjectId) -> Result<(), FlameError> {
        let offset = (id - 1) * self.metadata.size as u64;
        self.data.seek(SeekFrom::Start(offset))?;

        tracing::debug!("seek to offset: {}", offset);

        Ok(())
    }

    pub fn debug(&self) -> Result<(), FlameError> {
        tracing::debug!("metadata: {:?}", self.metadata);
        tracing::debug!("owners: {:?}", self.owners);

        let data = fs::read(self.data_file_path.to_string_lossy().as_ref())?;
        tracing::debug!("data: {:?}", data);

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Utc;

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
}
