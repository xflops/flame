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

use crate::model::{Chunk, Object, ObjectFilter, ObjectKey, ObjectMetadata, Part};
use async_trait::async_trait;
use bytes::Bytes;
use common::{ctx::FlameCachePool, FlameError};
use futures::{Stream, StreamExt};
use std::collections::HashMap;
use std::fs;
use std::io::Write;
use std::os::unix::fs::FileExt;
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::sync::{Arc, Mutex};

mod dao;
mod object_reader;
mod object_writer;
mod part_cache;
mod part_reader;
mod part_writer;

use dao::{ObjectDao, ObjectMetadataDao, PartDao};
use object_reader::ObjectReader;
pub(crate) use object_writer::ObjectWriter;
use part_cache::PartCache;
pub(crate) use part_reader::PartReader;
pub(crate) use part_writer::PartWriter;

pub(crate) const MAX_APPENDS_PER_OBJECT: u64 = 1000;
pub(crate) const CHUNK_SIZE: usize = 1024 * 1024;

/// Allocate parallel immutable parts, expose committed logical views, and allow GC to unlink expired session data.
#[async_trait]
pub(crate) trait StorageEngine: Send + Sync + 'static {
    /// Capture one fixed prefix, refresh its metadata, and lazily stream its filesystem payload chunks.
    async fn read_object(&self, metadata: &mut ObjectMetadata) -> Result<ChunkStream, FlameError>;
    /// Capture patches after the supplied version and replace metadata with the selected latest committed end.
    async fn read_patch(&self, metadata: &mut ObjectMetadata) -> Result<ChunkStream, FlameError>;
    /// Consume object bytes, creating missing objects and publishing cached state after filesystem commit.
    async fn write_object(
        &self,
        key: &ObjectKey,
        data_type: &str,
        size: u64,
        chunks: ByteStream,
    ) -> Result<ObjectMetadata, FlameError>;
    /// Consume patch bytes with the same create-if-missing allocation and durable publication.
    async fn write_patch(
        &self,
        key: &ObjectKey,
        data_type: &str,
        size: u64,
        chunks: ByteStream,
    ) -> Result<ObjectMetadata, FlameError>;
    /// List matching metadata, optionally restricted to a committed part prefix.
    async fn list(&self, filter: &ObjectFilter) -> Result<Vec<ObjectMetadata>, FlameError>;
    /// Delete the exact object or all objects selected by an application/session key.
    async fn delete_objects(&self, key: &ObjectKey) -> Result<(), FlameError>;
}

pub(crate) type StorageEnginePtr = Box<dyn StorageEngine>;

/// Carry domain payload chunks across the storage boundary without exposing RPC frame types.
pub(crate) type ByteStream =
    Pin<Box<dyn Stream<Item = Result<Bytes, FlameError>> + Send + 'static>>;

/// Own captured filesystem readers and expose only domain chunks through EOF, failure, or stream destruction.
pub(crate) type ChunkStream =
    Pin<Box<dyn Stream<Item = Result<Chunk, FlameError>> + Send + 'static>>;

const METADATA_FILE: &str = "metadata.json";
const INDEX_FILE: &str = "index.bin";
const DATA_FILE: &str = "data.bin";
const STAGING_DIR: &str = ".uploads";
const RECORD_LEN: usize = 72;
type Catalog = Arc<Mutex<HashMap<String, ObjectDao>>>;

struct ObjectFiles {
    object: ObjectDao,
}
impl ObjectFiles {
    /// Load complete descriptors without modifying filesystem state or interpreting interrupted writes.
    fn load(dir: &Path) -> Result<Option<Self>, FlameError> {
        let index = match fs::File::open(dir.join(INDEX_FILE)) {
            Ok(file) => file,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(error) => return Err(error.into()),
        };
        let metadata: ObjectMetadataDao =
            serde_json::from_slice(&fs::read(dir.join(METADATA_FILE))?).map_err(|error| {
                FlameError::InvalidState(format!(
                    "invalid object metadata in {}: {error}",
                    dir.display()
                ))
            })?;
        let size = index.metadata()?.len() as usize;
        let complete = size / RECORD_LEN * RECORD_LEN;
        let mut bytes = vec![0; complete];
        index.read_exact_at(&mut bytes, 0)?;
        let parts = bytes
            .as_chunks::<RECORD_LEN>()
            .0
            .iter()
            .map(PartDao::decode)
            .collect();
        Ok(Some(Self {
            object: ObjectDao {
                metadata,
                parts,
                cached_version: 0,
            },
        }))
    }

    /// Load and validate the complete committed view without repairing or rewriting records.
    #[cfg(test)]
    fn open(dir: &Path) -> Result<Option<Self>, FlameError> {
        let Some(mut files) = Self::load(dir)? else {
            return Ok(None);
        };
        files.object.refresh_version()?;
        files.validate_extents(dir)?;
        Ok(Some(files))
    }

    /// Repair interrupted states and torn index tails explicitly during storage startup.
    fn recover(dir: &Path) -> Result<Option<Self>, FlameError> {
        let Some(mut files) = Self::load(dir)? else {
            return Ok(None);
        };
        // Retire unacknowledged logical tails before checking extents to preserve acknowledged data.
        files.object.recover_parts()?;
        let index = fs::OpenOptions::new()
            .write(true)
            .open(dir.join(INDEX_FILE))?;
        index.set_len(files.object.parts.len() as u64 * RECORD_LEN as u64)?;
        for (ordinal, part) in files.object.parts.iter().enumerate() {
            index.write_all_at(&part.encode(), ordinal as u64 * RECORD_LEN as u64)?;
        }
        index.sync_data()?;
        files.validate_extents(dir)?;
        Ok(Some(files))
    }

    /// Reject retained committed descriptors whose payload extents do not fit in the data file.
    fn validate_extents(&self, dir: &Path) -> Result<(), FlameError> {
        let data_len = fs::File::open(dir.join(DATA_FILE))?.metadata()?.len();
        if self
            .object
            .parts
            .iter()
            .filter(|part| part.state == dao::PartState::Committed)
            .any(|part| {
                part.offset
                    .checked_add(part.len)
                    .is_none_or(|end| end > data_len)
            })
        {
            return Err(FlameError::DataLoss(format!(
                "truncated object in {}",
                dir.display()
            )));
        }
        Ok(())
    }
}

#[derive(Clone)]
pub struct FsStorage {
    storage_path: PathBuf,
    catalog: Catalog,
    part_cache: Arc<PartCache>,
}
impl FsStorage {
    /// Open an uncached filesystem fixture to exercise the authoritative disk path.
    #[cfg(test)]
    pub fn new(storage_path: PathBuf) -> Result<Self, FlameError> {
        Self::with_cache(
            storage_path,
            FlameCachePool {
                size: 0,
                ..Default::default()
            },
        )
    }

    /// Recover filesystem storage and initialize a shared byte-bounded part cache.
    pub fn with_cache(storage_path: PathBuf, pool: FlameCachePool) -> Result<Self, FlameError> {
        fs::create_dir_all(&storage_path)?;
        let catalog = recover(&storage_path)?;
        Ok(Self {
            storage_path,
            catalog: Arc::new(Mutex::new(catalog)),
            part_cache: Arc::new(PartCache::new(pool)),
        })
    }
    /// Locate the session directory containing this object.
    fn session_dir(&self, key: &ObjectKey) -> PathBuf {
        self.storage_path.join(&key.app_name).join(&key.session_id)
    }
    /// Locate the directory for a complete object key.
    fn object_dir(&self, key: &ObjectKey) -> PathBuf {
        self.session_dir(key)
            .join(key.object_id.as_ref().expect("complete object key"))
    }
}

/// Initialize one object's filesystem metadata before inserting its first Writing descriptor in storage's catalog.
fn create_object_files(dir: &Path, metadata: &ObjectMetadataDao) -> Result<(), FlameError> {
    fs::create_dir_all(dir.parent().expect("object session"))?;
    fs::create_dir(dir)?;
    let result = (|| {
        let bytes = serde_json::to_vec(metadata)
            .map_err(|error| FlameError::Internal(error.to_string()))?;
        let mut file = fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(dir.join(METADATA_FILE))?;
        file.write_all(&bytes)?;
        fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(dir.join(INDEX_FILE))?;
        fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(dir.join(DATA_FILE))?;
        Ok(())
    })();
    if result.is_err() {
        let _ = fs::remove_dir_all(dir);
    }
    result
}

/// Recover committed descriptors and clean abandoned staging before storage serves requests.
fn recover(root: &Path) -> Result<HashMap<String, ObjectDao>, FlameError> {
    let mut items = HashMap::new();
    for app in fs::read_dir(root)? {
        let app = app?;
        if !app.path().is_dir() || app.file_name() == "lost+found" {
            continue;
        }
        for session in fs::read_dir(app.path())? {
            let session = session?;
            if !session.path().is_dir() {
                continue;
            }
            for object in fs::read_dir(session.path())? {
                let object = object?;
                if !object.path().is_dir() {
                    continue;
                }
                if object.file_name() == STAGING_DIR {
                    fs::remove_dir_all(object.path())?;
                    continue;
                }
                let object_path = object.path();
                let relative = object_path
                    .strip_prefix(root)
                    .map_err(|error| FlameError::InvalidState(error.to_string()))?;
                let key = ObjectKey::try_from(relative.to_str().ok_or_else(|| {
                    FlameError::InvalidState("object path is not UTF-8".to_string())
                })?)?;
                if fs::metadata(object.path().join(INDEX_FILE))
                    .map(|metadata| metadata.len() == 0)
                    .unwrap_or(true)
                {
                    fs::remove_dir_all(object.path())?;
                    continue;
                }
                if let Some(files) = ObjectFiles::recover(&object.path())? {
                    if files.object.version() == 0 {
                        fs::remove_dir_all(object.path())?;
                    } else {
                        items.insert(key.to_key().expect("complete key"), files.object);
                    }
                }
            }
            remove_empty_dir(&session.path());
        }
        remove_empty_dir(&app.path());
    }
    Ok(items)
}

/// Translate a blocking worker failure into a cache internal error.
fn joined<T>(result: Result<T, tokio::task::JoinError>) -> Result<T, FlameError> {
    result.map_err(|error| FlameError::Internal(format!("storage task failed: {error}")))
}
/// Describe a poisoned published-metadata catalog lock.
fn catalog_error() -> FlameError {
    FlameError::Internal("storage catalog poisoned".to_string())
}
/// Project the readable committed sequence, skipping stale slots and never waiting for active writers.
fn project_metadata(
    catalog: &Catalog,
    key: &str,
    version: Option<u64>,
) -> Result<Option<ObjectMetadata>, FlameError> {
    let objects = catalog.lock().map_err(|_| catalog_error())?;
    let Some(object) = objects.get(key).filter(|object| object.version() > 0) else {
        return Ok(None);
    };
    let mut metadata = object.metadata(key, "");
    if let Some(version) = version {
        if version > metadata.version {
            return Err(FlameError::InvalidConfig(
                "requested version is ahead of object".into(),
            ));
        }
        metadata.version = version;
        metadata.size = object
            .visible_parts()
            .take(version as usize)
            .map(|part| part.len)
            .sum();
    }
    Ok(Some(metadata))
}

/// Reject a changed incarnation after handles are opened without retaining reader/deletion locks.
fn validate_uuid(dir: &Path, expected: &str) -> Result<(), FlameError> {
    let metadata: ObjectMetadataDao =
        serde_json::from_slice(&fs::read(dir.join(METADATA_FILE))?)
            .map_err(|error| FlameError::DataLoss(format!("invalid object metadata: {error}")))?;
    if metadata.uuid != expected {
        return Err(FlameError::InvalidVersion("object UUID changed".into()));
    }
    Ok(())
}

impl FsStorage {
    /// Unlink committed objects directly; captured descriptors remain readable and GC never waits for active I/O.
    fn delete_committed(&self, key: &ObjectKey) -> Result<(), FlameError> {
        let keys: Vec<_> = if let Some(key) = key.to_key() {
            vec![key]
        } else {
            self.catalog
                .lock()
                .map_err(|_| catalog_error())?
                .keys()
                .filter(|candidate| key.matches(candidate))
                .cloned()
                .collect()
        };
        for key in keys {
            let path = self.storage_path.join(&key);
            {
                let mut published = self.catalog.lock().map_err(|_| catalog_error())?;
                match fs::remove_dir_all(&path) {
                    Ok(()) => {}
                    Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                    Err(error) => return Err(error.into()),
                }
                if let Some(object) = published.remove(&key) {
                    self.part_cache.remove_object(&object.metadata.uuid);
                }
            }
            if let Some(session) = path.parent() {
                remove_empty_dir(&session.join(STAGING_DIR));
                remove_empty_dir(session);
                if let Some(app) = session.parent() {
                    remove_empty_dir(app);
                }
            }
        }
        Ok(())
    }

    /// Merge cached and filesystem reader maps in captured order, opening only misses.
    fn part_readers(
        &self,
        metadata: &ObjectMetadata,
        records: &[PartDao],
    ) -> Result<Vec<Box<dyn PartReader>>, FlameError> {
        if records.is_empty() {
            return Ok(Vec::new());
        }
        let cached: HashMap<_, _> = self
            .part_cache
            .patch_readers(&metadata.uuid)
            .into_iter()
            .map(|reader| (reader.part().part, reader))
            .collect();
        let misses: Vec<_> = records
            .iter()
            .filter(|part| !cached.contains_key(&part.part))
            .copied()
            .collect();
        let disk = self.open_readers(&metadata.key, &misses)?;
        validate_uuid(&self.storage_path.join(&metadata.key), &metadata.uuid)?;
        records
            .iter()
            .map(|part| {
                if let Some(reader) = cached.get(&part.part) {
                    Ok(Box::new(reader.clone()) as Box<dyn PartReader>)
                } else if let Some(reader) = disk.get(&part.part) {
                    Ok(Box::new(reader.clone()) as Box<dyn PartReader>)
                } else {
                    Err(FlameError::DataLoss(format!(
                        "captured reader missing for part {}",
                        part.part
                    )))
                }
            })
            .collect()
    }

    /// Stream direct memory/filesystem readers and cache complete verified misses without another reader wrapper.
    fn read_stream(&self, reader: ObjectReader) -> ChunkStream {
        let key = reader.metadata().key.clone();
        let uuid = reader.metadata().uuid.clone();
        let parts: HashMap<_, _> = reader
            .parts()
            .iter()
            .filter(|part| {
                self.part_cache.cacheable(part.size)
                    && self.part_cache.get_patch(&uuid, part.part).is_none()
            })
            .map(|part| (part.part, *part))
            .collect();
        if parts.is_empty() {
            return reader.into_stream();
        }
        let storage = self.clone();
        let mut loading: Option<Vec<Bytes>> = None;
        Box::pin(reader.into_stream().inspect(move |result| {
            let Ok(chunk) = result else {
                loading = None;
                return;
            };
            let Some(part) = parts.get(&chunk.part) else {
                loading = None;
                return;
            };
            if chunk.offset == 0 {
                loading = Some(Vec::new());
            }
            if let Some(chunks) = &mut loading {
                chunks.push(chunk.data.clone());
            }
            // FsReader verifies the checksum before returning the final chunk.
            if chunk.offset + chunk.data.len() as u64 == part.size {
                if let Some(chunks) = loading.take() {
                    // Optional cache publication requires a live matching incarnation.
                    if let Ok(objects) = storage.catalog.lock() {
                        if objects
                            .get(&key)
                            .is_some_and(|object| object.metadata.uuid == uuid)
                        {
                            storage.part_cache.insert_patch(&uuid, *part, chunks);
                        }
                    }
                }
            }
        }))
    }

    #[cfg(test)]
    /// Capture one part reader through the shared snapshot-opening path.
    async fn reader(&self, key: &ObjectKey, part: u64) -> Result<Box<dyn PartReader>, FlameError> {
        let mut readers = self.readers(key, &[part]).await?;
        Ok(readers.remove(0))
    }

    #[cfg(test)]
    /// Open captured file handles at the catalog's committed boundary without retaining read registrations.
    async fn readers(
        &self,
        key: &ObjectKey,
        parts: &[u64],
    ) -> Result<Vec<Box<dyn PartReader>>, FlameError> {
        if parts.is_empty() {
            return Ok(Vec::new());
        }
        let key_string = key
            .to_key()
            .ok_or_else(|| FlameError::InvalidConfig("complete object key required".into()))?;
        let storage = self.clone();
        let selected = {
            let objects = self.catalog.lock().map_err(|_| catalog_error())?;
            let object = objects
                .get(&key_string)
                .ok_or_else(|| FlameError::NotFound(format!("object <{key_string}> not found")))?;
            let visible: Vec<_> = object.visible_parts().copied().collect();
            let selected = parts
                .iter()
                .map(|part| {
                    visible
                        .get(*part as usize)
                        .copied()
                        .ok_or_else(|| FlameError::NotFound(format!("part {part} not found")))
                })
                .collect::<Result<Vec<_>, _>>()?;
            (object.metadata(&key_string, ""), selected)
        };
        joined(
            tokio::task::spawn_blocking(move || storage.part_readers(&selected.0, &selected.1))
                .await,
        )?
    }

    /// Select committed metadata and open an immutable file view, failing if GC removes or replaces it during capture.
    async fn capture(
        &self,
        selected: &ObjectMetadata,
        start: u64,
        end: Option<u64>,
    ) -> Result<ObjectReader, FlameError> {
        if ObjectKey::try_from(selected.key.as_str())?
            .to_key()
            .is_none()
        {
            return Err(FlameError::InvalidConfig(
                "complete object key required".into(),
            ));
        }
        let storage = self.clone();
        let selected = selected.clone();
        joined(
            tokio::task::spawn_blocking(move || {
                let (metadata, records) = {
                    let objects = storage.catalog.lock().map_err(|_| catalog_error())?;
                    let object = objects
                        .get(&selected.key)
                        .filter(|object| object.version() > 0)
                        .ok_or_else(|| {
                            FlameError::NotFound(format!("object <{}> not found", selected.key))
                        })?;
                    if selected.uuid.is_empty() || selected.uuid != object.metadata.uuid {
                        return Err(FlameError::InvalidVersion("object UUID changed".into()));
                    }
                    let visible: Vec<_> = object.visible_parts().copied().collect();
                    let version = visible.len() as u64;
                    let end = end.unwrap_or(version);
                    if selected.version > version || end > version {
                        return Err(FlameError::InvalidConfig(
                            "requested version is ahead of object".into(),
                        ));
                    }
                    if start > end {
                        return Err(FlameError::InvalidConfig(
                            "starting part is ahead of ending part".into(),
                        ));
                    }
                    let mut metadata = object.metadata(&selected.key, "");
                    metadata.version = end;
                    metadata.size = visible.iter().take(end as usize).map(|part| part.len).sum();
                    let records = visible
                        .iter()
                        .skip(start as usize)
                        .take((end - start) as usize)
                        .copied()
                        .collect::<Vec<_>>();
                    (metadata, records)
                };
                let readers = storage.part_readers(&metadata, &records)?;
                let parts = readers.iter().map(|reader| reader.part()).collect();
                Ok(ObjectReader::new(Object { metadata, parts }, readers))
            })
            .await,
        )?
    }

    /// Reserve one disjoint payload extent under a short allocation lock, then stream without that lock.
    async fn writer(
        &self,
        key: &ObjectKey,
        data_type: &str,
        size: u64,
    ) -> Result<ObjectWriter, FlameError> {
        if key.is_all_sessions() {
            return Err(FlameError::InvalidConfig(
                "wildcard is not an upload namespace".into(),
            ));
        }
        let key = if key.object_id.is_none() {
            key.clone().with_generated_id()
        } else {
            key.clone()
        };
        let key_string = key
            .to_key()
            .ok_or_else(|| FlameError::InvalidConfig("complete object key required".into()))?;
        let dir = self.object_dir(&key);
        let data_type = data_type.to_owned();
        let catalog = Arc::clone(&self.catalog);
        joined(
            tokio::task::spawn_blocking(move || {
                let mut objects = catalog.lock().map_err(|_| catalog_error())?;
                let created = !objects.contains_key(&key_string);
                if created {
                    let metadata = ObjectMetadataDao {
                        uuid: uuid::Uuid::new_v4().to_string(),
                        data_type: data_type.clone(),
                    };
                    create_object_files(&dir, &metadata)?;
                    objects.insert(
                        key_string.clone(),
                        ObjectDao {
                            metadata,
                            parts: Vec::new(),
                            cached_version: 0,
                        },
                    );
                }
                let result = (|| {
                    let object = objects.get_mut(&key_string).expect("initialized object");
                    validate_uuid(&dir, &object.metadata.uuid)?;
                    if object.metadata.data_type != data_type {
                        return Err(FlameError::InvalidConfig(
                            "patch data type does not match object".into(),
                        ));
                    }
                    if object
                        .parts
                        .iter()
                        .filter(|part| part.state != dao::PartState::Stale)
                        .count() as u64
                        > MAX_APPENDS_PER_OBJECT
                    {
                        return Err(FlameError::InvalidState(
                            "maximum number of appends reached".into(),
                        ));
                    }
                    let physical = object.parts.len() as u64;
                    let (record, writer) = part_writer::open(dir.clone(), physical, size)?;
                    object.parts.push(record);
                    let part = Part {
                        part: physical,
                        size,
                        creation_time: record.creation_time,
                        checksum: [0; 32],
                    };
                    Ok(ObjectWriter::new(
                        key_string.clone(),
                        object.metadata.uuid.clone(),
                        part,
                        writer,
                    ))
                })();
                if result.is_err() && created {
                    objects.remove(&key_string);
                    let _ = fs::remove_dir_all(&dir);
                }
                result
            })
            .await,
        )?
    }
    /// Own an accepted upload task through source consumption, filesystem durability, and state publication or cleanup.
    async fn write_part(
        &self,
        key: &ObjectKey,
        data_type: &str,
        size: u64,
        chunks: ByteStream,
    ) -> Result<ObjectMetadata, FlameError> {
        let storage = self.clone();
        let key = key.clone();
        let data_type = data_type.to_owned();
        tokio::spawn(async move { storage.write_chunks(&key, &data_type, size, chunks).await })
            .await
            .map_err(|error| FlameError::Internal(format!("storage write task failed: {error}")))?
    }
    /// Consume a domain byte stream and finalize every ordinary source, size, or filesystem failure inside storage.
    async fn write_chunks(
        &self,
        key: &ObjectKey,
        data_type: &str,
        size: u64,
        mut chunks: ByteStream,
    ) -> Result<ObjectMetadata, FlameError> {
        let mut writer = self.writer(key, data_type, size).await?;
        while let Some(chunk) = chunks.next().await {
            let result = match chunk {
                Ok(bytes) => writer.write_chunk(bytes).await,
                Err(error) => Err(error),
            };
            if let Err(error) = result {
                self.abort_writer(writer)?;
                return Err(error);
            }
        }
        let metadata = match writer.descriptor() {
            Ok(metadata) => metadata,
            Err(error) => {
                self.abort_writer(writer)?;
                return Err(error);
            }
        };
        self.commit_part(writer, metadata).await
    }
    /// Require an unchanged filesystem incarnation and a live physical part before mutating its cached descriptor.
    fn check_writer<'a>(
        &self,
        writer: &ObjectWriter,
        objects: &'a mut HashMap<String, ObjectDao>,
    ) -> Result<&'a mut ObjectDao, FlameError> {
        let object = objects
            .get_mut(&writer.key)
            .ok_or_else(|| FlameError::InvalidVersion("part was collected".into()))?;
        if object.metadata.uuid != writer.uuid
            || !object
                .parts
                .get(writer.part.part as usize)
                .is_some_and(|part| part.state != dao::PartState::Stale)
        {
            return Err(FlameError::InvalidVersion(
                "part is stale or was collected".into(),
            ));
        }
        Ok(object)
    }
    /// Commit one part and publish its durable prefix, retiring only unprepared failures.
    async fn commit_part(
        &self,
        mut writer: ObjectWriter,
        metadata: Part,
    ) -> Result<ObjectMetadata, FlameError> {
        let result = async {
            writer.sync_payload(metadata).await?;
            let dir = self.storage_path.join(&writer.key);
            let fence = {
                let mut objects = self.catalog.lock().map_err(|_| catalog_error())?;
                validate_uuid(&dir, &writer.uuid)?;
                let object = self.check_writer(&writer, &mut objects)?;
                let logical = object
                    .parts
                    .iter()
                    .filter(|part| part.part != u64::MAX && part.state != dao::PartState::Stale)
                    .count() as u64;
                let mut prepared = metadata;
                prepared.part = logical;
                writer.prepare(prepared)?;
                let offset = object.parts[writer.part.part as usize].offset;
                let mut record = PartDao::from_part(prepared, offset);
                record.state = dao::PartState::Writing;
                object.parts[writer.part.part as usize] = record;
                logical
            };
            writer.commit().await?;
            let mut objects = self.catalog.lock().map_err(|_| catalog_error())?;
            validate_uuid(&dir, &writer.uuid)?;
            let object = self.check_writer(&writer, &mut objects)?;
            object.publish(fence);
            Ok(object.metadata_prefix(&writer.key, "", fence + 1))
        }
        .await;
        if result.is_err() {
            self.abort_writer(writer)?;
        }
        result
    }
    /// Mark only unprepared failures stale; prepared logical IDs remain available to another durability fence or recovery.
    fn abort_writer(&self, mut writer: ObjectWriter) -> Result<(), FlameError> {
        let mut objects = self.catalog.lock().map_err(|_| catalog_error())?;
        let Ok(object) = self.check_writer(&writer, &mut objects) else {
            return Ok(());
        };
        let part = &mut object.parts[writer.part.part as usize];
        if part.state != dao::PartState::Writing || part.part != u64::MAX {
            return Ok(());
        }
        let result = writer.abort();
        part.state = dao::PartState::Stale;
        if object
            .parts
            .iter()
            .all(|part| part.state == dao::PartState::Stale)
        {
            objects.remove(&writer.key);
            let dir = self.storage_path.join(&writer.key);
            if validate_uuid(&dir, &writer.uuid).is_ok() {
                fs::remove_dir_all(dir)?;
            }
        }
        result
    }
    /// Open a partial create in filesystem tests while keeping the production streaming boundary closed.
    #[cfg(test)]
    async fn object_writer(
        &self,
        key: &ObjectKey,
        data_type: &str,
        size: u64,
    ) -> Result<ObjectWriter, FlameError> {
        self.writer(key, data_type, size).await
    }
    /// Open a partial append in filesystem tests for deterministic independent-write scenarios.
    #[cfg(test)]
    async fn patch_writer(
        &self,
        key: &ObjectKey,
        data_type: &str,
        size: u64,
    ) -> Result<ObjectWriter, FlameError> {
        self.writer(key, data_type, size).await
    }
    /// Exercise storage's normal commit coordinator with a manually driven filesystem payload in tests.
    #[cfg(test)]
    async fn commit_test_writer(&self, writer: ObjectWriter) -> Result<ObjectMetadata, FlameError> {
        let metadata = match writer.descriptor() {
            Ok(metadata) => metadata,
            Err(error) => {
                self.abort_writer(writer)?;
                return Err(error);
            }
        };
        self.commit_part(writer, metadata).await
    }
}

#[async_trait]
impl StorageEngine for FsStorage {
    /// Capture a fixed requested prefix and return its authoritative metadata before payload polling begins.
    async fn read_object(&self, metadata: &mut ObjectMetadata) -> Result<ChunkStream, FlameError> {
        let reader = self.capture(metadata, 0, Some(metadata.version)).await?;
        *metadata = reader.metadata().clone();
        Ok(self.read_stream(reader))
    }
    /// Treat the input version as the patch cursor and replace metadata with the captured latest committed view.
    async fn read_patch(&self, metadata: &mut ObjectMetadata) -> Result<ChunkStream, FlameError> {
        let reader = self.capture(metadata, metadata.version, None).await?;
        *metadata = reader.metadata().clone();
        Ok(self.read_stream(reader))
    }
    /// Run a create stream to completion under storage ownership, independently of the waiting caller.
    async fn write_object(
        &self,
        key: &ObjectKey,
        data_type: &str,
        size: u64,
        chunks: ByteStream,
    ) -> Result<ObjectMetadata, FlameError> {
        self.write_part(key, data_type, size, chunks).await
    }
    /// Run an append stream to completion with result-based state publication and cleanup.
    async fn write_patch(
        &self,
        key: &ObjectKey,
        data_type: &str,
        size: u64,
        chunks: ByteStream,
    ) -> Result<ObjectMetadata, FlameError> {
        self.write_part(key, data_type, size, chunks).await
    }
    /// List committed metadata and project requested prefixes without reader/deletion bookkeeping.
    async fn list(&self, filter: &ObjectFilter) -> Result<Vec<ObjectMetadata>, FlameError> {
        let (key, version) = match filter {
            ObjectFilter::AllObject => (None, None),
            ObjectFilter::ByKey { key, version } => (Some(key), *version),
        };
        let current: Vec<_> = {
            let objects = self.catalog.lock().map_err(|_| catalog_error())?;
            if let Some(key) = key.and_then(ObjectKey::to_key) {
                objects
                    .get(&key)
                    .filter(|object| object.version() > 0)
                    .map(|object| object.metadata(&key, ""))
                    .into_iter()
                    .collect()
            } else {
                objects
                    .iter()
                    .filter(|(key, object)| filter.matches(key) && object.version() > 0)
                    .map(|(key, object)| object.metadata(key, ""))
                    .collect()
            }
        };
        let Some(version) = version else {
            return Ok(current);
        };
        let mut items = Vec::with_capacity(current.len());
        for metadata in current {
            if let Some(metadata) = project_metadata(&self.catalog, &metadata.key, Some(version))? {
                items.push(metadata);
            }
        }
        Ok(items)
    }
    /// Unlink expired-session data on a blocking worker without waiting for readers or writers.
    async fn delete_objects(&self, key: &ObjectKey) -> Result<(), FlameError> {
        let storage = self.clone();
        let key = key.clone();
        joined(tokio::task::spawn_blocking(move || storage.delete_committed(&key)).await)?
    }
}
/// Prune an empty directory while tolerating absent or still-populated namespaces.
fn remove_empty_dir(path: &Path) {
    match fs::remove_dir(path) {
        Ok(()) => {}
        Err(error)
            if matches!(
                error.kind(),
                std::io::ErrorKind::NotFound | std::io::ErrorKind::DirectoryNotEmpty
            ) => {}
        Err(error) => tracing::warn!(path = %path.display(), %error, "failed to prune directory"),
    }
}

/// Open configured filesystem object storage, honoring `FLAME_CACHE_STORAGE`.
pub(crate) async fn connect(
    url: &str,
    pool: &FlameCachePool,
) -> Result<StorageEnginePtr, FlameError> {
    let url = std::env::var("FLAME_CACHE_STORAGE")
        .ok()
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| url.to_string());
    if url.is_empty() || url == "none" {
        return Err(FlameError::InvalidConfig(
            "filesystem object storage is required".into(),
        ));
    }
    if url.contains("://") && !url.starts_with("fs://") && !url.starts_with("file://") {
        return Err(FlameError::InvalidConfig(format!(
            "unsupported object storage URL: {url}"
        )));
    }
    let path = if url.starts_with("fs://") || url.starts_with("file://") {
        parse_storage_url(&url)?
    } else {
        PathBuf::from(url)
    };
    let pool = pool.clone();
    let storage =
        joined(tokio::task::spawn_blocking(move || FsStorage::with_cache(path, pool)).await)??;
    Ok(Box::new(storage))
}

/// Resolve filesystem storage URLs, relative to `FLAME_HOME` when needed.
fn parse_storage_url(url: &str) -> Result<PathBuf, FlameError> {
    let path_part = url
        .strip_prefix("fs://")
        .or_else(|| url.strip_prefix("file://"))
        .ok_or_else(|| FlameError::InvalidConfig(format!("Invalid storage URL: {}", url)))?;

    if path_part.is_empty() {
        return Err(FlameError::InvalidConfig(
            "filesystem storage path is empty".into(),
        ));
    }
    if path_part.starts_with('/') {
        Ok(PathBuf::from(path_part))
    } else {
        let flame_home =
            std::env::var("FLAME_HOME").unwrap_or_else(|_| "/var/lib/flame".to_string());
        Ok(PathBuf::from(flame_home).join(path_part))
    }
}

#[cfg(test)]
mod tests;
