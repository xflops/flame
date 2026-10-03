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

use std::sync::Arc;

use crate::apis::{Event, EventOwner, SessionGID};
use crate::FlameError;

mod fs;
mod memory;

pub use fs::FsEventManager;
pub use memory::MemoryEventManager;

pub trait EventManager: Send + Sync {
    fn record_event(&self, owner: EventOwner, event: Event) -> Result<(), FlameError>;
    fn find_events(&self, owner: EventOwner) -> Result<Vec<Event>, FlameError>;
    fn remove_events(&self, session: &SessionGID) -> Result<(), FlameError>;
    fn clear(&self) -> Result<(), FlameError>;
}

pub type EventManagerPtr = Arc<dyn EventManager>;
