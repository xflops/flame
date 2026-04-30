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

use common::FlameError;

use crate::cache::{Object, ObjectKey, ObjectMetadata};

use super::StorageEngine;

pub struct NoneStorage;

impl NoneStorage {
    pub fn new() -> Self {
        tracing::info!("Using none storage engine (no persistence)");
        Self
    }
}

impl Default for NoneStorage {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl StorageEngine for NoneStorage {
    async fn write_object(&self, _key: &ObjectKey, _object: &Object) -> Result<(), FlameError> {
        Ok(())
    }

    async fn read_object(&self, _key: &ObjectKey) -> Result<Option<Object>, FlameError> {
        Ok(None)
    }

    async fn patch_object(
        &self,
        key: &ObjectKey,
        _delta: &Object,
    ) -> Result<ObjectMetadata, FlameError> {
        Ok(ObjectMetadata {
            endpoint: String::new(),
            key: key.to_key().unwrap_or_default(),
            version: 0,
            size: 0,
            delta_count: 0,
        })
    }

    async fn delete_objects(&self, _key: &ObjectKey) -> Result<(), FlameError> {
        Ok(())
    }

    async fn load_objects(&self) -> Result<Vec<(ObjectKey, Object)>, FlameError> {
        Ok(Vec::new())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_key() -> ObjectKey {
        ObjectKey {
            app_name: "app".to_string(),
            session_id: "session".to_string(),
            object_id: Some("obj1".to_string()),
        }
    }

    #[tokio::test]
    async fn test_none_storage_basic_operations() {
        let storage = NoneStorage::new();

        let key = test_key();
        let object = Object::new(1, vec![1, 2, 3]);
        storage.write_object(&key, &object).await.unwrap();

        let result = storage.read_object(&key).await.unwrap();
        assert!(result.is_none());
    }

    #[tokio::test]
    async fn test_none_storage_patch_returns_ok() {
        let storage = NoneStorage::new();

        let key = test_key();
        let delta = Object::new(1, vec![4, 5, 6]);
        let result = storage.patch_object(&key, &delta).await;

        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn test_none_storage_delete_objects() {
        let storage = NoneStorage::new();
        let key = ObjectKey::from_path("app/session").unwrap();
        storage.delete_objects(&key).await.unwrap();
    }

    #[tokio::test]
    async fn test_none_storage_load_objects() {
        let storage = NoneStorage::new();

        let objects = storage.load_objects().await.unwrap();
        assert!(objects.is_empty());
    }
}
