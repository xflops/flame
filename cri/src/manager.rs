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

use std::time::Duration;

use async_trait::async_trait;
use common::FlameError;
#[cfg(unix)]
use hyper_util::rt::TokioIo;
#[cfg(unix)]
use tokio::net::UnixStream;
use tonic::transport::Channel;
#[cfg(unix)]
use tonic::transport::{Endpoint, Uri};
use tonic::{Code, Request, Status};
#[cfg(unix)]
use tower::service_fn;
use tracing::info;

use crate::apis::{
    ContainerStatus, LABEL_WORKLOAD_UID, WorkloadFilter, WorkloadHandle, WorkloadSpec,
    WorkloadStatus, image_spec,
};
use crate::cri_v1::image_service_client::ImageServiceClient;
use crate::cri_v1::runtime_service_client::RuntimeServiceClient;
use crate::cri_v1::{
    ContainerFilter, ContainerStatusRequest, ContainerStatusResponse, CreateContainerRequest,
    CreateContainerResponse, ImageStatusRequest, ImageStatusResponse, ListContainersRequest,
    ListContainersResponse, ListPodSandboxRequest, ListPodSandboxResponse, PodSandboxFilter,
    PodSandboxStatus, PodSandboxStatusRequest, PodSandboxStatusResponse, PullImageRequest,
    PullImageResponse, RemoveContainerRequest, RemoveContainerResponse, RemovePodSandboxRequest,
    RemovePodSandboxResponse, RunPodSandboxRequest, RunPodSandboxResponse, StartContainerRequest,
    StartContainerResponse, StatusRequest, StopContainerRequest, StopContainerResponse,
    StopPodSandboxRequest, StopPodSandboxResponse, VersionRequest,
};

pub const CONTAINERD_SOCKET: &str = "/run/containerd/containerd.sock";
const CRI_CLIENT_VERSION: &str = "0.1.0";
const CRI_API_VERSION: &str = "v1";
const RPC_TIMEOUT: Duration = Duration::from_secs(10);
const IMAGE_PULL_TIMEOUT: Duration = Duration::from_secs(120);
const STOP_TIMEOUT_SECONDS: i64 = 10;

#[async_trait]
trait RuntimeApi: Send {
    async fn run_pod_sandbox(
        &mut self,
        request: Request<RunPodSandboxRequest>,
    ) -> Result<tonic::Response<RunPodSandboxResponse>, Status>;
    async fn create_container(
        &mut self,
        request: Request<CreateContainerRequest>,
    ) -> Result<tonic::Response<CreateContainerResponse>, Status>;
    async fn start_container(
        &mut self,
        request: Request<StartContainerRequest>,
    ) -> Result<tonic::Response<StartContainerResponse>, Status>;
    async fn pod_sandbox_status(
        &mut self,
        request: Request<PodSandboxStatusRequest>,
    ) -> Result<tonic::Response<PodSandboxStatusResponse>, Status>;
    async fn list_pod_sandbox(
        &mut self,
        request: Request<ListPodSandboxRequest>,
    ) -> Result<tonic::Response<ListPodSandboxResponse>, Status>;
    async fn list_containers(
        &mut self,
        request: Request<ListContainersRequest>,
    ) -> Result<tonic::Response<ListContainersResponse>, Status>;
    async fn container_status(
        &mut self,
        request: Request<ContainerStatusRequest>,
    ) -> Result<tonic::Response<ContainerStatusResponse>, Status>;
    async fn stop_container(
        &mut self,
        request: Request<StopContainerRequest>,
    ) -> Result<tonic::Response<StopContainerResponse>, Status>;
    async fn stop_pod_sandbox(
        &mut self,
        request: Request<StopPodSandboxRequest>,
    ) -> Result<tonic::Response<StopPodSandboxResponse>, Status>;
    async fn remove_container(
        &mut self,
        request: Request<RemoveContainerRequest>,
    ) -> Result<tonic::Response<RemoveContainerResponse>, Status>;
    async fn remove_pod_sandbox(
        &mut self,
        request: Request<RemovePodSandboxRequest>,
    ) -> Result<tonic::Response<RemovePodSandboxResponse>, Status>;
}

#[async_trait]
impl RuntimeApi for RuntimeServiceClient<Channel> {
    async fn run_pod_sandbox(
        &mut self,
        request: Request<RunPodSandboxRequest>,
    ) -> Result<tonic::Response<RunPodSandboxResponse>, Status> {
        RuntimeServiceClient::run_pod_sandbox(self, request).await
    }
    async fn create_container(
        &mut self,
        request: Request<CreateContainerRequest>,
    ) -> Result<tonic::Response<CreateContainerResponse>, Status> {
        RuntimeServiceClient::create_container(self, request).await
    }
    async fn start_container(
        &mut self,
        request: Request<StartContainerRequest>,
    ) -> Result<tonic::Response<StartContainerResponse>, Status> {
        RuntimeServiceClient::start_container(self, request).await
    }
    async fn pod_sandbox_status(
        &mut self,
        request: Request<PodSandboxStatusRequest>,
    ) -> Result<tonic::Response<PodSandboxStatusResponse>, Status> {
        RuntimeServiceClient::pod_sandbox_status(self, request).await
    }
    async fn list_pod_sandbox(
        &mut self,
        request: Request<ListPodSandboxRequest>,
    ) -> Result<tonic::Response<ListPodSandboxResponse>, Status> {
        RuntimeServiceClient::list_pod_sandbox(self, request).await
    }
    async fn list_containers(
        &mut self,
        request: Request<ListContainersRequest>,
    ) -> Result<tonic::Response<ListContainersResponse>, Status> {
        RuntimeServiceClient::list_containers(self, request).await
    }
    async fn container_status(
        &mut self,
        request: Request<ContainerStatusRequest>,
    ) -> Result<tonic::Response<ContainerStatusResponse>, Status> {
        RuntimeServiceClient::container_status(self, request).await
    }
    async fn stop_container(
        &mut self,
        request: Request<StopContainerRequest>,
    ) -> Result<tonic::Response<StopContainerResponse>, Status> {
        RuntimeServiceClient::stop_container(self, request).await
    }
    async fn stop_pod_sandbox(
        &mut self,
        request: Request<StopPodSandboxRequest>,
    ) -> Result<tonic::Response<StopPodSandboxResponse>, Status> {
        RuntimeServiceClient::stop_pod_sandbox(self, request).await
    }
    async fn remove_container(
        &mut self,
        request: Request<RemoveContainerRequest>,
    ) -> Result<tonic::Response<RemoveContainerResponse>, Status> {
        RuntimeServiceClient::remove_container(self, request).await
    }
    async fn remove_pod_sandbox(
        &mut self,
        request: Request<RemovePodSandboxRequest>,
    ) -> Result<tonic::Response<RemovePodSandboxResponse>, Status> {
        RuntimeServiceClient::remove_pod_sandbox(self, request).await
    }
}

#[async_trait]
trait ImageApi: Send {
    async fn image_status(
        &mut self,
        request: Request<ImageStatusRequest>,
    ) -> Result<tonic::Response<ImageStatusResponse>, Status>;

    async fn pull_image(
        &mut self,
        request: Request<PullImageRequest>,
    ) -> Result<tonic::Response<PullImageResponse>, Status>;
}

#[async_trait]
impl ImageApi for ImageServiceClient<Channel> {
    async fn image_status(
        &mut self,
        request: Request<ImageStatusRequest>,
    ) -> Result<tonic::Response<ImageStatusResponse>, Status> {
        ImageServiceClient::image_status(self, request).await
    }

    async fn pull_image(
        &mut self,
        request: Request<PullImageRequest>,
    ) -> Result<tonic::Response<PullImageResponse>, Status> {
        ImageServiceClient::pull_image(self, request).await
    }
}

pub struct WorkloadManager {
    rt_client: Box<dyn RuntimeApi>,
    img_client: Box<dyn ImageApi>,
    version: String,
}

impl WorkloadManager {
    #[cfg(test)]
    fn with_apis(rt_client: Box<dyn RuntimeApi>, img_client: Box<dyn ImageApi>) -> Self {
        Self {
            rt_client,
            img_client,
            version: "fake/1.0.0".to_string(),
        }
    }

    pub async fn connect() -> Result<Self, FlameError> {
        Self::connect_to(CONTAINERD_SOCKET).await
    }

    /// Connect to a CRI v1 Unix socket. Flame uses [`CONTAINERD_SOCKET`]; the
    /// explicit endpoint exists for local fake-runtime and integration tests.
    pub async fn connect_to(endpoint: &str) -> Result<Self, FlameError> {
        let channel = Self::new_channel(endpoint).await?;
        let mut rt_client = RuntimeServiceClient::new(channel);
        let channel = Self::new_channel(endpoint).await?;
        let img_client = ImageServiceClient::new(channel);

        let response = rt_client
            .version(request_with_timeout(
                VersionRequest {
                    version: CRI_CLIENT_VERSION.to_string(),
                },
                RPC_TIMEOUT,
            ))
            .await
            .map_err(|status| rpc_error("Version", status))?
            .into_inner();
        if response.runtime_api_version != CRI_API_VERSION {
            return Err(FlameError::VersionMismatch(format!(
                "CRI runtime API version <{}> is unsupported; expected <{}>",
                response.runtime_api_version, CRI_API_VERSION
            )));
        }

        let runtime_status = rt_client
            .status(request_with_timeout(
                StatusRequest { verbose: false },
                RPC_TIMEOUT,
            ))
            .await
            .map_err(|status| rpc_error("Status", status))?
            .into_inner()
            .status
            .ok_or_else(|| {
                FlameError::InvalidState("CRI Status response is missing status".to_string())
            })?;
        for required in ["RuntimeReady", "NetworkReady"] {
            let condition = runtime_status
                .conditions
                .iter()
                .find(|condition| condition.r#type == required)
                .ok_or_else(|| {
                    FlameError::InvalidState(format!(
                        "CRI Status response is missing required condition <{required}>"
                    ))
                })?;
            if !condition.status {
                return Err(FlameError::InvalidState(format!(
                    "CRI condition <{required}> is false: {}: {}",
                    condition.reason, condition.message
                )));
            }
        }

        info!(
            "CRI runtime: {}/{} ({})",
            response.runtime_name, response.runtime_version, response.runtime_api_version
        );

        Ok(Self {
            rt_client: Box::new(rt_client),
            img_client: Box::new(img_client),
            version: format!("{}/{}", response.runtime_name, response.runtime_version),
        })
    }

    #[cfg(unix)]
    async fn new_channel(endpoint: &str) -> Result<Channel, FlameError> {
        let endpoint = endpoint.to_string();
        Endpoint::try_from("http://[::]:50051")
            .expect("static tonic endpoint is valid")
            .connect_timeout(RPC_TIMEOUT)
            .connect_with_connector({
                let service_addr = endpoint.clone();
                service_fn(move |_: Uri| {
                    let service_addr = service_addr.clone();
                    async move {
                        UnixStream::connect(service_addr)
                            .await
                            .map(TokioIo::new)
                            .map_err(std::io::Error::other)
                    }
                })
            })
            .await
            .map_err(|error| {
                FlameError::Network(format!(
                    "failed to connect to CRI service <{endpoint}>: {error}"
                ))
            })
    }

    #[cfg(not(unix))]
    async fn new_channel(endpoint: &str) -> Result<Channel, FlameError> {
        Err(FlameError::Network(format!(
            "CRI Unix sockets are unsupported on this platform: {endpoint}"
        )))
    }

    pub fn version(&self) -> &str {
        &self.version
    }

    pub async fn create(&mut self, spec: &WorkloadSpec) -> Result<WorkloadHandle, FlameError> {
        spec.validate()?;
        let sandbox_config = spec.sandbox_config();

        for container in &spec.containers {
            let image = image_spec(&container.image);
            let image_present = match self
                .img_client
                .image_status(request_with_timeout(
                    ImageStatusRequest {
                        image: Some(image.clone()),
                        verbose: false,
                    },
                    RPC_TIMEOUT,
                ))
                .await
            {
                Ok(response) => response.into_inner().image.is_some(),
                Err(status) if status.code() == Code::NotFound => false,
                Err(status) => return Err(rpc_error("ImageStatus", status)),
            };
            if !image_present {
                self.img_client
                    .pull_image(request_with_timeout(
                        PullImageRequest {
                            image: Some(image),
                            auth: None,
                            sandbox_config: Some(sandbox_config.clone()),
                        },
                        IMAGE_PULL_TIMEOUT,
                    ))
                    .await
                    .map_err(|status| rpc_error("PullImage", status))?;
            }
        }

        let sandbox_id = match self
            .rt_client
            .run_pod_sandbox(request_with_timeout(
                RunPodSandboxRequest {
                    config: Some(sandbox_config.clone()),
                    runtime_handler: String::new(),
                },
                RPC_TIMEOUT,
            ))
            .await
        {
            Ok(response) => response.into_inner().pod_sandbox_id,
            Err(status) => {
                let primary = rpc_error("RunPodSandbox", status);
                let cleanup = self
                    .delete_creation(&spec.metadata.filter(), &spec.metadata.uid)
                    .await;
                return Err(with_cleanup(primary, cleanup.err()));
            }
        };
        if sandbox_id.is_empty() {
            return Err(FlameError::InvalidState(
                "RunPodSandbox returned an empty sandbox ID".to_string(),
            ));
        }

        let mut handle = WorkloadHandle::new(
            sandbox_id,
            Vec::with_capacity(spec.containers.len()),
            spec.metadata.filter(),
        );

        for container in &spec.containers {
            let config = spec.container_config(container)?;
            let container_id = match self
                .rt_client
                .create_container(request_with_timeout(
                    CreateContainerRequest {
                        pod_sandbox_id: handle.sandbox_id().to_string(),
                        config: Some(config),
                        sandbox_config: Some(sandbox_config.clone()),
                    },
                    RPC_TIMEOUT,
                ))
                .await
            {
                Ok(response) => response.into_inner().container_id,
                Err(status) => {
                    let primary = rpc_error("CreateContainer", status);
                    let cleanup = self.delete(&handle).await;
                    return Err(with_cleanup(primary, cleanup.err()));
                }
            };
            if container_id.is_empty() {
                let primary = FlameError::InvalidState(
                    "CreateContainer returned an empty container ID".to_string(),
                );
                let cleanup = self.delete(&handle).await;
                return Err(with_cleanup(primary, cleanup.err()));
            }
            handle.container_ids.push(container_id.clone());

            if let Err(status) = self
                .rt_client
                .start_container(request_with_timeout(
                    StartContainerRequest { container_id },
                    RPC_TIMEOUT,
                ))
                .await
            {
                let primary = rpc_error("StartContainer", status);
                let cleanup = self.delete(&handle).await;
                return Err(with_cleanup(primary, cleanup.err()));
            }
        }

        Ok(handle)
    }

    pub async fn status(&mut self, handle: &WorkloadHandle) -> Result<WorkloadStatus, FlameError> {
        let sandbox = self.sandbox_status(handle.sandbox_id()).await?;
        ensure_owned(&sandbox, handle.filter())?;
        let mut containers = Vec::new();
        for container_id in self.container_ids(handle.sandbox_id()).await? {
            let response = self
                .rt_client
                .container_status(request_with_timeout(
                    ContainerStatusRequest {
                        container_id,
                        verbose: false,
                    },
                    RPC_TIMEOUT,
                ))
                .await
                .map_err(|status| rpc_error("ContainerStatus", status))?
                .into_inner();
            let status = response.status.ok_or_else(|| {
                FlameError::InvalidState(
                    "CRI ContainerStatus response is missing status".to_string(),
                )
            })?;
            if !handle.filter().matches(&status.labels) {
                return Err(FlameError::InvalidState(format!(
                    "refusing to inspect container <{}> outside the requested owner scope",
                    status.id
                )));
            }
            containers.push(ContainerStatus::try_from_cri(handle.sandbox_id(), status)?);
        }
        WorkloadStatus::from_sandbox(&sandbox, containers)
    }

    pub async fn list_workload(
        &mut self,
        filter: &WorkloadFilter,
    ) -> Result<Vec<WorkloadHandle>, FlameError> {
        self.list_pods_with_labels(filter, filter.labels()).await
    }

    async fn list_pods_with_labels(
        &mut self,
        filter: &WorkloadFilter,
        selector: std::collections::HashMap<String, String>,
    ) -> Result<Vec<WorkloadHandle>, FlameError> {
        let response = self
            .rt_client
            .list_pod_sandbox(request_with_timeout(
                ListPodSandboxRequest {
                    filter: Some(PodSandboxFilter {
                        label_selector: selector.clone(),
                        ..Default::default()
                    }),
                },
                RPC_TIMEOUT,
            ))
            .await
            .map_err(|status| rpc_error("ListPodSandbox", status))?
            .into_inner();

        let mut workloads = Vec::with_capacity(response.items.len());
        for sandbox in response.items {
            if !filter.matches(&sandbox.labels)
                || !selector
                    .iter()
                    .all(|(key, value)| sandbox.labels.get(key) == Some(value))
            {
                continue;
            }
            let container_ids = self.container_ids(&sandbox.id).await?;
            let handle = WorkloadHandle::new(sandbox.id, container_ids, filter.clone());
            workloads.push(handle);
        }
        Ok(workloads)
    }

    pub async fn stop(&mut self, handle: &WorkloadHandle) -> Result<(), FlameError> {
        let sandbox = match self.sandbox_status(handle.sandbox_id()).await {
            Ok(sandbox) => sandbox,
            Err(FlameError::NotFound(_)) => return Ok(()),
            Err(error) => return Err(error),
        };
        ensure_owned(&sandbox, handle.filter())?;

        let container_ids = self.container_ids(handle.sandbox_id()).await?;
        let mut errors = Vec::new();
        for container_id in container_ids {
            if let Err(status) = self
                .rt_client
                .stop_container(request_with_timeout(
                    StopContainerRequest {
                        container_id,
                        timeout: STOP_TIMEOUT_SECONDS,
                    },
                    RPC_TIMEOUT + Duration::from_secs(STOP_TIMEOUT_SECONDS as u64),
                ))
                .await
                && status.code() != Code::NotFound
            {
                errors.push(format!("StopContainer: {status}"));
            }
        }
        if let Err(status) = self
            .rt_client
            .stop_pod_sandbox(request_with_timeout(
                StopPodSandboxRequest {
                    pod_sandbox_id: handle.sandbox_id().to_string(),
                },
                RPC_TIMEOUT,
            ))
            .await
            && status.code() != Code::NotFound
        {
            errors.push(format!("StopPodSandbox: {status}"));
        }
        cleanup_result(errors)
    }

    pub async fn delete(&mut self, handle: &WorkloadHandle) -> Result<(), FlameError> {
        let sandbox = match self.sandbox_status(handle.sandbox_id()).await {
            Ok(sandbox) => sandbox,
            Err(FlameError::NotFound(_)) => return Ok(()),
            Err(error) => return Err(error),
        };
        ensure_owned(&sandbox, handle.filter())?;

        let mut errors = Vec::new();
        if let Err(error) = self.stop(handle).await {
            errors.push(error.to_string());
        }
        for container_id in self
            .container_ids(handle.sandbox_id())
            .await
            .unwrap_or_else(|error| {
                errors.push(error.to_string());
                handle.container_ids().to_vec()
            })
        {
            if let Err(status) = self
                .rt_client
                .remove_container(request_with_timeout(
                    RemoveContainerRequest { container_id },
                    RPC_TIMEOUT,
                ))
                .await
                && status.code() != Code::NotFound
            {
                errors.push(format!("RemoveContainer: {status}"));
            }
        }
        if let Err(status) = self
            .rt_client
            .remove_pod_sandbox(request_with_timeout(
                RemovePodSandboxRequest {
                    pod_sandbox_id: handle.sandbox_id().to_string(),
                },
                RPC_TIMEOUT,
            ))
            .await
            && status.code() != Code::NotFound
        {
            errors.push(format!("RemovePodSandbox: {status}"));
        }
        cleanup_result(errors)
    }

    async fn delete_creation(
        &mut self,
        filter: &WorkloadFilter,
        workload_uid: &str,
    ) -> Result<(), FlameError> {
        let mut selector = filter.labels();
        selector.insert(LABEL_WORKLOAD_UID.to_string(), workload_uid.to_string());
        let response = self
            .rt_client
            .list_pod_sandbox(request_with_timeout(
                ListPodSandboxRequest {
                    filter: Some(PodSandboxFilter {
                        label_selector: selector,
                        ..Default::default()
                    }),
                },
                RPC_TIMEOUT,
            ))
            .await
            .map_err(|status| rpc_error("ListPodSandbox after ambiguous create", status))?
            .into_inner();
        let mut errors = Vec::new();
        for sandbox in response.items {
            if !filter.matches(&sandbox.labels)
                || sandbox.labels.get(LABEL_WORKLOAD_UID).map(String::as_str) != Some(workload_uid)
            {
                continue;
            }
            let ids = self.container_ids(&sandbox.id).await.unwrap_or_default();
            let handle = WorkloadHandle::new(sandbox.id, ids, filter.clone());
            if let Err(error) = self.delete(&handle).await {
                errors.push(error.to_string());
            }
        }
        cleanup_result(errors)
    }

    async fn sandbox_status(&mut self, sandbox_id: &str) -> Result<PodSandboxStatus, FlameError> {
        let response = self
            .rt_client
            .pod_sandbox_status(request_with_timeout(
                PodSandboxStatusRequest {
                    pod_sandbox_id: sandbox_id.to_string(),
                    verbose: false,
                },
                RPC_TIMEOUT,
            ))
            .await
            .map_err(|status| rpc_error("PodSandboxStatus", status))?
            .into_inner();
        response.status.ok_or_else(|| {
            FlameError::InvalidState("CRI PodSandboxStatus response is missing status".to_string())
        })
    }

    async fn container_ids(&mut self, sandbox_id: &str) -> Result<Vec<String>, FlameError> {
        let response = self
            .rt_client
            .list_containers(request_with_timeout(
                ListContainersRequest {
                    filter: Some(ContainerFilter {
                        pod_sandbox_id: sandbox_id.to_string(),
                        ..Default::default()
                    }),
                },
                RPC_TIMEOUT,
            ))
            .await
            .map_err(|status| rpc_error("ListContainers", status))?
            .into_inner();
        Ok(response
            .containers
            .into_iter()
            .filter(|container| container.pod_sandbox_id == sandbox_id)
            .map(|container| container.id)
            .collect())
    }
}

fn request_with_timeout<T>(message: T, timeout: Duration) -> Request<T> {
    let mut request = Request::new(message);
    request.set_timeout(timeout);
    request
}

fn ensure_owned(sandbox: &PodSandboxStatus, filter: &WorkloadFilter) -> Result<(), FlameError> {
    if filter.matches(&sandbox.labels) {
        Ok(())
    } else {
        Err(FlameError::InvalidState(format!(
            "refusing to operate on sandbox <{}> outside the requested owner scope",
            sandbox.id
        )))
    }
}

fn rpc_error(operation: &str, status: Status) -> FlameError {
    if status.code() == Code::NotFound {
        FlameError::NotFound(format!("{operation}: {}", status.message()))
    } else {
        FlameError::Network(format!("{operation}: {status}"))
    }
}

fn cleanup_result(errors: Vec<String>) -> Result<(), FlameError> {
    if errors.is_empty() {
        Ok(())
    } else {
        Err(FlameError::Internal(format!(
            "CRI cleanup failed: {}",
            errors.join("; ")
        )))
    }
}

fn with_cleanup(primary: FlameError, cleanup: Option<FlameError>) -> FlameError {
    match cleanup {
        Some(cleanup) => FlameError::Internal(format!("{primary}; rollback failed: {cleanup}")),
        None => primary,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::apis::{
        ContainerSecurityContext, ContainerSpec, LABEL_EXECUTOR_ID, ResourceLimits,
        WorkloadMetadata,
    };
    use crate::cri_v1::{
        Container, ContainerMetadata, ContainerState, PodSandbox, PodSandboxState,
    };
    use std::collections::{BTreeMap, HashMap};
    use std::sync::{Arc, Mutex};

    #[derive(Default)]
    struct FakeState {
        calls: Mutex<Vec<&'static str>>,
        labels: Mutex<HashMap<String, String>>,
        sandbox_present: Mutex<bool>,
        container_present: Mutex<bool>,
        image_present: Mutex<bool>,
    }

    struct FakeRuntime {
        state: Arc<FakeState>,
        fail_start: bool,
    }

    impl FakeRuntime {
        fn call(&self, name: &'static str) {
            self.state.calls.lock().unwrap().push(name);
        }

        fn sandbox_status_value(&self) -> PodSandboxStatus {
            PodSandboxStatus {
                id: "sandbox-1".to_string(),
                metadata: Some(Default::default()),
                state: PodSandboxState::SandboxReady.into(),
                created_at: 1,
                labels: self.state.labels.lock().unwrap().clone(),
                ..Default::default()
            }
        }
    }

    #[async_trait]
    impl RuntimeApi for FakeRuntime {
        async fn run_pod_sandbox(
            &mut self,
            request: Request<RunPodSandboxRequest>,
        ) -> Result<tonic::Response<RunPodSandboxResponse>, Status> {
            self.call("run_sandbox");
            let labels = request.into_inner().config.unwrap().labels;
            *self.state.labels.lock().unwrap() = labels;
            *self.state.sandbox_present.lock().unwrap() = true;
            Ok(tonic::Response::new(RunPodSandboxResponse {
                pod_sandbox_id: "sandbox-1".to_string(),
            }))
        }

        async fn create_container(
            &mut self,
            _: Request<CreateContainerRequest>,
        ) -> Result<tonic::Response<CreateContainerResponse>, Status> {
            self.call("create_container");
            *self.state.container_present.lock().unwrap() = true;
            Ok(tonic::Response::new(CreateContainerResponse {
                container_id: "container-1".to_string(),
            }))
        }

        async fn start_container(
            &mut self,
            _: Request<StartContainerRequest>,
        ) -> Result<tonic::Response<StartContainerResponse>, Status> {
            self.call("start_container");
            if self.fail_start {
                Err(Status::internal("injected start failure"))
            } else {
                Ok(tonic::Response::new(StartContainerResponse {}))
            }
        }

        async fn pod_sandbox_status(
            &mut self,
            _: Request<PodSandboxStatusRequest>,
        ) -> Result<tonic::Response<PodSandboxStatusResponse>, Status> {
            self.call("sandbox_status");
            if !*self.state.sandbox_present.lock().unwrap() {
                return Err(Status::not_found("sandbox removed"));
            }
            Ok(tonic::Response::new(PodSandboxStatusResponse {
                status: Some(self.sandbox_status_value()),
                ..Default::default()
            }))
        }

        async fn list_pod_sandbox(
            &mut self,
            _: Request<ListPodSandboxRequest>,
        ) -> Result<tonic::Response<ListPodSandboxResponse>, Status> {
            self.call("list_sandboxes");
            let labels = self.state.labels.lock().unwrap().clone();
            let mut foreign_labels = labels.clone();
            foreign_labels.insert(LABEL_EXECUTOR_ID.to_string(), "foreign".to_string());
            Ok(tonic::Response::new(ListPodSandboxResponse {
                items: vec![
                    PodSandbox {
                        id: "sandbox-1".to_string(),
                        labels,
                        ..Default::default()
                    },
                    PodSandbox {
                        id: "foreign-sandbox".to_string(),
                        labels: foreign_labels,
                        ..Default::default()
                    },
                ],
            }))
        }

        async fn list_containers(
            &mut self,
            request: Request<ListContainersRequest>,
        ) -> Result<tonic::Response<ListContainersResponse>, Status> {
            self.call("list_containers");
            let sandbox_id = request
                .into_inner()
                .filter
                .map(|filter| filter.pod_sandbox_id)
                .unwrap_or_default();
            let containers =
                if sandbox_id == "sandbox-1" && *self.state.container_present.lock().unwrap() {
                    vec![Container {
                        id: "container-1".to_string(),
                        pod_sandbox_id: sandbox_id,
                        metadata: Some(ContainerMetadata {
                            name: "application".to_string(),
                            attempt: 0,
                        }),
                        image: Some(image_spec("example/image")),
                        image_ref: "sha256:test".to_string(),
                        state: ContainerState::ContainerRunning.into(),
                        created_at: 1,
                        labels: self.state.labels.lock().unwrap().clone(),
                        ..Default::default()
                    }]
                } else {
                    Vec::new()
                };
            Ok(tonic::Response::new(ListContainersResponse { containers }))
        }

        async fn container_status(
            &mut self,
            _: Request<ContainerStatusRequest>,
        ) -> Result<tonic::Response<ContainerStatusResponse>, Status> {
            self.call("container_status");
            Ok(tonic::Response::new(ContainerStatusResponse {
                status: Some(crate::cri_v1::ContainerStatus {
                    id: "container-1".to_string(),
                    metadata: Some(ContainerMetadata {
                        name: "application".to_string(),
                        attempt: 0,
                    }),
                    state: ContainerState::ContainerRunning.into(),
                    created_at: 1,
                    started_at: 1,
                    image: Some(image_spec("example/image")),
                    image_ref: "sha256:test".to_string(),
                    labels: self.state.labels.lock().unwrap().clone(),
                    ..Default::default()
                }),
                ..Default::default()
            }))
        }

        async fn stop_container(
            &mut self,
            _: Request<StopContainerRequest>,
        ) -> Result<tonic::Response<StopContainerResponse>, Status> {
            self.call("stop_container");
            Ok(tonic::Response::new(StopContainerResponse {}))
        }

        async fn stop_pod_sandbox(
            &mut self,
            _: Request<StopPodSandboxRequest>,
        ) -> Result<tonic::Response<StopPodSandboxResponse>, Status> {
            self.call("stop_sandbox");
            Ok(tonic::Response::new(StopPodSandboxResponse {}))
        }

        async fn remove_container(
            &mut self,
            _: Request<RemoveContainerRequest>,
        ) -> Result<tonic::Response<RemoveContainerResponse>, Status> {
            self.call("remove_container");
            *self.state.container_present.lock().unwrap() = false;
            Ok(tonic::Response::new(RemoveContainerResponse {}))
        }

        async fn remove_pod_sandbox(
            &mut self,
            _: Request<RemovePodSandboxRequest>,
        ) -> Result<tonic::Response<RemovePodSandboxResponse>, Status> {
            self.call("remove_sandbox");
            *self.state.sandbox_present.lock().unwrap() = false;
            Ok(tonic::Response::new(RemovePodSandboxResponse {}))
        }
    }

    struct FakeImages {
        state: Arc<FakeState>,
    }

    #[async_trait]
    impl ImageApi for FakeImages {
        async fn image_status(
            &mut self,
            _: Request<ImageStatusRequest>,
        ) -> Result<tonic::Response<ImageStatusResponse>, Status> {
            self.state.calls.lock().unwrap().push("image_status");
            let image = (*self.state.image_present.lock().unwrap()).then(|| crate::cri_v1::Image {
                id: "image-1".to_string(),
                ..Default::default()
            });
            Ok(tonic::Response::new(ImageStatusResponse {
                image,
                ..Default::default()
            }))
        }

        async fn pull_image(
            &mut self,
            _: Request<PullImageRequest>,
        ) -> Result<tonic::Response<PullImageResponse>, Status> {
            self.state.calls.lock().unwrap().push("pull_image");
            *self.state.image_present.lock().unwrap() = true;
            Ok(tonic::Response::new(PullImageResponse {
                image_ref: "image-1".to_string(),
            }))
        }
    }

    fn fake_spec(executor_id: &str) -> WorkloadSpec {
        WorkloadSpec {
            metadata: WorkloadMetadata {
                name: "workload".to_string(),
                namespace: "flame".to_string(),
                uid: "uid".to_string(),
                executor_id: executor_id.to_string(),
                application: "app".to_string(),
            },
            containers: vec![ContainerSpec {
                name: "application".to_string(),
                image: "example/image".to_string(),
                command: None,
                args: Vec::new(),
                env: BTreeMap::new(),
                working_directory: String::new(),
                mounts: Vec::new(),
                resources: ResourceLimits::default(),
                security_context: ContainerSecurityContext::default(),
            }],
            log_directory: "/tmp/logs".to_string(),
        }
    }

    #[test]
    fn ownership_check_requires_executor_id() {
        let filter = WorkloadFilter::new("executor-a").unwrap();
        let mut sandbox = PodSandboxStatus {
            id: "sandbox".to_string(),
            labels: filter.labels(),
            ..Default::default()
        };
        assert!(ensure_owned(&sandbox, &filter).is_ok());
        sandbox.labels.insert(
            "io.xflops.flame.executor-id".to_string(),
            "executor-b".to_string(),
        );
        assert!(ensure_owned(&sandbox, &filter).is_err());
    }

    #[test]
    fn cleanup_error_preserves_primary_error() {
        let error = with_cleanup(
            FlameError::Network("create failed".to_string()),
            Some(FlameError::Internal("remove failed".to_string())),
        );
        assert!(error.to_string().contains("create failed"));
        assert!(error.to_string().contains("remove failed"));
    }

    #[tokio::test]
    async fn create_pulls_missing_image_before_starting_sandbox() {
        let state = Arc::new(FakeState::default());
        let runtime = FakeRuntime {
            state: state.clone(),
            fail_start: false,
        };
        let images = FakeImages {
            state: state.clone(),
        };
        let mut manager = WorkloadManager::with_apis(Box::new(runtime), Box::new(images));

        manager.create(&fake_spec("executor-1")).await.unwrap();

        let calls = state.calls.lock().unwrap();
        assert_eq!(calls[0..3], ["image_status", "pull_image", "run_sandbox"]);
    }

    #[tokio::test]
    async fn create_uses_cached_image_without_pulling() {
        let state = Arc::new(FakeState::default());
        *state.image_present.lock().unwrap() = true;
        let runtime = FakeRuntime {
            state: state.clone(),
            fail_start: false,
        };
        let images = FakeImages {
            state: state.clone(),
        };
        let mut manager = WorkloadManager::with_apis(Box::new(runtime), Box::new(images));

        manager.create(&fake_spec("executor-1")).await.unwrap();

        let calls = state.calls.lock().unwrap();
        assert_eq!(calls[0..2], ["image_status", "run_sandbox"]);
        assert!(!calls.contains(&"pull_image"));
    }

    #[tokio::test]
    async fn start_failure_rolls_back_container_and_sandbox() {
        let state = Arc::new(FakeState::default());
        let runtime = FakeRuntime {
            state: state.clone(),
            fail_start: true,
        };
        let images = FakeImages {
            state: state.clone(),
        };
        let mut manager = WorkloadManager::with_apis(Box::new(runtime), Box::new(images));
        let error = manager.create(&fake_spec("executor-1")).await.unwrap_err();

        assert!(error.to_string().contains("StartContainer"));
        assert!(!*state.container_present.lock().unwrap());
        assert!(!*state.sandbox_present.lock().unwrap());
        let calls = state.calls.lock().unwrap();
        assert!(
            calls
                .windows(2)
                .any(|calls| calls == ["stop_container", "stop_sandbox"])
        );
        assert!(
            calls
                .windows(2)
                .any(|calls| calls == ["remove_container", "remove_sandbox"])
        );
    }

    #[tokio::test]
    async fn executor_scoped_list_excludes_foreign_sandbox() {
        let state = Arc::new(FakeState::default());
        let runtime = FakeRuntime {
            state: state.clone(),
            fail_start: false,
        };
        let images = FakeImages {
            state: state.clone(),
        };
        let mut manager = WorkloadManager::with_apis(Box::new(runtime), Box::new(images));
        let filter = WorkloadFilter::new("executor-1").unwrap();
        let handle = manager.create(&fake_spec("executor-1")).await.unwrap();

        let listed = manager.list_workload(&filter).await.unwrap();

        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0].sandbox_id(), handle.sandbox_id());
    }
}
