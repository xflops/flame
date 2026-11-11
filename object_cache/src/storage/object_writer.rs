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

//! Traverse one filesystem upload, maintaining its incremental checksum and bounded chunk offsets.
use super::{PartWriter, CHUNK_SIZE};
use crate::model::{Chunk, Part};
use bytes::Bytes;
use common::FlameError;
use sha2::{Digest, Sha256};

/// Own filesystem traversal and integrity state; storage owns all catalog updates and request lifetime.
pub(crate) struct ObjectWriter {
    pub(super) key: String,
    pub(super) uuid: String,
    pub(super) part: Part,
    writer: Box<dyn PartWriter>,
    size: u64,
    hash: Sha256,
    failed: bool,
}
impl ObjectWriter {
    /// Capture the filesystem identity and selected physical extent without retaining catalog access.
    pub(super) fn new(key: String, uuid: String, part: Part, writer: Box<dyn PartWriter>) -> Self {
        Self {
            key,
            uuid,
            part,
            writer,
            size: 0,
            hash: Sha256::new(),
            failed: false,
        }
    }
    /// Return the physical descriptor for low-level filesystem validation fixtures.
    #[cfg(test)]
    pub(super) fn physical_part(&self) -> Part {
        self.part
    }
    /// Exercise raw chunk validation without manufacturing storage ownership in backend fixtures.
    #[cfg(test)]
    pub(super) async fn write_raw_chunk(&mut self, chunk: Chunk) -> Result<(), FlameError> {
        self.writer.write_chunk(chunk).await
    }
    /// Hash supplied bytes while forwarding only bounded sequential slices to filesystem I/O.
    pub(crate) async fn write_chunk(&mut self, data: Bytes) -> Result<(), FlameError> {
        let size = self
            .size
            .checked_add(data.len() as u64)
            .ok_or_else(|| FlameError::InvalidConfig("part size overflow".into()))?;
        if self.failed || size > self.part.size {
            self.failed = true;
            return Err(FlameError::InvalidConfig(
                "payload exceeds declared part size".into(),
            ));
        }
        for offset in (0..data.len()).step_by(CHUNK_SIZE) {
            let end = (offset + CHUNK_SIZE).min(data.len());
            self.writer
                .write_chunk(Chunk {
                    part: self.part.part,
                    offset: self.size + offset as u64,
                    data: data.slice(offset..end),
                })
                .await?;
        }
        self.hash.update(&data);
        self.size = size;
        Ok(())
    }
    /// Finalize the request checksum and reject an invalid declared-size stream before preparation.
    pub(super) fn descriptor(&self) -> Result<Part, FlameError> {
        if self.failed || self.size != self.part.size {
            return Err(FlameError::InvalidConfig(
                "payload size differs from declared part size".into(),
            ));
        }
        let mut part = self.part;
        part.checksum = self.hash.clone().finalize().into();
        Ok(part)
    }
    /// Sync physical payload bytes before storage allocates a stable logical index.
    pub(super) async fn sync_payload(&mut self, metadata: Part) -> Result<(), FlameError> {
        self.writer.sync_payload(metadata).await
    }
    /// Persist an assigned logical index without changing cached object or part state.
    pub(super) fn prepare(&mut self, metadata: Part) -> Result<(), FlameError> {
        self.writer.prepare(metadata)
    }
    /// Return the filesystem durability result for storage to publish or preserve as prepared.
    pub(super) async fn commit(&mut self) -> Result<(), FlameError> {
        self.writer.commit().await
    }
    /// Persist a stale physical descriptor at storage's explicit cleanup request.
    pub(super) fn abort(&mut self) -> Result<(), FlameError> {
        self.writer.abort()
    }
}
