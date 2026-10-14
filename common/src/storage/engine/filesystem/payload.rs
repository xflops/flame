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

use crate::FlameError;
use std::fs;
use std::os::unix::fs::FileExt;
use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};

/// The per-session files that are opened once and shared for positional I/O.
#[derive(Clone, Copy)]
pub(super) enum Part {
    Tasks,
    Inputs,
    Outputs,
    Affinity,
}

impl Part {
    pub(super) const COUNT: usize = 4;

    pub(super) fn file_name(self) -> &'static str {
        match self {
            Part::Tasks => "tasks.bin",
            Part::Inputs => "inputs.bin",
            Part::Outputs => "outputs.bin",
            Part::Affinity => "affinity.bin",
        }
    }
}

/// A file used through positional I/O only: no shared cursor and no file-wide lock.
pub(super) struct PayloadFile {
    pub(super) file: fs::File,
    /// Next unreserved offset. Payloads are immutable once published, so ranges are
    /// reserved atomically and then written and synced without holding any lock.
    end: AtomicU64,
}

impl PayloadFile {
    pub(super) fn open(path: &Path, create: bool) -> Result<Self, FlameError> {
        // Not opened in append mode: positional writes on an O_APPEND file ignore
        // the requested offset on Linux.
        let file = fs::OpenOptions::new()
            .read(true)
            .write(create)
            .create(create)
            .truncate(false)
            .open(path)
            .map_err(|e| FlameError::Storage(format!("Failed to open {path:?}: {e}")))?;
        let end = file
            .metadata()
            .map_err(|e| FlameError::Storage(format!("Failed to stat {path:?}: {e}")))?
            .len();
        Ok(Self {
            file,
            end: AtomicU64::new(end),
        })
    }

    /// Reserve a unique range, write the complete payload there and sync it.
    ///
    /// A failed or cancelled write leaves an unreferenced range; the allocator is
    /// never rolled back because other reservations may follow it.
    pub(super) fn append(&self, name: &str, data: &[u8]) -> Result<u64, FlameError> {
        let len = data.len() as u64;
        // An explicit CAS loop: `fetch_update` is deprecated on newer toolchains.
        let mut offset = self.end.load(Ordering::Acquire);
        loop {
            let next = offset.checked_add(len).ok_or_else(|| {
                FlameError::Storage(format!("Offset overflow appending to {name}"))
            })?;
            match self
                .end
                .compare_exchange_weak(offset, next, Ordering::AcqRel, Ordering::Acquire)
            {
                Ok(_) => break,
                Err(current) => offset = current,
            }
        }

        self.file
            .write_all_at(data, offset)
            .map_err(|e| FlameError::Storage(format!("Failed to write to {name}: {e}")))?;
        self.file
            .sync_data()
            .map_err(|e| FlameError::Storage(format!("Failed to sync {name}: {e}")))?;

        Ok(offset)
    }
}
