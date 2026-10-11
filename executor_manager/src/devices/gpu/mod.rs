/*
Copyright 2023 The Flame Authors.
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

//! Physical GPU reservations retained across an executor's session bindings.

mod discovery;
mod storage;

use discovery::discover_gpu_devices;

use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::Mutex;

use common::apis::ResourceRequirement;
use common::FlameError;

use crate::devices::{DeviceAssignment, DeviceManager};
use crate::executor::Executor;

#[derive(Debug, Default)]
struct GpuState {
    inventory: Vec<String>,
    assignments: HashMap<String, DeviceAssignment>,
}

#[derive(Debug)]
pub struct GpuManager {
    state: Mutex<GpuState>,
    workdir: PathBuf,
}

impl Default for GpuManager {
    fn default() -> Self {
        Self::new()
    }
}

impl GpuManager {
    pub fn new() -> Self {
        Self {
            state: Mutex::new(GpuState::default()),
            workdir: PathBuf::from("."),
        }
    }

    pub fn with_workdir(mut self, workdir: impl Into<PathBuf>) -> Self {
        self.workdir = workdir.into();
        self
    }

    pub fn with_inventory(self, inventory: Vec<String>) -> Result<Self, FlameError> {
        {
            let mut state = self
                .state
                .lock()
                .map_err(|error| FlameError::Internal(error.to_string()))?;
            Self::validate_inventory(&inventory, &state.assignments)?;
            state.inventory = inventory;
        }
        Ok(self)
    }

    fn validate_inventory(
        inventory: &[String],
        assignments: &HashMap<String, DeviceAssignment>,
    ) -> Result<(), FlameError> {
        let unique: HashSet<_> = inventory.iter().collect();
        if unique.len() != inventory.len() {
            return Err(FlameError::InvalidConfig(
                "GPU inventory contains duplicate identifiers".into(),
            ));
        }
        if assignments
            .values()
            .flat_map(|assignment| assignment.devices())
            .any(|device| !unique.contains(device))
        {
            return Err(FlameError::InvalidState(
                "refreshed GPU inventory is missing an allocated device".into(),
            ));
        }
        Ok(())
    }

    fn refresh_inventory(&self, inventory: Vec<String>) -> Result<(), FlameError> {
        let mut state = self
            .state
            .lock()
            .map_err(|error| FlameError::Internal(error.to_string()))?;
        Self::validate_inventory(&inventory, &state.assignments)?;
        // Read and validate persistence under the allocation lock, so concurrent
        // allocate/release cannot make the ledger differ from active ownership.
        let restored = storage::load(&self.workdir)?;
        let available: HashSet<_> = inventory.iter().collect();
        let mut occupied = HashSet::new();
        for (executor_id, assignment) in &restored {
            if executor_id.is_empty() {
                return Err(FlameError::InvalidConfig(
                    "invalid persisted GPU allocation identity".into(),
                ));
            }
            for device in assignment.devices() {
                if !available.contains(device) || !occupied.insert(device) {
                    return Err(FlameError::InvalidConfig(
                        "persisted GPU allocations contain unavailable or duplicate devices".into(),
                    ));
                }
            }
            let expected_envs = HashMap::from([
                (
                    "CUDA_VISIBLE_DEVICES".to_string(),
                    assignment.devices().join(","),
                ),
                ("NVIDIA_VISIBLE_DEVICES".to_string(), "void".to_string()),
            ]);
            let expected_cdi = assignment
                .devices()
                .iter()
                .map(|device| format!("nvidia.com/gpu={device}"))
                .collect::<Vec<_>>();
            if assignment.envs != expected_envs || assignment.cdi != expected_cdi {
                return Err(FlameError::InvalidConfig(
                    "persisted GPU assignment environment or CDI devices do not match inventory"
                        .into(),
                ));
            }
        }
        if !state.assignments.is_empty() && state.assignments != restored {
            return Err(FlameError::InvalidState(
                "persisted GPU ledger differs from active allocations".into(),
            ));
        }
        // Publish inventory and ownership only after every validation succeeds.
        *state = GpuState {
            inventory,
            assignments: restored,
        };
        Ok(())
    }
}

impl DeviceManager for GpuManager {
    fn discover(&self) -> Result<(), FlameError> {
        // Hardware discovery may be slow; it does not hold the allocation lock.
        self.refresh_inventory(discover_gpu_devices()?)
    }

    fn capacity(&self) -> ResourceRequirement {
        ResourceRequirement {
            gpu: self
                .state
                .lock()
                .expect("GPU state lock poisoned")
                .inventory
                .len() as i32,
            ..Default::default()
        }
    }

    fn allocate(&self, executor: &Executor) -> Result<DeviceAssignment, FlameError> {
        storage::validate_executor_id(&executor.id)?;
        let mut state = self.state.lock().map_err(|error| {
            FlameError::Internal(format!("failed to lock GPU reservations: {error}"))
        })?;
        if let Some(assignment) = state.assignments.get(&executor.id) {
            return Ok(assignment.clone());
        }
        let count = usize::try_from(executor.resreq.gpu).map_err(|_| {
            FlameError::InvalidConfig("executor GPU requirement must not be negative".to_string())
        })?;
        let occupied: HashSet<_> = state
            .assignments
            .values()
            .flat_map(|assignment| assignment.devices())
            .collect();
        let devices: Vec<String> = state
            .inventory
            .iter()
            .filter(|device| !occupied.contains(*device))
            .take(count)
            .cloned()
            .collect();
        if devices.len() != count {
            return Err(FlameError::InvalidState(format!(
                "executor requests {count} GPUs, but only {} are available",
                devices.len()
            )));
        }
        let visibility = devices.join(",");
        let cdi = devices
            .iter()
            .map(|device| format!("nvidia.com/gpu={device}"))
            .collect();
        let mut assignment = DeviceAssignment::new(devices);
        assignment
            .envs
            .insert("CUDA_VISIBLE_DEVICES".to_string(), visibility);
        assignment
            .envs
            .insert("NVIDIA_VISIBLE_DEVICES".into(), "void".into());
        assignment.cdi = cdi;
        storage::save(&self.workdir, &executor.id, &assignment)?;
        state
            .assignments
            .insert(executor.id.clone(), assignment.clone());
        Ok(assignment)
    }

    fn release(&self, executor: &Executor) -> Result<(), FlameError> {
        let mut state = self.state.lock().map_err(|error| {
            FlameError::Internal(format!("failed to lock GPU reservations: {error}"))
        })?;
        storage::validate_executor_id(&executor.id)?;
        if state.assignments.contains_key(&executor.id) {
            storage::remove(&self.workdir, &executor.id)?;
            state.assignments.remove(&executor.id);
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use common::apis::ExecutorState;
    use std::path::Path;
    use tempfile::TempDir;

    fn manager() -> (TempDir, GpuManager) {
        let directory = tempfile::tempdir().unwrap();
        let manager = GpuManager::new()
            .with_workdir(directory.path())
            .with_inventory(vec!["GPU-a".into(), "GPU-b".into()])
            .unwrap();
        (directory, manager)
    }
    fn executor(gpu: i32) -> Executor {
        crate::devices::test_executor(ResourceRequirement {
            gpu,
            ..Default::default()
        })
    }
    fn path(manager: &GpuManager, executor: &Executor) -> PathBuf {
        storage::assignment_path(&manager.workdir, &executor.id).unwrap()
    }
    fn restored(workdir: &Path) -> GpuManager {
        let manager = GpuManager::new()
            .with_workdir(workdir)
            .with_inventory(vec!["GPU-a".into(), "GPU-b".into()])
            .unwrap();
        manager
            .refresh_inventory(vec!["GPU-a".into(), "GPU-b".into()])
            .unwrap();
        manager
    }

    #[test]
    fn builders_do_not_create_storage_or_change_default_workdir() {
        let default = GpuManager::new();
        assert_eq!(default.workdir, Path::new("."));
        let (directory, _manager) = manager();
        assert!(!directory.path().join("gpu").exists());
    }

    #[test]
    fn executor_owned_allocations_are_idempotent_disjoint_and_explicitly_released() {
        let (_directory, manager) = manager();
        let first = executor(1);
        let second = executor(1);
        let assignment = manager.allocate(&first).unwrap();
        assert_eq!(manager.allocate(&first).unwrap(), assignment);
        assert_eq!(assignment.devices(), ["GPU-a"]);
        assert_eq!(assignment.envs["CUDA_VISIBLE_DEVICES"], "GPU-a");
        assert_eq!(assignment.envs["NVIDIA_VISIBLE_DEVICES"], "void");
        assert_eq!(assignment.cdi, ["nvidia.com/gpu=GPU-a"]);
        assert_eq!(manager.allocate(&second).unwrap().devices(), ["GPU-b"]);
        assert!(manager.allocate(&executor(1)).is_err());
        drop(assignment);
        assert!(manager.allocate(&executor(1)).is_err());
        manager.release(&first).unwrap();
        manager.release(&first).unwrap();
        assert_eq!(manager.allocate(&executor(1)).unwrap().devices(), ["GPU-a"]);
    }

    #[test]
    fn invalid_requests_and_executor_paths_never_consume_capacity() {
        let (directory, manager) = manager();
        for id in [
            "",
            ".",
            "..",
            "../outside",
            "a/b",
            "a\\b",
            "/absolute",
            ".hidden",
            "a\0b",
        ] {
            let mut request = executor(1);
            request.id = id.into();
            assert!(manager.allocate(&request).is_err(), "{id:?}");
            assert!(manager.release(&request).is_err());
        }
        assert!(!directory.path().join("gpu").exists());
        assert!(manager.allocate(&executor(-1)).is_err());
        assert!(manager.allocate(&executor(3)).is_err());
        assert!(manager.allocate(&executor(2)).is_ok());
    }

    #[test]
    fn cpu_assignments_hide_gpus_without_consuming_capacity() {
        let (_directory, manager) = manager();
        let request = executor(0);
        let assignment = manager.allocate(&request).unwrap();
        assert!(assignment.devices().is_empty());
        assert_eq!(assignment.envs["CUDA_VISIBLE_DEVICES"], "");
        assert_eq!(manager.allocate(&request).unwrap(), assignment);
        assert!(manager.allocate(&executor(2)).is_ok());
    }

    #[test]
    fn inventory_refresh_preserves_claims_and_rejects_missing_or_duplicate_devices() {
        let (directory, _) = manager();
        let manager = GpuManager::new()
            .with_workdir(directory.path())
            .with_inventory(vec!["GPU-b".into()])
            .unwrap();
        let request = executor(1);
        let original = manager.allocate(&request).unwrap();
        manager
            .refresh_inventory(vec!["GPU-a".into(), "GPU-b".into()])
            .unwrap();
        assert_eq!(manager.allocate(&request).unwrap(), original);
        let added = manager.allocate(&executor(1)).unwrap();
        assert_eq!(added.cdi, ["nvidia.com/gpu=GPU-a"]);
        assert_eq!(added.envs["CUDA_VISIBLE_DEVICES"], "GPU-a");
        for invalid in [
            vec!["GPU-a".into()],
            vec!["GPU-a".into(), "GPU-a".into()],
            vec!["0".into()],
        ] {
            assert!(manager.refresh_inventory(invalid).is_err());
            assert_eq!(manager.capacity().gpu, 2);
        }
    }

    #[test]
    fn persisted_ledger_conflict_rolls_back_inventory_and_active_claims() {
        let (_directory, manager) = manager();
        let request = executor(1);
        let assignment = manager.allocate(&request).unwrap();
        std::fs::remove_file(path(&manager, &request)).unwrap();
        assert!(manager
            .refresh_inventory(vec!["GPU-a".into(), "GPU-b".into(), "GPU-c".into()])
            .is_err());
        assert_eq!(manager.capacity().gpu, 2);
        assert_eq!(manager.allocate(&request).unwrap(), assignment);
        assert!(manager.allocate(&executor(2)).is_err());
        storage::save(&manager.workdir, &request.id, &assignment).unwrap();
        manager
            .refresh_inventory(vec!["GPU-a".into(), "GPU-b".into(), "GPU-c".into()])
            .unwrap();
        assert_eq!(manager.capacity().gpu, 3);
        assert_eq!(manager.allocate(&request).unwrap(), assignment);
        assert_eq!(
            manager.allocate(&executor(2)).unwrap().devices(),
            ["GPU-b", "GPU-c"]
        );
    }

    #[test]
    fn concurrent_refresh_allocation_and_release_preserve_persisted_ownership() {
        let (_directory, manager) = manager();
        let manager = std::sync::Arc::new(manager);
        let barrier = std::sync::Arc::new(std::sync::Barrier::new(2));
        let refresher = {
            let manager = manager.clone();
            let barrier = barrier.clone();
            std::thread::spawn(move || {
                barrier.wait();
                for _ in 0..64 {
                    manager
                        .refresh_inventory(vec!["GPU-a".into(), "GPU-b".into()])
                        .unwrap();
                }
            })
        };
        barrier.wait();
        for _ in 0..64 {
            let request = executor(1);
            let assignment = manager.allocate(&request).unwrap();
            assert_eq!(manager.allocate(&request).unwrap(), assignment);
            manager.release(&request).unwrap();
        }
        refresher.join().unwrap();
        assert!(manager.allocate(&executor(2)).is_ok());
    }

    #[test]
    fn assignment_files_have_plain_shape_and_restart_restores_exact_ownership() {
        let (directory, manager) = manager();
        let first = executor(1);
        let second = executor(1);
        let assignment = manager.allocate(&first).unwrap();
        let second_assignment = manager.allocate(&second).unwrap();
        let second_bytes = std::fs::read(path(&manager, &second)).unwrap();
        let json: serde_json::Value =
            serde_json::from_slice(&std::fs::read(path(&manager, &first)).unwrap()).unwrap();
        let fields = json.as_object().unwrap();
        assert_eq!(fields.len(), 3);
        assert!(
            fields.contains_key("devices")
                && fields.contains_key("envs")
                && fields.contains_key("cdi")
        );
        std::fs::write(directory.path().join("unrelated.txt"), "keep").unwrap();
        drop(manager);
        let manager = restored(directory.path());
        assert_eq!(manager.allocate(&first).unwrap(), assignment);
        assert_eq!(manager.allocate(&second).unwrap(), second_assignment);
        assert!(manager.allocate(&executor(1)).is_err());
        manager.release(&first).unwrap();
        assert!(!path(&manager, &first).exists());
        assert_eq!(
            std::fs::read(path(&manager, &second)).unwrap(),
            second_bytes
        );
        assert_eq!(
            std::fs::read_to_string(directory.path().join("unrelated.txt")).unwrap(),
            "keep"
        );
        drop(manager);
        let manager = restored(directory.path());
        assert!(!manager
            .state
            .lock()
            .unwrap()
            .assignments
            .contains_key(&first.id));
        assert_eq!(manager.allocate(&first).unwrap().devices(), ["GPU-a"]);
    }

    #[test]
    fn release_retry_handles_already_missing_file() {
        let (_directory, manager) = manager();
        let request = executor(1);
        manager.allocate(&request).unwrap();
        std::fs::remove_file(path(&manager, &request)).unwrap();
        manager.release(&request).unwrap();
        assert!(manager.state.lock().unwrap().assignments.is_empty());
    }

    #[test]
    fn existing_executor_identity_reuses_assignment_regardless_of_request_or_state() {
        let (_directory, manager) = manager();
        let mut request = executor(1);
        let original = manager.allocate(&request).unwrap();
        for gpu in [-1, 0, 2, 3] {
            request.resreq.gpu = gpu;
            for state in [
                ExecutorState::Idle,
                ExecutorState::Bound,
                ExecutorState::Unbinding,
                ExecutorState::Releasing,
                ExecutorState::Released,
            ] {
                request.state = state;
                assert_eq!(manager.allocate(&request).unwrap(), original);
            }
        }
        manager.release(&request).unwrap();
    }

    #[test]
    fn new_executor_allocations_follow_the_same_contract_in_every_state() {
        let (_directory, manager) = manager();
        for state in [
            ExecutorState::Idle,
            ExecutorState::Bound,
            ExecutorState::Unbinding,
            ExecutorState::Releasing,
            ExecutorState::Released,
        ] {
            let mut request = executor(1);
            request.state = state;
            assert_eq!(manager.allocate(&request).unwrap().devices(), ["GPU-a"]);
            assert!(path(&manager, &request).is_file());
            manager.release(&request).unwrap();
            assert!(!path(&manager, &request).exists());
        }
    }

    #[test]
    fn allocation_io_failure_keeps_capacity_free() {
        let (directory, mut manager) = manager();
        let blocker = directory.path().join("blocker");
        std::fs::write(&blocker, "file").unwrap();
        manager.workdir = blocker;
        assert!(manager.allocate(&executor(1)).is_err());
        assert!(manager.state.lock().unwrap().assignments.is_empty());
    }

    #[test]
    fn removal_failure_keeps_memory_and_unrelated_assignment_unchanged() {
        let (_directory, manager) = manager();
        let first = executor(1);
        let second = executor(1);
        let original = manager.allocate(&first).unwrap();
        manager.allocate(&second).unwrap();
        let second_bytes = std::fs::read(path(&manager, &second)).unwrap();
        let first_path = path(&manager, &first);
        std::fs::remove_file(&first_path).unwrap();
        std::fs::create_dir(&first_path).unwrap();
        assert!(manager.release(&first).is_err());
        assert_eq!(manager.allocate(&first).unwrap(), original);
        assert_eq!(
            std::fs::read(path(&manager, &second)).unwrap(),
            second_bytes
        );
        std::fs::remove_dir(&first_path).unwrap();
        manager.release(&first).unwrap();
    }

    #[test]
    fn malformed_files_environment_mismatch_and_device_collisions_fail_closed() {
        let (_directory, manager) = manager();
        let first = executor(1);
        let second = executor(1);
        let original = manager.allocate(&first).unwrap();
        manager.allocate(&second).unwrap();
        let first_path = path(&manager, &first);
        let original_bytes = std::fs::read(&first_path).unwrap();
        std::fs::write(&first_path, b"bad JSON").unwrap();
        assert!(manager
            .refresh_inventory(vec!["GPU-a".into(), "GPU-b".into()])
            .is_err());
        assert_eq!(manager.allocate(&first).unwrap(), original);
        std::fs::write(&first_path, &original_bytes).unwrap();
        let mut json: serde_json::Value = serde_json::from_slice(&original_bytes).unwrap();
        json["envs"]["CUDA_VISIBLE_DEVICES"] = serde_json::json!("wrong");
        std::fs::write(&first_path, serde_json::to_vec(&json).unwrap()).unwrap();
        assert!(manager
            .refresh_inventory(vec!["GPU-a".into(), "GPU-b".into()])
            .is_err());
        std::fs::write(&first_path, &original_bytes).unwrap();
        std::fs::write(path(&manager, &second), &original_bytes).unwrap();
        assert!(manager
            .refresh_inventory(vec!["GPU-a".into(), "GPU-b".into()])
            .is_err());
    }

    #[test]
    fn recovery_skips_only_atomic_temporary_files_and_rejects_unsafe_filenames() {
        let (directory, manager) = manager();
        let request = executor(1);
        manager.allocate(&request).unwrap();
        let gpu_dir = directory.path().join("gpu");
        let temporary = gpu_dir.join(format!(".gpu-assignment-{}.tmp", uuid::Uuid::new_v4()));
        std::fs::write(&temporary, b"unfinished").unwrap();
        manager
            .refresh_inventory(vec!["GPU-a".into(), "GPU-b".into()])
            .unwrap();
        for name in [
            ".hidden.json",
            "unrelated.txt",
            ".gpu-assignment-invalid.tmp",
        ] {
            let unexpected = gpu_dir.join(name);
            std::fs::write(&unexpected, b"bad JSON").unwrap();
            assert!(manager
                .refresh_inventory(vec!["GPU-a".into(), "GPU-b".into()])
                .is_err());
            std::fs::remove_file(unexpected).unwrap();
        }
        assert!(temporary.exists());
    }

    #[test]
    fn restricted_inventory_order_and_duplicate_checks_are_preserved() {
        let (directory, _) = manager();
        let manager = GpuManager::new()
            .with_workdir(directory.path())
            .with_inventory(vec!["3".into(), "1".into()])
            .unwrap();
        assert_eq!(
            manager.allocate(&executor(2)).unwrap().devices(),
            ["3", "1"]
        );
        assert!(GpuManager::new()
            .with_inventory(vec!["3".into(), "3".into()])
            .is_err());
    }
}
