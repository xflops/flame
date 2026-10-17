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

//! Disk layout of one object, a directory of three files:
//!
//! ```text
//! <root>/<app>/<session>/<object>/
//! ├── metadata.json   # data type of all the parts
//! ├── index.bin       # one fixed-size record per part
//! └── data.bin        # payloads of the parts, concatenated and append-only
//! ```
//!
//! An object is created once and then only grows by appended data, like the layers
//! of an overlay filesystem. An append is committed by its index record: payload
//! bytes in `data.bin` past the last record are ignored: the next append is
//! appended after them.
//! The version of an object is its number of data: there is no version on disk.

use async_trait::async_trait;
use bytes::Bytes;
use futures::stream::BoxStream;
use futures::StreamExt;
use serde::{Deserialize, Serialize};
use std::fs;
use std::io::{BufWriter, Write};
use std::os::unix::fs::FileExt;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use tokio::sync::mpsc;

use common::FlameError;

#[cfg(test)]
use crate::cache::Object;
use crate::cache::{ObjectData, ObjectKey, ObjectMetadata};

use super::{payload_range, ObjectStat, Record, StorageEngine, MAX_APPENDS_PER_OBJECT};

const METADATA_FILE: &str = "metadata.json";
const INDEX_FILE: &str = "index.bin";
const DATA_FILE: &str = "data.bin";
const LOST_FOUND_DIR: &str = "lost+found";
/// Bytes of one record in `index.bin`: payload offset, length and creation time.
const RECORD_LEN: usize = 3 * 8;
/// Chunks buffered between the async writer and the blocking file writer.
const WRITE_QUEUE_DEPTH: usize = 4;

/// The `metadata.json` of an object.
#[derive(Serialize, Deserialize)]
struct StoredMetadata {
    data_type: String,
}

impl Record {
    /// The record as stored in `index.bin`.
    fn encode(&self) -> [u8; RECORD_LEN] {
        let mut bytes = [0u8; RECORD_LEN];
        bytes[0..8].copy_from_slice(&self.offset.to_le_bytes());
        bytes[8..16].copy_from_slice(&self.len.to_le_bytes());
        bytes[16..24].copy_from_slice(&self.creation_time.to_le_bytes());
        bytes
    }

    fn decode(bytes: &[u8; RECORD_LEN]) -> Self {
        let word = |at: usize| <[u8; 8]>::try_from(&bytes[at..at + 8]).unwrap();
        Self {
            offset: u64::from_le_bytes(word(0)),
            len: u64::from_le_bytes(word(8)),
            creation_time: i64::from_le_bytes(word(16)),
        }
    }

    fn end(&self) -> u64 {
        self.offset + self.len
    }
}

/// An open object: its data type, the open `index.bin` and `data.bin` files, and
/// the number of parts. It is held by whoever serves the object (the resident
/// cache), so nothing is opened per read, and it keeps no payload or index in
/// memory: the records are fixed-size, so the record of part `i` is read at
/// `i * RECORD_LEN` of `index.bin`, and then its payload at the record's offset
/// of `data.bin`. One handle serves concurrent readers. Data is appended
/// through the same handle, so it never goes stale: both files are opened for
/// append, and an append is two writes. Writes to one object must be serialized
/// by the caller.
#[derive(Debug)]
pub struct ObjectFiles {
    data_type: String,
    index: fs::File,
    data: fs::File,
    /// Number of committed parts. Only grows.
    count: AtomicUsize,
}

impl ObjectFiles {
    /// Open the object in `dir`. Returns None if there is no such object.
    fn open(dir: &Path) -> Result<Option<Self>, FlameError> {
        let invalid =
            |what: &str| FlameError::InvalidState(format!("{} in {}", what, dir.display()));
        let open = |name: &str| {
            fs::OpenOptions::new()
                .read(true)
                .append(true)
                .open(dir.join(name))
        };
        let index = match open(INDEX_FILE) {
            Ok(file) => file,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(e) => return Err(e.into()),
        };
        let metadata = fs::read(dir.join(METADATA_FILE))?;
        let metadata: StoredMetadata =
            serde_json::from_slice(&metadata).map_err(|_| invalid("Invalid object metadata"))?;
        let data = open(DATA_FILE)?;

        // A partial record at the end is an interrupted append: not committed.
        // It is cut off so that the records appended next stay aligned.
        let count = index.metadata()?.len() as usize / RECORD_LEN;
        index.set_len((count * RECORD_LEN) as u64)?;
        let files = Self {
            data_type: metadata.data_type,
            index,
            data,
            count: AtomicUsize::new(count),
        };
        // The last payload is the furthest, so it being in `data.bin` is
        // enough to know the data was not truncated.
        match files.record(count.wrapping_sub(1))? {
            Some(last) if last.end() <= files.data.metadata()?.len() => Ok(Some(files)),
            _ => Err(invalid("Truncated object data")),
        }
    }

    fn count(&self) -> usize {
        self.count.load(Ordering::Acquire)
    }

    /// The record of part `part`, read from `index.bin` at its position.
    fn record(&self, part: usize) -> Result<Option<Record>, FlameError> {
        if part >= self.count() {
            return Ok(None);
        }
        let mut bytes = [0u8; RECORD_LEN];
        self.index
            .read_exact_at(&mut bytes, (part * RECORD_LEN) as u64)?;
        Ok(Some(Record::decode(&bytes)))
    }

    /// All records, in one read of `index.bin`.
    fn records(&self) -> Result<Vec<Record>, FlameError> {
        let mut bytes = vec![0u8; self.count() * RECORD_LEN];
        self.index.read_exact_at(&mut bytes, 0)?;
        Ok(bytes
            .as_chunks::<RECORD_LEN>()
            .0
            .iter()
            .map(Record::decode)
            .collect())
    }

    /// The object's metadata, from the index.
    pub fn stat(&self) -> Result<ObjectStat, FlameError> {
        Ok(ObjectStat {
            data_type: self.data_type.clone(),
            records: self.records()?,
        })
    }

    /// Read `len` bytes of part `part` (0 is the first) at `offset`: its record
    /// is read by position, then the payload.
    pub fn read_range(
        &self,
        key: &str,
        part: usize,
        offset: u64,
        len: usize,
    ) -> Result<Bytes, FlameError> {
        let record = self.record(part)?.ok_or_else(|| {
            FlameError::NotFound(format!("part {} of object <{}> not found", part, key))
        })?;
        payload_range(key, offset, len, record.len)?;
        let mut buf = vec![0u8; len];
        self.data.read_exact_at(&mut buf, record.offset + offset)?;
        Ok(Bytes::from(buf))
    }

    /// Append data through the open files. The payload is made durable
    /// before the index record commits it. Returns the metadata of the
    /// object.
    pub fn append(&self, key: &str, part: &ObjectData) -> Result<ObjectMetadata, FlameError> {
        if self.count() as u64 > MAX_APPENDS_PER_OBJECT {
            return Err(FlameError::InvalidState(format!(
                "object <{}> has reached maximum number of appends ({})",
                key, MAX_APPENDS_PER_OBJECT
            )));
        }

        let record = Record {
            // After everything in the file, including bytes of an interrupted append.
            offset: self.data.metadata()?.len(),
            len: part.size_bytes(),
            creation_time: part.creation_time,
        };
        (&self.data).write_all(&part.data)?;
        self.data.sync_data()?;
        (&self.index).write_all(&record.encode())?;
        self.index.sync_data()?;
        self.count.fetch_add(1, Ordering::Release);

        let stat = self.stat()?;
        Ok(ObjectMetadata::new()
            .with_key(key)
            .with_version(stat.version())
            .with_size(stat.size())
            .with_creation_time(stat.creation_time())
            .with_data_type(stat.data_type))
    }

    /// Bytes this handle keeps in memory, for the resident cache's accounting.
    pub fn memory_size(&self) -> u64 {
        (std::mem::size_of::<Self>() + self.data_type.len()) as u64
    }
}

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

    fn object_dir(&self, key: &ObjectKey) -> PathBuf {
        let object_id = key.object_id.as_ref().expect("object_id required");
        self.session_dir(key).join(object_id)
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
    async fn write_stream(
        &self,
        key: &ObjectKey,
        meta: ObjectMetadata,
        chunks: BoxStream<'_, Result<Bytes, FlameError>>,
    ) -> Result<ObjectMetadata, FlameError> {
        let session_dir = self.session_dir(key);
        let object_dir = self.object_dir(key);
        if object_dir.exists() {
            return Err(FlameError::AlreadyExist(format!(
                "object <{}> already exists",
                key.to_key().unwrap_or_default()
            )));
        }

        // The object is built in a temporary directory on the blocking pool;
        // chunks reach it through a small channel. Closing the channel without
        // the `None` commit marker aborts the write and removes the directory.
        let (tx, rx) = mpsc::channel(WRITE_QUEUE_DEPTH);
        let header_meta = meta.clone();
        let writer = tokio::task::spawn_blocking(move || {
            fs::create_dir_all(&session_dir)?;
            let temp_dir = unique_temp_path(&object_dir);
            let result = create_object(&temp_dir, &object_dir, &header_meta, rx);
            match &result {
                Ok(_) => tracing::debug!("Created object on disk: {:?}", object_dir),
                Err(_) => {
                    let _ = fs::remove_dir_all(&temp_dir);
                }
            }
            result
        });

        let fed = feed_chunks(tx, chunks).await;
        let written = writer
            .await
            .map_err(|e| FlameError::Internal(format!("Task join error: {}", e)))?;
        fed?;
        Ok(meta.with_size(written?))
    }

    async fn read_range(
        &self,
        key: &ObjectKey,
        part: usize,
        offset: u64,
        len: usize,
    ) -> Result<Bytes, FlameError> {
        let key_str = key.to_key().expect("object_id required");
        let files = self
            .open(key)
            .await?
            .ok_or_else(|| FlameError::NotFound(format!("object <{}> not found", key_str)))?;
        tokio::task::spawn_blocking(move || files.read_range(&key_str, part, offset, len))
            .await
            .map_err(|e| FlameError::Internal(format!("Task join error: {}", e)))?
    }

    async fn open(&self, key: &ObjectKey) -> Result<Option<Arc<ObjectFiles>>, FlameError> {
        let object_dir = self.object_dir(key);
        tokio::task::spawn_blocking(move || Ok(ObjectFiles::open(&object_dir)?.map(Arc::new)))
            .await
            .map_err(|e| FlameError::Internal(format!("Task join error: {}", e)))?
    }

    async fn stat(&self, key: &ObjectKey) -> Result<Option<ObjectStat>, FlameError> {
        match self.open(key).await? {
            Some(files) => Ok(Some(files.stat()?)),
            None => Ok(None),
        }
    }

    #[cfg(test)]
    async fn read_object(
        &self,
        key: &ObjectKey,
        version: u64,
    ) -> Result<Option<Object>, FlameError> {
        let Some(files) = self.open(key).await? else {
            return Ok(None);
        };
        let key_str = key.to_key().expect("object_id required");
        let stat = files.stat()?;
        if version > stat.version() {
            return Err(FlameError::InvalidState(format!(
                "version {} is beyond object <{}> at {}",
                version,
                key_str,
                stat.version()
            )));
        }
        // Only the parts after `version` are read; version 0 reads all of them.
        let mut parts = Vec::new();
        let records = files.records()?;
        for (index, record) in records.iter().enumerate().skip(version as usize) {
            let data = files.read_range(&key_str, index, 0, record.len as usize)?;
            parts.push(ObjectData {
                creation_time: record.creation_time,
                data,
            });
        }
        Ok(Some(Object {
            version,
            data_type: stat.data_type,
            data: parts,
        }))
    }

    async fn patch_object(
        &self,
        key: &ObjectKey,
        data: &ObjectData,
    ) -> Result<ObjectMetadata, FlameError> {
        let object_dir = self.object_dir(key);
        let key_str = key.to_key().expect("object_id required");
        let data = data.clone();

        tokio::task::spawn_blocking(move || {
            let files = ObjectFiles::open(&object_dir)?.ok_or_else(|| {
                FlameError::NotFound(format!("object <{}> not found, must put first", key_str))
            })?;
            let meta = files.append(&key_str, &data)?;
            tracing::debug!("Appended data to disk: {:?}", object_dir);
            Ok(meta)
        })
        .await
        .map_err(|e| FlameError::Internal(format!("Task join error: {}", e)))?
    }

    async fn delete_objects(&self, key: &ObjectKey) -> Result<(), FlameError> {
        let session_dir = self.session_dir(key);
        let app_dir = self.app_dir(key);
        let dir_to_delete = if key.object_id.is_some() {
            self.object_dir(key)
        } else if key.is_all_sessions() {
            self.app_dir(key)
        } else {
            self.session_dir(key)
        };

        tokio::task::spawn_blocking(move || {
            if dir_to_delete.exists() {
                fs::remove_dir_all(&dir_to_delete)?;
                tracing::debug!("Deleted directory: {:?}", dir_to_delete);
            }
            remove_empty_dir(&session_dir);
            remove_empty_dir(&app_dir);
            Ok(())
        })
        .await
        .map_err(|e| FlameError::Internal(format!("Task join error: {}", e)))?
    }

    async fn scan(&self) -> Result<Vec<(ObjectKey, ObjectStat)>, FlameError> {
        let storage_path = self.storage_path.clone();

        tokio::task::spawn_blocking(move || {
            let mut results = Vec::new();

            if !storage_path.exists() {
                return Ok(results);
            }

            for app_entry in fs::read_dir(&storage_path)? {
                let app_entry = app_entry?;
                let app_path = app_entry.path();

                if !app_path.is_dir() || app_entry.file_name() == LOST_FOUND_DIR {
                    continue;
                }
                let app_name = dir_name(&app_path, "app")?;

                for session_entry in fs::read_dir(&app_path)? {
                    let session_path = session_entry?.path();
                    if !session_path.is_dir() {
                        continue;
                    }
                    let session_id = dir_name(&session_path, "session")?;

                    for object_entry in fs::read_dir(&session_path)? {
                        let object_path = object_entry?.path();
                        if !object_path.is_dir() {
                            continue;
                        }
                        let object_id = dir_name(&object_path, "object")?;
                        // A directory left by an interrupted create.
                        if object_id.starts_with('.') {
                            let _ = fs::remove_dir_all(&object_path);
                            continue;
                        }
                        if let Some(files) = ObjectFiles::open(&object_path)? {
                            let key = ObjectKey {
                                app_name: app_name.clone(),
                                session_id: session_id.clone(),
                                object_id: Some(object_id),
                            };
                            results.push((key, files.stat()?));
                        }
                    }
                    remove_empty_dir(&session_path);
                }
                remove_empty_dir(&app_path);
            }

            tracing::info!("Scanned {} objects on disk", results.len());
            Ok(results)
        })
        .await
        .map_err(|e| FlameError::Internal(format!("Task join error: {}", e)))?
    }
}

fn dir_name(path: &Path, what: &str) -> Result<String, FlameError> {
    path.file_name()
        .and_then(|n| n.to_str())
        .map(str::to_string)
        .ok_or_else(|| FlameError::Internal(format!("Invalid {} directory name", what)))
}

fn remove_empty_dir(path: &Path) {
    match fs::remove_dir(path) {
        Ok(()) => {
            tracing::debug!("Removed empty cache directory: {:?}", path);
        }
        Err(error)
            if matches!(
                error.kind(),
                std::io::ErrorKind::NotFound | std::io::ErrorKind::DirectoryNotEmpty
            ) => {}
        Err(error) => tracing::warn!(
            path = %path.display(),
            error = %error,
            "failed to remove empty cache directory"
        ),
    }
}

fn unique_temp_path(path: &Path) -> PathBuf {
    path.with_file_name(format!(
        ".{}.tmp-{}",
        path.file_name().unwrap().to_string_lossy(),
        uuid::Uuid::new_v4()
    ))
}

/// Forward chunks to the blocking writer, then send the commit marker. The
/// sender is dropped on return, which aborts the write unless it was committed.
async fn feed_chunks(
    tx: mpsc::Sender<Option<Bytes>>,
    mut chunks: BoxStream<'_, Result<Bytes, FlameError>>,
) -> Result<(), FlameError> {
    while let Some(chunk) = chunks.next().await {
        if tx.send(Some(chunk?)).await.is_err() {
            // The writer stopped early; its error is reported by the caller.
            return Ok(());
        }
    }
    let _ = tx.send(None).await;
    Ok(())
}

/// Create an object from received chunks in `temp_dir`: the payload is the
/// first data in `data.bin`, then the index and metadata, all synced; the directory
/// is renamed to `object_dir` last, so an object appears complete or not at all.
/// Returns the payload length.
fn create_object(
    temp_dir: &Path,
    object_dir: &Path,
    meta: &ObjectMetadata,
    mut chunks: mpsc::Receiver<Option<Bytes>>,
) -> Result<u64, FlameError> {
    fs::create_dir(temp_dir)?;
    let file = fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(temp_dir.join(DATA_FILE))?;
    let mut writer = BufWriter::new(file);
    let mut len = 0u64;
    loop {
        match chunks.blocking_recv() {
            Some(Some(chunk)) => {
                writer.write_all(&chunk)?;
                len += chunk.len() as u64;
            }
            Some(None) => break,
            None => return Err(FlameError::Internal("object write aborted".to_string())),
        }
    }
    writer
        .into_inner()
        .map_err(|e| e.into_error())?
        .sync_all()?;

    let record = Record {
        offset: 0,
        len,
        creation_time: meta.creation_time,
    };
    write_synced(&temp_dir.join(INDEX_FILE), &record.encode())?;
    let metadata = StoredMetadata {
        data_type: meta.data_type.clone(),
    };
    let metadata = serde_json::to_vec(&metadata)
        .map_err(|e| FlameError::Internal(format!("Failed to encode metadata: {}", e)))?;
    write_synced(&temp_dir.join(METADATA_FILE), &metadata)?;

    fs::rename(temp_dir, object_dir).map_err(|e| match e.kind() {
        std::io::ErrorKind::AlreadyExists | std::io::ErrorKind::DirectoryNotEmpty => {
            FlameError::AlreadyExist(format!("object {} already exists", object_dir.display()))
        }
        _ => e.into(),
    })?;
    Ok(len)
}

fn write_synced(path: &Path, bytes: &[u8]) -> Result<(), FlameError> {
    let mut file = fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(path)?;
    file.write_all(bytes)?;
    file.sync_all()?;
    Ok(())
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

    fn object(data: &[u8]) -> Object {
        Object::new().append_data(data.to_vec())
    }

    fn part(data: &[u8]) -> ObjectData {
        ObjectData::new().with_data(data.to_vec())
    }

    fn chunk_stream(
        chunks: Vec<Result<Bytes, FlameError>>,
    ) -> BoxStream<'static, Result<Bytes, FlameError>> {
        Box::pin(futures::stream::iter(chunks))
    }

    #[tokio::test]
    async fn test_disk_storage_layout_and_roundtrip() {
        let temp_dir = tempdir().unwrap();
        let storage = DiskStorage::new(temp_dir.path().to_path_buf()).unwrap();
        let key = test_key("test-app", "test-session", "obj1");
        let mut object = object(&[1, 2, 3, 4, 5]).with_data_type("arrow_table.zstd");
        object.data[0].creation_time = 1_234_567;
        storage.write_object(&key, &object).await.unwrap();

        let dir = storage.object_dir(&key);
        assert_eq!(fs::read(dir.join(DATA_FILE)).unwrap(), [1, 2, 3, 4, 5]);
        assert_eq!(fs::read(dir.join(INDEX_FILE)).unwrap().len(), RECORD_LEN);
        let metadata: StoredMetadata =
            serde_json::from_slice(&fs::read(dir.join(METADATA_FILE)).unwrap()).unwrap();
        assert_eq!(metadata.data_type, "arrow_table.zstd");

        let loaded = storage.read_object(&key, 0).await.unwrap().unwrap();
        assert_eq!(loaded.data_type, "arrow_table.zstd");
        assert_eq!(loaded.data.len(), 1);
        assert_eq!(loaded.current_version(), 1);
        assert_eq!(loaded.data[0].creation_time, 1_234_567);
        assert_eq!(loaded.opaque_data().unwrap(), &[1, 2, 3, 4, 5]);
    }

    #[tokio::test]
    async fn test_disk_storage_create_only() {
        let temp_dir = tempdir().unwrap();
        let storage = DiskStorage::new(temp_dir.path().to_path_buf()).unwrap();
        let key = test_key("app", "session", "obj");
        storage.write_object(&key, &object(&[1])).await.unwrap();

        let again = storage.write_object(&key, &object(&[2])).await;
        assert!(matches!(again, Err(FlameError::AlreadyExist(_))));

        // The object is unchanged and nothing else is left in the session.
        let stat = storage.stat(&key).await.unwrap().unwrap();
        assert_eq!((stat.version(), stat.size()), (1, 1));
        assert_eq!(fs::read_dir(storage.session_dir(&key)).unwrap().count(), 1);

        // After a delete the key can be created again.
        storage.delete_objects(&key).await.unwrap();
        storage.write_object(&key, &object(&[3])).await.unwrap();
        assert_eq!(storage.stat(&key).await.unwrap().unwrap().size(), 1);
    }

    #[tokio::test]
    async fn test_disk_storage_append() {
        let temp_dir = tempdir().unwrap();
        let storage = DiskStorage::new(temp_dir.path().to_path_buf()).unwrap();
        let key = test_key("test-app", "test-session", "obj1");
        let mut object = object(&[1, 2, 3]).with_data_type("arrow_table.zstd");
        object.data[0].creation_time = 42;
        storage.write_object(&key, &object).await.unwrap();

        let meta = storage.patch_object(&key, &part(&[4, 5, 6])).await.unwrap();
        assert_eq!((meta.size, meta.version), (6, 2));
        // The payloads follow each other in one data file with one index entry each.
        let dir = storage.object_dir(&key);
        assert_eq!(fs::read(dir.join(DATA_FILE)).unwrap(), [1, 2, 3, 4, 5, 6]);
        assert_eq!(
            fs::read(dir.join(INDEX_FILE)).unwrap().len(),
            2 * RECORD_LEN
        );

        let loaded = storage.read_object(&key, 0).await.unwrap().unwrap();
        assert_eq!(loaded.data_type, "arrow_table.zstd");
        assert_eq!(loaded.data.len(), 2);
        assert_eq!(loaded.current_version(), 2);
        assert_eq!(&loaded.data[1..][0].data[..], &[4, 5, 6]);
        let recovered = storage.scan().await.unwrap();
        let stat = &recovered[0].1;
        assert_eq!(stat.version(), 2);
        assert_eq!((stat.creation_time(), stat.size()), (42, 6));
        let located: Vec<_> = stat.records.iter().map(|r| (r.offset, r.len)).collect();
        assert_eq!(located, [(0, 3), (3, 3)]);
        assert_eq!(stat.data_type, "arrow_table.zstd");
    }

    #[tokio::test]
    async fn test_disk_storage_reads_only_the_data_after_a_version() {
        let temp_dir = tempdir().unwrap();
        let storage = DiskStorage::new(temp_dir.path().to_path_buf()).unwrap();
        let key = test_key("app", "session", "obj");
        storage.write_object(&key, &object(b"first")).await.unwrap();
        for data in [b"one".as_slice(), b"two".as_slice()] {
            storage.patch_object(&key, &part(data)).await.unwrap();
        }

        let all = storage.read_object(&key, 0).await.unwrap().unwrap();
        assert_eq!((all.version, all.data.len()), (0, 3));
        let tail = storage.read_object(&key, 1).await.unwrap().unwrap();
        assert_eq!((tail.version, tail.current_version()), (1, 3));
        assert_eq!(&tail.data[0].data[..], b"one");
        let none = storage.read_object(&key, 3).await.unwrap().unwrap();
        assert!(none.data.is_empty());
        assert!(matches!(
            storage.read_object(&key, 4).await,
            Err(FlameError::InvalidState(_))
        ));
    }

    #[tokio::test]
    async fn test_disk_storage_append_limit_and_missing_object() {
        let temp_dir = tempdir().unwrap();
        let storage = DiskStorage::new(temp_dir.path().to_path_buf()).unwrap();
        let key = test_key("app", "session", "obj");
        assert!(matches!(
            storage.patch_object(&key, &part(&[1])).await,
            Err(FlameError::NotFound(_))
        ));

        storage.write_object(&key, &object(&[])).await.unwrap();
        for _ in 0..MAX_APPENDS_PER_OBJECT {
            storage.patch_object(&key, &part(&[1])).await.unwrap();
        }
        assert!(matches!(
            storage.patch_object(&key, &part(&[1])).await,
            Err(FlameError::InvalidState(_))
        ));
    }

    #[tokio::test]
    async fn test_disk_storage_ignores_uncommitted_appended_bytes() {
        let temp_dir = tempdir().unwrap();
        let storage = DiskStorage::new(temp_dir.path().to_path_buf()).unwrap();
        let key = test_key("app", "session", "obj");
        storage.write_object(&key, &object(b"first")).await.unwrap();

        // An append that wrote data but crashed before its index record, and a
        // torn index entry.
        let dir = storage.object_dir(&key);
        let mut data = fs::read(dir.join(DATA_FILE)).unwrap();
        data.extend_from_slice(b"orphan");
        fs::write(dir.join(DATA_FILE), data).unwrap();
        let mut index = fs::read(dir.join(INDEX_FILE)).unwrap();
        index.extend_from_slice(&[7u8; 5]);
        fs::write(dir.join(INDEX_FILE), index).unwrap();

        let stat = storage.stat(&key).await.unwrap().unwrap();
        assert_eq!((stat.version(), stat.size()), (1, 5));

        // The next append goes after the orphan bytes and overwrites the
        // torn record.
        storage.patch_object(&key, &part(b"next")).await.unwrap();
        assert_eq!(storage.stat(&key).await.unwrap().unwrap().version(), 2);
        assert_eq!(
            storage.read_range(&key, 1, 0, 4).await.unwrap().as_ref(),
            b"next"
        );
        assert_eq!(
            storage.read_range(&key, 0, 0, 5).await.unwrap().as_ref(),
            b"first"
        );
    }

    #[tokio::test]
    async fn test_disk_storage_rejects_corrupt_objects() {
        let temp_dir = tempdir().unwrap();
        let storage = DiskStorage::new(temp_dir.path().to_path_buf()).unwrap();

        let truncated = test_key("app", "session", "truncated");
        storage
            .write_object(&truncated, &object(b"payload"))
            .await
            .unwrap();
        let data = fs::OpenOptions::new()
            .write(true)
            .open(storage.object_dir(&truncated).join(DATA_FILE))
            .unwrap();
        data.set_len(3).unwrap();
        assert!(matches!(
            storage.stat(&truncated).await,
            Err(FlameError::InvalidState(_))
        ));

        let bad_metadata = test_key("app", "session", "bad-metadata");
        storage
            .write_object(&bad_metadata, &object(b"x"))
            .await
            .unwrap();
        fs::write(storage.object_dir(&bad_metadata).join(METADATA_FILE), b"{").unwrap();
        assert!(matches!(
            storage.stat(&bad_metadata).await,
            Err(FlameError::InvalidState(_))
        ));

        let bad_index = test_key("app", "session", "bad-index");
        storage
            .write_object(&bad_index, &object(b"x"))
            .await
            .unwrap();
        fs::write(storage.object_dir(&bad_index).join(INDEX_FILE), b"").unwrap();
        assert!(matches!(
            storage.stat(&bad_index).await,
            Err(FlameError::InvalidState(_))
        ));
    }

    #[tokio::test]
    async fn test_disk_storage_delete_objects() {
        let temp_dir = tempdir().unwrap();
        let storage = DiskStorage::new(temp_dir.path().to_path_buf()).unwrap();

        let key1 = test_key("test-app", "test-session", "obj1");
        let key2 = test_key("test-app", "test-session", "obj2");
        storage
            .write_object(&key1, &object(&[1, 2, 3]))
            .await
            .unwrap();
        storage
            .write_object(&key2, &object(&[4, 5, 6]))
            .await
            .unwrap();

        let delete_key = ObjectKey::from_path("test-app/test-session").unwrap();
        storage.delete_objects(&delete_key).await.unwrap();

        assert!(storage.stat(&key1).await.unwrap().is_none());
        assert!(storage.stat(&key2).await.unwrap().is_none());
        assert!(!storage.app_dir(&key1).exists());
        assert!(temp_dir.path().exists());
    }

    #[tokio::test]
    async fn test_disk_storage_delete_exact_object() {
        let temp_dir = tempdir().unwrap();
        let storage = DiskStorage::new(temp_dir.path().to_path_buf()).unwrap();

        let key1 = test_key("test-app", "test-session", "obj1");
        let key2 = test_key("test-app", "test-session", "obj2");
        storage
            .write_object(&key1, &object(&[1, 2, 3]))
            .await
            .unwrap();
        storage
            .write_object(&key2, &object(&[4, 5, 6]))
            .await
            .unwrap();
        storage.patch_object(&key1, &part(&[7])).await.unwrap();

        storage.delete_objects(&key1).await.unwrap();

        assert!(storage.stat(&key1).await.unwrap().is_none());
        assert!(!storage.object_dir(&key1).exists());
        assert!(storage.stat(&key2).await.unwrap().is_some());
        assert!(storage.session_dir(&key2).exists());

        storage.delete_objects(&key2).await.unwrap();
        assert!(!storage.session_dir(&key2).exists());
        assert!(!storage.app_dir(&key2).exists());
        assert!(temp_dir.path().exists());
        storage.delete_objects(&key2).await.unwrap();
    }

    #[tokio::test]
    async fn test_disk_storage_pruning_preserves_other_sessions() {
        let temp_dir = tempdir().unwrap();
        let storage = DiskStorage::new(temp_dir.path().to_path_buf()).unwrap();
        let key1 = test_key("app", "session1", "obj");
        let key2 = test_key("app", "session2", "obj");
        for key in [&key1, &key2] {
            storage.write_object(key, &object(&[1])).await.unwrap();
        }

        storage.delete_objects(&key1).await.unwrap();
        assert!(!storage.session_dir(&key1).exists());
        assert!(storage.app_dir(&key1).exists());
        assert!(storage.stat(&key2).await.unwrap().is_some());

        storage
            .delete_objects(&ObjectKey::from_path("app/*").unwrap())
            .await
            .unwrap();
        assert!(!storage.app_dir(&key1).exists());
        assert!(temp_dir.path().exists());
    }

    #[tokio::test]
    async fn test_disk_storage_scan_prunes_empty_and_interrupted_directories() {
        let temp_dir = tempdir().unwrap();
        let storage = DiskStorage::new(temp_dir.path().to_path_buf()).unwrap();
        let key = test_key("live-app", "live-session", "obj");
        storage.write_object(&key, &object(&[1])).await.unwrap();
        for directory in [
            "old-app/pkg",
            "old-app/session",
            "live-app/old-session",
            "live-app/live-session/.obj.tmp-1",
            LOST_FOUND_DIR,
        ] {
            fs::create_dir_all(temp_dir.path().join(directory)).unwrap();
        }

        assert_eq!(storage.scan().await.unwrap().len(), 1);
        assert!(!temp_dir.path().join("old-app").exists());
        assert!(!temp_dir.path().join("live-app/old-session").exists());
        assert!(!temp_dir
            .path()
            .join("live-app/live-session/.obj.tmp-1")
            .exists());
        assert!(temp_dir.path().join(LOST_FOUND_DIR).exists());
        assert!(storage.stat(&key).await.unwrap().is_some());
    }

    #[tokio::test]
    async fn test_disk_storage_pruning_with_independent_application_writes() {
        let temp_dir = tempdir().unwrap();
        let storage = Arc::new(DiskStorage::new(temp_dir.path().to_path_buf()).unwrap());
        let mut tasks = Vec::new();
        for i in 0..32 {
            let storage = Arc::clone(&storage);
            tasks.push(tokio::spawn(async move {
                let stale = test_key(&format!("stale-app-{i}"), "session", "object");
                let retained = test_key(&format!("active-app-{i}"), "session", "object");
                storage.write_object(&stale, &object(&[1])).await.unwrap();
                let object = object(&[2]);
                let (deleted, written) = tokio::join!(
                    storage.delete_objects(&stale),
                    storage.write_object(&retained, &object),
                );
                deleted.unwrap();
                written.unwrap();
                retained
            }));
        }
        for task in tasks {
            let key = task.await.unwrap();
            assert!(storage.stat(&key).await.unwrap().is_some());
        }
        assert_eq!(storage.scan().await.unwrap().len(), 32);
    }

    #[tokio::test]
    async fn test_disk_storage_write_stream_and_ranges() {
        let temp_dir = tempdir().unwrap();
        let storage = DiskStorage::new(temp_dir.path().to_path_buf()).unwrap();
        let key = test_key("app", "session", "streamed");

        let chunks = vec![
            Ok(Bytes::from_static(b"hello ")),
            Ok(Bytes::new()),
            Ok(Bytes::from_static(b"world")),
        ];
        let written = storage
            .write_stream(
                &key,
                ObjectMetadata::new().with_version(3).with_creation_time(77),
                chunk_stream(chunks),
            )
            .await
            .unwrap();
        assert_eq!(
            (written.version, written.size, written.creation_time),
            (3, 11, 77)
        );

        let stat = storage.stat(&key).await.unwrap().unwrap();
        assert_eq!(
            (stat.version(), stat.records[0].len, stat.size()),
            (1, 11, 11)
        );
        assert_eq!(stat.data_type, "raw");
        let read = |part, offset, len| storage.read_range(&key, part, offset, len);
        assert_eq!(read(0, 6, 5).await.unwrap().as_ref(), b"world");
        assert!(read(0, 6, 6).await.is_err());

        // Part 0 is the first data; part `i` is the i-th data appended.
        for data in [b"one".as_slice(), b"three".as_slice()] {
            storage.patch_object(&key, &part(data)).await.unwrap();
        }
        assert_eq!(read(2, 1, 3).await.unwrap().as_ref(), b"hre");
        assert!(read(3, 0, 0).await.is_err());
        assert!(matches!(
            storage
                .read_range(&test_key("app", "session", "none"), 0, 0, 0)
                .await,
            Err(FlameError::NotFound(_))
        ));
        assert!(storage
            .stat(&test_key("app", "session", "none"))
            .await
            .unwrap()
            .is_none());
    }

    #[tokio::test]
    async fn test_disk_storage_write_stream_aborts_on_error() {
        let temp_dir = tempdir().unwrap();
        let storage = DiskStorage::new(temp_dir.path().to_path_buf()).unwrap();
        let key = test_key("app", "session", "broken");

        let chunks = vec![
            Ok(Bytes::from_static(b"partial")),
            Err(FlameError::InvalidConfig("bad chunk".to_string())),
        ];
        let result = storage
            .write_stream(
                &key,
                ObjectMetadata::new().with_version(2),
                chunk_stream(chunks),
            )
            .await;
        assert!(matches!(result, Err(FlameError::InvalidConfig(_))));

        // Nothing is stored and no temporary directory is left.
        assert!(storage.stat(&key).await.unwrap().is_none());
        assert_eq!(fs::read_dir(storage.session_dir(&key)).unwrap().count(), 0);
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
        let key = key("opaque-0");
        let mut state = 1u64;
        let payload: Vec<u8> = (0..8 * 1024 * 1024)
            .map(|_| {
                state ^= state << 13;
                state ^= state >> 7;
                state ^= state << 17;
                state as u8
            })
            .collect();
        let object = Object::new().append_data(payload);

        let writes = 8;
        let started = Instant::now();
        for index in 0..writes {
            let key = self::key(&format!("opaque-{index}"));
            storage.write_object(&key, &object).await.unwrap();
        }
        let write_time = started.elapsed();

        let reads = 16;
        let started = Instant::now();
        for _ in 0..reads {
            black_box(storage.read_object(&key, 0).await.unwrap().unwrap());
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
        let key = key("appended");
        storage
            .write_object(&key, &Object::new().append_data(vec![0; 1024]))
            .await
            .unwrap();

        let mut first_hundred = None;
        let mut window_started = Instant::now();
        for index in 0..600 {
            storage
                .patch_object(&key, &ObjectData::new().with_data(vec![index as u8; 1024]))
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
            "append, 1 KiB: first_100={:?}/op last_100={:?}/op",
            first_hundred.unwrap() / 100,
            last_hundred / 100
        );
    }
}
