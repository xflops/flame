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

use std::collections::{BTreeMap, HashMap};

use chrono::{DateTime, Utc};
use common::FlameError;

use crate::cri_v1::{
    ContainerConfig as CriContainerConfig, ContainerMetadata, ContainerState,
    ContainerStatus as CriContainerStatus, ImageSpec, Int64Value, KeyValue, LinuxContainerConfig,
    LinuxContainerResources, LinuxContainerSecurityContext, LinuxPodSandboxConfig,
    LinuxSandboxSecurityContext, Mount as CriMount, MountPropagation, PodSandboxConfig,
    PodSandboxMetadata, PodSandboxState, PodSandboxStatus as CriPodSandboxStatus, Signal,
};

pub const LABEL_MANAGED_BY: &str = "io.xflops.flame.managed-by";
pub const LABEL_EXECUTOR_ID: &str = "io.xflops.flame.executor-id";
pub const LABEL_APPLICATION: &str = "io.xflops.flame.application";
pub const LABEL_WORKLOAD_UID: &str = "io.xflops.flame.workload-uid";
pub const MANAGED_BY_EXECUTOR_MANAGER: &str = "executor-manager";

pub const ANNOTATION_APPLICATION: &str = "io.xflops.flame.application";

const CPU_PERIOD_US: i64 = 100_000;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorkloadFilter {
    pub executor_id: String,
}

impl WorkloadFilter {
    pub fn new(executor_id: impl Into<String>) -> Result<Self, FlameError> {
        let filter = Self {
            executor_id: executor_id.into(),
        };
        if filter.executor_id.is_empty() {
            return Err(FlameError::InvalidConfig(
                "CRI workload filter executor ID must not be empty".to_string(),
            ));
        }
        Ok(filter)
    }

    pub(crate) fn labels(&self) -> HashMap<String, String> {
        HashMap::from([
            (
                LABEL_MANAGED_BY.to_string(),
                MANAGED_BY_EXECUTOR_MANAGER.to_string(),
            ),
            (LABEL_EXECUTOR_ID.to_string(), self.executor_id.clone()),
        ])
    }

    pub(crate) fn matches(&self, labels: &HashMap<String, String>) -> bool {
        self.labels()
            .iter()
            .all(|(key, value)| labels.get(key) == Some(value))
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorkloadMetadata {
    pub name: String,
    pub namespace: String,
    pub uid: String,
    pub executor_id: String,
    pub application: String,
}

impl WorkloadMetadata {
    pub(crate) fn labels(&self) -> HashMap<String, String> {
        let mut labels = WorkloadFilter {
            executor_id: self.executor_id.clone(),
        }
        .labels();
        labels.insert(LABEL_APPLICATION.to_string(), self.application.clone());
        labels.insert(LABEL_WORKLOAD_UID.to_string(), self.uid.clone());
        labels
    }

    pub(crate) fn filter(&self) -> WorkloadFilter {
        WorkloadFilter {
            executor_id: self.executor_id.clone(),
        }
    }

    pub(crate) fn annotations(&self) -> HashMap<String, String> {
        HashMap::from([(ANNOTATION_APPLICATION.to_string(), self.application.clone())])
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ResourceLimits {
    /// Whole logical CPUs. Zero means no CRI CPU limit.
    pub cpu: u64,
    /// Bytes. Zero means no CRI memory limit.
    pub memory: u64,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ContainerSecurityContext {
    pub run_as_user: Option<i64>,
    pub run_as_group: Option<i64>,
    pub supplemental_groups: Vec<i64>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Mount {
    pub host_path: String,
    pub container_path: String,
    pub readonly: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ContainerSpec {
    pub name: String,
    pub image: String,
    pub command: Option<String>,
    pub args: Vec<String>,
    pub env: BTreeMap<String, String>,
    pub working_directory: String,
    pub mounts: Vec<Mount>,
    pub resources: ResourceLimits,
    pub security_context: ContainerSecurityContext,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorkloadSpec {
    pub metadata: WorkloadMetadata,
    pub containers: Vec<ContainerSpec>,
    pub log_directory: String,
}

impl WorkloadSpec {
    pub fn validate(&self) -> Result<(), FlameError> {
        let metadata = &self.metadata;
        if metadata.name.is_empty()
            || metadata.namespace.is_empty()
            || metadata.uid.is_empty()
            || metadata.executor_id.is_empty()
        {
            return Err(FlameError::InvalidConfig(
                "CRI workload metadata contains an empty required field".to_string(),
            ));
        }
        if self.containers.is_empty() {
            return Err(FlameError::InvalidConfig(
                "CRI workload must contain at least one container".to_string(),
            ));
        }
        if self.log_directory.is_empty() {
            return Err(FlameError::InvalidConfig(
                "CRI workload log directory must not be empty".to_string(),
            ));
        }
        for container in &self.containers {
            if container.name.is_empty() || container.image.is_empty() {
                return Err(FlameError::InvalidConfig(
                    "CRI container name and image must not be empty".to_string(),
                ));
            }
            if container.security_context.run_as_group.is_some()
                && container.security_context.run_as_user.is_none()
            {
                return Err(FlameError::InvalidConfig(
                    "CRI run_as_group requires run_as_user".to_string(),
                ));
            }
            linux_resources(&container.resources)?;
        }
        Ok(())
    }

    pub(crate) fn sandbox_config(&self) -> PodSandboxConfig {
        PodSandboxConfig {
            metadata: Some(PodSandboxMetadata {
                name: self.metadata.name.clone(),
                uid: self.metadata.uid.clone(),
                namespace: self.metadata.namespace.clone(),
                attempt: 0,
            }),
            hostname: self.metadata.name.clone(),
            log_directory: self.log_directory.clone(),
            labels: self.metadata.labels(),
            annotations: self.metadata.annotations(),
            linux: Some(LinuxPodSandboxConfig {
                security_context: Some(LinuxSandboxSecurityContext {
                    privileged: false,
                    ..Default::default()
                }),
                ..Default::default()
            }),
            ..Default::default()
        }
    }

    pub(crate) fn container_config(
        &self,
        container: &ContainerSpec,
    ) -> Result<CriContainerConfig, FlameError> {
        let command = container.command.clone().into_iter().collect();
        let envs = container
            .env
            .iter()
            .map(|(key, value)| KeyValue {
                key: key.clone(),
                value: value.clone(),
            })
            .collect();
        let mounts = container
            .mounts
            .iter()
            .map(|mount| CriMount {
                container_path: mount.container_path.clone(),
                host_path: mount.host_path.clone(),
                readonly: mount.readonly,
                propagation: MountPropagation::PropagationPrivate.into(),
                ..Default::default()
            })
            .collect();
        let security_context = &container.security_context;

        Ok(CriContainerConfig {
            metadata: Some(ContainerMetadata {
                name: container.name.clone(),
                attempt: 0,
            }),
            image: Some(image_spec(&container.image)),
            command,
            args: container.args.clone(),
            envs,
            working_dir: container.working_directory.clone(),
            linux: Some(LinuxContainerConfig {
                resources: Some(linux_resources(&container.resources)?),
                security_context: Some(LinuxContainerSecurityContext {
                    privileged: false,
                    run_as_user: security_context
                        .run_as_user
                        .map(|value| Int64Value { value }),
                    run_as_group: security_context
                        .run_as_group
                        .map(|value| Int64Value { value }),
                    supplemental_groups: security_context.supplemental_groups.clone(),
                    no_new_privs: true,
                    ..Default::default()
                }),
            }),
            labels: self.metadata.labels(),
            annotations: self.metadata.annotations(),
            log_path: format!("{}.log", container.name),
            mounts,
            stop_signal: Signal::Sigterm.into(),
            ..Default::default()
        })
    }
}

pub(crate) fn image_spec(image: &str) -> ImageSpec {
    ImageSpec {
        image: image.to_string(),
        user_specified_image: image.to_string(),
        runtime_handler: String::new(),
        ..Default::default()
    }
}

fn linux_resources(resources: &ResourceLimits) -> Result<LinuxContainerResources, FlameError> {
    let cpu_quota = if resources.cpu == 0 {
        0
    } else {
        let cpu = i64::try_from(resources.cpu).map_err(|_| {
            FlameError::InvalidConfig("CRI CPU limit exceeds i64 range".to_string())
        })?;
        cpu.checked_mul(CPU_PERIOD_US).ok_or_else(|| {
            FlameError::InvalidConfig("CRI CPU quota exceeds i64 range".to_string())
        })?
    };
    let memory_limit_in_bytes = i64::try_from(resources.memory)
        .map_err(|_| FlameError::InvalidConfig("CRI memory limit exceeds i64 range".to_string()))?;

    Ok(LinuxContainerResources {
        cpu_period: if resources.cpu == 0 { 0 } else { CPU_PERIOD_US },
        cpu_quota,
        memory_limit_in_bytes,
        ..Default::default()
    })
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorkloadHandle {
    pub(crate) sandbox_id: String,
    pub(crate) container_ids: Vec<String>,
    pub(crate) filter: WorkloadFilter,
}

impl WorkloadHandle {
    pub fn sandbox_id(&self) -> &str {
        &self.sandbox_id
    }

    pub fn container_ids(&self) -> &[String] {
        &self.container_ids
    }

    pub fn filter(&self) -> &WorkloadFilter {
        &self.filter
    }

    pub(crate) fn new(
        sandbox_id: String,
        container_ids: Vec<String>,
        filter: WorkloadFilter,
    ) -> Self {
        Self {
            sandbox_id,
            container_ids,
            filter,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SandboxState {
    Ready,
    NotReady,
}

impl TryFrom<i32> for SandboxState {
    type Error = FlameError;

    fn try_from(value: i32) -> Result<Self, Self::Error> {
        match PodSandboxState::try_from(value).map_err(FlameError::from)? {
            PodSandboxState::SandboxReady => Ok(Self::Ready),
            PodSandboxState::SandboxNotready => Ok(Self::NotReady),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ContainerRuntimeState {
    Created,
    Running,
    Exited,
    Unknown,
}

impl TryFrom<i32> for ContainerRuntimeState {
    type Error = FlameError;

    fn try_from(value: i32) -> Result<Self, Self::Error> {
        match ContainerState::try_from(value).map_err(FlameError::from)? {
            ContainerState::ContainerCreated => Ok(Self::Created),
            ContainerState::ContainerRunning => Ok(Self::Running),
            ContainerState::ContainerExited => Ok(Self::Exited),
            ContainerState::ContainerUnknown => Ok(Self::Unknown),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ContainerStatus {
    pub id: String,
    pub sandbox_id: String,
    pub name: String,
    pub image: String,
    pub image_ref: String,
    pub state: ContainerRuntimeState,
    pub created_at: DateTime<Utc>,
    pub started_at: Option<DateTime<Utc>>,
    pub finished_at: Option<DateTime<Utc>>,
    pub exit_code: i32,
    pub reason: String,
    pub message: String,
}

impl ContainerStatus {
    pub(crate) fn try_from_cri(
        sandbox_id: &str,
        status: CriContainerStatus,
    ) -> Result<Self, FlameError> {
        let metadata = status.metadata.ok_or_else(|| {
            FlameError::InvalidState("CRI container status is missing metadata".to_string())
        })?;
        let image = status.image.ok_or_else(|| {
            FlameError::InvalidState("CRI container status is missing image".to_string())
        })?;

        Ok(Self {
            id: required_string(status.id, "container ID")?,
            sandbox_id: sandbox_id.to_string(),
            name: required_string(metadata.name, "container name")?,
            image: required_string(image.image, "container image")?,
            image_ref: status.image_ref,
            state: ContainerRuntimeState::try_from(status.state)?,
            created_at: required_timestamp(status.created_at, "container created_at")?,
            started_at: optional_timestamp(status.started_at, "container started_at")?,
            finished_at: optional_timestamp(status.finished_at, "container finished_at")?,
            exit_code: status.exit_code,
            reason: status.reason,
            message: status.message,
        })
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorkloadStatus {
    pub sandbox_id: String,
    pub state: SandboxState,
    pub created_at: DateTime<Utc>,
    pub containers: Vec<ContainerStatus>,
}

impl WorkloadStatus {
    pub(crate) fn from_sandbox(
        sandbox: &CriPodSandboxStatus,
        containers: Vec<ContainerStatus>,
    ) -> Result<Self, FlameError> {
        Ok(Self {
            sandbox_id: required_string(sandbox.id.clone(), "sandbox ID")?,
            state: SandboxState::try_from(sandbox.state)?,
            created_at: required_timestamp(sandbox.created_at, "sandbox created_at")?,
            containers,
        })
    }

    pub fn healthy(&self) -> bool {
        self.state == SandboxState::Ready
            && !self.containers.is_empty()
            && self
                .containers
                .iter()
                .all(|container| container.state == ContainerRuntimeState::Running)
    }
}

fn required_string(value: String, field: &str) -> Result<String, FlameError> {
    if value.is_empty() {
        Err(FlameError::InvalidState(format!(
            "CRI response is missing {field}"
        )))
    } else {
        Ok(value)
    }
}

fn required_timestamp(value: i64, field: &str) -> Result<DateTime<Utc>, FlameError> {
    if value <= 0 {
        return Err(FlameError::InvalidState(format!(
            "CRI {field} must be positive"
        )));
    }
    timestamp(value, field)
}

fn optional_timestamp(value: i64, field: &str) -> Result<Option<DateTime<Utc>>, FlameError> {
    if value == 0 {
        return Ok(None);
    }
    if value < 0 {
        return Err(FlameError::InvalidState(format!(
            "CRI {field} must not be negative"
        )));
    }
    timestamp(value, field).map(Some)
}

fn timestamp(value: i64, field: &str) -> Result<DateTime<Utc>, FlameError> {
    let seconds = value.div_euclid(1_000_000_000);
    let nanos = value.rem_euclid(1_000_000_000) as u32;
    DateTime::from_timestamp(seconds, nanos).ok_or_else(|| {
        FlameError::InvalidState(format!("CRI {field} is outside the supported range"))
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_spec() -> WorkloadSpec {
        WorkloadSpec {
            metadata: WorkloadMetadata {
                name: "flame-exec".to_string(),
                namespace: "flame".to_string(),
                uid: "uid".to_string(),
                executor_id: "executor".to_string(),
                application: "application".to_string(),
            },
            containers: vec![ContainerSpec {
                name: "application".to_string(),
                image: "example/image:latest".to_string(),
                command: Some("run".to_string()),
                args: vec!["--flag".to_string()],
                env: BTreeMap::from([
                    ("Z_KEY".to_string(), "z".to_string()),
                    ("A_KEY".to_string(), "a".to_string()),
                ]),
                working_directory: "/work".to_string(),
                mounts: vec![Mount {
                    host_path: "/host/work".to_string(),
                    container_path: "/work".to_string(),
                    readonly: false,
                }],
                resources: ResourceLimits {
                    cpu: 2,
                    memory: 1024,
                },
                security_context: ContainerSecurityContext::default(),
            }],
            log_directory: "/var/log/flame/executors/executor".to_string(),
        }
    }

    #[test]
    fn request_construction_is_deterministic_and_scoped() {
        let spec = test_spec();
        spec.validate().unwrap();
        let sandbox = spec.sandbox_config();
        let container = spec.container_config(&spec.containers[0]).unwrap();

        assert_eq!(
            sandbox.labels.get(LABEL_EXECUTOR_ID),
            Some(&"executor".to_string())
        );
        assert_eq!(
            sandbox.labels.get(LABEL_WORKLOAD_UID),
            Some(&"uid".to_string())
        );
        assert_eq!(sandbox.metadata.as_ref().unwrap().attempt, 0);
        assert!(!sandbox.labels.contains_key("io.xflops.flame.session-id"));
        assert_eq!(container.metadata.as_ref().unwrap().attempt, 0);
        assert_eq!(container.log_path, "application.log");
        assert_eq!(container.envs[0].key, "A_KEY");
        assert_eq!(container.envs[1].key, "Z_KEY");
        let resources = container.linux.unwrap().resources.unwrap();
        assert_eq!(resources.cpu_period, CPU_PERIOD_US);
        assert_eq!(resources.cpu_quota, 2 * CPU_PERIOD_US);
        assert_eq!(resources.memory_limit_in_bytes, 1024);
        assert!(container.labels.contains_key(LABEL_EXECUTOR_ID));
    }

    #[test]
    fn resource_overflow_is_rejected() {
        let mut spec = test_spec();
        spec.containers[0].resources.cpu = u64::MAX;
        assert!(spec.validate().is_err());
        spec.containers[0].resources.cpu = 1;
        spec.containers[0].resources.memory = u64::MAX;
        assert!(spec.validate().is_err());
    }

    #[test]
    fn optional_zero_timestamps_are_absent() {
        let status = CriContainerStatus {
            id: "container".to_string(),
            metadata: Some(ContainerMetadata {
                name: "application".to_string(),
                attempt: 0,
            }),
            state: ContainerState::ContainerRunning.into(),
            created_at: 1,
            started_at: 0,
            finished_at: 0,
            image: Some(image_spec("example/image")),
            ..Default::default()
        };

        let status = ContainerStatus::try_from_cri("sandbox", status).unwrap();
        assert_eq!(status.started_at, None);
        assert_eq!(status.finished_at, None);
        assert_eq!(status.sandbox_id, "sandbox");
    }

    #[test]
    fn workload_filter_requires_executor_id() {
        assert!(WorkloadFilter::new("").is_err());
    }
}
