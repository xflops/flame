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

//! Image access used by the sandbox lifecycle, as a trait so tests do not need
//! a registry.

use async_trait::async_trait;
use flame_rs::apis::FlameError;

mod progress;
mod store;
mod unpack;

pub use progress::human_bytes;
pub use store::{parse_reference, ImageConfig, ImageInfo, ImageRoot, ImageStore};

/// Provides unpacked images.
#[async_trait]
pub trait Images: Send + Sync {
    /// Makes sure the image is available locally and returns it.
    async fn pull_image(&self, reference: &str) -> Result<ImageRoot, FlameError>;

    /// The images in the local cache, newest first.
    async fn list_images(&self) -> Result<Vec<ImageInfo>, FlameError>;
}

#[async_trait]
impl Images for ImageStore {
    async fn pull_image(&self, reference: &str) -> Result<ImageRoot, FlameError> {
        ImageStore::pull_image(self, reference).await
    }

    async fn list_images(&self) -> Result<Vec<ImageInfo>, FlameError> {
        ImageStore::list(self)
    }
}
