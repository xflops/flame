/*
Copyright 2025 The xflops Authors.
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

use super::*;

#[test]
fn task_record_size_is_constant_for_extreme_values() {
    let max = TaskMetadata {
        id: u64::MAX,
        version: u32::MAX,
        checksum: u32::MAX,
        state: u8::MAX,
        creation_time: i64::MIN,
        completion_time: i64::MAX,
        input_offset: u64::MAX,
        input_len: u64::MAX,
        output_offset: u64::MAX,
        output_len: u64::MAX,
        affinity_offset: u64::MAX,
        affinity_len: u64::MAX,
    };
    let encoded = postcard::to_allocvec(&max).unwrap();
    assert_eq!(encoded.len(), task_record_size());
    let (decoded, _): (TaskMetadata, _) = postcard::take_from_bytes(&encoded).unwrap();
    assert_eq!(decoded.id, u64::MAX);
    assert_eq!(decoded.creation_time, i64::MIN);
}
