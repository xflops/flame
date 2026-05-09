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

use flame_rs::client::ResourceRequirement;

/// Formats a byte count into a human-readable string with appropriate unit suffix.
/// Uses binary prefixes (Ki, Mi, Gi) following Kubernetes conventions.
pub fn format_memory(bytes: u64) -> String {
    if bytes >= 1024 * 1024 * 1024 {
        format!("{}Gi", bytes / (1024 * 1024 * 1024))
    } else if bytes >= 1024 * 1024 {
        format!("{}Mi", bytes / (1024 * 1024))
    } else if bytes >= 1024 {
        format!("{}Ki", bytes / 1024)
    } else {
        format!("{}B", bytes)
    }
}

/// Formats an optional `ResourceRequirement` for display in `flmctl`.
///
/// Returns `"-"` when no requirement is set so list/view output stays aligned
/// with the rest of the CLI's "missing value" convention. Otherwise renders as
/// `cpu=N,mem=Xunit,gpu=N` (no spaces) — matching the syntax accepted by
/// `ResourceRequirement::from(&String)` so the printed value can be re-used
/// verbatim with `flmctl create -s --resreq ...`.
pub fn format_resreq(r: &Option<ResourceRequirement>) -> String {
    match r {
        None => "-".to_string(),
        Some(r) => format!(
            "cpu={},mem={},gpu={}",
            r.cpu,
            format_memory(r.memory),
            r.gpu
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn format_resreq_none_renders_dash() {
        assert_eq!(format_resreq(&None), "-");
    }

    #[test]
    fn format_resreq_typical_values() {
        let rr = ResourceRequirement {
            cpu: 4,
            memory: 16 * 1024 * 1024 * 1024,
            gpu: 2,
        };
        assert_eq!(format_resreq(&Some(rr)), "cpu=4,mem=16Gi,gpu=2");
    }

    #[test]
    fn format_resreq_zero_values() {
        let rr = ResourceRequirement {
            cpu: 0,
            memory: 0,
            gpu: 0,
        };
        assert_eq!(format_resreq(&Some(rr)), "cpu=0,mem=0B,gpu=0");
    }
}
