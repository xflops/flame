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

use async_trait::async_trait;
use bytes::Bytes;
use futures::stream::BoxStream;
use futures::TryStreamExt;

use common::FlameError;

#[cfg(test)]
use crate::cache::Object;
use crate::cache::{ObjectData, ObjectKey, ObjectMetadata};

use super::{ObjectStat, StorageEngine};

/// The absence of a store: writes are accepted and discarded, and nothing is
/// ever found. `ResidentStorage` over it is a cache that is the only copy of
/// its objects.
pub struct NoStorage;

impl NoStorage {
    pub fn new() -> Self {
        tracing::info!("Using none storage engine (no persistence)");
        Self
    }
}

impl Default for NoStorage {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl StorageEngine for NoStorage {
    async fn write_stream(
        &self,
        _key: &ObjectKey,
        meta: ObjectMetadata,
        chunks: BoxStream<'_, Result<Bytes, FlameError>>,
    ) -> Result<ObjectMetadata, FlameError> {
        let size = chunks
            .try_fold(
                0u64,
                |size, chunk| async move { Ok(size + chunk.len() as u64) },
            )
            .await?;
        Ok(meta.with_size(size))
    }

    async fn read_range(
        &self,
        key: &ObjectKey,
        _patch: usize,
        _offset: u64,
        _len: usize,
    ) -> Result<Bytes, FlameError> {
        Err(FlameError::NotFound(format!(
            "object <{:?}> not found",
            key.to_key()
        )))
    }

    async fn stat(&self, _key: &ObjectKey) -> Result<Option<ObjectStat>, FlameError> {
        Ok(None)
    }

    #[cfg(test)]
    async fn read_object(
        &self,
        _key: &ObjectKey,
        _version: u64,
    ) -> Result<Option<Object>, FlameError> {
        Ok(None)
    }

    async fn patch_object(
        &self,
        key: &ObjectKey,
        _data: &ObjectData,
    ) -> Result<ObjectMetadata, FlameError> {
        Ok(ObjectMetadata::new()
            .with_key(key.to_key().unwrap_or_default())
            .with_creation_time(0)
            .with_data_type(""))
    }

    async fn delete_objects(&self, _key: &ObjectKey) -> Result<(), FlameError> {
        Ok(())
    }

    async fn scan(&self) -> Result<Vec<(ObjectKey, ObjectStat)>, FlameError> {
        Ok(Vec::new())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cache::Object;

    fn test_key() -> ObjectKey {
        ObjectKey::from_path("app/session")
            .unwrap()
            .with_object_id("obj1".to_string())
            .unwrap()
    }

    #[tokio::test]
    async fn writes_are_discarded_and_nothing_is_found() {
        let storage = NoStorage::new();
        let key = test_key();
        storage
            .write_object(&key, &Object::new().append_data(vec![1, 2, 3]))
            .await
            .unwrap();

        assert!(storage.stat(&key).await.unwrap().is_none());
        assert!(storage.read_object(&key, 0).await.unwrap().is_none());
        assert!(storage.read_range(&key, 0, 0, 1).await.is_err());
        assert!(storage.scan().await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn a_failed_stream_is_an_error() {
        let storage = NoStorage::new();
        let chunks = futures::stream::iter([
            Ok(Bytes::from_static(b"a")),
            Err(FlameError::Internal("broken".to_string())),
        ]);
        let result = storage
            .write_stream(
                &test_key(),
                ObjectMetadata::new().with_version(1),
                Box::pin(chunks),
            )
            .await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn append_and_delete_succeed() {
        let storage = NoStorage::new();
        let key = test_key();
        storage
            .patch_object(&key, &ObjectData::new().with_data(vec![4]))
            .await
            .unwrap();
        storage.delete_objects(&key).await.unwrap();
    }
}
