/*
Copyright 2025 The Flame Authors.
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
/// Verify an absolute filesystem URL preserves its storage path.
fn test_parse_storage_url_absolute() {
    let path = parse_storage_url("fs:///var/lib/flame/cache").unwrap();
    assert_eq!(path, PathBuf::from("/var/lib/flame/cache"));
}

#[test]
/// Verify relative filesystem URLs resolve beneath the configured home.
fn test_parse_storage_url_relative() {
    let path = parse_storage_url("fs://cache").unwrap();
    assert!(path.ends_with("cache"));
    assert!(path.is_absolute() || path.starts_with("cache"));
}

#[test]
/// Verify the file URL scheme selects the same filesystem path.
fn test_parse_storage_url_file_scheme() {
    let path = parse_storage_url("file:///tmp/cache").unwrap();
    assert_eq!(path, PathBuf::from("/tmp/cache"));
}

#[tokio::test]
/// Reject removed and unsupported backends rather than interpreting them as filesystem paths.
async fn test_connect_rejects_non_filesystem_backends() {
    for url in [
        "",
        "none",
        "memory://cache",
        "s3://bucket",
        "fs://",
        "file://",
    ] {
        assert!(
            matches!(
                connect(url, &pool(0)).await,
                Err(FlameError::InvalidConfig(_))
            ),
            "{url}"
        );
    }
}
#[tokio::test]
/// Connect filesystem storage through a plain absolute directory path.
async fn test_connect_filesystem_path() {
    let directory = tempfile::tempdir().unwrap();
    let storage = connect(directory.path().to_str().unwrap(), &pool(0))
        .await
        .unwrap();
    assert!(storage
        .list(&ObjectFilter::AllObject)
        .await
        .unwrap()
        .is_empty());
}
