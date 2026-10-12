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

//! Host memory reservations. Assignments carry the reserved value and no device IDs; the claim
//! also bounds how much memory FEM hands out.

use common::apis::ResourceRequirement;
use common::FlameError;
use stdng::lock_ptr;

use crate::devices::ledger::{Ledger, LedgerPtr};
use crate::devices::{DeviceAssignment, DeviceManager};
use crate::executor::Executor;

#[cfg(not(target_os = "linux"))]
fn total_memory() -> u64 {
    0
}

#[cfg(target_os = "linux")]
fn total_memory() -> u64 {
    rustix::system::sysinfo().totalram
}

#[derive(Debug)]
pub struct MemoryManager {
    ledger: LedgerPtr,
}

impl Default for MemoryManager {
    fn default() -> Self {
        Self::new()
    }
}

impl MemoryManager {
    pub fn new() -> Self {
        Self {
            ledger: stdng::new_ptr(Ledger::new(crate::devices::MEMORY, total_memory())),
        }
    }
}

impl DeviceManager for MemoryManager {
    fn discover(&self) -> Result<(), FlameError> {
        lock_ptr!(self.ledger)?.refresh(total_memory())
    }

    fn capacity(&self) -> ResourceRequirement {
        ResourceRequirement {
            memory: lock_ptr!(self.ledger)
                .map(|ledger| ledger.total())
                .unwrap_or(0),
            ..Default::default()
        }
    }

    fn allocate(&self, executor: &Executor) -> Result<DeviceAssignment, FlameError> {
        let value = lock_ptr!(self.ledger)?.allocate(&executor.id, executor.resreq.memory)?;
        Ok(DeviceAssignment::new(Vec::new()).with_value(value))
    }

    fn release(&self, executor: &Executor) -> Result<(), FlameError> {
        lock_ptr!(self.ledger)?.release(&executor.id)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::devices::test_executor;

    #[test]
    fn reports_only_memory_and_bounds_reservations() {
        let manager = MemoryManager::new();
        manager.discover().unwrap();
        let capacity = manager.capacity();
        assert_eq!((capacity.cpu, capacity.gpu), (0, 0));
        if capacity.memory == 0 {
            return; // memory discovery is Linux-only
        }
        let request = |amount| {
            test_executor(ResourceRequirement {
                memory: amount,
                ..Default::default()
            })
        };
        let total = capacity.memory;
        let first = request(total);
        let assignment = manager.allocate(&first).unwrap();
        assert!(assignment.devices().is_empty());
        assert_eq!(assignment.value(), total);
        assert!(manager.allocate(&request(1)).is_err());
        manager.release(&first).unwrap();
        manager.release(&first).unwrap();
        manager.allocate(&request(total)).unwrap();
    }
}
