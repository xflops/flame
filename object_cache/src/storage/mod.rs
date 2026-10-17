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

mod disk;
mod none;
mod resident;

pub use disk::{DiskStorage, ObjectFiles};
pub use none::NoStorage;
pub use resident::ResidentStorage;

use async_trait::async_trait;
use bytes::Bytes;
use futures::stream::BoxStream;
#[cfg(test)]
use futures::{future, stream};
use std::path::PathBuf;
use std::sync::Arc;

use common::FlameError;

use crate::cache::{Object, ObjectData, ObjectKey, ObjectMetadata};
use crate::eviction::EvictionConfig;

/// Most data an object can have appended after the data it is created with.
const MAX_APPENDS_PER_OBJECT: u64 = 1000;

/// The record of one part: where its payload is in the object's payload
/// storage, its length and its creation time.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Record {
    pub offset: u64,
    pub len: u64,
    pub creation_time: i64,
}

/// Metadata of a stored object, read without touching its payload. An object
/// is a list of data parts in the order they were written; its version is the
/// number of parts.
#[derive(Debug, Clone)]
pub struct ObjectStat {
    pub data_type: String,
    /// One record per part.
    pub records: Vec<Record>,
}

impl ObjectStat {
    /// The metadata of an object held in memory, whose parts are laid out one
    /// after the other.
    fn of(object: &Object) -> Self {
        debug_assert_eq!(
            object.version, 0,
            "the metadata needs all the data of the object"
        );
        let mut offset = 0;
        let records = object
            .data
            .iter()
            .map(|part| {
                let record = Record {
                    offset,
                    len: part.size_bytes(),
                    creation_time: part.creation_time,
                };
                offset += record.len;
                record
            })
            .collect();
        Self {
            data_type: object.data_type.clone(),
            records,
        }
    }

    /// Current version: one per part, so a new object is version 1.
    pub fn version(&self) -> u64 {
        self.records.len() as u64
    }

    /// Creation time of the object: that of its first part.
    pub fn creation_time(&self) -> i64 {
        self.records[0].creation_time
    }

    /// Payload size of all the parts.
    pub fn size(&self) -> u64 {
        self.records.iter().map(|record| record.len).sum()
    }
}

/// `offset..offset + len` as a range within a payload of `total` bytes.
fn payload_range(
    key: &str,
    offset: u64,
    len: usize,
    total: u64,
) -> Result<std::ops::Range<usize>, FlameError> {
    match offset.checked_add(len as u64) {
        Some(end) if end <= total => Ok(offset as usize..end as usize),
        _ => Err(FlameError::InvalidState(format!(
            "Range {}+{} is outside object <{}> of {} bytes",
            offset, len, key, total
        ))),
    }
}

/// Storage engine trait for ObjectCache.
///
/// Implementations must be thread-safe (Send + Sync).
#[async_trait]
pub trait StorageEngine: Send + Sync + 'static {
    /// Create an object from a stream of chunks, its first part, and commit
    /// it atomically. An object is never replaced: creating one that exists is
    /// an `AlreadyExist` error, and it only changes by `patch_object`. The
    /// object is not stored unless the stream ends successfully.
    ///
    /// `meta` describes the object being written: its data type and creation
    /// time are stored (its version is 1 by construction); `meta.key` is the same key as `key`, and
    /// `endpoint` is ignored. Returns `meta` with `size` set to the number of
    /// payload bytes written.
    ///
    /// Writes to one object (`write_stream`, `patch_object`, `delete_objects`)
    /// must be serialized by the caller.
    async fn write_stream(
        &self,
        key: &ObjectKey,
        meta: ObjectMetadata,
        chunks: BoxStream<'_, Result<Bytes, FlameError>>,
    ) -> Result<ObjectMetadata, FlameError>;

    /// Create an object from its first data.
    #[cfg(test)]
    async fn write_object(&self, key: &ObjectKey, object: &Object) -> Result<(), FlameError> {
        let chunks = stream::once(future::ready(Ok(object.data[0].data.clone())));
        let meta = ObjectMetadata::new()
            .with_key(key.to_key().unwrap_or_default())
            .with_version(1)
            .with_creation_time(object.data[0].creation_time)
            .with_data_type(object.data_type.clone());
        self.write_stream(key, meta, Box::pin(chunks))
            .await
            .map(|_| ())
    }

    /// Read `len` bytes of part `part` of the object (0 is the first) starting
    /// at `offset`. This is the one way payload bytes are read for serving.
    async fn read_range(
        &self,
        key: &ObjectKey,
        part: usize,
        offset: u64,
        len: usize,
    ) -> Result<Bytes, FlameError>;

    /// Open an object's files for reading, for engines that store objects in
    /// files: the handle can be kept to read the object without opening it
    /// again, but is a snapshot that a write makes stale. Returns None if the
    /// object is not found or the engine has no files.
    async fn open(&self, _key: &ObjectKey) -> Result<Option<Arc<ObjectFiles>>, FlameError> {
        Ok(None)
    }

    /// Read an object's metadata. Returns None if not found.
    async fn stat(&self, key: &ObjectKey) -> Result<Option<ObjectStat>, FlameError>;

    #[cfg(test)]
    /// Read an object after `version`: all the parts for 0, else only the
    /// parts after that version. Returns None if not found, and an
    /// `InvalidState` error if `version` is beyond the object's.
    async fn read_object(
        &self,
        key: &ObjectKey,
        version: u64,
    ) -> Result<Option<Object>, FlameError>;

    /// Append data to an existing object. Returns NotFound if the object doesn't exist.
    async fn patch_object(
        &self,
        key: &ObjectKey,
        data: &ObjectData,
    ) -> Result<ObjectMetadata, FlameError>;

    /// Delete all objects matching the key prefix (app/session).
    async fn delete_objects(&self, key: &ObjectKey) -> Result<(), FlameError>;

    /// List all stored objects with their metadata (for startup recovery).
    async fn scan(&self) -> Result<Vec<(ObjectKey, ObjectStat)>, FlameError>;

    /// Record that an object is being read, as a recency hint for engines that
    /// evict.
    fn touch(&self, _key: &ObjectKey) {}

    /// Objects and payload bytes held in memory on top of the engine's own
    /// storage.
    #[cfg(test)]
    fn resident(&self) -> (usize, u64) {
        (0, 0)
    }
}

pub type StorageEnginePtr = Box<dyn StorageEngine>;

/// Open the storage engine named by `url`. The handles of the objects of a
/// persistent engine are also kept open in memory as `eviction` allows. `none`
/// has no store: the in-memory cache is the only copy of its objects and
/// evicted objects are lost.
pub async fn connect(
    url: &str,
    eviction: Option<&EvictionConfig>,
) -> Result<StorageEnginePtr, FlameError> {
    let url = std::env::var("FLAME_CACHE_STORAGE")
        .ok()
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| url.to_string());

    if url.is_empty() {
        tracing::warn!("No cache storage configured - using none storage (cache only)");
        return Ok(Box::new(ResidentStorage::cache_only(eviction)));
    }

    if url == "none" {
        tracing::info!("Cache storage: none (cache only, evicted objects are lost)");
        return Ok(Box::new(ResidentStorage::cache_only(eviction)));
    }

    let storage_path = if url.starts_with("fs://") || url.starts_with("file://") {
        parse_storage_url(&url)?
    } else {
        PathBuf::from(&url)
    };

    tracing::info!("Cache storage: filesystem at {:?}", storage_path);
    Ok(Box::new(ResidentStorage::new(
        Box::new(DiskStorage::new(storage_path)?),
        eviction,
    )))
}

fn parse_storage_url(url: &str) -> Result<PathBuf, FlameError> {
    let path_part = url
        .strip_prefix("fs://")
        .or_else(|| url.strip_prefix("file://"))
        .ok_or_else(|| FlameError::InvalidConfig(format!("Invalid storage URL: {}", url)))?;

    if path_part.starts_with('/') {
        Ok(PathBuf::from(path_part))
    } else {
        let flame_home =
            std::env::var("FLAME_HOME").unwrap_or_else(|_| "/var/lib/flame".to_string());
        Ok(PathBuf::from(flame_home).join(path_part))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_storage_url_absolute() {
        let path = parse_storage_url("fs:///var/lib/flame/cache").unwrap();
        assert_eq!(path, PathBuf::from("/var/lib/flame/cache"));
    }

    #[test]
    fn test_parse_storage_url_relative() {
        let path = parse_storage_url("fs://cache").unwrap();
        assert!(path.ends_with("cache"));
        assert!(path.is_absolute() || path.starts_with("cache"));
    }

    #[test]
    fn test_parse_storage_url_file_scheme() {
        let path = parse_storage_url("file:///tmp/cache").unwrap();
        assert_eq!(path, PathBuf::from("/tmp/cache"));
    }

    #[tokio::test]
    async fn test_connect_none() {
        let storage = connect("none", None).await.unwrap();
        let objects = storage.scan().await.unwrap();
        assert!(objects.is_empty());
    }
}
