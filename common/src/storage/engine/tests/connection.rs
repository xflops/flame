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
#[tokio::test]
async fn unsupported_storage_urls_are_rejected() {
    for url in [
        "sqlite:///tmp/flame.db",
        "sqlite:flame.db",
        "postgres://localhost/flame",
        "unknown",
        "",
    ] {
        assert!(
            matches!(connect(url).await, Err(FlameError::InvalidConfig(_))),
            "{url}"
        );
    }
}
#[tokio::test]
async fn filesystem_aliases_and_none_are_supported() {
    assert!(connect("none").await.is_ok());
    let dir = tempfile::tempdir().unwrap();
    for scheme in ["fs", "file", "filesystem"] {
        assert!(
            connect(&format!("{scheme}://{}", dir.path().join(scheme).display()))
                .await
                .is_ok()
        );
    }
}
