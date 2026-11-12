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

//! Capture descriptor-only object snapshots and stream selected storage parts.
use super::{ChunkStream, PartReader};
use crate::model::ObjectMetadata;
use crate::model::{Chunk, Object, Part};
use common::FlameError;

/// Own selected part descriptors and preopened storage readers independently of later path mutation.
pub(super) struct ObjectReader {
    object: Object,
    current: Option<Box<dyn PartReader>>,
    readers: std::vec::IntoIter<Box<dyn PartReader>>,
    position: usize,
    done: bool,
}
impl ObjectReader {
    /// Retain a captured descriptor view and its ordered storage handles through consumption or cancellation.
    pub(super) fn new(object: Object, readers: Vec<Box<dyn PartReader>>) -> Self {
        Self {
            object,
            current: None,
            readers: readers.into_iter(),
            position: 0,
            done: false,
        }
    }

    /// Return selected-prefix metadata or current full metadata for an open-ended suffix.
    pub(super) fn metadata(&self) -> &ObjectMetadata {
        &self.object.metadata
    }

    /// Return the captured part descriptors for verified stream cache population.
    pub(super) fn parts(&self) -> &[Part] {
        &self.object.parts
    }

    /// Let the storage-owned stream retain captured readers and terminate permanently after any error.
    pub(super) fn into_stream(self) -> ChunkStream {
        Box::pin(futures::stream::unfold(self, |mut reader| async move {
            match reader.next_chunk().await {
                Ok(Some(chunk)) => Some((Ok(chunk), reader)),
                Ok(None) => None,
                Err(error) => Some((Err(error), reader)),
            }
        }))
    }

    /// Forward storage chunks and release each part reader when its verified EOF is observed.
    pub(super) async fn next_chunk(&mut self) -> Result<Option<Chunk>, FlameError> {
        if self.done {
            return Ok(None);
        }
        let result = self.read_chunk().await;
        if result.is_err() || matches!(&result, Ok(None)) {
            self.current = None;
            self.readers = Vec::new().into_iter();
            self.done = true;
        }
        result
    }

    /// Advance captured readers without assembling payloads or repeating backend integrity checks.
    async fn read_chunk(&mut self) -> Result<Option<Chunk>, FlameError> {
        loop {
            if self.current.is_none() {
                let Some(part) = self.object.parts.get(self.position) else {
                    self.done = true;
                    return Ok(None);
                };
                if part.part >= self.object.metadata.version {
                    return Err(FlameError::DataLoss(
                        "captured part exceeds the selected object version".into(),
                    ));
                }
                self.current = Some(self.readers.next().ok_or_else(|| {
                    FlameError::DataLoss(format!("captured reader missing for part {}", part.part))
                })?);
            }
            if let Some(chunk) = self
                .current
                .as_mut()
                .expect("selected part reader")
                .next_chunk()
                .await?
            {
                return Ok(Some(chunk));
            }
            self.current = None;
            self.position += 1;
        }
    }
}
