/*
Copyright 2026 The Flame Authors.
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

//! Plain-text formatting of the listings.

use std::io::Write;

pub fn row<const N: usize>(cells: [&str; N]) -> Vec<String> {
    cells.iter().map(|c| c.to_string()).collect()
}

/// Prints rows as left-aligned columns separated by two spaces.
pub fn print_table(rows: &[Vec<String>]) {
    let columns = rows.iter().map(Vec::len).max().unwrap_or(0);
    let widths: Vec<usize> = (0..columns)
        .map(|c| {
            rows.iter()
                .filter_map(|r| r.get(c))
                .map(|s| s.len())
                .max()
                .unwrap_or(0)
        })
        .collect();
    let mut out = std::io::stdout().lock();
    for r in rows {
        let line: Vec<String> = r
            .iter()
            .enumerate()
            .map(|(c, cell)| format!("{cell:<width$}", width = widths[c]))
            .collect();
        let _ = writeln!(out, "{}", line.join("  ").trim_end());
    }
}

/// `sha256:` and the first twelve hex digits.
pub fn short_digest(digest: &str) -> &str {
    let end = digest.find(':').map_or(0, |i| i + 1) + 12;
    &digest[..end.min(digest.len())]
}

/// `45s`, `3m`, `2h`, `4d`.
pub fn human_age(secs: u64) -> String {
    match secs {
        0..=59 => format!("{secs}s"),
        60..=3599 => format!("{}m", secs / 60),
        3600..=86399 => format!("{}h", secs / 3600),
        _ => format!("{}d", secs / 86400),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn listing_helpers() {
        assert_eq!(
            short_digest("sha256:0123456789abcdef0123"),
            "sha256:0123456789ab"
        );
        assert_eq!(short_digest("sha256:ab"), "sha256:ab");
        assert_eq!(human_age(5), "5s");
        assert_eq!(human_age(150), "2m");
        assert_eq!(human_age(7200), "2h");
        assert_eq!(human_age(3 * 86400), "3d");
    }
}
