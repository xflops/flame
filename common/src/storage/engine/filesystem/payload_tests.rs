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

use super::payload::PayloadFile;
use std::os::unix::fs::FileExt;
use std::sync::Arc;
use tempfile::TempDir;

#[test]
fn test_concurrent_payload_appends_reserve_distinct_ranges() {
    let dir = TempDir::new().unwrap();
    let file = Arc::new(PayloadFile::open(&dir.path().join("payload.bin"), true).unwrap());

    let mut threads = vec![];
    for tag in 0..8u8 {
        let file = file.clone();
        threads.push(std::thread::spawn(move || {
            (0..25)
                .map(|_| (tag, file.append("payload.bin", &[tag; 8]).unwrap()))
                .collect::<Vec<_>>()
        }));
    }
    let mut written: Vec<_> = threads
        .into_iter()
        .flat_map(|thread| thread.join().unwrap())
        .collect();
    written.sort_by_key(|(_, offset)| *offset);

    for (index, (tag, offset)) in written.iter().enumerate() {
        assert_eq!(
            *offset,
            index as u64 * 8,
            "ranges are unique and contiguous"
        );
        let mut buffer = [0u8; 8];
        file.file.read_exact_at(&mut buffer, *offset).unwrap();
        assert_eq!(buffer, [*tag; 8]);
    }
}
