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

//! Executor-scoped reservations of one countable host resource.

use std::collections::HashMap;

use common::FlameError;

/// Tracks per-executor claims against a host total. Claims are in memory only:
/// unlike devices, nothing outside FEM needs them to survive a restart.
#[derive(Debug)]
pub(super) struct Ledger {
    resource: &'static str,
    total: u64,
    claims: HashMap<String, u64>,
}

pub(super) type LedgerPtr = stdng::MutexPtr<Ledger>;

impl Ledger {
    pub(super) fn new(resource: &'static str, total: u64) -> Self {
        Self {
            resource,
            total,
            claims: HashMap::new(),
        }
    }

    pub(super) fn total(&self) -> u64 {
        self.total
    }

    /// Update the host total; it may not drop below what is already reserved.
    pub(super) fn refresh(&mut self, total: u64) -> Result<(), FlameError> {
        let reserved: u64 = self.claims.values().sum();
        if total < reserved {
            return Err(FlameError::InvalidState(format!(
                "refreshed {} capacity {total} is below reserved {reserved}",
                self.resource
            )));
        }
        self.total = total;
        Ok(())
    }

    /// Return the claimed amount; a repeated executor ID
    /// keeps its existing claim.
    pub(super) fn allocate(&mut self, executor_id: &str, amount: u64) -> Result<u64, FlameError> {
        if let Some(claimed) = self.claims.get(executor_id) {
            return Ok(*claimed);
        }
        let available = self.total - self.claims.values().sum::<u64>();
        if amount > available {
            return Err(FlameError::InvalidState(format!(
                "executor requests {amount} {}, but only {available} are available",
                self.resource
            )));
        }
        self.claims.insert(executor_id.to_string(), amount);
        Ok(amount)
    }

    pub(super) fn release(&mut self, executor_id: &str) -> Result<(), FlameError> {
        self.claims.remove(executor_id);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn allocation_is_bounded_idempotent_and_released() {
        let mut ledger = Ledger::new(crate::devices::CPU, 4);
        assert_eq!(ledger.allocate("a", 3).unwrap(), 3);
        assert_eq!(ledger.allocate("a", 4).unwrap(), 3);
        assert!(ledger.allocate("b", 2).is_err());
        ledger.allocate("b", 1).unwrap();
        ledger.release("a").unwrap();
        ledger.release("a").unwrap();
        ledger.allocate("c", 3).unwrap();
    }

    #[test]
    fn refresh_cannot_drop_below_reserved() {
        let mut ledger = Ledger::new(crate::devices::CPU, 4);
        ledger.allocate("a", 3).unwrap();
        assert!(ledger.refresh(2).is_err());
        assert_eq!(ledger.total(), 4);
        ledger.refresh(8).unwrap();
        assert_eq!(ledger.total(), 8);
    }
}
