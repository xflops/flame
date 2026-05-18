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

use chrono::Duration;
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
/// `ResourceRequirement::parse` so the printed value can be re-used
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

/// Formats a duration as compact CLI output such as `45s` or `1h 5m`.
pub fn format_duration(duration: Duration) -> String {
    let seconds_raw = duration.num_seconds();
    let sign = if seconds_raw < 0 { "-" } else { "" };
    let mut seconds = seconds_raw.unsigned_abs();

    let days = seconds / 86_400;
    seconds %= 86_400;
    let hours = seconds / 3_600;
    seconds %= 3_600;
    let minutes = seconds / 60;
    seconds %= 60;

    let mut parts = Vec::new();
    if days > 0 {
        parts.push(format!("{days}d"));
    }
    if hours > 0 {
        parts.push(format!("{hours}h"));
    }
    if minutes > 0 {
        parts.push(format!("{minutes}m"));
    }
    if seconds > 0 || parts.is_empty() {
        parts.push(format!("{seconds}s"));
    }

    format!("{sign}{}", parts.join(" "))
}

/// Formats an optional duration, using `-` for unset values.
pub fn format_optional_duration(duration: &Option<Duration>) -> String {
    match duration {
        Some(duration) => format_duration(*duration),
        None => "-".to_string(),
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

    #[test]
    fn format_duration_seconds() {
        assert_eq!(format_duration(Duration::seconds(60)), "1m");
    }

    #[test]
    fn format_duration_compound_values() {
        assert_eq!(
            format_duration(Duration::seconds(86_400 + 3_600 + 120 + 3)),
            "1d 1h 2m 3s"
        );
    }

    #[test]
    fn format_duration_zero() {
        assert_eq!(format_duration(Duration::zero()), "0s");
    }

    #[test]
    fn format_duration_negative() {
        assert_eq!(format_duration(Duration::seconds(-65)), "-1m 5s");
    }

    #[test]
    fn format_optional_duration_some() {
        assert_eq!(format_optional_duration(&Some(Duration::seconds(60))), "1m");
    }

    #[test]
    fn format_optional_duration_none_renders_dash() {
        assert_eq!(format_optional_duration(&None), "-");
    }
}
