/* Copyright 2026 The Flame Authors.
Licensed under the Apache License, Version 2.0 (the "License");
you may not use this file except in compliance with the License.
You may obtain a copy of the License at http://www.apache.org/licenses/LICENSE-2.0
Unless required by applicable law or agreed to in writing, software distributed
under the License is distributed on an "AS IS" BASIS, WITHOUT WARRANTIES OR
CONDITIONS OF ANY KIND, either express or implied. See the License for the
specific language governing permissions and limitations under the License. */

use bytes::Bytes;
use common::FlameError;
#[cfg(test)]
use sha2::{Digest, Sha256};
use std::time::{SystemTime, UNIX_EPOCH};

/// Wildcard session identifier for matching all sessions of an application
pub const WILDCARD_SESSION: &str = "*";

/// Parsed object key: `<app_name>/<session_id>/<object_id>`
/// session_id can be "*" for wildcard (all sessions), requires object_id to be None
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ObjectKey {
    pub app_name: String,
    pub session_id: String,
    pub object_id: Option<String>,
}

impl ObjectKey {
    /// Parse from path string.
    ///
    /// Wildcard '*' handling:
    /// - Only allowed for session_id (e.g., "app/*" for delete all sessions)
    /// - Not allowed for app_name or object_id
    /// - Wildcard session cannot have object_id (e.g., "app/*/obj" is invalid)
    pub fn from_path(path_str: &str) -> Result<Self, FlameError> {
        let parts: Vec<&str> = path_str.split('/').collect();

        for (i, part) in parts.iter().enumerate() {
            if part.is_empty() || part.contains("..") || part.contains('\\') {
                return Err(FlameError::InvalidConfig(format!(
                    "Invalid key component: '{}'",
                    part
                )));
            }
            // Wildcard only allowed at index 1 (session_id position)
            if *part == WILDCARD_SESSION && i != 1 {
                return Err(FlameError::InvalidConfig(
                    "Wildcard '*' only allowed for session_id".to_string(),
                ));
            }
        }

        match parts.len() {
            2 => Ok(ObjectKey {
                app_name: parts[0].to_string(),
                session_id: parts[1].to_string(),
                object_id: None,
            }),
            3 => {
                // Wildcard session cannot reference specific objects
                if parts[1] == WILDCARD_SESSION {
                    return Err(FlameError::InvalidConfig(
                        "Wildcard session '*' cannot have object_id".to_string(),
                    ));
                }
                if parts[2] == ".uploads" {
                    return Err(FlameError::InvalidConfig(
                        "Object ID '.uploads' is reserved for staged uploads".to_string(),
                    ));
                }
                // Object ID cannot be wildcard
                if parts[2] == WILDCARD_SESSION {
                    return Err(FlameError::InvalidConfig(
                        "Wildcard '*' not allowed for object_id".to_string(),
                    ));
                }
                Ok(ObjectKey {
                    app_name: parts[0].to_string(),
                    session_id: parts[1].to_string(),
                    object_id: Some(parts[2].to_string()),
                })
            }
            _ => Err(FlameError::InvalidConfig(format!(
                "Invalid path '{}': expected '<app>/<ssn>' or '<app>/<ssn>/<uuid>'",
                path_str
            ))),
        }
    }

    /// Whether this key selects the entire application namespace.
    pub fn is_all_sessions(&self) -> bool {
        self.session_id == WILDCARD_SESSION
    }

    /// Return a complete object path, or `None` for a namespace selector.
    pub fn to_key(&self) -> Option<String> {
        if self.is_all_sessions() {
            return None;
        }
        self.object_id
            .as_ref()
            .map(|oid| format!("{}/{}/{}", self.app_name, self.session_id, oid))
    }

    /// Return the application or session prefix selected by this key.
    pub fn to_prefix(&self) -> String {
        if self.is_all_sessions() {
            self.app_name.clone()
        } else {
            format!("{}/{}", self.app_name, self.session_id)
        }
    }

    /// Match a complete object path against this exact key or namespace selector.
    pub fn matches(&self, key_str: &str) -> bool {
        if self.is_all_sessions() {
            key_str.starts_with(&format!("{}/", self.app_name))
        } else if let Some(full_key) = self.to_key() {
            key_str == full_key
        } else {
            key_str.starts_with(&format!("{}/", self.to_prefix()))
        }
    }

    /// Generate a new object ID beneath this key's application and session.
    pub fn with_generated_id(self) -> Self {
        Self {
            object_id: Some(uuid::Uuid::new_v4().to_string()),
            ..self
        }
    }

    /// Set a validated object ID, rejecting wildcard sessions and reserved paths.
    pub fn with_object_id(self, id: String) -> Result<Self, FlameError> {
        if self.is_all_sessions() {
            return Err(FlameError::InvalidConfig(
                "Wildcard session '*' cannot have object_id".to_string(),
            ));
        }
        if id == ".uploads"
            || id.is_empty()
            || id.contains("..")
            || id.contains('\\')
            || id.contains('/')
        {
            return Err(FlameError::InvalidConfig(format!(
                "Invalid object_id: '{}'",
                id
            )));
        }
        Ok(Self {
            object_id: Some(id),
            ..self
        })
    }
}

impl TryFrom<&str> for ObjectKey {
    type Error = FlameError;
    /// Parse a complete object key, rejecting namespace-only selectors.
    fn try_from(key: &str) -> Result<Self, Self::Error> {
        let parsed = Self::from_path(key)?;
        if parsed.object_id.is_none() {
            return Err(FlameError::InvalidConfig(format!(
                "complete object key required: {key}"
            )));
        }
        Ok(parsed)
    }
}

impl From<&ObjectKey> for String {
    /// Render a complete key or the prefix represented by a namespace selector.
    fn from(key: &ObjectKey) -> Self {
        key.to_key().unwrap_or_else(|| key.to_prefix())
    }
}

/// Return Unix time in milliseconds, clamping pre-epoch or overflowing values.
pub(crate) fn current_time_millis() -> i64 {
    let millis = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis();
    i64::try_from(millis).unwrap_or(i64::MAX)
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct ObjectMetadata {
    pub uuid: String,
    pub endpoint: String,
    pub key: String,
    pub version: u64,
    pub size: u64,
    pub creation_time: i64,
    pub data_type: String,
}

impl ObjectMetadata {
    /// Metadata with default values: empty endpoint and key, version 0, empty
    /// payload, created now, and a "raw" data type.
    pub(crate) fn new() -> Self {
        Self {
            uuid: String::new(),
            endpoint: String::new(),
            key: String::new(),
            version: 0,
            size: 0,
            creation_time: current_time_millis(),
            data_type: "raw".to_string(),
        }
    }

    /// Set the UUID that distinguishes this object from same-key recreations.
    pub(crate) fn with_uuid(self, uuid: impl Into<String>) -> Self {
        Self {
            uuid: uuid.into(),
            ..self
        }
    }

    /// Set the replica endpoint clients use to retrieve this object.
    pub(crate) fn with_endpoint(self, endpoint: impl Into<String>) -> Self {
        Self {
            endpoint: endpoint.into(),
            ..self
        }
    }

    /// Set the complete application/session/object path.
    pub(crate) fn with_key(self, key: impl Into<String>) -> Self {
        Self {
            key: key.into(),
            ..self
        }
    }

    /// Set the committed part count represented by this metadata snapshot.
    pub(crate) fn with_version(self, version: u64) -> Self {
        Self { version, ..self }
    }

    /// Set the total encoded payload size of the represented parts.
    pub(crate) fn with_size(self, size: u64) -> Self {
        Self { size, ..self }
    }

    /// Set the object creation timestamp in Unix milliseconds.
    pub(crate) fn with_creation_time(self, creation_time: i64) -> Self {
        Self {
            creation_time,
            ..self
        }
    }

    /// Set the opaque payload encoding label shared by all object parts.
    pub(crate) fn with_data_type(self, data_type: impl Into<String>) -> Self {
        Self {
            data_type: data_type.into(),
            ..self
        }
    }
}

/// Select all objects explicitly or project an exact key or namespace to a committed prefix.
#[derive(Debug, Clone)]
pub enum ObjectFilter {
    AllObject,
    ByKey {
        key: ObjectKey,
        /// `None` selects latest metadata; `Some(0)` selects an empty prefix.
        version: Option<u64>,
    },
}
impl ObjectFilter {
    /// Match a complete object key against the selected exact key, namespace, or explicit all-object scope.
    pub fn matches(&self, key: &str) -> bool {
        match self {
            Self::AllObject => true,
            Self::ByKey { key: selector, .. } => selector.matches(key),
        }
    }
}

/// Metadata and the immutable descriptors selected for one captured object view.
#[derive(Debug, Clone)]
pub struct Object {
    pub metadata: ObjectMetadata,
    pub parts: Vec<Part>,
}

/// One committed part's identity, byte length, creation time, and checksum; payload stays in storage.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Part {
    pub part: u64,
    pub size: u64,
    pub creation_time: i64,
    pub checksum: [u8; 32],
}
impl Part {
    #[cfg(test)]
    /// Describe a test payload without retaining its backing bytes.
    pub fn new(part: u64, data: Bytes) -> Self {
        Self {
            part,
            size: data.len() as u64,
            creation_time: current_time_millis(),
            checksum: Sha256::digest(&data).into(),
        }
    }
    #[cfg(test)]
    /// Override the captured part timestamp for deterministic storage assertions.
    pub fn with_creation_time(mut self, creation_time: i64) -> Self {
        self.creation_time = creation_time;
        self
    }
}

/// Internal controller/storage transfer carrying its owning part and part-relative offset.
/// gRPC maps this model to wire responses; it is not exposed as an end-user type.
#[derive(Debug, Clone)]
pub(crate) struct Chunk {
    pub part: u64,
    pub offset: u64,
    pub data: Bytes,
}

#[cfg(test)]
mod tests;
