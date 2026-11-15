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
fn test_one_data() {
    let tmp_dir = tempfile::tempdir().unwrap();
    let mut data_storage =
        DataStorage::new(tmp_dir.path().to_string_lossy().as_ref(), "test_1").unwrap();
    let index = data_storage.save(b"hello, world").unwrap();
    assert_eq!(index.start, 0);
    assert_eq!(index.end, 12);
    let data = data_storage.load(&index).unwrap();
    assert_eq!(data, b"hello, world");

    data_storage.clear().unwrap();
}

#[test]
fn test_two_data() {
    let tmp_dir = tempfile::tempdir().unwrap();
    let mut data_storage =
        DataStorage::new(tmp_dir.path().to_string_lossy().as_ref(), "test_2").unwrap();
    let index = data_storage.save(b"hello, world").unwrap();
    assert_eq!(index.start, 0);
    assert_eq!(index.end, 12);
    let data = data_storage.load(&index).unwrap();
    assert_eq!(data, b"hello, world");

    let index2 = data_storage.save(b"Good morning, Klaus").unwrap();
    assert_eq!(index2.start, 12);
    assert_eq!(index2.end, 31);
    let data = data_storage.load(&index2).unwrap();
    assert_eq!(data, b"Good morning, Klaus");

    let data = data_storage.load(&index).unwrap();
    assert_eq!(data, b"hello, world");

    data_storage.clear().unwrap();
}
