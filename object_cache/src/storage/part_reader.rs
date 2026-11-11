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

use super::{joined, validate_uuid, PartDao, CHUNK_SIZE, DATA_FILE};
use crate::model::{Chunk, Part};
use async_trait::async_trait;
use bytes::Bytes;
use common::FlameError;
use sha2::{Digest, Sha256};
use std::{fs, os::unix::fs::FileExt, path::Path, sync::Arc};

#[async_trait]
pub(crate) trait PartReader: Send {
    /// Return the immutable domain descriptor of this captured part.
    fn part(&self) -> Part;
    /// Read the next part-relative chunk; verify integrity before successful EOF.
    async fn next_chunk(&mut self) -> Result<Option<Chunk>, FlameError>;
}
struct FsReader {
    part: u64,
    record: PartDao,
    data: Arc<fs::File>,
    offset: u64,
    hash: Sha256,
    state: ReadState,
}
enum ReadState {
    Reading,
    Verified,
    Failed,
}
#[async_trait]
impl PartReader for FsReader {
    /// Return the captured part descriptor without exposing absolute index offsets.
    fn part(&self) -> Part {
        Part {
            part: self.part,
            size: self.record.len,
            creation_time: self.record.creation_time,
            checksum: self.record.checksum,
        }
    }
    /// Read a bounded chunk from the captured inode and verify SHA-256 before final delivery.
    async fn next_chunk(&mut self) -> Result<Option<Chunk>, FlameError> {
        match self.state {
            ReadState::Verified | ReadState::Failed => return Ok(None),
            ReadState::Reading => {}
        }
        let len = (self.record.len - self.offset).min(CHUNK_SIZE as u64) as usize;
        let data = Arc::clone(&self.data);
        let position = self
            .record
            .offset
            .checked_add(self.offset)
            .ok_or_else(|| FlameError::DataLoss("part offset overflow".to_string()))?;
        let bytes = joined(
            tokio::task::spawn_blocking(move || {
                let mut bytes = vec![0; len];
                data.read_exact_at(&mut bytes, position).map_err(|error| {
                    FlameError::DataLoss(format!("failed to read complete part: {error}"))
                })?;
                Ok::<_, FlameError>(Bytes::from(bytes))
            })
            .await,
        )??;
        self.hash.update(&bytes);
        let offset = self.offset;
        self.offset += bytes.len() as u64;
        if self.offset == self.record.len {
            let actual: [u8; 32] = self.hash.clone().finalize().into();
            if actual != self.record.checksum {
                self.state = ReadState::Failed;
                return Err(FlameError::DataLoss(format!(
                    "SHA-256 checksum mismatch for part {}",
                    self.part
                )));
            }
            self.state = ReadState::Verified;
        }
        Ok(Some(Chunk {
            part: self.part,
            offset,
            data: bytes,
        }))
    }
}

/// Open a captured inode from logical part IDs and immutable physical descriptors selected before payload access.
pub(super) fn open_readers(
    dir: &Path,
    parts: &[(u64, PartDao)],
    uuid: &str,
) -> Result<Vec<Box<dyn PartReader>>, FlameError> {
    if parts.is_empty() {
        return Ok(Vec::new());
    }
    let file = fs::File::open(dir.join(DATA_FILE))?;
    let data_len = file.metadata()?.len();
    let data = Arc::new(file);
    let mut readers = Vec::with_capacity(parts.len());
    for &(part, record) in parts {
        if record.state != super::dao::PartState::Committed {
            return Err(FlameError::InvalidVersion("part is not committed".into()));
        }
        if record
            .offset
            .checked_add(record.len)
            .is_none_or(|end| end > data_len)
        {
            return Err(FlameError::DataLoss(format!("truncated part {part}")));
        }
        readers.push(Box::new(FsReader {
            part,
            record,
            data: Arc::clone(&data),
            offset: 0,
            hash: Sha256::new(),
            state: ReadState::Reading,
        }) as Box<dyn PartReader>);
    }
    validate_uuid(dir, uuid)?;
    Ok(readers)
}
