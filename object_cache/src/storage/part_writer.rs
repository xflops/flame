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

use super::{dao::PartState, joined, PartDao, DATA_FILE, INDEX_FILE, METADATA_FILE, RECORD_LEN};
use crate::model::{current_time_millis, Chunk, Part};
use async_trait::async_trait;
use common::FlameError;
use std::{fs, os::unix::fs::FileExt, path::PathBuf, sync::Arc};

#[async_trait]
pub(crate) trait PartWriter: Send {
    /// Write the next sequential chunk into the selected physical extent.
    async fn write_chunk(&mut self, chunk: Chunk) -> Result<(), FlameError>;
    /// Validate the exact-size physical payload and sync it before storage assigns its logical ID.
    async fn sync_payload(&mut self, metadata: Part) -> Result<(), FlameError>;
    /// Write a prepared index descriptor using the logical ID assigned by storage.
    fn prepare(&mut self, metadata: Part) -> Result<(), FlameError>;
    /// Sync prepared index and object metadata bytes without changing the catalog.
    async fn commit(&mut self) -> Result<(), FlameError>;
    /// Persist a stale descriptor when storage retires an unprepared physical part.
    fn abort(&mut self) -> Result<(), FlameError>;
}

/// Own only filesystem handles, extent bounds, and sequential I/O validation for one physical part.
struct FsPartWriter {
    dir: PathBuf,
    part: u64,
    record: PartDao,
    index: Arc<fs::File>,
    data: Arc<fs::File>,
    size: u64,
    failed: bool,
}

/// Allocate a disjoint extent and its initial index record, returning the descriptor for storage's catalog.
pub(super) fn open(
    dir: PathBuf,
    part: u64,
    size: u64,
) -> Result<(PartDao, Box<dyn PartWriter>), FlameError> {
    let index = fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(dir.join(INDEX_FILE))?;
    let data = fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(dir.join(DATA_FILE))?;
    let offset = data.metadata()?.len();
    let end = offset
        .checked_add(size)
        .ok_or_else(|| FlameError::InvalidConfig("part allocation size overflow".into()))?;
    data.set_len(end)?;
    let record = PartDao {
        offset,
        len: size,
        creation_time: current_time_millis(),
        checksum: [0; 32],
        state: PartState::Writing,
        part: u64::MAX,
    };
    index.write_all_at(&record.encode(), part * RECORD_LEN as u64)?;
    Ok((
        record,
        Box::new(FsPartWriter {
            dir,
            part,
            record,
            index: Arc::new(index),
            data: Arc::new(data),
            size: 0,
            failed: false,
        }),
    ))
}
#[async_trait]
impl PartWriter for FsPartWriter {
    /// Perform positional payload writes outside metadata locks and poison interrupted borrowed writes.
    async fn write_chunk(&mut self, chunk: Chunk) -> Result<(), FlameError> {
        let next = self.size.checked_add(chunk.data.len() as u64);
        if self.failed
            || chunk.part != self.part
            || chunk.offset != self.size
            || next.is_none_or(|next| next > self.record.len)
        {
            self.failed = true;
            return Err(FlameError::InvalidConfig(
                "chunk exceeds declared part size or has invalid part/offset".into(),
            ));
        }
        self.failed = true;
        let data = Arc::clone(&self.data);
        let offset = self.record.offset + self.size;
        joined(tokio::task::spawn_blocking(move || data.write_all_at(&chunk.data, offset)).await)??;
        self.failed = false;
        self.size = next.expect("validated size");
        Ok(())
    }
    /// Require the declared physical part and complete payload before syncing any preparation dependency.
    async fn sync_payload(&mut self, metadata: Part) -> Result<(), FlameError> {
        if self.failed
            || metadata.part != self.part
            || metadata.size != self.size
            || self.size != self.record.len
        {
            return Err(FlameError::InvalidConfig(
                "committed size differs from declared part size".into(),
            ));
        }
        let data = Arc::clone(&self.data);
        joined(tokio::task::spawn_blocking(move || data.sync_data()).await)??;
        Ok(())
    }
    /// Persist the caller's logical descriptor at this handle's independent physical index position.
    fn prepare(&mut self, metadata: Part) -> Result<(), FlameError> {
        self.index.write_all_at(
            &PartDao::from_part(metadata, self.record.offset).encode(),
            self.part * RECORD_LEN as u64,
        )?;
        Ok(())
    }
    /// Complete filesystem durability only; storage publishes cached state after this result succeeds.
    async fn commit(&mut self) -> Result<(), FlameError> {
        let index = Arc::clone(&self.index);
        let dir = self.dir.clone();
        joined(
            tokio::task::spawn_blocking(move || {
                index.sync_data()?;
                fs::File::open(dir.join(METADATA_FILE))?.sync_all()?;
                fs::File::open(&dir)?.sync_all()?;
                fs::File::open(dir.parent().expect("object session"))?.sync_all()?;
                Ok::<_, std::io::Error>(())
            })
            .await,
        )??;
        Ok(())
    }
    /// Record a stale physical extent without deleting files or touching any in-memory object state.
    fn abort(&mut self) -> Result<(), FlameError> {
        let mut stale = self.record;
        stale.state = PartState::Stale;
        self.index
            .write_all_at(&stale.encode(), self.part * RECORD_LEN as u64)?;
        Ok(())
    }
}
