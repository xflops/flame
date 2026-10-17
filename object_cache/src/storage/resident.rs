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
use std::sync::Arc;

use async_trait::async_trait;
use bytes::Bytes;
use futures::stream::BoxStream;
use futures::TryStreamExt;
use stdng::{lock_ptr, new_ptr, MutexPtr};

use common::FlameError;

use crate::cache::{Object, ObjectData, ObjectKey, ObjectMetadata};
use crate::eviction::{new_policy, EvictionConfig, EvictionPolicyPtr};

use super::{payload_range, NoStorage, ObjectFiles, ObjectStat, StorageEngine, StorageEnginePtr};

/// Batch size for eviction operations.
const EVICTION_BATCH_SIZE: usize = 10;

/// Keeps the open files of the objects of a file engine in memory: for each
/// object its key, its metadata and index, and the open `index.bin` and
/// `data.bin`, so serving an object does not open and close files per read.
/// No payload bytes are kept. A write to an object drops its handle (it is a
/// snapshot); the next read opens the object again. The eviction policy bounds
/// the handles, which are only a cache: an evicted one is opened again.
///
/// Over `NoStorage` (`storage: none`) there is no engine to read back from, so
/// the cache is the only copy: every object with all its parts is
/// resident whatever its size, and an evicted object is lost.
pub struct ResidentStorage {
    inner: StorageEnginePtr,
    /// There is no engine behind the cache: it holds every object with all its parts.
    cache_only: bool,
    /// The objects with all their parts; used when `cache_only`.
    objects: MutexPtr<HashMap<String, Arc<Object>>>,
    /// The open files of the objects of the engine; used when not `cache_only`.
    handles: MutexPtr<HashMap<String, Arc<ObjectFiles>>>,
    policy: EvictionPolicyPtr,
}

fn key_string(key: &ObjectKey) -> Result<String, FlameError> {
    key.to_key()
        .ok_or_else(|| FlameError::Internal("object_id required".to_string()))
}

fn joined<T>(result: Result<T, tokio::task::JoinError>) -> Result<T, FlameError> {
    result.map_err(|e| FlameError::Internal(format!("Task join error: {}", e)))
}

impl ResidentStorage {
    /// A cache of the open files of the objects of `inner`.
    pub fn new(inner: StorageEnginePtr, eviction: Option<&EvictionConfig>) -> Self {
        Self::with_engine(inner, false, eviction)
    }

    /// A cache with no store behind it: objects stay in memory with all their parts.
    pub fn cache_only(eviction: Option<&EvictionConfig>) -> Self {
        Self::with_engine(Box::new(NoStorage::new()), true, eviction)
    }

    fn with_engine(
        inner: StorageEnginePtr,
        cache_only: bool,
        eviction: Option<&EvictionConfig>,
    ) -> Self {
        Self {
            inner,
            cache_only,
            objects: new_ptr(HashMap::new()),
            handles: new_ptr(HashMap::new()),
            policy: new_policy(eviction),
        }
    }

    fn resident_of(&self, key: &str) -> Result<Option<Arc<Object>>, FlameError> {
        let objects = lock_ptr!(self.objects)?;
        Ok(objects.get(key).cloned())
    }

    fn run_eviction(&self) -> Result<(), FlameError> {
        loop {
            let victims = self.policy.victims(EVICTION_BATCH_SIZE);
            if victims.is_empty() {
                break;
            }

            for key in victims {
                let removed = {
                    let mut objects = lock_ptr!(self.objects)?;
                    let mut handles = lock_ptr!(self.handles)?;
                    objects.remove(&key).is_some() | handles.remove(&key).is_some()
                };
                if removed {
                    self.policy.on_evict(&key);
                    tracing::debug!("Evicted object from memory: {}", key);
                }
            }
        }
        Ok(())
    }

    /// Make `object` the resident copy of `key`, replacing a resident one.
    fn set_resident(&self, key: &str, object: Arc<Object>) -> Result<(), FlameError> {
        let size = object.size_bytes();
        let existing = {
            let mut objects = lock_ptr!(self.objects)?;
            objects.insert(key.to_string(), object).is_some()
        };
        self.account(key, size, existing)
    }

    fn account(&self, key: &str, size: u64, existing: bool) -> Result<(), FlameError> {
        if existing {
            self.policy.on_update(key, size);
        } else {
            self.policy.on_add(key, size);
        }
        self.run_eviction()
    }

    /// The open files of an object of the engine: the cached handle, or the
    /// object opened and kept. None if the engine has no such object or no
    /// files.
    async fn files(&self, key: &ObjectKey) -> Result<Option<Arc<ObjectFiles>>, FlameError> {
        let key_str = key_string(key)?;
        {
            let handles = lock_ptr!(self.handles)?;
            if let Some(files) = handles.get(&key_str) {
                return Ok(Some(Arc::clone(files)));
            }
        }
        let Some(files) = self.inner.open(key).await? else {
            return Ok(None);
        };
        let existing = {
            let mut handles = lock_ptr!(self.handles)?;
            handles
                .insert(key_str.clone(), Arc::clone(&files))
                .is_some()
        };
        self.account(&key_str, files.memory_size(), existing)?;
        tracing::debug!("Object opened: {}", key_str);
        Ok(Some(files))
    }
}

#[async_trait]
impl StorageEngine for ResidentStorage {
    async fn write_stream(
        &self,
        key: &ObjectKey,
        meta: ObjectMetadata,
        chunks: BoxStream<'_, Result<Bytes, FlameError>>,
    ) -> Result<ObjectMetadata, FlameError> {
        let key_str = key_string(key)?;
        if !self.cache_only {
            return self.inner.write_stream(key, meta, chunks).await;
        }

        if self.resident_of(&key_str)?.is_some() {
            return Err(FlameError::AlreadyExist(format!(
                "object <{}> already exists",
                key_str
            )));
        }
        // With no engine the stream itself is the object: keep it as it passes.
        let mut data = Vec::new();
        let meta = {
            let chunks = chunks.inspect_ok(|chunk| data.extend_from_slice(chunk));
            self.inner.write_stream(key, meta, Box::pin(chunks)).await?
        };
        let first = ObjectData::new()
            .with_creation_time(meta.creation_time)
            .with_data(data);
        let object = Object {
            version: 0,
            data_type: meta.data_type.clone(),
            data: vec![first],
        };
        self.set_resident(&key_str, Arc::new(object))?;
        Ok(meta)
    }

    async fn read_range(
        &self,
        key: &ObjectKey,
        part: usize,
        offset: u64,
        len: usize,
    ) -> Result<Bytes, FlameError> {
        let key_str = key_string(key)?;
        if self.cache_only {
            let object = self
                .resident_of(&key_str)?
                .ok_or_else(|| FlameError::NotFound(format!("object <{}> not found", key_str)))?;
            let data = &object
                .data
                .get(part)
                .ok_or_else(|| {
                    FlameError::NotFound(format!("part {} of object <{}> not found", part, key_str))
                })?
                .data;
            let range = payload_range(&key_str, offset, len, data.len() as u64)?;
            return Ok(data.slice(range));
        }
        match self.files(key).await? {
            Some(files) => joined(
                tokio::task::spawn_blocking(move || files.read_range(&key_str, part, offset, len))
                    .await,
            )?,
            None => self.inner.read_range(key, part, offset, len).await,
        }
    }

    async fn stat(&self, key: &ObjectKey) -> Result<Option<ObjectStat>, FlameError> {
        if self.cache_only {
            return Ok(self
                .resident_of(&key_string(key)?)?
                .map(|object| ObjectStat::of(&object)));
        }
        match self.files(key).await? {
            Some(files) => Ok(Some(files.stat()?)),
            None => self.inner.stat(key).await,
        }
    }

    #[cfg(test)]
    async fn read_object(
        &self,
        key: &ObjectKey,
        version: u64,
    ) -> Result<Option<Object>, FlameError> {
        if !self.cache_only {
            return self.inner.read_object(key, version).await;
        }
        let Some(object) = self.resident_of(&key_string(key)?)? else {
            return Ok(None);
        };
        object.since(version).map(Some).ok_or_else(|| {
            FlameError::InvalidState(format!("version {} is beyond object {:?}", version, key))
        })
    }

    async fn patch_object(
        &self,
        key: &ObjectKey,
        data: &ObjectData,
    ) -> Result<ObjectMetadata, FlameError> {
        let key_str = key_string(key)?;
        if self.cache_only {
            let metadata = self.inner.patch_object(key, data).await?;
            if let Some(object) = self.resident_of(&key_str)? {
                // The object is replaced, so readers of the old one keep a
                // consistent snapshot.
                let mut appended = (*object).clone();
                appended.data.push(data.clone());
                self.set_resident(&key_str, Arc::new(appended))?;
            }
            return Ok(metadata);
        }

        // The part goes through the open handle, which stays valid.
        let Some(files) = self.files(key).await? else {
            return self.inner.patch_object(key, data).await;
        };
        let appended = data.clone();
        let metadata = {
            let files = Arc::clone(&files);
            joined(tokio::task::spawn_blocking(move || files.append(&key_str, &appended)).await)??
        };
        self.account(&metadata.key, files.memory_size(), true)?;
        Ok(metadata)
    }

    async fn delete_objects(&self, key: &ObjectKey) -> Result<(), FlameError> {
        self.inner.delete_objects(key).await?;
        let removed: Vec<String> = {
            let mut objects = lock_ptr!(self.objects)?;
            let mut handles = lock_ptr!(self.handles)?;
            let removed = objects
                .keys()
                .chain(handles.keys())
                .filter(|stored| key.matches(stored))
                .cloned()
                .collect();
            objects.retain(|stored, _| !key.matches(stored));
            handles.retain(|stored, _| !key.matches(stored));
            removed
        };
        for stored in &removed {
            self.policy.on_remove(stored);
        }
        Ok(())
    }

    async fn scan(&self) -> Result<Vec<(ObjectKey, ObjectStat)>, FlameError> {
        self.inner.scan().await
    }

    fn touch(&self, key: &ObjectKey) {
        if let Some(key) = key.to_key() {
            self.policy.on_access(&key);
        }
    }

    #[cfg(test)]
    fn resident(&self) -> (usize, u64) {
        let objects = lock_ptr!(self.objects).unwrap();
        let handles = lock_ptr!(self.handles).unwrap();
        (
            objects.len() + handles.len(),
            objects
                .values()
                .map(|object| object.size_bytes())
                .sum::<u64>()
                + handles
                    .values()
                    .map(|files| files.memory_size())
                    .sum::<u64>(),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::storage::DiskStorage;
    use std::path::Path;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use tempfile::tempdir;

    const MIB: usize = 1024 * 1024;

    fn eviction(max_objects: usize) -> EvictionConfig {
        EvictionConfig {
            policy: Some("lru".to_string()),
            max_memory: Some("100M".to_string()),
            max_objects: Some(max_objects),
        }
    }

    fn resident_storage(path: &Path, max_objects: usize) -> ResidentStorage {
        ResidentStorage::new(
            Box::new(DiskStorage::new(path.to_path_buf()).unwrap()),
            Some(&eviction(max_objects)),
        )
    }

    /// `count` chunks of 1 MiB, produced lazily; chunk `i` is filled with `i`.
    fn chunks(count: usize) -> BoxStream<'static, Result<Bytes, FlameError>> {
        Box::pin(futures::stream::iter(
            (0..count).map(|index| Ok(Bytes::from(vec![index as u8; MIB]))),
        ))
    }

    fn object_key(name: &str) -> ObjectKey {
        ObjectKey::from_path("app/session")
            .unwrap()
            .with_object_id(name.to_string())
            .unwrap()
    }

    fn head(name: &str, version: u64) -> ObjectMetadata {
        ObjectMetadata::new()
            .with_key(object_key(name).to_key().unwrap())
            .with_version(version)
    }

    async fn put(storage: &ResidentStorage, name: &str, version: u64, mib: usize) {
        storage
            .write_stream(&object_key(name), head(name, version), chunks(mib))
            .await
            .unwrap();
    }

    async fn append(storage: &ResidentStorage, name: &str, data: &[u8]) {
        storage
            .patch_object(
                &object_key(name),
                &ObjectData::new().with_data(data.to_vec()),
            )
            .await
            .unwrap();
    }

    async fn read(
        storage: &ResidentStorage,
        name: &str,
        part: usize,
        offset: u64,
        len: usize,
    ) -> Vec<u8> {
        storage
            .read_range(&object_key(name), part, offset, len)
            .await
            .unwrap()
            .to_vec()
    }

    /// Delegates to disk storage and counts how often an object is opened.
    struct CountingStorage {
        inner: DiskStorage,
        opens: Arc<AtomicUsize>,
    }

    #[async_trait]
    impl StorageEngine for CountingStorage {
        async fn write_stream(
            &self,
            key: &ObjectKey,
            meta: ObjectMetadata,
            chunks: BoxStream<'_, Result<Bytes, FlameError>>,
        ) -> Result<ObjectMetadata, FlameError> {
            self.inner.write_stream(key, meta, chunks).await
        }

        async fn read_range(
            &self,
            key: &ObjectKey,
            part: usize,
            offset: u64,
            len: usize,
        ) -> Result<Bytes, FlameError> {
            self.inner.read_range(key, part, offset, len).await
        }

        async fn open(&self, key: &ObjectKey) -> Result<Option<Arc<ObjectFiles>>, FlameError> {
            self.opens.fetch_add(1, Ordering::SeqCst);
            self.inner.open(key).await
        }

        async fn stat(&self, key: &ObjectKey) -> Result<Option<ObjectStat>, FlameError> {
            self.inner.stat(key).await
        }

        async fn read_object(
            &self,
            key: &ObjectKey,
            version: u64,
        ) -> Result<Option<Object>, FlameError> {
            self.inner.read_object(key, version).await
        }

        async fn patch_object(
            &self,
            key: &ObjectKey,
            data: &ObjectData,
        ) -> Result<ObjectMetadata, FlameError> {
            self.inner.patch_object(key, data).await
        }

        async fn delete_objects(&self, key: &ObjectKey) -> Result<(), FlameError> {
            self.inner.delete_objects(key).await
        }

        async fn scan(&self) -> Result<Vec<(ObjectKey, ObjectStat)>, FlameError> {
            self.inner.scan().await
        }
    }

    #[tokio::test]
    async fn an_object_is_opened_once_for_many_reads() {
        let directory = tempdir().unwrap();
        let opens = Arc::new(AtomicUsize::new(0));
        let storage = ResidentStorage::new(
            Box::new(CountingStorage {
                inner: DiskStorage::new(directory.path().to_path_buf()).unwrap(),
                opens: Arc::clone(&opens),
            }),
            Some(&eviction(10)),
        );
        put(&storage, "big", 1, 4).await;
        // Writing does not open the object.
        assert_eq!(opens.load(Ordering::SeqCst), 0);

        let key = object_key("big");
        for chunk in 0..4 {
            let data = read(&storage, "big", 0, (chunk * MIB) as u64, 2).await;
            assert_eq!(data, [chunk as u8; 2]);
        }
        assert_eq!(
            storage.stat(&key).await.unwrap().unwrap().size(),
            (4 * MIB) as u64
        );
        assert_eq!(opens.load(Ordering::SeqCst), 1);
        // No payload bytes are resident, only the handle.
        let (count, bytes) = storage.resident();
        assert_eq!(count, 1);
        assert!(bytes < 1024);
    }

    #[tokio::test]
    async fn an_append_updates_the_open_handle() {
        let directory = tempdir().unwrap();
        let storage = resident_storage(directory.path(), 10);
        let key = object_key("appended");
        put(&storage, "appended", 1, 1).await;
        assert_eq!(storage.stat(&key).await.unwrap().unwrap().version(), 1);
        assert_eq!(storage.resident().0, 1);

        // The append goes through the open handle, which stays resident.
        append(&storage, "appended", b"second").await;
        assert_eq!(storage.resident().0, 1);

        let stat = storage.stat(&key).await.unwrap().unwrap();
        assert_eq!((stat.version(), stat.size()), (2, (MIB + 6) as u64));
        // Part 0 is the first data, part 1 the data appended.
        assert_eq!(read(&storage, "appended", 1, 1, 4).await, b"econ");
        assert_eq!(read(&storage, "appended", 0, 0, 2).await, [0, 0]);
        assert!(storage.read_range(&key, 2, 0, 1).await.is_err());
        assert_eq!(storage.resident().0, 1);
    }

    #[tokio::test]
    async fn the_object_cannot_be_replaced() {
        let directory = tempdir().unwrap();
        let storage = resident_storage(directory.path(), 10);
        put(&storage, "a", 1, 1).await;
        assert_eq!(read(&storage, "a", 0, 0, 1).await, [0]);

        let again = storage
            .write_stream(&object_key("a"), head("a", 2), chunks(2))
            .await;
        assert!(matches!(again, Err(FlameError::AlreadyExist(_))));
        let stat = storage.stat(&object_key("a")).await.unwrap().unwrap();
        assert_eq!((stat.version(), stat.size()), (1, MIB as u64));

        // A deleted key can be created again, and reads see the new object.
        storage.delete_objects(&object_key("a")).await.unwrap();
        put(&storage, "a", 3, 2).await;
        let stat = storage.stat(&object_key("a")).await.unwrap().unwrap();
        assert_eq!((stat.version(), stat.size()), (1, (2 * MIB) as u64));
    }

    #[tokio::test]
    async fn failed_write_stores_nothing() {
        let directory = tempdir().unwrap();
        let storage = resident_storage(directory.path(), 10);
        let failing = futures::stream::iter(vec![
            Ok(Bytes::from_static(b"new")),
            Err(FlameError::Network("lost".to_string())),
        ]);
        let key = object_key("a");
        assert!(storage
            .write_stream(&key, head("a", 1), Box::pin(failing))
            .await
            .is_err());
        assert!(storage.stat(&key).await.unwrap().is_none());
        assert_eq!(storage.resident().0, 0);
    }

    #[tokio::test]
    async fn handles_stay_within_the_object_limit() {
        let directory = tempdir().unwrap();
        let writer = resident_storage(directory.path(), 10);
        for name in ["a", "b", "c", "d"] {
            put(&writer, name, 1, 1).await;
        }

        // A new process starts with no handles; reads open objects on demand.
        let storage = resident_storage(directory.path(), 2);
        assert_eq!(storage.scan().await.unwrap().len(), 4);
        assert_eq!(storage.resident().0, 0);
        for name in ["a", "b", "c", "d"] {
            assert_eq!(read(&storage, name, 0, 0, 1).await, [0]);
            assert!(storage.resident().0 <= 2);
        }
        // An evicted object is opened again.
        assert_eq!(read(&storage, "a", 0, 0, 1).await, [0]);
    }

    #[tokio::test]
    async fn delete_drops_handles() {
        let directory = tempdir().unwrap();
        let storage = resident_storage(directory.path(), 10);
        for name in ["a", "b"] {
            put(&storage, name, 1, 1).await;
            read(&storage, name, 0, 0, 1).await;
        }
        assert_eq!(storage.resident().0, 2);

        storage.delete_objects(&object_key("a")).await.unwrap();
        assert_eq!(storage.resident().0, 1);
        assert!(storage.read_range(&object_key("a"), 0, 0, 1).await.is_err());
        storage
            .delete_objects(&ObjectKey::from_path("app/session").unwrap())
            .await
            .unwrap();
        assert_eq!(storage.resident().0, 0);
        assert!(storage.stat(&object_key("b")).await.unwrap().is_none());
    }

    fn cache_only(max_memory: &str) -> ResidentStorage {
        ResidentStorage::cache_only(Some(&EvictionConfig {
            policy: Some("lru".to_string()),
            max_memory: Some(max_memory.to_string()),
            max_objects: None,
        }))
    }

    #[tokio::test]
    async fn cache_only_keeps_whole_objects_with_appended_data() {
        let storage = cache_only("100M");
        let key = object_key("big");
        put(&storage, "big", 1, 9).await;

        // Not limited to a first chunk.
        assert_eq!(storage.resident(), (1, (9 * MIB) as u64));
        assert_eq!(
            read(&storage, "big", 0, (3 * MIB - 1) as u64, 2).await,
            [2, 3]
        );

        let before = storage.read_object(&key, 0).await.unwrap().unwrap();
        append(&storage, "big", b"second").await;
        assert_eq!(storage.resident(), (1, (9 * MIB + 6) as u64));
        let after = storage.read_object(&key, 0).await.unwrap().unwrap();
        // A reader of the old object keeps its snapshot, and the first data is shared.
        assert_eq!((before.current_version(), after.current_version()), (1, 2));
        assert_eq!(before.data[0].data.as_ptr(), after.data[0].data.as_ptr());
        let stat = storage.stat(&key).await.unwrap().unwrap();
        assert_eq!(stat.version(), 2);
        assert_eq!(read(&storage, "big", 1, 0, 6).await, b"second");
    }

    #[tokio::test]
    async fn cache_only_eviction_loses_the_object() {
        let storage = cache_only("4M");
        put(&storage, "a", 1, 3).await;
        put(&storage, "b", 1, 3).await;

        assert!(storage.stat(&object_key("a")).await.unwrap().is_none());
        assert!(storage.read_range(&object_key("a"), 0, 0, 1).await.is_err());
        assert!(storage.scan().await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn cache_only_rejects_an_existing_object() {
        let storage = cache_only("100M");
        let key = object_key("a");
        put(&storage, "a", 1, 2).await;

        let again = storage.write_stream(&key, head("a", 2), chunks(1)).await;
        assert!(matches!(again, Err(FlameError::AlreadyExist(_))));
        let stat = storage.stat(&key).await.unwrap().unwrap();
        assert_eq!((stat.version(), stat.size()), (1, (2 * MIB) as u64));

        let broken = futures::stream::iter([Err(FlameError::Internal("broken".to_string()))]);
        assert!(storage
            .write_stream(&object_key("b"), head("b", 1), Box::pin(broken))
            .await
            .is_err());
        assert!(storage.stat(&object_key("b")).await.unwrap().is_none());
    }
}
