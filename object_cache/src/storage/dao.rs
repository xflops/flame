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

use super::RECORD_LEN;
use crate::model::{ObjectMetadata, Part};
use common::FlameError;
use serde::{Deserialize, Serialize};

/// Distinguish reserved payload extents from durable, readable parts.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum PartState {
    Writing,
    Committed,
    Stale,
}

/// Persist one part descriptor and its absolute data-file offset in the fixed-size index.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct PartDao {
    pub(super) offset: u64,
    pub(super) len: u64,
    pub(super) creation_time: i64,
    pub(super) checksum: [u8; 32],
    pub(super) state: PartState,
    pub(super) part: u64,
}

/// Represent a filesystem object as its persisted identity and ordered committed parts.
#[derive(Debug, Clone)]
pub(super) struct ObjectDao {
    pub(super) metadata: ObjectMetadataDao,
    pub(super) parts: Vec<PartDao>,
    pub(super) cached_version: u64,
}
impl ObjectDao {
    /// Iterate published committed parts in stable logical commit order, ignoring Writing and Stale slots.
    pub(super) fn visible_parts(&self) -> impl Iterator<Item = &PartDao> {
        let mut parts: Vec<_> = self
            .parts
            .iter()
            .filter(|part| part.state == PartState::Committed && part.part < self.cached_version)
            .collect();
        parts.sort_unstable_by_key(|part| part.part);
        parts.into_iter()
    }
    /// Read the cached published logical part count without rescanning the physical records.
    pub(super) fn version(&self) -> u64 {
        self.cached_version
    }
    /// Validate and reconstruct a read-only inspected committed prefix without repairing records.
    #[cfg(test)]
    pub(super) fn refresh_version(&mut self) -> Result<(), FlameError> {
        let mut ids: Vec<_> = self
            .parts
            .iter()
            .filter(|part| part.state == PartState::Committed)
            .map(|part| part.part)
            .collect();
        ids.sort_unstable();
        if ids
            .iter()
            .enumerate()
            .any(|(index, part)| *part != index as u64)
        {
            return Err(FlameError::DataLoss(
                "committed logical part IDs are not dense and unique".into(),
            ));
        }
        self.cached_version = ids.len() as u64;
        Ok(())
    }
    /// Retire crash-interrupted records and unacknowledged logical tails while preserving every dense acknowledged prefix.
    pub(super) fn recover_parts(&mut self) -> Result<(), FlameError> {
        for part in &mut self.parts {
            if part.state == PartState::Writing {
                part.state = PartState::Stale;
            }
        }
        let mut ids: Vec<_> = self
            .parts
            .iter()
            .filter(|part| part.state == PartState::Committed)
            .map(|part| part.part)
            .collect();
        ids.sort_unstable();
        if ids.windows(2).any(|pair| pair[0] == pair[1]) {
            return Err(FlameError::DataLoss(
                "duplicate committed logical part IDs".into(),
            ));
        }
        let prefix = ids
            .iter()
            .enumerate()
            .take_while(|(index, part)| **part == *index as u64)
            .count() as u64;
        for part in &mut self.parts {
            if part.state == PartState::Committed && part.part >= prefix {
                part.state = PartState::Stale;
            }
        }
        self.cached_version = prefix;
        Ok(())
    }
    /// Publish only the prepared logical IDs covered by a completed index sync fence.
    pub(super) fn publish(&mut self, fence: u64) {
        for part in &mut self.parts {
            if part.state != PartState::Stale && part.part <= fence {
                part.state = PartState::Committed;
            }
        }
        self.cached_version = self.cached_version.max(fence + 1);
    }
    /// Sum bytes only from the currently published logical prefix.
    pub(super) fn size(&self) -> u64 {
        self.parts
            .iter()
            .filter(|part| part.state == PartState::Committed && part.part < self.cached_version)
            .map(|part| part.len)
            .sum()
    }
    /// Return the timestamp of the first logically committed part.
    pub(super) fn creation_time(&self) -> i64 {
        self.parts
            .iter()
            .find(|part| part.state == PartState::Committed && part.part == 0)
            .map(|part| part.creation_time)
            .unwrap_or_default()
    }
    /// Build metadata restricted to one stable logical prefix captured by a reader or commit acknowledgment.
    pub(super) fn metadata_prefix(
        &self,
        key: impl Into<String>,
        endpoint: impl Into<String>,
        version: u64,
    ) -> ObjectMetadata {
        let mut metadata = self.metadata(key, endpoint);
        metadata.version = version;
        metadata.size = self
            .parts
            .iter()
            .filter(|part| part.state == PartState::Committed && part.part < version)
            .map(|part| part.len)
            .sum();
        metadata
    }
    /// Build object metadata for this captured part prefix and advertised endpoint.
    pub(super) fn metadata(
        &self,
        key: impl Into<String>,
        endpoint: impl Into<String>,
    ) -> ObjectMetadata {
        ObjectMetadata::new()
            .with_uuid(&self.metadata.uuid)
            .with_key(key)
            .with_endpoint(endpoint)
            .with_version(self.version())
            .with_size(self.size())
            .with_creation_time(self.creation_time())
            .with_data_type(&self.metadata.data_type)
    }
}

/// Store the object identity and data type using the metadata JSON representation.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub(super) struct ObjectMetadataDao {
    pub(super) uuid: String,
    pub(super) data_type: String,
}

impl PartDao {
    /// Combine a domain part descriptor with its absolute payload offset in the index.
    pub(super) fn from_part(part: Part, offset: u64) -> Self {
        Self {
            offset,
            len: part.size,
            creation_time: part.creation_time,
            checksum: part.checksum,
            state: PartState::Committed,
            part: part.part,
        }
    }

    /// Encode one committed part descriptor as a fixed-size index record.
    pub(super) fn encode(&self) -> [u8; RECORD_LEN] {
        let mut bytes = [0; RECORD_LEN];
        bytes[..8].copy_from_slice(&self.offset.to_le_bytes());
        bytes[8..16].copy_from_slice(&self.len.to_le_bytes());
        bytes[16..24].copy_from_slice(&self.creation_time.to_le_bytes());
        bytes[24..56].copy_from_slice(&self.checksum);
        let state = match self.state {
            PartState::Writing => 0u64,
            PartState::Committed => 1,
            PartState::Stale => 2,
        };
        bytes[56..64].copy_from_slice(&state.to_le_bytes());
        bytes[64..72].copy_from_slice(&self.part.to_le_bytes());
        bytes
    }
    /// Decode the byte offsets, timestamp, and SHA-256 digest of one index record.
    pub(super) fn decode(bytes: &[u8; RECORD_LEN]) -> Self {
        let word = |offset| <[u8; 8]>::try_from(&bytes[offset..offset + 8]).unwrap();
        Self {
            offset: u64::from_le_bytes(word(0)),
            len: u64::from_le_bytes(word(8)),
            creation_time: i64::from_le_bytes(word(16)),
            checksum: bytes[24..56].try_into().unwrap(),
            state: match u64::from_le_bytes(word(56)) {
                0 => PartState::Writing,
                1 => PartState::Committed,
                _ => PartState::Stale,
            },
            part: u64::from_le_bytes(word(64)),
        }
    }
}
