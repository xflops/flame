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
use rayon::prelude::*;
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};

use common::FlameError;

use crate::cache::{Object, ObjectKey, ObjectMetadata};

use super::StorageEngine;

const MAX_DELTAS_PER_OBJECT: u64 = 1000;
const OPAQUE_MAGIC: &[u8; 8] = b"FLCACHE\0";
const OPAQUE_FORMAT_VERSION: u8 = 3;
const OPAQUE_HEADER_LEN: usize = 8 + 1 + 8 + 8 + 8 + 4;

pub struct DiskStorage {
    storage_path: PathBuf,
}

impl DiskStorage {
    pub fn new(storage_path: PathBuf) -> Result<Self, FlameError> {
        if !storage_path.exists() {
            tracing::info!("Creating storage directory: {:?}", storage_path);
            fs::create_dir_all(&storage_path)?;
        }
        Ok(Self { storage_path })
    }

    fn object_path(&self, key: &ObjectKey) -> PathBuf {
        let object_id = key.object_id.as_ref().expect("object_id required");
        self.storage_path
            .join(&key.app_name)
            .join(&key.session_id)
            .join(format!("{}.bin", object_id))
    }

    fn delta_dir(&self, key: &ObjectKey) -> PathBuf {
        let object_id = key.object_id.as_ref().expect("object_id required");
        self.storage_path
            .join(&key.app_name)
            .join(&key.session_id)
            .join(format!("{}.deltas", object_id))
    }

    fn session_dir(&self, key: &ObjectKey) -> PathBuf {
        self.storage_path.join(&key.app_name).join(&key.session_id)
    }

    fn app_dir(&self, key: &ObjectKey) -> PathBuf {
        self.storage_path.join(&key.app_name)
    }
}

#[async_trait]
impl StorageEngine for DiskStorage {
    async fn write_object(&self, key: &ObjectKey, object: &Object) -> Result<(), FlameError> {
        let session_dir = self.session_dir(key);
        let object_path = self.object_path(key);
        let delta_dir = self.delta_dir(key);
        let object_clone = object.clone();

        tokio::task::spawn_blocking(move || {
            fs::create_dir_all(&session_dir)?;
            write_object_atomically(&object_path, &object_clone)?;
            if delta_dir.exists() {
                fs::remove_dir_all(&delta_dir)?;
            }
            tracing::debug!("Wrote object to disk: {:?}", object_path);
            Ok(())
        })
        .await
        .map_err(|e| FlameError::Internal(format!("Task join error: {}", e)))?
    }

    async fn read_object(&self, key: &ObjectKey) -> Result<Option<Object>, FlameError> {
        let object_path = self.object_path(key);
        let delta_dir = self.delta_dir(key);

        tokio::task::spawn_blocking(move || {
            if !object_path.exists() {
                return Ok(None);
            }
            let base = load_object_from_file(&object_path)?;
            let deltas = read_deltas_sync(&delta_dir, base.version, &base.data_type)?;
            Ok(Some(Object::with_data_at(
                base.version,
                base.data,
                base.data_type,
                deltas,
                base.creation_time,
            )))
        })
        .await
        .map_err(|e| FlameError::Internal(format!("Task join error: {}", e)))?
    }

    async fn patch_object(
        &self,
        key: &ObjectKey,
        delta: &Object,
    ) -> Result<ObjectMetadata, FlameError> {
        let object_path = self.object_path(key);
        let delta_dir = self.delta_dir(key);
        let key_str = key.to_key().expect("object_id required");
        let delta_clone = delta.clone();

        tokio::task::spawn_blocking(move || {
            if !object_path.exists() {
                return Err(FlameError::NotFound(format!(
                    "object <{}> not found, must put first",
                    key_str
                )));
            }

            fs::create_dir_all(&delta_dir)?;

            let mut index = count_deltas_sync(&delta_dir);

            loop {
                if index >= MAX_DELTAS_PER_OBJECT {
                    return Err(FlameError::InvalidState(format!(
                        "object <{}> has reached maximum delta count ({})",
                        key_str, MAX_DELTAS_PER_OBJECT
                    )));
                }

                let delta_path = delta_dir.join(format!("{}.bin", index));
                let temp_path = unique_temp_path(&delta_path);
                let prepared: Result<(), FlameError> = (|| {
                    let file = fs::OpenOptions::new()
                        .write(true)
                        .create_new(true)
                        .open(&temp_path)?;
                    write_opaque_to_writer(file, &delta_clone, false)?;
                    fs::File::open(&temp_path)?.sync_all()?;
                    Ok(())
                })();
                if let Err(error) = prepared {
                    let _ = fs::remove_file(&temp_path);
                    return Err(error);
                }
                let result = fs::hard_link(&temp_path, &delta_path);
                let _ = fs::remove_file(&temp_path);
                match result {
                    Ok(()) => {
                        tracing::debug!("Wrote delta {} to disk: {:?}", index, delta_path);
                        break;
                    }
                    Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
                        index += 1;
                        continue;
                    }
                    Err(e) => {
                        return Err(FlameError::Internal(format!(
                            "Failed to create delta file: {}",
                            e
                        )));
                    }
                }
            }

            let size = fs::metadata(&object_path)?.len();
            Ok(ObjectMetadata {
                endpoint: String::new(),
                key: key_str,
                version: 0,
                size,
                delta_count: index + 1,
                creation_time: 0,
                data_type: String::new(),
            })
        })
        .await
        .map_err(|e| FlameError::Internal(format!("Task join error: {}", e)))?
    }

    async fn delete_objects(&self, key: &ObjectKey) -> Result<(), FlameError> {
        let object_path = key.object_id.as_ref().map(|_| self.object_path(key));
        let delta_dir = key.object_id.as_ref().map(|_| self.delta_dir(key));
        let dir_to_delete = if object_path.is_none() && key.is_all_sessions() {
            Some(self.app_dir(key))
        } else if object_path.is_none() {
            Some(self.session_dir(key))
        } else {
            None
        };

        tokio::task::spawn_blocking(move || {
            if let Some(path) = object_path {
                if path.exists() {
                    fs::remove_file(&path)?;
                    tracing::debug!("Deleted object file: {:?}", path);
                }
            }
            if let Some(path) = delta_dir {
                if path.exists() {
                    fs::remove_dir_all(&path)?;
                    tracing::debug!("Deleted delta directory: {:?}", path);
                }
            }
            if let Some(path) = dir_to_delete {
                if path.exists() {
                    fs::remove_dir_all(&path)?;
                    tracing::debug!("Deleted directory: {:?}", path);
                }
            }
            Ok(())
        })
        .await
        .map_err(|e| FlameError::Internal(format!("Task join error: {}", e)))?
    }

    async fn load_objects(&self) -> Result<Vec<(ObjectKey, Object)>, FlameError> {
        let storage_path = self.storage_path.clone();

        tokio::task::spawn_blocking(move || {
            let mut results = Vec::new();

            if !storage_path.exists() {
                return Ok(results);
            }

            for app_entry in fs::read_dir(&storage_path)? {
                let app_entry = app_entry?;
                let app_path = app_entry.path();

                if !app_path.is_dir() {
                    continue;
                }

                let app_name = app_path
                    .file_name()
                    .and_then(|n| n.to_str())
                    .ok_or_else(|| FlameError::Internal("Invalid app directory name".to_string()))?
                    .to_string();

                for session_entry in fs::read_dir(&app_path)? {
                    let session_entry = session_entry?;
                    let session_path = session_entry.path();

                    if !session_path.is_dir() {
                        continue;
                    }

                    let session_id = session_path
                        .file_name()
                        .and_then(|n| n.to_str())
                        .ok_or_else(|| {
                            FlameError::Internal("Invalid session directory name".to_string())
                        })?
                        .to_string();

                    for object_entry in fs::read_dir(&session_path)? {
                        let object_entry = object_entry?;
                        let object_path = object_entry.path();

                        if object_path.is_dir() {
                            continue;
                        }

                        let extension = object_path.extension().and_then(|e| e.to_str());
                        if extension != Some("bin") {
                            continue;
                        }

                        let object_id = object_path
                            .file_stem()
                            .and_then(|n| n.to_str())
                            .ok_or_else(|| {
                                FlameError::Internal("Invalid object file name".to_string())
                            })?
                            .to_string();

                        let key = ObjectKey {
                            app_name: app_name.clone(),
                            session_id: session_id.clone(),
                            object_id: Some(object_id.clone()),
                        };

                        let delta_dir = session_path.join(format!("{}.deltas", object_id));
                        let base = load_object_from_file(&object_path)?;
                        let deltas = read_deltas_sync(&delta_dir, base.version, &base.data_type)?;
                        let object = Object::with_data_at(
                            base.version,
                            base.data,
                            base.data_type,
                            deltas,
                            base.creation_time,
                        );

                        results.push((key, object));
                    }
                }
            }

            tracing::info!("Loaded {} objects from disk", results.len());
            Ok(results)
        })
        .await
        .map_err(|e| FlameError::Internal(format!("Task join error: {}", e)))?
    }
}

fn count_deltas_sync(delta_dir: &Path) -> u64 {
    if !delta_dir.exists() {
        return 0;
    }

    fs::read_dir(delta_dir)
        .map(|entries| {
            entries
                .filter_map(|e| e.ok())
                .filter(|e| e.path().extension().and_then(|ext| ext.to_str()) == Some("bin"))
                .count() as u64
        })
        .unwrap_or(0)
}

fn read_deltas_sync(
    delta_dir: &Path,
    base_version: u64,
    base_data_type: &str,
) -> Result<Vec<Object>, FlameError> {
    if !delta_dir.exists() {
        return Ok(Vec::new());
    }

    let mut delta_files: Vec<_> = fs::read_dir(delta_dir)?
        .filter_map(|e| e.ok())
        .filter(|e| e.path().extension().and_then(|ext| ext.to_str()) == Some("bin"))
        .collect();

    delta_files.sort_by_key(|e| {
        e.path()
            .file_stem()
            .and_then(|s| s.to_str())
            .and_then(|s| s.parse::<u64>().ok())
            .unwrap_or(u64::MAX)
    });

    let deltas: Vec<Object> = delta_files
        .into_par_iter()
        .map(|entry| load_delta_from_file(&entry.path(), base_data_type))
        .collect::<Result<Vec<_>, _>>()?;
    validate_delta_versions(&deltas, base_version)?;

    Ok(deltas)
}

fn validate_delta_versions(deltas: &[Object], base_version: u64) -> Result<(), FlameError> {
    let mut previous_version = base_version;
    for delta in deltas {
        if delta.version <= previous_version {
            return Err(FlameError::InvalidState(format!(
                "Patch versions must be strictly increasing after base version {}; found {} after {}",
                base_version, delta.version, previous_version
            )));
        }
        previous_version = delta.version;
    }
    Ok(())
}

fn write_object_to_file(path: &Path, object: &Object) -> Result<(), FlameError> {
    write_opaque_to_writer(fs::File::create(path)?, object, true)
}

fn unique_temp_path(path: &Path) -> PathBuf {
    path.with_file_name(format!(
        ".{}.tmp-{}",
        path.file_name().unwrap().to_string_lossy(),
        uuid::Uuid::new_v4()
    ))
}

fn write_object_atomically(path: &Path, object: &Object) -> Result<(), FlameError> {
    let temp_path = unique_temp_path(path);
    let result = (|| {
        write_object_to_file(&temp_path, object)?;
        fs::File::open(&temp_path)?.sync_all()?;
        fs::rename(&temp_path, path)?;
        Ok(())
    })();
    if result.is_err() {
        let _ = fs::remove_file(&temp_path);
    }
    result
}

fn write_opaque_to_writer(
    mut file: fs::File,
    object: &Object,
    include_data_type: bool,
) -> Result<(), FlameError> {
    let data_type = if include_data_type {
        object.data_type.as_bytes()
    } else {
        &[]
    };
    let data_type_len = u32::try_from(data_type.len())
        .map_err(|_| FlameError::InvalidConfig("Cache data type is too long".to_string()))?;
    if include_data_type && data_type.is_empty() {
        return Err(FlameError::InvalidConfig(
            "Cache data type cannot be empty".to_string(),
        ));
    }
    file.write_all(OPAQUE_MAGIC)?;
    file.write_all(&[OPAQUE_FORMAT_VERSION])?;
    file.write_all(&object.version.to_le_bytes())?;
    file.write_all(&object.creation_time.to_le_bytes())?;
    let payload = object.opaque_data()?;
    file.write_all(&(payload.len() as u64).to_le_bytes())?;
    file.write_all(&data_type_len.to_le_bytes())?;
    file.write_all(data_type)?;
    file.write_all(payload)?;
    file.flush()?;
    Ok(())
}

fn load_object_from_file(path: &Path) -> Result<Object, FlameError> {
    load_record_from_file(path, None)
}

fn load_delta_from_file(path: &Path, base_data_type: &str) -> Result<Object, FlameError> {
    load_record_from_file(path, Some(base_data_type))
}

fn load_record_from_file(
    path: &Path,
    inherited_data_type: Option<&str>,
) -> Result<Object, FlameError> {
    let bytes = fs::read(path)?;
    if bytes.len() < OPAQUE_HEADER_LEN
        || &bytes[..OPAQUE_MAGIC.len()] != OPAQUE_MAGIC
        || bytes[OPAQUE_MAGIC.len()] != OPAQUE_FORMAT_VERSION
    {
        return Err(FlameError::InvalidState(format!(
            "Invalid opaque cache header in {}",
            path.display()
        )));
    }
    let version = u64::from_le_bytes(bytes[9..17].try_into().unwrap());
    let creation_time = i64::from_le_bytes(bytes[17..25].try_into().unwrap());
    let payload_len = u64::from_le_bytes(bytes[25..33].try_into().unwrap());
    let data_type_len = u32::from_le_bytes(bytes[33..37].try_into().unwrap()) as usize;
    let available = bytes.len() - OPAQUE_HEADER_LEN;
    if (data_type_len == 0) != inherited_data_type.is_some() || data_type_len > available {
        return Err(FlameError::InvalidState(format!(
            "Invalid cache data type length in {}",
            path.display()
        )));
    }
    let payload_start = OPAQUE_HEADER_LEN + data_type_len;
    if payload_len != (bytes.len() - payload_start) as u64 {
        return Err(FlameError::InvalidState(format!(
            "Opaque cache payload length mismatch in {}",
            path.display()
        )));
    }
    let data_type = match inherited_data_type {
        Some(data_type) => data_type.to_string(),
        None => std::str::from_utf8(&bytes[OPAQUE_HEADER_LEN..payload_start])
            .map_err(|_| {
                FlameError::InvalidState(format!("Invalid cache data type in {}", path.display()))
            })?
            .to_string(),
    };
    if creation_time < 0 {
        return Err(FlameError::InvalidState(format!(
            "Invalid cache creation time in {}",
            path.display()
        )));
    }
    Ok(Object::new_typed_at(
        version,
        Bytes::from(bytes).slice(payload_start..),
        data_type,
        creation_time,
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    fn test_key(app: &str, session: &str, object: &str) -> ObjectKey {
        ObjectKey {
            app_name: app.to_string(),
            session_id: session.to_string(),
            object_id: Some(object.to_string()),
        }
    }

    #[tokio::test]
    async fn test_disk_storage_write_read() {
        let temp_dir = tempdir().unwrap();
        let storage = DiskStorage::new(temp_dir.path().to_path_buf()).unwrap();

        let key = test_key("test-app", "test-session", "obj1");
        let object = Object::new_at(1, vec![1, 2, 3, 4, 5], 1_234_567);
        storage.write_object(&key, &object).await.unwrap();

        let result = storage.read_object(&key).await.unwrap();
        let bytes = fs::read(storage.object_path(&key)).unwrap();
        assert_eq!(&bytes[..8], OPAQUE_MAGIC);
        assert_eq!(&bytes[OPAQUE_HEADER_LEN..OPAQUE_HEADER_LEN + 3], b"raw");
        assert_eq!(&bytes[OPAQUE_HEADER_LEN + 3..], &[1, 2, 3, 4, 5]);
        assert!(result.is_some());
        let loaded = result.unwrap();
        assert_eq!(loaded.version, 1);
        assert_eq!(loaded.creation_time, 1_234_567);
        assert_eq!(loaded.data_type, "raw");
        assert_eq!(loaded.opaque_data().unwrap(), &[1, 2, 3, 4, 5]);
        assert!(loaded.deltas.is_empty());
    }

    #[tokio::test]
    async fn test_disk_storage_patch() {
        let temp_dir = tempdir().unwrap();
        let storage = DiskStorage::new(temp_dir.path().to_path_buf()).unwrap();

        let key = test_key("test-app", "test-session", "obj1");
        let object = Object::new_typed_at(1, vec![1, 2, 3], "arrow_table.zstd", 42);
        storage.write_object(&key, &object).await.unwrap();

        let delta = Object::new_typed(2, vec![4, 5, 6], "arrow_table.zstd");
        let meta = storage.patch_object(&key, &delta).await.unwrap();
        assert_eq!(meta.delta_count, 1);
        let delta_bytes = fs::read(storage.delta_dir(&key).join("0.bin")).unwrap();
        assert_eq!(&delta_bytes[33..37], &[0, 0, 0, 0]);
        assert_eq!(&delta_bytes[OPAQUE_HEADER_LEN..], &[4, 5, 6]);

        let loaded = storage.read_object(&key).await.unwrap().unwrap();
        assert_eq!(loaded.creation_time, 42);
        assert_eq!(loaded.data_type, "arrow_table.zstd");
        assert_eq!(loaded.deltas.len(), 1);
        assert_eq!(loaded.deltas[0].version, 2);
        assert_eq!(loaded.deltas[0].data_type, "arrow_table.zstd");
        assert_eq!(loaded.deltas[0].opaque_data().unwrap(), &[4, 5, 6]);
        assert!(storage.delta_dir(&key).join("0.bin").exists());
        let recovered = storage.load_objects().await.unwrap();
        assert_eq!(recovered[0].1.current_version(), 2);
        assert_eq!(recovered[0].1.data_type, "arrow_table.zstd");
        assert_eq!(recovered[0].1.deltas[0].data_type, "arrow_table.zstd");
    }

    #[tokio::test]
    async fn test_disk_storage_rejects_invalid_raw_header() {
        let temp_dir = tempdir().unwrap();
        let storage = DiskStorage::new(temp_dir.path().to_path_buf()).unwrap();
        let key = test_key("app", "session", "bad");
        fs::create_dir_all(storage.session_dir(&key)).unwrap();
        fs::write(storage.object_path(&key), b"broken").unwrap();
        assert!(matches!(
            storage.read_object(&key).await,
            Err(FlameError::InvalidState(_))
        ));
    }

    #[tokio::test]
    async fn test_disk_storage_rejects_truncated_raw_payload() {
        let temp_dir = tempdir().unwrap();
        let storage = DiskStorage::new(temp_dir.path().to_path_buf()).unwrap();
        let key = test_key("app", "session", "truncated");
        storage
            .write_object(&key, &Object::new_at(2, b"payload".to_vec(), 99))
            .await
            .unwrap();
        let path = storage.object_path(&key);
        let file = fs::OpenOptions::new().write(true).open(&path).unwrap();
        file.set_len((OPAQUE_HEADER_LEN + 3) as u64).unwrap();
        assert!(matches!(
            storage.read_object(&key).await,
            Err(FlameError::InvalidState(_))
        ));
    }

    #[tokio::test]
    async fn test_disk_storage_rejects_zero_delta_versions() {
        let temp_dir = tempdir().unwrap();
        let storage = DiskStorage::new(temp_dir.path().to_path_buf()).unwrap();

        let key = test_key("test-app", "test-session", "obj1");
        let object = Object::new(5, vec![1, 2, 3]);
        storage.write_object(&key, &object).await.unwrap();

        storage
            .patch_object(&key, &Object::new(0, vec![4]))
            .await
            .unwrap();
        assert!(matches!(
            storage.read_object(&key).await,
            Err(FlameError::InvalidState(_))
        ));
    }

    #[tokio::test]
    async fn test_disk_storage_rejects_non_monotonic_delta_versions() {
        let temp_dir = tempdir().unwrap();
        let storage = DiskStorage::new(temp_dir.path().to_path_buf()).unwrap();

        let key = test_key("test-app", "test-session", "obj1");
        storage
            .write_object(&key, &Object::new(5, vec![1, 2, 3]))
            .await
            .unwrap();
        storage
            .patch_object(&key, &Object::new(4, vec![4]))
            .await
            .unwrap();

        let result = storage.read_object(&key).await;

        assert!(result.is_err());
    }

    #[tokio::test]
    async fn test_disk_storage_delete_objects() {
        let temp_dir = tempdir().unwrap();
        let storage = DiskStorage::new(temp_dir.path().to_path_buf()).unwrap();

        let key1 = test_key("test-app", "test-session", "obj1");
        let key2 = test_key("test-app", "test-session", "obj2");
        storage
            .write_object(&key1, &Object::new(1, vec![1, 2, 3]))
            .await
            .unwrap();
        storage
            .write_object(&key2, &Object::new(2, vec![4, 5, 6]))
            .await
            .unwrap();

        let delete_key = ObjectKey::from_path("test-app/test-session").unwrap();
        storage.delete_objects(&delete_key).await.unwrap();

        assert!(storage.read_object(&key1).await.unwrap().is_none());
        assert!(storage.read_object(&key2).await.unwrap().is_none());
    }

    #[tokio::test]
    async fn test_disk_storage_delete_exact_object() {
        let temp_dir = tempdir().unwrap();
        let storage = DiskStorage::new(temp_dir.path().to_path_buf()).unwrap();

        let key1 = test_key("test-app", "test-session", "obj1");
        let key2 = test_key("test-app", "test-session", "obj2");
        storage
            .write_object(&key1, &Object::new(1, vec![1, 2, 3]))
            .await
            .unwrap();
        storage
            .write_object(&key2, &Object::new(2, vec![4, 5, 6]))
            .await
            .unwrap();
        storage
            .patch_object(&key1, &Object::new(0, vec![7]))
            .await
            .unwrap();
        let key1_delta_dir = storage.delta_dir(&key1);
        assert!(key1_delta_dir.exists());

        storage.delete_objects(&key1).await.unwrap();

        assert!(storage.read_object(&key1).await.unwrap().is_none());
        assert!(!key1_delta_dir.exists());
        assert!(storage.read_object(&key2).await.unwrap().is_some());
    }

    #[tokio::test]
    async fn test_disk_storage_load_objects() {
        let temp_dir = tempdir().unwrap();
        let storage = DiskStorage::new(temp_dir.path().to_path_buf()).unwrap();

        let key1 = test_key("app1", "session1", "obj1");
        let key2 = test_key("app1", "session2", "obj2");
        storage
            .write_object(&key1, &Object::new(1, vec![1, 2, 3]))
            .await
            .unwrap();
        storage
            .write_object(&key2, &Object::new(2, vec![4, 5, 6]))
            .await
            .unwrap();

        let objects = storage.load_objects().await.unwrap();
        assert_eq!(objects.len(), 2);
    }
}

#[cfg(test)]
mod cache_benchmarks {
    use super::*;
    use std::hint::black_box;
    use std::time::Instant;
    use tempfile::tempdir;

    fn key(name: &str) -> ObjectKey {
        ObjectKey {
            app_name: "bench".to_string(),
            session_id: "session".to_string(),
            object_id: Some(name.to_string()),
        }
    }

    #[tokio::test]
    #[ignore = "manual performance benchmark"]
    async fn opaque_disk_write_and_reload() {
        let directory = tempdir().unwrap();
        let storage = DiskStorage::new(directory.path().to_path_buf()).unwrap();
        let key = key("opaque");
        let mut state = 1u64;
        let payload: Vec<u8> = (0..8 * 1024 * 1024)
            .map(|_| {
                state ^= state << 13;
                state ^= state >> 7;
                state ^= state << 17;
                state as u8
            })
            .collect();
        let object = Object::new(1, payload);

        let writes = 8;
        let started = Instant::now();
        for _ in 0..writes {
            storage.write_object(&key, &object).await.unwrap();
        }
        let write_time = started.elapsed();

        let reads = 16;
        let started = Instant::now();
        for _ in 0..reads {
            black_box(storage.read_object(&key).await.unwrap().unwrap());
        }
        let read_time = started.elapsed();
        println!(
            "opaque disk, 8 MiB: write={:?}/op reload={:?}/op",
            write_time / writes,
            read_time / reads
        );
    }

    #[tokio::test]
    #[ignore = "manual performance benchmark"]
    async fn sequential_patch_append() {
        let directory = tempdir().unwrap();
        let storage = DiskStorage::new(directory.path().to_path_buf()).unwrap();
        let key = key("patched");
        storage
            .write_object(&key, &Object::new(1, vec![0; 1024]))
            .await
            .unwrap();

        let mut first_hundred = None;
        let mut window_started = Instant::now();
        for index in 0..600 {
            storage
                .patch_object(&key, &Object::new(index + 2, vec![index as u8; 1024]))
                .await
                .unwrap();
            if index == 99 {
                first_hundred = Some(window_started.elapsed());
            }
            if index == 499 {
                window_started = Instant::now();
            }
        }
        let last_hundred = window_started.elapsed();
        println!(
            "patch append, 1 KiB: first_100={:?}/op last_100={:?}/op",
            first_hundred.unwrap() / 100,
            last_hundred / 100
        );
    }
}
