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

pub use disk::DiskStorage;
pub use none::NoneStorage;

use std::path::PathBuf;

use async_trait::async_trait;

use common::FlameError;

use crate::cache::{Object, ObjectKey, ObjectMetadata};

/// Storage engine trait for ObjectCache.
///
/// Implementations must be thread-safe (Send + Sync).
#[async_trait]
pub trait StorageEngine: Send + Sync + 'static {
    /// Write an object to persistent storage. Clears any existing deltas.
    async fn write_object(&self, key: &ObjectKey, object: &Object) -> Result<(), FlameError>;

    /// Read an object with all deltas. Returns None if not found.
    async fn read_object(&self, key: &ObjectKey) -> Result<Option<Object>, FlameError>;

    /// Append a delta to an existing object. Returns NotFound if base doesn't exist.
    async fn patch_object(
        &self,
        key: &ObjectKey,
        delta: &Object,
    ) -> Result<ObjectMetadata, FlameError>;

    /// Delete all objects matching the key prefix (app/session).
    async fn delete_objects(&self, key: &ObjectKey) -> Result<(), FlameError>;

    /// Load all objects from storage (for startup recovery).
    async fn load_objects(&self) -> Result<Vec<(ObjectKey, Object)>, FlameError>;
}

pub type StorageEnginePtr = Box<dyn StorageEngine>;

pub async fn connect(url: &str) -> Result<StorageEnginePtr, FlameError> {
    let url = std::env::var("FLAME_CACHE_STORAGE")
        .ok()
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| url.to_string());

    if url.is_empty() {
        tracing::warn!("No cache storage configured - using none storage (no persistence)");
        return Ok(Box::new(NoneStorage::new()));
    }

    if url == "none" {
        tracing::info!("Cache storage: none (memory-only, no persistence)");
        return Ok(Box::new(NoneStorage::new()));
    }

    let storage_path = if url.starts_with("fs://") || url.starts_with("file://") {
        parse_storage_url(&url)?
    } else {
        PathBuf::from(&url)
    };

    tracing::info!("Cache storage: filesystem at {:?}", storage_path);
    Ok(Box::new(DiskStorage::new(storage_path)?))
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
        let storage = connect("none").await.unwrap();
        let objects = storage.load_objects().await.unwrap();
        assert!(objects.is_empty());
    }
}
