/* Copyright 2026 The Flame Authors.
Licensed under the Apache License, Version 2.0 (the "License");
you may not use this file except in compliance with the License.
You may obtain a copy of the License at http://www.apache.org/licenses/LICENSE-2.0
Unless required by applicable law or agreed to in writing, software distributed
under the License is distributed on an "AS IS" BASIS, WITHOUT WARRANTIES OR
CONDITIONS OF ANY KIND, either express or implied. See the License for the
specific language governing permissions and limitations under the License. */

use super::*;

mod cached_reads;
mod connection;
mod filesystem;
mod object_reader;
mod part_cache;

mod parallel_writes;

/// Configure a test pool with the requested resident payload budget.
fn pool(size: u64) -> common::ctx::FlameCachePool {
    common::ctx::FlameCachePool {
        size,
        ..Default::default()
    }
}
