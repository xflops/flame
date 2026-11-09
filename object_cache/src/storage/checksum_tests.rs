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

use super::*;
use futures::{stream, StreamExt};
use sha2::{Digest, Sha256};
use std::io::Write;
use tempfile::tempdir;

fn key() -> ObjectKey {
    ObjectKey::try_from("checksums/session/object").unwrap()
}

fn part(bytes: &[u8]) -> ObjectData {
    ObjectData::new()
        .with_creation_time(1_700_000_000_000)
        .with_data(bytes.to_vec())
}

async fn put(storage: &DiskStorage, key: &ObjectKey, bytes: &[u8]) {
    storage
        .write_stream(
            key,
            ObjectMetadata::new().with_data_type("raw"),
            Box::pin(stream::iter(vec![Ok(Bytes::copy_from_slice(bytes))])),
        )
        .await
        .unwrap();
}

fn flip(path: &Path, offset: u64) {
    let file = fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(path)
        .unwrap();
    let mut byte = [0];
    file.read_exact_at(&mut byte, offset).unwrap();
    byte[0] ^= 0x80;
    file.write_all_at(&byte, offset).unwrap();
    file.sync_all().unwrap();
}

fn checksum_error<T>(result: Result<T, FlameError>) {
    let error = match result {
        Ok(_) => panic!("corrupt payload was accepted"),
        Err(error) => error.to_string().to_lowercase(),
    };
    assert!(error.contains("checksum"), "{error}");
}

#[tokio::test]
async fn same_length_corruption_fails_cached_and_reopened_reads() {
    let dir = tempdir().unwrap();
    let storage = DiskStorage::new(dir.path().to_path_buf()).unwrap();
    let key = key();
    let payload = vec![0x52; 3 * 1024 * 1024 + 17];
    put(&storage, &key, &payload).await;
    let files = storage.open(&key).await.unwrap().unwrap();
    let mut first = files.clone().read_part(key.to_key().unwrap(), 0).unwrap();
    while let Some(chunk) = first.next().await {
        chunk.unwrap();
    }
    let data_path = storage.object_dir(&key).join(DATA_FILE);
    flip(&data_path, payload.len() as u64 - 1);
    assert_eq!(
        fs::metadata(&data_path).unwrap().len(),
        payload.len() as u64
    );

    // A previous successful read must not permanently mark the handle verified.
    let mut corrupted = files.read_part(key.to_key().unwrap(), 0).unwrap();
    let mut delivered = 0;
    let mut failed = false;
    while let Some(chunk) = corrupted.next().await {
        match chunk {
            Ok(bytes) => delivered += bytes.len(),
            Err(error) => {
                checksum_error::<()>(Err(error));
                failed = true;
                break;
            }
        }
    }
    assert!(
        delivered > 0,
        "fixture must exercise failure after partial delivery"
    );
    assert!(
        failed,
        "stream ended successfully after a late payload bit flip"
    );
    assert!(
        corrupted.next().await.is_none(),
        "failure must terminate the stream"
    );

    let reopened = DiskStorage::new(dir.path().to_path_buf()).unwrap();
    checksum_error(reopened.read_object(&key, 0).await);
}

#[tokio::test]
async fn corrupted_append_does_not_invalidate_earlier_part() {
    let dir = tempdir().unwrap();
    let storage = DiskStorage::new(dir.path().to_path_buf()).unwrap();
    let key = key();
    put(&storage, &key, b"first").await;
    storage.patch_object(&key, &part(b"second")).await.unwrap();
    flip(&storage.object_dir(&key).join(DATA_FILE), 5);
    let reopened = DiskStorage::new(dir.path().to_path_buf()).unwrap();
    assert_eq!(
        reopened.read_range(&key, 0, 0, 5).await.unwrap(),
        Bytes::from_static(b"first")
    );
    checksum_error(reopened.read_object(&key, 1).await);
    checksum_error(reopened.read_object(&key, 0).await);
}

#[tokio::test]
async fn range_read_checks_corruption_outside_requested_bytes() {
    let dir = tempdir().unwrap();
    let storage = DiskStorage::new(dir.path().to_path_buf()).unwrap();
    let key = key();
    put(&storage, &key, b"prefix-valid-suffix").await;
    flip(&storage.object_dir(&key).join(DATA_FILE), 18);
    checksum_error(storage.read_range(&key, 0, 0, 6).await);
}

#[tokio::test]
async fn empty_put_and_patch_remain_two_verified_parts_after_reopen() {
    let dir = tempdir().unwrap();
    let storage = DiskStorage::new(dir.path().to_path_buf()).unwrap();
    let key = key();
    put(&storage, &key, b"").await;
    storage.patch_object(&key, &part(b"")).await.unwrap();
    let reopened = DiskStorage::new(dir.path().to_path_buf()).unwrap();
    let stat = reopened.stat(&key).await.unwrap().unwrap();
    assert_eq!((stat.version(), stat.size()), (2, 0));
    let expected: [u8; 32] = Sha256::digest(b"").into();
    assert!(stat
        .records
        .iter()
        .all(|record| record.checksum == Some(expected)));
    let object = reopened.read_object(&key, 0).await.unwrap().unwrap();
    assert_eq!(object.data.len(), 2);
    assert!(object.data.iter().all(|part| part.data.is_empty()));
    assert_eq!(
        reopened
            .read_object(&key, 1)
            .await
            .unwrap()
            .unwrap()
            .data
            .len(),
        1
    );
}

#[tokio::test]
async fn persists_standard_sha256_of_chunked_payload() {
    let dir = tempdir().unwrap();
    let storage = DiskStorage::new(dir.path().to_path_buf()).unwrap();
    let key = key();
    storage
        .write_stream(
            &key,
            ObjectMetadata::new().with_data_type("raw"),
            Box::pin(stream::iter(vec![
                Ok(Bytes::from_static(b"a")),
                Ok(Bytes::new()),
                Ok(Bytes::from_static(b"bc")),
            ])),
        )
        .await
        .unwrap();
    let object_dir = storage.object_dir(&key);
    let metadata: serde_json::Value =
        serde_json::from_slice(&fs::read(object_dir.join(METADATA_FILE)).unwrap()).unwrap();
    assert_eq!(metadata, serde_json::json!({"data_type": "raw"}));
    let index = fs::read(object_dir.join(INDEX_FILE)).unwrap();
    assert_eq!(index.len(), 56);
    let expected = [
        0xba, 0x78, 0x16, 0xbf, 0x8f, 0x01, 0xcf, 0xea, 0x41, 0x41, 0x40, 0xde, 0x5d, 0xae, 0x22,
        0x23, 0xb0, 0x03, 0x61, 0xa3, 0x96, 0x17, 0x7a, 0x9c, 0xb4, 0x10, 0xff, 0x61, 0xf2, 0x00,
        0x15, 0xad,
    ];
    assert_eq!(&index[24..], &expected);
    assert_eq!(fs::read(object_dir.join(DATA_FILE)).unwrap(), b"abc");
}

#[tokio::test]
async fn recovers_partial_index_without_committing_orphan_payload() {
    let dir = tempdir().unwrap();
    let storage = DiskStorage::new(dir.path().to_path_buf()).unwrap();
    let key = key();
    put(&storage, &key, b"first").await;
    let object_dir = storage.object_dir(&key);
    {
        let mut data = fs::OpenOptions::new()
            .append(true)
            .open(object_dir.join(DATA_FILE))
            .unwrap();
        data.write_all(b"orphan").unwrap();
        let mut index = fs::OpenOptions::new()
            .append(true)
            .open(object_dir.join(INDEX_FILE))
            .unwrap();
        index.write_all(&[0xab; 31]).unwrap();
    }
    let reopened = DiskStorage::new(dir.path().to_path_buf()).unwrap();
    let stat = reopened.stat(&key).await.unwrap().unwrap();
    assert_eq!((stat.version(), stat.size()), (1, 5));
    assert_eq!(fs::metadata(object_dir.join(INDEX_FILE)).unwrap().len(), 56);
    reopened.patch_object(&key, &part(b"next")).await.unwrap();
    let stat = reopened.stat(&key).await.unwrap().unwrap();
    assert_eq!((stat.version(), stat.size()), (2, 9));
    assert_eq!(
        stat.records[1].offset, 11,
        "orphan bytes remain outside committed parts"
    );
    let object = reopened.read_object(&key, 0).await.unwrap().unwrap();
    assert_eq!(object.data[0].data, Bytes::from_static(b"first"));
    assert_eq!(object.data[1].data, Bytes::from_static(b"next"));
}

#[tokio::test]
async fn appended_part_persists_digest_and_rejects_late_corruption_after_reopen() {
    let dir = tempdir().unwrap();
    let storage = DiskStorage::new(dir.path().to_path_buf()).unwrap();
    let key = key();
    put(&storage, &key, b"first").await;
    let payload: Vec<u8> = (0..4)
        .flat_map(|index| vec![index as u8; 1024 * 1024])
        .collect();
    let expected: [u8; 32] = Sha256::digest(&payload).into();
    storage
        .patch_object(
            &key,
            &ObjectData::new()
                .with_creation_time(1_700_000_000_001)
                .with_data(payload),
        )
        .await
        .unwrap();
    let reopened = DiskStorage::new(dir.path().to_path_buf()).unwrap();
    let stat = reopened.stat(&key).await.unwrap().unwrap();
    assert_eq!(stat.version(), 2);
    assert_eq!(stat.records[1].checksum, Some(expected));
    assert_eq!(stat.records[1].len, 4 * 1024 * 1024);
    assert_eq!(stat.records[1].creation_time, 1_700_000_000_001);
    // Consume the entire suffix before changing bytes; no verified-read cache
    // may hide corruption discovered by a subsequent stream from the same files.
    let files = reopened.open(&key).await.unwrap().unwrap();
    let mut valid = files.clone().read_part(key.to_key().unwrap(), 1).unwrap();
    let mut length = 0;
    while let Some(chunk) = valid.next().await {
        length += chunk.unwrap().len();
    }
    assert_eq!(length, 4 * 1024 * 1024);
    flip(
        &storage.object_dir(&key).join(DATA_FILE),
        stat.records[1].offset + stat.records[1].len - 1,
    );
    let mut corrupted = files.read_part(key.to_key().unwrap(), 1).unwrap();
    let mut delivered = 0;
    let mut failed = false;
    while let Some(chunk) = corrupted.next().await {
        match chunk {
            Ok(bytes) => delivered += bytes.len(),
            Err(error) => {
                checksum_error::<()>(Err(error));
                failed = true;
                break;
            }
        }
    }
    assert!(delivered > 0, "exercise failure after earlier patch chunks");
    assert!(failed, "an appended part with a late bit flip was accepted");
    assert!(corrupted.next().await.is_none());
    assert_eq!(
        reopened.read_range(&key, 0, 0, 5).await.unwrap(),
        Bytes::from_static(b"first")
    );
    checksum_error(reopened.read_object(&key, 1).await);
}
