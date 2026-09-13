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

use std::collections::HashSet;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

#[cfg(unix)]
use tokio::net::UnixListener;
#[cfg(unix)]
use tokio_stream::wrappers::UnixListenerStream;
#[cfg(unix)]
use tonic::transport::Server;
#[cfg(unix)]
use tonic::{Request, Response, Status};

#[cfg(unix)]
use self::rpc::instance_server::{Instance, InstanceServer};
use crate::apis::flame::v1 as rpc;

use crate::apis::{ApplicationID, CommonData, FlameError, TaskInput, TaskOutput};

pub use tonic::async_trait;

pub mod message {
    pub use crate::message::*;
}

#[cfg(unix)]
const FLAME_INSTANCE_ENDPOINT: &str = "FLAME_INSTANCE_ENDPOINT";

#[derive(Clone, Debug)]
pub struct ApplicationContext {
    pub name: String,
    pub image: Option<String>,
    pub command: Option<String>,
}

const MAX_EXECUTOR_ATTRIBUTES: usize = 1_024;
const MAX_EXECUTOR_ATTRIBUTE_BYTES: usize = 256;
const MAX_EXECUTOR_ATTRIBUTES_BYTES: usize = 64 * 1_024;

#[derive(Clone, Debug, Default)]
pub struct Publisher {
    attributes: Arc<Mutex<HashSet<Vec<u8>>>>,
    attributes_bytes: Arc<AtomicUsize>,
}

impl Publisher {
    fn publish<I, B>(&self, attributes: I) -> Result<(), FlameError>
    where
        I: IntoIterator<Item = B>,
        B: Into<Vec<u8>>,
    {
        let mut snapshot = HashSet::new();
        let mut snapshot_bytes = 0;
        for value in attributes.into_iter().map(Into::into) {
            let value_len = value.len();
            if value.is_empty() || value_len > MAX_EXECUTOR_ATTRIBUTE_BYTES {
                return Err(FlameError::InvalidConfig(
                    "executor attributes must be nonempty and at most 256 bytes".to_string(),
                ));
            }
            if snapshot.insert(value) {
                snapshot_bytes += value_len;
                if snapshot.len() > MAX_EXECUTOR_ATTRIBUTES
                    || snapshot_bytes > MAX_EXECUTOR_ATTRIBUTES_BYTES
                {
                    return Err(FlameError::InvalidConfig(
                        "executor attribute publication exceeds configured limits".to_string(),
                    ));
                }
            }
        }

        let mut current = self
            .attributes
            .lock()
            .map_err(|_| FlameError::Internal("publisher mutex poisoned".to_string()))?;
        let current_bytes = self.attributes_bytes.load(Ordering::Relaxed);
        let (added_count, added_bytes) = snapshot
            .difference(&current)
            .fold((0, 0), |(count, bytes), value| {
                (count + 1, bytes + value.len())
            });
        if current.len() + added_count > MAX_EXECUTOR_ATTRIBUTES
            || current_bytes + added_bytes > MAX_EXECUTOR_ATTRIBUTES_BYTES
        {
            return Err(FlameError::InvalidConfig(
                "executor attribute publication exceeds configured limits".to_string(),
            ));
        }
        current.extend(snapshot);
        self.attributes_bytes
            .fetch_add(added_bytes, Ordering::Relaxed);
        Ok(())
    }

    #[doc(hidden)]
    pub fn take(&self) -> rpc::ExecutorAttributes {
        let mut attributes = self.attributes.lock().expect("publisher mutex poisoned");
        self.attributes_bytes.store(0, Ordering::Relaxed);
        rpc::ExecutorAttributes {
            attr: std::mem::take(&mut *attributes).into_iter().collect(),
        }
    }
}

#[derive(Clone, Debug)]
pub struct SessionContext {
    pub session_id: String,
    pub application: ApplicationContext,
    pub common_data: Option<CommonData>,
}

impl SessionContext {
    pub fn new(
        session_id: String,
        application: ApplicationContext,
        common_data: Option<CommonData>,
    ) -> Self {
        Self {
            session_id,
            application,
            common_data,
        }
    }
}

#[derive(Clone, Debug)]
pub struct TaskContext {
    pub task_id: String,
    pub session_id: String,
    pub input: Option<TaskInput>,
}

#[derive(Clone, Debug)]
pub struct FlameInstance {
    session: SessionContext,
    publisher: Publisher,
    publisher_attached: bool,
}

impl FlameInstance {
    pub fn new(session: SessionContext) -> Self {
        Self {
            session,
            publisher: Publisher::default(),
            publisher_attached: false,
        }
    }

    #[doc(hidden)]
    pub fn with_publisher(session: SessionContext, publisher: Publisher) -> Self {
        Self {
            session,
            publisher,
            publisher_attached: true,
        }
    }

    pub fn publish<I, B>(&self, attributes: I) -> Result<(), FlameError>
    where
        I: IntoIterator<Item = B>,
        B: Into<Vec<u8>>,
    {
        if !self.publisher_attached {
            return Err(FlameError::InvalidConfig(
                "flame instance is not attached to a service".to_string(),
            ));
        }
        self.publisher.publish(attributes)
    }

    pub fn session_id(&self) -> &str {
        &self.session.session_id
    }

    pub fn application(&self) -> &ApplicationContext {
        &self.session.application
    }

    pub fn application_name(&self) -> &ApplicationID {
        &self.session.application.name
    }

    pub fn common_data<T>(&self) -> Result<Option<T>, FlameError>
    where
        T: crate::message::FlameMessage,
    {
        crate::message::decode_common_data(self.session.common_data.as_ref())
    }
}

#[tonic::async_trait]
pub trait FlameService: Send + Sync + 'static {
    fn publisher(&self) -> &Publisher;

    fn publish<I, B>(&self, attributes: I) -> Result<(), FlameError>
    where
        Self: Sized,
        I: IntoIterator<Item = B>,
        B: Into<Vec<u8>>,
    {
        self.publisher().publish(attributes)
    }

    async fn on_session_enter(&self, _: SessionContext) -> Result<(), FlameError>;
    async fn on_task_invoke(&self, _: TaskContext) -> Result<Option<TaskOutput>, FlameError>;
    async fn on_session_leave(&self) -> Result<(), FlameError>;
}

pub type FlameServicePtr = Arc<dyn FlameService>;

#[cfg(unix)]
struct ShimService {
    service: FlameServicePtr,
}

#[cfg(unix)]
#[tonic::async_trait]
impl Instance for ShimService {
    async fn on_session_enter(
        &self,
        req: Request<rpc::SessionContext>,
    ) -> Result<Response<rpc::OnSessionEnterResponse>, Status> {
        tracing::debug!("ShimService::on_session_enter");

        let req = req.into_inner();
        let resp = match SessionContext::try_from(req) {
            Ok(ctx) => self.service.on_session_enter(ctx).await,
            Err(e) => Err(e),
        };

        match resp {
            Ok(()) => Ok(Response::new(rpc::OnSessionEnterResponse {
                result: Some(rpc::Result {
                    return_code: 0,
                    message: None,
                }),
                attributes: Some(self.service.publisher().take()),
            })),
            Err(e) => Ok(Response::new(rpc::OnSessionEnterResponse {
                result: Some(rpc::Result {
                    return_code: -1,
                    message: Some(e.to_string()),
                }),
                attributes: None,
            })),
        }
    }

    async fn on_task_invoke(
        &self,
        req: Request<rpc::TaskContext>,
    ) -> Result<Response<rpc::OnTaskInvokeResponse>, Status> {
        tracing::debug!("ShimService::on_task_invoke");
        let req = req.into_inner();
        let resp = self.service.on_task_invoke(TaskContext::from(req)).await;
        let attributes = Some(self.service.publisher().take());

        match resp {
            Ok(data) => Ok(Response::new(rpc::OnTaskInvokeResponse {
                task_result: Some(rpc::TaskResult {
                    return_code: 0,
                    output: data.map(|d| d.into()),
                    message: None,
                }),
                attributes,
            })),
            Err(e) => Ok(Response::new(rpc::OnTaskInvokeResponse {
                task_result: Some(rpc::TaskResult {
                    return_code: -1,
                    output: None,
                    message: Some(e.to_string()),
                }),
                attributes,
            })),
        }
    }

    async fn on_session_leave(
        &self,
        _: Request<rpc::EmptyRequest>,
    ) -> Result<Response<rpc::Result>, Status> {
        tracing::debug!("ShimService::on_session_leave");
        let resp = self.service.on_session_leave().await;

        match resp {
            Ok(_) => Ok(Response::new(rpc::Result {
                return_code: 0,
                message: None,
            })),
            Err(e) => Ok(Response::new(rpc::Result {
                return_code: -1,
                message: Some(e.to_string()),
            })),
        }
    }
}

#[cfg(unix)]
pub async fn run(service: impl FlameService) -> Result<(), Box<dyn std::error::Error>> {
    let shim_service = ShimService {
        service: Arc::new(service),
    };

    let endpoint = std::env::var(FLAME_INSTANCE_ENDPOINT)
        .map_err(|_| FlameError::InvalidConfig("FLAME_INSTANCE_ENDPOINT not found".to_string()))?;

    let uds_stream = UnixListenerStream::new(UnixListener::bind(endpoint)?);

    Server::builder()
        .add_service(InstanceServer::new(shim_service))
        .serve_with_incoming(uds_stream)
        .await?;

    Ok(())
}

#[cfg(not(unix))]
pub async fn run(_service: impl FlameService) -> Result<(), Box<dyn std::error::Error>> {
    Err(FlameError::InvalidConfig(
        "Unix domain sockets are not supported on this platform".to_string(),
    )
    .into())
}

impl From<rpc::ApplicationContext> for ApplicationContext {
    fn from(ctx: rpc::ApplicationContext) -> Self {
        Self {
            name: ctx.name.clone(),
            image: ctx.image.clone(),
            command: ctx.command.clone(),
        }
    }
}

impl TryFrom<rpc::SessionContext> for SessionContext {
    type Error = FlameError;

    fn try_from(ctx: rpc::SessionContext) -> Result<Self, Self::Error> {
        let application = ctx
            .application
            .map(ApplicationContext::from)
            .ok_or_else(|| {
                FlameError::InvalidConfig("session context missing application".to_string())
            })?;

        Ok(SessionContext::new(
            ctx.session_id.clone(),
            application,
            ctx.common_data.map(|data| data.into()),
        ))
    }
}

impl From<rpc::TaskContext> for TaskContext {
    fn from(ctx: rpc::TaskContext) -> Self {
        TaskContext {
            task_id: ctx.task_id.clone(),
            session_id: ctx.session_id.clone(),
            input: ctx.input.map(|data| data.into()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn session_context_requires_application() {
        let ctx = rpc::SessionContext {
            session_id: "ssn-1".to_string(),
            application: None,
            common_data: None,
        };

        assert!(SessionContext::try_from(ctx).is_err());
    }

    #[cfg(unix)]
    #[derive(Default)]
    struct TaskPublishingService {
        publisher: Publisher,
    }

    #[cfg(unix)]
    #[tonic::async_trait]
    impl FlameService for TaskPublishingService {
        fn publisher(&self) -> &Publisher {
            &self.publisher
        }

        async fn on_session_enter(&self, context: SessionContext) -> Result<(), FlameError> {
            if context.session_id == "ssn-failed" {
                self.publish([b"failed-enter-key".to_vec()])?;
                return Err(FlameError::Internal("session enter failed".to_string()));
            }
            Ok(())
        }

        async fn on_task_invoke(
            &self,
            task: TaskContext,
        ) -> Result<Option<TaskOutput>, FlameError> {
            if task.task_id == "task-1" {
                self.publish([b"a".to_vec()])?;
                self.publish([b"b".to_vec(), b"c".to_vec()])?;
                self.publish(Vec::<Vec<u8>>::new())?;
            } else if task.task_id == "task-error" {
                self.publish([b"error-key".to_vec()])?;
                return Err(FlameError::Internal("task failed".to_string()));
            }
            Ok(None)
        }

        async fn on_session_leave(&self) -> Result<(), FlameError> {
            Ok(())
        }
    }

    #[cfg(unix)]
    struct ConcurrentPublishingService {
        key: Vec<u8>,
        barrier: Arc<tokio::sync::Barrier>,
        publisher: Publisher,
    }

    #[cfg(unix)]
    #[tonic::async_trait]
    impl FlameService for ConcurrentPublishingService {
        fn publisher(&self) -> &Publisher {
            &self.publisher
        }

        async fn on_session_enter(&self, _: SessionContext) -> Result<(), FlameError> {
            Ok(())
        }

        async fn on_task_invoke(&self, _: TaskContext) -> Result<Option<TaskOutput>, FlameError> {
            self.barrier.wait().await;
            self.publish([self.key.clone()])?;
            Ok(None)
        }

        async fn on_session_leave(&self) -> Result<(), FlameError> {
            Ok(())
        }
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn concurrent_shims_publish_to_their_own_instance() {
        let barrier = Arc::new(tokio::sync::Barrier::new(2));
        let shim_a = ShimService {
            service: Arc::new(ConcurrentPublishingService {
                key: b"instance-a".to_vec(),
                barrier: barrier.clone(),
                publisher: Publisher::default(),
            }),
        };
        let shim_b = ShimService {
            service: Arc::new(ConcurrentPublishingService {
                key: b"instance-b".to_vec(),
                barrier,
                publisher: Publisher::default(),
            }),
        };
        let request = |task_id: &str| {
            Request::new(rpc::TaskContext {
                task_id: task_id.to_string(),
                session_id: "session".to_string(),
                input: None,
            })
        };

        let (response_a, response_b) = tokio::join!(
            Instance::on_task_invoke(&shim_a, request("task-a")),
            Instance::on_task_invoke(&shim_b, request("task-b")),
        );

        assert_eq!(
            response_a.unwrap().into_inner().attributes.unwrap().attr,
            vec![b"instance-a".to_vec()]
        );
        assert_eq!(
            response_b.unwrap().into_inner().attributes.unwrap().attr,
            vec![b"instance-b".to_vec()]
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn instance_publication_is_retained_and_delivered_once() {
        let shim = ShimService {
            service: Arc::new(TaskPublishingService::default()),
        };
        let publisher = shim.service.publisher().clone();

        let detached = FlameInstance::new(SessionContext::new(
            "detached".to_string(),
            ApplicationContext {
                name: "test-app".to_string(),
                image: None,
                command: None,
            },
            None,
        ));
        assert!(detached.publish([b"not-delivered".to_vec()]).is_err());

        publisher
            .publish(vec![b"deduplicated".to_vec(); 1_025])
            .unwrap();
        assert_eq!(publisher.take().attr.len(), 1);
        let instance = FlameInstance::with_publisher(
            SessionContext::new(
                "ssn-handle".to_string(),
                ApplicationContext {
                    name: "test-app".to_string(),
                    image: None,
                    command: None,
                },
                None,
            ),
            publisher.clone(),
        );
        instance.publish([b"instance-key".to_vec()]).unwrap();
        assert_eq!(publisher.take().attr, vec![b"instance-key".to_vec()]);
        assert!(publisher.publish([vec![b'x'; 257]]).is_err());
        let too_many = (0_u16..1_025).map(|value| value.to_be_bytes().to_vec());
        assert!(publisher.publish(too_many).is_err());

        let failed_enter = Instance::on_session_enter(
            &shim,
            Request::new(rpc::SessionContext {
                session_id: "ssn-failed".to_string(),
                application: Some(rpc::ApplicationContext {
                    name: "test-app".to_string(),
                    ..Default::default()
                }),
                common_data: None,
            }),
        )
        .await
        .unwrap()
        .into_inner();
        assert_eq!(failed_enter.result.unwrap().return_code, -1);
        assert!(failed_enter.attributes.is_none());
        assert!(publisher
            .attributes
            .lock()
            .unwrap()
            .contains(b"failed-enter-key".as_slice()));

        let enter = Instance::on_session_enter(
            &shim,
            Request::new(rpc::SessionContext {
                session_id: "ssn-1".to_string(),
                application: Some(rpc::ApplicationContext {
                    name: "test-app".to_string(),
                    ..Default::default()
                }),
                common_data: None,
            }),
        )
        .await
        .unwrap()
        .into_inner();
        assert_eq!(
            enter
                .attributes
                .unwrap()
                .attr
                .into_iter()
                .collect::<HashSet<_>>(),
            HashSet::from([b"failed-enter-key".to_vec()])
        );

        let invoke = Instance::on_task_invoke(
            &shim,
            Request::new(rpc::TaskContext {
                task_id: "task-1".to_string(),
                session_id: "ssn-1".to_string(),
                input: None,
            }),
        )
        .await
        .unwrap()
        .into_inner();
        assert_eq!(
            invoke
                .attributes
                .unwrap()
                .attr
                .into_iter()
                .collect::<HashSet<_>>(),
            HashSet::from([b"a".to_vec(), b"b".to_vec(), b"c".to_vec()])
        );

        let next_invoke = Instance::on_task_invoke(
            &shim,
            Request::new(rpc::TaskContext {
                task_id: "task-2".to_string(),
                session_id: "ssn-1".to_string(),
                input: None,
            }),
        )
        .await
        .unwrap()
        .into_inner();
        assert!(next_invoke.attributes.unwrap().attr.is_empty());

        let republished = Instance::on_task_invoke(
            &shim,
            Request::new(rpc::TaskContext {
                task_id: "task-2".to_string(),
                session_id: "ssn-1".to_string(),
                input: None,
            }),
        )
        .await
        .unwrap()
        .into_inner();
        assert!(republished.attributes.unwrap().attr.is_empty());

        assert!(publisher.publish([Vec::new()]).is_err());
        publisher.publish([b"retained-key".to_vec()]).unwrap();
        Instance::on_session_leave(&shim, Request::new(rpc::EmptyRequest {}))
            .await
            .unwrap();
        assert!(publisher
            .attributes
            .lock()
            .unwrap()
            .contains(b"retained-key".as_slice()));

        let reenter = Instance::on_session_enter(
            &shim,
            Request::new(rpc::SessionContext {
                session_id: "ssn-2".to_string(),
                application: Some(rpc::ApplicationContext {
                    name: "test-app".to_string(),
                    ..Default::default()
                }),
                common_data: None,
            }),
        )
        .await
        .unwrap()
        .into_inner();
        let expected = HashSet::from([b"retained-key".to_vec()]);
        assert_eq!(
            reenter
                .attributes
                .unwrap()
                .attr
                .into_iter()
                .collect::<HashSet<_>>(),
            expected
        );
        assert!(publisher.take().attr.is_empty());

        let empty_enter = Instance::on_session_enter(
            &shim,
            Request::new(rpc::SessionContext {
                session_id: "ssn-3".to_string(),
                application: Some(rpc::ApplicationContext {
                    name: "test-app".to_string(),
                    ..Default::default()
                }),
                common_data: None,
            }),
        )
        .await
        .unwrap()
        .into_inner();
        assert!(empty_enter.attributes.unwrap().attr.is_empty());

        let failed = Instance::on_task_invoke(
            &shim,
            Request::new(rpc::TaskContext {
                task_id: "task-error".to_string(),
                session_id: "ssn-2".to_string(),
                input: None,
            }),
        )
        .await
        .unwrap()
        .into_inner();
        assert_eq!(failed.task_result.unwrap().return_code, -1);
        assert_eq!(failed.attributes.unwrap().attr, vec![b"error-key".to_vec()]);

        let barrier = Arc::new(tokio::sync::Barrier::new(2));
        let publish_barrier = barrier.clone();
        let publish_publisher = publisher.clone();
        let publish_task = tokio::spawn(async move {
            publish_barrier.wait().await;
            publish_publisher
                .publish([b"race-a".to_vec(), b"race-b".to_vec()])
                .unwrap();
        });
        barrier.wait().await;
        let raced = publisher.take();
        publish_task.await.unwrap();
        let after_race = publisher.take();
        let expected = HashSet::from([b"race-a".to_vec(), b"race-b".to_vec()]);
        let nonempty = [raced, after_race]
            .into_iter()
            .filter(|attributes| !attributes.attr.is_empty())
            .map(|attributes| attributes.attr.into_iter().collect::<HashSet<_>>())
            .collect::<Vec<_>>();
        assert_eq!(nonempty, vec![expected]);

        publisher
            .publish((0..600).map(|index| format!("limit-key-{index}").into_bytes()))
            .unwrap();
        publisher
            .publish(
                (600..MAX_EXECUTOR_ATTRIBUTES)
                    .map(|index| format!("limit-key-{index}").into_bytes()),
            )
            .unwrap();
        assert!(publisher.publish([b"one-key-too-many".to_vec()]).is_err());
        assert_eq!(publisher.take().attr.len(), MAX_EXECUTOR_ATTRIBUTES);

        let byte_boundary = (0_u16..256)
            .map(|index| {
                let mut value = index.to_be_bytes().to_vec();
                value.resize(MAX_EXECUTOR_ATTRIBUTE_BYTES, 0);
                value
            })
            .collect::<Vec<_>>();
        publisher.publish(byte_boundary.clone()).unwrap();
        publisher.publish(byte_boundary.clone()).unwrap();
        assert!(publisher.publish([b"overflow".to_vec()]).is_err());
        assert_eq!(publisher.take().attr.len(), byte_boundary.len());

        publisher.publish(byte_boundary.clone()).unwrap();
        assert_eq!(publisher.take().attr.len(), byte_boundary.len());
        assert!(publisher.take().attr.is_empty());

        drop(shim);
        drop(publisher);
    }
}
