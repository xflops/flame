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

pub mod cpu;
pub mod gpu;
mod ledger;
pub mod memory;

use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use crate::executor::Executor;
use common::apis::ResourceRequirement;
use common::FlameError;
use serde_derive::{Deserialize, Serialize};

pub const CPU: &str = "cpu";
pub const MEMORY: &str = "memory";
pub const GPU: &str = "gpu";

/// A copyable description of a manager-owned allocation. Dropping it does not
/// release devices; FEM calls DeviceManager::release after service cleanup.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DeviceAssignment {
    pub envs: HashMap<String, String>,
    pub cdi: Vec<String>,
    devices: Vec<String>,
    /// Reserved quantity: CPU cores, memory bytes or GPU count.
    value: u64,
}

impl DeviceAssignment {
    pub(crate) fn new(devices: Vec<String>) -> Self {
        Self {
            envs: HashMap::new(),
            cdi: Vec::new(),
            devices,
            value: 0,
        }
    }

    pub(crate) fn with_value(mut self, value: u64) -> Self {
        self.value = value;
        self
    }

    pub fn value(&self) -> u64 {
        self.value
    }

    pub fn devices(&self) -> &[String] {
        &self.devices
    }
}

pub type DeviceAssignments = HashMap<String, DeviceAssignment>;

/// Manages one kind of local hardware for executor-scoped reservations.
pub trait DeviceManager: Send + Sync {
    /// Refresh and validate local inventory without changing manager configuration
    /// or active allocations. FEM calls this on every node refresh.
    fn discover(&self) -> Result<(), FlameError>;

    /// Capacity for this device type, including reserved devices.
    fn capacity(&self) -> ResourceRequirement;

    /// Validate and reserve this type's request atomically; failures consume no capacity.
    /// Existing executor IDs return their assignment; otherwise create a new claim.
    /// Executor lifecycle state does not affect allocation.
    /// The executor supplies allocation identity and resource requirements.
    /// The registry delegates without device-specific checks.
    fn allocate(&self, executor: &Executor) -> Result<DeviceAssignment, FlameError>;

    /// Explicitly free an allocation after all service cleanup completes.
    /// Executor IDs are globally unique; repeated releases are idempotent.
    fn release(&self, executor: &Executor) -> Result<(), FlameError>;
}

#[derive(Clone, Default)]
pub struct DeviceManagers {
    managers: Vec<(String, Arc<dyn DeviceManager>)>,
}

impl DeviceManagers {
    pub fn load(names: &[String]) -> Result<Self, FlameError> {
        let mut seen = HashSet::new();
        // Validate the entire configuration before discovering any hardware.
        for name in names {
            if !seen.insert(name) {
                return Err(FlameError::InvalidConfig(format!(
                    "duplicate device manager '{name}'"
                )));
            }
            if matches!(name.as_str(), CPU | MEMORY) {
                return Err(FlameError::InvalidConfig(format!(
                    "device manager '{name}' is mandatory and cannot be configured"
                )));
            }
            if name != GPU {
                return Err(FlameError::InvalidConfig(format!(
                    "unsupported device manager '{name}'; available: gpu"
                )));
            }
        }
        // CPU and memory are node capacity and always loaded; names select extras.
        let mut managers: Vec<(String, Arc<dyn DeviceManager>)> = vec![
            (CPU.into(), Arc::new(cpu::CpuManager::new())),
            (MEMORY.into(), Arc::new(memory::MemoryManager::new())),
        ];
        for name in names {
            let manager: Box<dyn DeviceManager> = match name.as_str() {
                GPU => Box::new(gpu::GpuManager::new()),
                _ => unreachable!("validated manager name"),
            };
            manager.discover()?;
            managers.push((name.clone(), Arc::from(manager)));
        }
        Ok(Self { managers })
    }

    pub fn discover(&self) -> Result<(), FlameError> {
        for (_, manager) in &self.managers {
            manager.discover()?;
        }
        Ok(())
    }

    pub fn capacity(&self) -> ResourceRequirement {
        let mut capacity = ResourceRequirement::default();
        for (_, manager) in &self.managers {
            let devices = manager.capacity();
            capacity.cpu += devices.cpu;
            capacity.memory += devices.memory;
            capacity.gpu += devices.gpu;
        }
        capacity
    }

    pub fn allocate(&self, executor: &Executor) -> Result<DeviceAssignments, FlameError> {
        let mut assignments = HashMap::new();
        for (name, manager) in &self.managers {
            match manager.allocate(executor) {
                Ok(assignment) => {
                    assignments.insert(name.clone(), assignment);
                }
                Err(error) => {
                    // Manager-owned claims survive a partial attempt and can be
                    // reused on retry or released explicitly with executor metadata.
                    return Err(error);
                }
            }
        }
        Ok(assignments)
    }

    pub fn release(&self, executor: &Executor) -> Result<(), FlameError> {
        for (_, manager) in &self.managers {
            manager.release(executor)?;
        }
        Ok(())
    }

    pub(crate) fn with_gpu(manager: gpu::GpuManager) -> Self {
        Self::with_manager(GPU, Arc::new(manager))
    }

    pub(crate) fn with_manager(name: &str, manager: Arc<dyn DeviceManager>) -> Self {
        Self {
            managers: vec![(name.to_string(), manager)],
        }
    }
}

/// Executor metadata fixture; no workers or manager pointers are created.
#[cfg(test)]
pub(crate) fn test_executor(resreq: ResourceRequirement) -> Executor {
    Executor {
        id: uuid::Uuid::new_v4().to_string(),
        application: "test-app".into(),
        node: "test-node".into(),
        resreq,
        assignments: Default::default(),
        shim: common::apis::Shim::Host,
        session: None,
        task: None,
        context: None,
        app_mgr: None,
        dev_mgr: None,
        shim_instance: None,
        state: common::apis::ExecutorState::Idle,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

    fn request() -> Executor {
        test_executor(ResourceRequirement {
            gpu: 1,
            ..Default::default()
        })
    }
    fn gpu(workdir: &std::path::Path) -> gpu::GpuManager {
        gpu::GpuManager::new()
            .with_workdir(workdir)
            .with_inventory(vec!["GPU-a".into()])
            .unwrap()
    }

    #[test]
    fn empty_config_loads_only_mandatory_host_managers() {
        let managers = DeviceManagers::load(&[]).unwrap();
        let capacity = managers.capacity();
        assert!(capacity.cpu > 0);
        assert_eq!(capacity.gpu, 0);
        let assignments = managers.allocate(&request()).unwrap();
        assert!(assignments.values().all(|a| a.devices().is_empty()));
        assert!(!assignments.contains_key(GPU));
        managers.release(&request()).unwrap();
    }

    #[test]
    fn duplicate_or_unsupported_managers_fail_before_discovery() {
        for names in [
            vec![GPU, GPU],
            vec!["tpu"],
            vec![GPU, "unknown"],
            vec![CPU],
            vec![MEMORY],
        ] {
            assert!(DeviceManagers::load(
                &names.into_iter().map(str::to_string).collect::<Vec<_>>(),
            )
            .is_err());
        }
    }

    struct Unavailable;
    impl DeviceManager for Unavailable {
        fn discover(&self) -> Result<(), FlameError> {
            Ok(())
        }
        fn capacity(&self) -> ResourceRequirement {
            ResourceRequirement::default()
        }
        fn allocate(&self, _executor: &Executor) -> Result<DeviceAssignment, FlameError> {
            Err(FlameError::InvalidState("unavailable".into()))
        }
        fn release(&self, _executor: &Executor) -> Result<(), FlameError> {
            Ok(())
        }
    }

    #[test]
    fn partial_failure_retains_claim_for_retry_and_cleanup_without_local_assignments() {
        let directory = tempfile::tempdir().unwrap();
        let gpu = Arc::new(gpu(directory.path()));
        let managers = DeviceManagers {
            managers: vec![
                (GPU.into(), gpu.clone()),
                ("other".into(), Arc::new(Unavailable)),
            ],
        };
        let executor = request();
        assert!(managers.allocate(&executor).is_err());
        let retained = gpu.allocate(&executor).unwrap();
        assert!(managers.allocate(&executor).is_err());
        assert_eq!(gpu.allocate(&executor).unwrap(), retained);
        assert!(gpu.allocate(&request()).is_err());
        assert!(executor.assignments.is_empty());
        managers.release(&executor).unwrap();
        assert!(gpu.allocate(&request()).is_ok());
    }

    struct Recorder {
        gpu: gpu::GpuManager,
        releases: Arc<AtomicUsize>,
        failure: Arc<AtomicBool>,
    }
    impl DeviceManager for Recorder {
        fn discover(&self) -> Result<(), FlameError> {
            self.gpu.discover()
        }
        fn capacity(&self) -> ResourceRequirement {
            self.gpu.capacity()
        }
        fn allocate(&self, executor: &Executor) -> Result<DeviceAssignment, FlameError> {
            self.gpu.allocate(executor)
        }
        fn release(&self, executor: &Executor) -> Result<(), FlameError> {
            self.releases.fetch_add(1, Ordering::SeqCst);
            if self.failure.load(Ordering::SeqCst) {
                Err(FlameError::Internal("persist failed".into()))
            } else {
                self.gpu.release(executor)
            }
        }
    }

    #[test]
    fn release_dispatches_by_executor_and_propagates_failure_without_freeing_claim() {
        let directory = tempfile::tempdir().unwrap();
        let releases = Arc::new(AtomicUsize::new(0));
        let failure = Arc::new(AtomicBool::new(true));
        let managers = DeviceManagers {
            managers: vec![(
                GPU.into(),
                Arc::new(Recorder {
                    gpu: gpu(directory.path()),
                    releases: releases.clone(),
                    failure: failure.clone(),
                }),
            )],
        };
        let executor = request();
        let original = managers.allocate(&executor).unwrap();
        assert!(managers.release(&executor).is_err());
        assert_eq!(managers.allocate(&executor).unwrap(), original);
        failure.store(false, Ordering::SeqCst);
        managers.release(&executor).unwrap();
        managers.release(&executor).unwrap();
        assert_eq!(releases.load(Ordering::SeqCst), 3);
        assert!(managers.allocate(&request()).is_ok());
    }
}
