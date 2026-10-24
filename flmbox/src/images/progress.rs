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

//! The progress output of image pulls: what happened, to stderr (stdout
//! carries the results).

use std::io::{IsTerminal, Write};

/// A step of an image pull.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PullEvent {
    /// The manifest was resolved; `blobs` (config and layers) of `bytes` in total will be downloaded.
    Resolved {
        digest: String,
        blobs: usize,
        bytes: u64,
    },
    /// The image is already in the node cache.
    Cached { digest: String },
    /// Progress of one blob download; the last event of a blob has `finished` set.
    Blob {
        name: String,
        done: u64,
        size: u64,
        finished: bool,
    },
    /// The layers are being unpacked.
    Unpacking,
    /// The image is in the cache.
    Pulled { digest: String },
}

/// Prints [`PullEvent`]s. A
/// terminal gets one live line per blob; a pipe gets one line per finished blob.
#[derive(Clone)]
pub(super) struct PullPrinter {
    tty: bool,
}

impl PullPrinter {
    pub(super) fn new() -> Self {
        Self {
            tty: std::io::stderr().is_terminal(),
        }
    }
}

impl PullPrinter {
    /// Prints one step.
    pub(super) fn report(&self, event: PullEvent) {
        let mut err = std::io::stderr().lock();
        let _ = match event {
            PullEvent::Cached { digest } => writeln!(err, "image is cached: {digest}"),
            PullEvent::Resolved {
                digest,
                blobs,
                bytes,
            } => writeln!(
                err,
                "pulling {digest}: {blobs} blobs, {}",
                human_bytes(bytes)
            ),
            PullEvent::Blob {
                name,
                done,
                size,
                finished,
            } => {
                let line = format!("  {name}: {} / {}", human_bytes(done), human_bytes(size));
                if finished {
                    if self.tty {
                        writeln!(err, "\r\x1b[2K{line} done")
                    } else {
                        writeln!(err, "{line} done")
                    }
                } else if self.tty {
                    write!(err, "\r\x1b[2K{line}")
                } else {
                    Ok(())
                }
            }
            PullEvent::Unpacking => writeln!(err, "unpacking"),
            PullEvent::Pulled { digest } => writeln!(err, "pulled {digest}"),
        };
    }
}

/// `1.5 MiB`-style size.
pub fn human_bytes(n: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KiB", "MiB", "GiB", "TiB"];
    let mut value = n as f64;
    let mut unit = 0;
    while value >= 1024.0 && unit < UNITS.len() - 1 {
        value /= 1024.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{n} B")
    } else {
        format!("{value:.1} {}", UNITS[unit])
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sizes_are_human_readable() {
        assert_eq!(human_bytes(0), "0 B");
        assert_eq!(human_bytes(1023), "1023 B");
        assert_eq!(human_bytes(1536), "1.5 KiB");
        assert_eq!(human_bytes(30 << 20), "30.0 MiB");
        assert_eq!(human_bytes(3 << 40), "3.0 TiB");
    }
}
