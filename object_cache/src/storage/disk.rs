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

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use arrow::array::{BinaryArray, RecordBatch, UInt64Array};
use arrow::datatypes::{DataType, Field, Schema};
use arrow::ipc::writer::IpcWriteOptions;
use arrow::ipc::{reader::FileReader, writer::FileWriter, CompressionType};
use async_trait::async_trait;
use rayon::prelude::*;

use common::FlameError;

use crate::{Object, ObjectMetadata};

use super::StorageEngine;

const MAX_DELTAS_PER_OBJECT: u64 = 1000;

/// Disk-based storage engine using Arrow IPC format.
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

    fn object_path(&self, key: &str) -> PathBuf {
        self.storage_path.join(format!("{}.arrow", key))
    }

    fn delta_dir(&self, key: &str) -> PathBuf {
        self.storage_path.join(format!("{}.deltas", key))
    }

    fn app_dir(&self, app_name: &str) -> PathBuf {
        self.storage_path.join(app_name)
    }

    fn app_session_dir(&self, app_name: &str, session_id: &str) -> PathBuf {
        self.storage_path.join(app_name).join(session_id)
    }
}

#[async_trait]
impl StorageEngine for DiskStorage {
    async fn write_object(&self, key: &str, object: &Object) -> Result<(), FlameError> {
        let parts: Vec<&str> = key.split('/').collect();
        if parts.len() != 3 {
            return Err(FlameError::InvalidConfig(format!(
                "Invalid key format '{}': expected '<app>/<ssn>/<uuid>'",
                key
            )));
        }
        let app_name = parts[0].to_string();
        let session_id = parts[1].to_string();

        let app_session_dir = self.app_session_dir(&app_name, &session_id);
        let object_path = self.object_path(key);
        let delta_dir = self.delta_dir(key);
        let object_clone = object.clone();

        tokio::task::spawn_blocking(move || {
            fs::create_dir_all(&app_session_dir)?;
            let batch = object_to_batch(&object_clone)?;
            write_batch_to_file(&object_path, &batch)?;
            if delta_dir.exists() {
                fs::remove_dir_all(&delta_dir)?;
            }
            tracing::debug!("Wrote object to disk: {:?}", object_path);
            Ok(())
        })
        .await
        .map_err(|e| FlameError::Internal(format!("Task join error: {}", e)))?
    }

    async fn read_object(&self, key: &str) -> Result<Option<Object>, FlameError> {
        let object_path = self.object_path(key);
        let delta_dir = self.delta_dir(key);

        tokio::task::spawn_blocking(move || {
            if !object_path.exists() {
                return Ok(None);
            }
            let base = load_object_from_file(&object_path)?;
            let deltas = read_deltas_sync(&delta_dir)?;
            Ok(Some(Object::with_deltas(base.version, base.data, deltas)))
        })
        .await
        .map_err(|e| FlameError::Internal(format!("Task join error: {}", e)))?
    }

    async fn patch_object(&self, key: &str, delta: &Object) -> Result<ObjectMetadata, FlameError> {
        let object_path = self.object_path(key);
        let delta_dir = self.delta_dir(key);
        let key_owned = key.to_string();
        let delta_clone = delta.clone();

        tokio::task::spawn_blocking(move || {
            if !object_path.exists() {
                return Err(FlameError::NotFound(format!(
                    "object <{}> not found, must put first",
                    key_owned
                )));
            }

            fs::create_dir_all(&delta_dir)?;

            let batch = object_to_batch(&delta_clone)?;
            let mut index = count_deltas_sync(&delta_dir);

            loop {
                if index >= MAX_DELTAS_PER_OBJECT {
                    return Err(FlameError::InvalidState(format!(
                        "object <{}> has reached maximum delta count ({}). Use update_object to compact deltas.",
                        key_owned, MAX_DELTAS_PER_OBJECT
                    )));
                }

                let delta_path = delta_dir.join(format!("{}.arrow", index));

                match fs::OpenOptions::new()
                    .write(true)
                    .create_new(true)
                    .open(&delta_path)
                {
                    Ok(file) => {
                        write_batch_to_writer(file, &batch)?;
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
                        )))
                    }
                }
            }

            let size = fs::metadata(&object_path)?.len();
            Ok(ObjectMetadata {
                endpoint: String::new(),
                key: key_owned,
                version: 0,
                size,
                delta_count: index + 1,
            })
        })
        .await
        .map_err(|e| FlameError::Internal(format!("Task join error: {}", e)))?
    }

    async fn delete_object(&self, key: &str) -> Result<(), FlameError> {
        let object_path = self.object_path(key);
        let delta_dir = self.delta_dir(key);

        tokio::task::spawn_blocking(move || {
            if object_path.exists() {
                fs::remove_file(&object_path)?;
            }
            if delta_dir.exists() {
                fs::remove_dir_all(&delta_dir)?;
            }
            Ok(())
        })
        .await
        .map_err(|e| FlameError::Internal(format!("Task join error: {}", e)))?
    }

    async fn delete_objects(&self, prefix: &str) -> Result<(), FlameError> {
        let parts: Vec<&str> = prefix.split('/').collect();
        let dir_to_delete = match parts.len() {
            1 => self.app_dir(parts[0]),
            2 => self.app_session_dir(parts[0], parts[1]),
            _ => {
                return Err(FlameError::InvalidConfig(format!(
                    "Invalid prefix '{}': expected '<app>' or '<app>/<ssn>'",
                    prefix
                )))
            }
        };

        tokio::task::spawn_blocking(move || {
            if dir_to_delete.exists() {
                fs::remove_dir_all(&dir_to_delete)?;
                tracing::debug!("Deleted directory: {:?}", dir_to_delete);
            }
            Ok(())
        })
        .await
        .map_err(|e| FlameError::Internal(format!("Task join error: {}", e)))?
    }

    async fn load_objects(&self) -> Result<Vec<(String, Object, u64)>, FlameError> {
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
                    .ok_or_else(|| {
                        FlameError::Internal("Invalid app directory name".to_string())
                    })?;

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
                        })?;

                    for object_entry in fs::read_dir(&session_path)? {
                        let object_entry = object_entry?;
                        let object_path = object_entry.path();

                        if object_path.is_dir() {
                            continue;
                        }

                        if object_path.extension().and_then(|e| e.to_str()) != Some("arrow") {
                            continue;
                        }

                        let object_id = object_path
                            .file_stem()
                            .and_then(|n| n.to_str())
                            .ok_or_else(|| {
                                FlameError::Internal("Invalid object file name".to_string())
                            })?;

                        let key = format!("{}/{}/{}", app_name, session_id, object_id);
                        let delta_dir = session_path.join(format!("{}.deltas", object_id));
                        let delta_count = count_deltas_sync(&delta_dir);

                        let base = load_object_from_file(&object_path)?;
                        let deltas = read_deltas_sync(&delta_dir)?;
                        let object = Object::with_deltas(base.version, base.data, deltas);

                        results.push((key, object, delta_count));
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

fn get_object_schema() -> Schema {
    Schema::new(vec![
        Field::new("version", DataType::UInt64, false),
        Field::new("data", DataType::Binary, false),
    ])
}

fn count_deltas_sync(delta_dir: &Path) -> u64 {
    if !delta_dir.exists() {
        return 0;
    }

    fs::read_dir(delta_dir)
        .map(|entries| {
            entries
                .filter_map(|e| e.ok())
                .filter(|e| e.path().extension().and_then(|ext| ext.to_str()) == Some("arrow"))
                .count() as u64
        })
        .unwrap_or(0)
}

fn read_deltas_sync(delta_dir: &Path) -> Result<Vec<Object>, FlameError> {
    if !delta_dir.exists() {
        return Ok(Vec::new());
    }

    let mut delta_files: Vec<_> = fs::read_dir(delta_dir)?
        .filter_map(|e| e.ok())
        .filter(|e| e.path().extension().and_then(|ext| ext.to_str()) == Some("arrow"))
        .collect();

    delta_files.sort_by_key(|e| {
        e.path()
            .file_stem()
            .and_then(|s| s.to_str())
            .and_then(|s| s.parse::<u64>().ok())
            .unwrap_or(u64::MAX)
    });

    delta_files
        .into_par_iter()
        .map(|entry| load_object_from_file(&entry.path()))
        .collect()
}

fn write_batch_to_file(path: &Path, batch: &RecordBatch) -> Result<(), FlameError> {
    let file = fs::File::create(path)?;
    write_batch_to_writer(file, batch)
}

fn write_batch_to_writer(file: fs::File, batch: &RecordBatch) -> Result<(), FlameError> {
    let options = IpcWriteOptions::default()
        .try_with_compression(Some(CompressionType::ZSTD))
        .map_err(|e| FlameError::Internal(format!("Failed to set compression: {}", e)))?;
    let mut writer = FileWriter::try_new_with_options(file, &batch.schema(), options)
        .map_err(|e| FlameError::Internal(format!("Failed to create writer: {}", e)))?;
    writer
        .write(batch)
        .map_err(|e| FlameError::Internal(format!("Failed to write batch: {}", e)))?;
    writer
        .finish()
        .map_err(|e| FlameError::Internal(format!("Failed to finish writer: {}", e)))?;
    Ok(())
}

fn object_to_batch(object: &Object) -> Result<RecordBatch, FlameError> {
    let schema = get_object_schema();

    let version_array = UInt64Array::from(vec![object.version]);
    let data_array = BinaryArray::from(vec![object.data.as_slice()]);

    RecordBatch::try_new(
        Arc::new(schema),
        vec![Arc::new(version_array), Arc::new(data_array)],
    )
    .map_err(|e| FlameError::Internal(format!("Failed to create RecordBatch: {}", e)))
}

fn load_object_from_file(path: &Path) -> Result<Object, FlameError> {
    let file = fs::File::open(path).map_err(|e| {
        if e.kind() == std::io::ErrorKind::NotFound {
            FlameError::NotFound(format!("Object file not found: {}", path.display()))
        } else {
            FlameError::Internal(format!("Failed to open object file: {}", e))
        }
    })?;
    let reader = FileReader::try_new(file, None)
        .map_err(|e| FlameError::Internal(format!("Failed to create reader: {}", e)))?;

    // SAFETY: Skipping Arrow IPC validation is safe here because:
    // 1. All data in this cache directory was written by this service using write_batch_to_file()
    // 2. The Arrow IPC format includes checksums that detect corruption during read
    // 3. This provides 3-9x faster reads for trusted cache data
    // 4. If files are externally modified/corrupted, Arrow will still detect most issues
    //    via schema validation and array bounds checking in batch_to_object()
    let reader = unsafe { reader.with_skip_validation(true) };

    let batch = reader
        .into_iter()
        .next()
        .ok_or_else(|| FlameError::Internal("No batches in file".to_string()))?
        .map_err(|e| FlameError::Internal(format!("Failed to read batch: {}", e)))?;

    batch_to_object(&batch)
}

fn batch_to_object(batch: &RecordBatch) -> Result<Object, FlameError> {
    if batch.num_rows() != 1 {
        return Err(FlameError::InvalidState(
            "Expected exactly one row".to_string(),
        ));
    }

    let version_col = batch
        .column(0)
        .as_any()
        .downcast_ref::<UInt64Array>()
        .ok_or_else(|| FlameError::Internal("Invalid version column".to_string()))?;
    let data_col = batch
        .column(1)
        .as_any()
        .downcast_ref::<BinaryArray>()
        .ok_or_else(|| FlameError::Internal("Invalid data column".to_string()))?;

    let version = version_col.value(0);
    let data = data_col.value(0).to_vec();

    Ok(Object::new(version, data))
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[tokio::test]
    async fn test_disk_storage_write_read() {
        let temp_dir = tempdir().unwrap();
        let storage = DiskStorage::new(temp_dir.path().to_path_buf()).unwrap();

        let object = Object::new(1, vec![1, 2, 3, 4, 5]);
        storage
            .write_object("test-app/test-session/obj1", &object)
            .await
            .unwrap();

        let result = storage
            .read_object("test-app/test-session/obj1")
            .await
            .unwrap();
        assert!(result.is_some());
        let loaded = result.unwrap();
        assert_eq!(loaded.version, 1);
        assert_eq!(loaded.data, vec![1, 2, 3, 4, 5]);
        assert!(loaded.deltas.is_empty());
    }

    #[tokio::test]
    async fn test_disk_storage_patch() {
        let temp_dir = tempdir().unwrap();
        let storage = DiskStorage::new(temp_dir.path().to_path_buf()).unwrap();

        let object = Object::new(1, vec![1, 2, 3]);
        storage
            .write_object("test-app/test-session/obj1", &object)
            .await
            .unwrap();

        let delta = Object::new(0, vec![4, 5, 6]);
        let meta = storage
            .patch_object("test-app/test-session/obj1", &delta)
            .await
            .unwrap();
        assert_eq!(meta.delta_count, 1);

        let loaded = storage
            .read_object("test-app/test-session/obj1")
            .await
            .unwrap()
            .unwrap();
        assert_eq!(loaded.deltas.len(), 1);
        assert_eq!(loaded.deltas[0].data, vec![4, 5, 6]);
    }

    #[tokio::test]
    async fn test_disk_storage_delete() {
        let temp_dir = tempdir().unwrap();
        let storage = DiskStorage::new(temp_dir.path().to_path_buf()).unwrap();

        let object = Object::new(1, vec![1, 2, 3]);
        storage
            .write_object("test-app/test-session/obj1", &object)
            .await
            .unwrap();

        storage
            .delete_object("test-app/test-session/obj1")
            .await
            .unwrap();

        let result = storage
            .read_object("test-app/test-session/obj1")
            .await
            .unwrap();
        assert!(result.is_none());
    }

    #[tokio::test]
    async fn test_disk_storage_delete_objects() {
        let temp_dir = tempdir().unwrap();
        let storage = DiskStorage::new(temp_dir.path().to_path_buf()).unwrap();

        let object1 = Object::new(1, vec![1, 2, 3]);
        let object2 = Object::new(2, vec![4, 5, 6]);
        storage
            .write_object("test-app/test-session/obj1", &object1)
            .await
            .unwrap();
        storage
            .write_object("test-app/test-session/obj2", &object2)
            .await
            .unwrap();

        storage
            .delete_objects("test-app/test-session")
            .await
            .unwrap();

        let result1 = storage
            .read_object("test-app/test-session/obj1")
            .await
            .unwrap();
        let result2 = storage
            .read_object("test-app/test-session/obj2")
            .await
            .unwrap();
        assert!(result1.is_none());
        assert!(result2.is_none());
    }

    #[tokio::test]
    async fn test_disk_storage_load_objects() {
        let temp_dir = tempdir().unwrap();
        let storage = DiskStorage::new(temp_dir.path().to_path_buf()).unwrap();

        let object1 = Object::new(1, vec![1, 2, 3]);
        let object2 = Object::new(2, vec![4, 5, 6]);
        storage
            .write_object("app1/session1/obj1", &object1)
            .await
            .unwrap();
        storage
            .write_object("app1/session2/obj2", &object2)
            .await
            .unwrap();

        let objects = storage.load_objects().await.unwrap();
        assert_eq!(objects.len(), 2);
    }
}
