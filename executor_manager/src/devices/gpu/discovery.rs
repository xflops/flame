/*
Copyright 2023 The Flame Authors.
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

//! FEM GPU inventory for resource advertisement and executor assignment.

use std::collections::HashSet;

use common::FlameError;

/// Return identifiers suitable for CUDA_VISIBLE_DEVICES. Explicit visibility is
/// preserved, including physical UUID and MIG identifiers. Without a restriction,
/// use NVML UUIDs so allocation is independent of CUDA's device ordering.
pub fn discover_gpu_devices() -> Result<Vec<String>, FlameError> {
    match std::env::var("CUDA_VISIBLE_DEVICES") {
        Ok(devices) => return parse_visible_devices(&devices),
        Err(std::env::VarError::NotPresent) => {}
        Err(error) => {
            return Err(FlameError::InvalidConfig(format!(
                "invalid CUDA_VISIBLE_DEVICES: {error}"
            )))
        }
    }

    let nvml = match nvml_wrapper::Nvml::init() {
        Ok(nvml) => nvml,
        Err(error) => {
            tracing::debug!("NVML unavailable, GPU discovery skipped: {error}");
            return Ok(Vec::new());
        }
    };
    let count = nvml
        .device_count()
        .map_err(|error| FlameError::Internal(format!("failed to discover GPU count: {error}")))?;
    (0..count)
        .map(|index| {
            nvml.device_by_index(index)
                .and_then(|device| device.uuid())
                .map_err(|error| {
                    FlameError::Internal(format!("failed to discover GPU {index}: {error}"))
                })
        })
        .collect()
}

fn parse_visible_devices(value: &str) -> Result<Vec<String>, FlameError> {
    let value = value.trim();
    if value.is_empty() || value == "-1" {
        return Ok(Vec::new());
    }
    // Index and UUID aliases can name the same physical GPU. A single family
    // avoids advertising overlapping devices as independently allocatable.
    let families: HashSet<_> = value
        .split(',')
        .map(|token| {
            let token = token.trim();
            if token.starts_with("GPU-") {
                "gpu"
            } else if token.starts_with("MIG-GPU-") {
                "mig-legacy"
            } else if token.starts_with("MIG-") {
                "mig"
            } else {
                "index"
            }
        })
        .collect();
    if families.len() > 1 {
        return Err(FlameError::InvalidConfig(
            "CUDA_VISIBLE_DEVICES must use only indices, GPU UUIDs, or MIG identifiers; mixed identifiers may alias the same GPU".to_string()
        ));
    }
    let mut seen = HashSet::new();
    value
        .split(',')
        .map(|token| {
            let token = token.trim();
            let is_index = !token.is_empty()
                && token.bytes().all(|byte| byte.is_ascii_digit())
                && token.parse::<u32>().is_ok();
            let id = if is_index {
                token.parse::<u32>().expect("validated index").to_string()
            } else {
                canonical_gpu_identifier(token).ok_or_else(|| FlameError::InvalidConfig(format!(
                    "invalid GPU identifier '{token}' in CUDA_VISIBLE_DEVICES; use a full GPU or MIG UUID"
                )))?
            };
            if !seen.insert(id.clone()) {
                return Err(FlameError::InvalidConfig(format!(
                    "duplicate GPU identifier '{id}' in CUDA_VISIBLE_DEVICES"
                )));
            }
            Ok(id)
        })
        .collect()
}

fn canonical_uuid(value: &str) -> Option<String> {
    let parsed = uuid::Uuid::parse_str(value).ok()?.hyphenated().to_string();
    (parsed == value.to_ascii_lowercase()).then_some(parsed)
}

fn canonical_gpu_identifier(value: &str) -> Option<String> {
    if let Some(value) = value.strip_prefix("GPU-") {
        return Some(format!("GPU-{}", canonical_uuid(value)?));
    }
    if let Some(value) = value.strip_prefix("MIG-GPU-") {
        let parts: Vec<_> = value.split('/').collect();
        if parts.len() != 3 {
            return None;
        }
        let parent = canonical_uuid(parts[0])?;
        let instance = |part: &str| {
            if part.is_empty() || !part.bytes().all(|byte| byte.is_ascii_digit()) {
                return None;
            }
            part.parse::<u32>().ok()
        };
        return Some(format!(
            "MIG-GPU-{parent}/{}/{}",
            instance(parts[1])?,
            instance(parts[2])?
        ));
    }
    value
        .strip_prefix("MIG-")
        .and_then(canonical_uuid)
        .map(|id| format!("MIG-{id}"))
}

#[cfg(test)]
mod tests {
    use super::*;
    const A: &str = "aaaaaaaa-aaaa-aaaa-aaaa-aaaaaaaaaaaa";
    const B: &str = "bbbbbbbb-bbbb-bbbb-bbbb-bbbbbbbbbbbb";

    #[test]
    fn preserves_restricted_identifiers_and_order() {
        assert_eq!(parse_visible_devices(" 3,1 ").unwrap(), ["3", "1"]);
        assert_eq!(
            parse_visible_devices(&format!("GPU-{A},GPU-{B}")).unwrap(),
            [format!("GPU-{A}"), format!("GPU-{B}")]
        );
        assert_eq!(
            parse_visible_devices(&format!("MIG-{A},MIG-{B}")).unwrap(),
            [format!("MIG-{A}"), format!("MIG-{B}")]
        );
        assert_eq!(
            parse_visible_devices(&format!("MIG-GPU-{A}/1/2,MIG-GPU-{B}/3/4")).unwrap(),
            [format!("MIG-GPU-{A}/1/2"), format!("MIG-GPU-{B}/3/4")]
        );
    }

    #[test]
    fn disabled_visibility_is_empty() {
        for value in ["", " ", "-1"] {
            assert!(parse_visible_devices(value).unwrap().is_empty());
        }
    }

    #[test]
    fn canonicalizes_uuid_case_and_legacy_instance_numbers() {
        assert_eq!(
            parse_visible_devices(&format!("GPU-{}", A.to_ascii_uppercase())).unwrap(),
            [format!("GPU-{A}")]
        );
        assert_eq!(
            parse_visible_devices(&format!("MIG-GPU-{A}/01/02")).unwrap(),
            [format!("MIG-GPU-{A}/1/2")]
        );
    }

    #[test]
    fn rejects_invalid_or_repeated_identifiers() {
        let invalid = [
            "0,0",
            "0,00",
            "0,",
            "all",
            "-2",
            "0,-1",
            "GPU-",
            "MIG-",
            "GPU-a",
            "GPU-ab",
            "GPU-a/b",
            "GPU-a b",
            "0,GPU-a",
            "GPU-a,MIG-a",
        ];
        for value in invalid {
            assert!(parse_visible_devices(value).is_err(), "{value}");
        }
        for value in [
            format!("GPU-{A},GPU-{}", A.to_ascii_uppercase()),
            format!("GPU-{A},MIG-{B}"),
            format!("MIG-GPU-{A}/1/2,MIG-GPU-{A}/01/02"),
            format!("MIG-GPU-{A}/1/2/3"),
            format!("MIG-GPU-{A}/-1/2"),
            format!("MIG-{A},MIG-GPU-{B}/1/2"),
        ] {
            assert!(parse_visible_devices(&value).is_err(), "{value}");
        }
    }
}
