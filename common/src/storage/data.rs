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

use std::fs::{self, File, OpenOptions};
use std::io::prelude::*;
use std::io::SeekFrom;
use std::path::PathBuf;

use serde_derive::{Deserialize, Serialize};

use crate::FlameError;

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Index {
    #[serde(with = "postcard::fixint::le")]
    pub start: u64,
    #[serde(with = "postcard::fixint::le")]
    pub end: u64,
}

pub struct DataStorage {
    path: PathBuf,
    data: File,
}

impl DataStorage {
    pub fn new(path: &str, name: &str) -> Result<Self, FlameError> {
        let data_file_path = PathBuf::from(format!("{}/{}.dat", path, name));
        let data = OpenOptions::new()
            .read(true)
            .create(true)
            .append(true)
            .open(&data_file_path)?;

        let path = fs::canonicalize(data_file_path)?;

        Ok(DataStorage { path, data })
    }

    pub fn save(&mut self, data: &[u8]) -> Result<Index, FlameError> {
        self.data.seek(SeekFrom::End(0))?;
        let tail = self.data.stream_position()?;

        self.data.write_all(data)?;
        self.data.flush()?;

        let index = Index {
            start: tail,
            end: tail + data.len() as u64,
        };

        Ok(index)
    }

    pub fn load(&mut self, index: &Index) -> Result<Vec<u8>, FlameError> {
        self.data.seek(SeekFrom::Start(index.start))?;
        let mut data = vec![0; (index.end - index.start) as usize];
        self.data.read_exact(&mut data)?;

        Ok(data)
    }

    pub fn clear(&mut self) -> Result<(), FlameError> {
        fs::remove_file(&self.path)
            .map_err(|e| FlameError::Storage(format!("failed to clear data storage: {}", e)))?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
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
}
