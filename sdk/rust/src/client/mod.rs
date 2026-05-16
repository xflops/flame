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

use std::collections::HashMap;
use std::future::Future;
use std::pin::Pin;
use std::sync::{Arc, Mutex};
use std::task::{Context, Poll};

use chrono::{DateTime, Duration, Utc};
use futures::TryFutureExt;
// use serde::{Deserialize, Serialize};
use serde_derive::{Deserialize, Serialize};
use stdng::{lock_ptr, trace_fn};
use tokio_stream::StreamExt;
use tonic::transport::Channel;
use tonic::transport::Endpoint;
use tonic::Request;
use url::Url;

use self::rpc::frontend_client::FrontendClient as FlameFrontendClient;
use self::rpc::{
    ApplicationSpec, CloseSessionRequest, CreateSessionRequest, CreateTaskRequest, Environment,
    GetApplicationRequest, GetNodeRequest, GetSessionRequest, GetTaskRequest,
    ListApplicationRequest, ListExecutorRequest, ListNodesRequest, ListSessionRequest,
    ListTaskRequest, OpenSessionRequest, RegisterApplicationRequest, SessionSpec, TaskSpec,
    UnregisterApplicationRequest, UpdateApplicationRequest, WatchTaskRequest,
};
use crate::apis::flame::v1 as rpc;
use crate::apis::FlameClientTls;
use crate::apis::{
    ApplicationID, ApplicationState, CommonData, ExecutorState, FlameError, SessionID,
    SessionState, Shim, TaskID, TaskInput, TaskOutput, TaskState,
};
use crate::message::{self, FromTaskOutput, IntoCommonData, IntoTaskInput};

type FlameClient = FlameFrontendClient<Channel>;
type TaskHandleInner<O> =
    Pin<Box<dyn Future<Output = Result<Option<O>, FlameError>> + Send + 'static>>;
type TaskFutureInner<O> =
    Pin<Box<dyn Future<Output = Result<TaskResult<O>, FlameError>> + Send + 'static>>;

/// Connect to a Flame service without TLS (plaintext).
///
/// Use `connect_with_tls` for TLS-enabled connections.
pub async fn connect(addr: &str) -> Result<Connection, FlameError> {
    connect_with_tls(addr, None).await
}

/// Connect to a Flame service with optional TLS configuration.
///
/// # Arguments
/// * `addr` - The endpoint URL (use https:// for TLS, http:// for plaintext)
/// * `tls_config` - Optional TLS configuration for secure connections
///
/// # TLS Behavior
/// - If `addr` starts with `https://` and `tls_config` is `Some`, use provided TLS config
/// - If `addr` starts with `https://` and `tls_config` is `None`, use default TLS config (system CA)
/// - If `addr` starts with `http://`, TLS is not used regardless of `tls_config`
pub async fn connect_with_tls(
    addr: &str,
    tls_config: Option<&FlameClientTls>,
) -> Result<Connection, FlameError> {
    let mut channel_builder = Endpoint::from_shared(addr.to_string())
        .map_err(|_| FlameError::InvalidConfig(format!("invalid address <{addr}>")))?;

    // Apply TLS if endpoint uses https://
    if addr.starts_with("https://") {
        // Extract domain name from URL for TLS verification
        let url = Url::parse(addr)
            .map_err(|e| FlameError::InvalidConfig(format!("invalid URL <{}>: {}", addr, e)))?;
        let domain = url
            .host_str()
            .ok_or_else(|| FlameError::InvalidConfig(format!("no host in URL <{}>", addr)))?;

        let client_tls_config = if let Some(tls) = tls_config {
            tls.client_tls_config(domain)?
        } else {
            // Use default TLS config (system CA bundle)
            tonic::transport::ClientTlsConfig::new().domain_name(domain)
        };

        channel_builder = channel_builder.tls_config(client_tls_config).map_err(|e| {
            FlameError::InvalidConfig(format!("TLS config error for <{}>: {}", addr, e))
        })?;
        tracing::debug!("TLS enabled for connection to {}", addr);
    }

    let channel = channel_builder.connect().await.map_err(|e| {
        FlameError::InvalidConfig(format!("failed to connect to <{}>: {}", addr, e))
    })?;

    Ok(Connection { channel })
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Event {
    pub code: i32,
    pub message: Option<String>,
    #[serde(with = "serde_utc")]
    pub creation_time: DateTime<Utc>,
}

#[derive(Clone)]
pub struct Connection {
    pub(crate) channel: Channel,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct SessionAttributes {
    pub id: SessionID,
    pub application: String,
    #[serde(with = "serde_message")]
    pub common_data: Option<CommonData>,
    pub min_instances: u32,
    pub max_instances: Option<u32>,
    #[serde(default = "default_batch_size")]
    pub batch_size: u32,
    #[serde(default)]
    pub priority: u32,
    #[serde(default)]
    pub resreq: Option<ResourceRequirement>,
}

fn default_batch_size() -> u32 {
    1
}

#[derive(Clone, Debug)]
pub struct SessionOptions {
    pub id: Option<SessionID>,
    pub application: String,
    common_data: Option<CommonData>,
    pub min_instances: u32,
    pub max_instances: Option<u32>,
    pub batch_size: u32,
    pub priority: u32,
    pub resreq: Option<ResourceRequirement>,
}

impl SessionOptions {
    pub fn new(application: impl Into<String>) -> Self {
        Self {
            id: None,
            application: application.into(),
            common_data: None,
            min_instances: 0,
            max_instances: None,
            batch_size: 1,
            priority: 0,
            resreq: None,
        }
    }

    pub fn id(mut self, id: impl Into<SessionID>) -> Self {
        self.id = Some(id.into());
        self
    }

    pub fn common_data(mut self, data: impl IntoCommonData) -> Result<Self, FlameError> {
        self.common_data = Some(data.into_common_data()?);
        Ok(self)
    }

    pub fn min_instances(mut self, value: u32) -> Self {
        self.min_instances = value;
        self
    }

    pub fn max_instances(mut self, value: u32) -> Self {
        self.max_instances = Some(value);
        self
    }

    pub fn batch_size(mut self, value: u32) -> Self {
        self.batch_size = value;
        self
    }

    pub fn priority(mut self, value: u32) -> Self {
        self.priority = value;
        self
    }

    pub fn resreq(mut self, value: impl Into<ResourceRequirement>) -> Self {
        self.resreq = Some(value.into());
        self
    }

    pub fn into_session_attributes(self) -> Result<SessionAttributes, FlameError> {
        if self.application.trim().is_empty() {
            return Err(FlameError::InvalidConfig(
                "session application must not be empty".to_string(),
            ));
        }
        Ok(SessionAttributes::from(self))
    }
}

impl From<&str> for SessionOptions {
    fn from(application: &str) -> Self {
        Self::new(application)
    }
}

impl From<String> for SessionOptions {
    fn from(application: String) -> Self {
        Self::new(application)
    }
}

impl From<SessionOptions> for SessionAttributes {
    fn from(options: SessionOptions) -> Self {
        let id = options
            .id
            .unwrap_or_else(|| format!("{}-{}", options.application, stdng::rand::short_name()));

        Self {
            id,
            application: options.application,
            common_data: options.common_data,
            min_instances: options.min_instances,
            max_instances: options.max_instances,
            batch_size: options.batch_size.max(1),
            priority: options.priority,
            resreq: options.resreq,
        }
    }
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct ResourceRequirement {
    pub cpu: u64,
    pub memory: u64,
    pub gpu: i32,
}

impl ResourceRequirement {
    /// Parse memory string like "16g" into bytes.
    fn parse_memory(s: &str) -> u64 {
        if s.is_empty() {
            return 0;
        }
        let s = s.to_lowercase();
        let v = s[..s.len() - 1].parse::<u64>().unwrap_or(0);
        let unit = s[s.len() - 1..].to_string();
        match unit.as_str() {
            "k" => v * 1024,
            "m" => v * 1024 * 1024,
            "g" => v * 1024 * 1024 * 1024,
            _ => s.parse::<u64>().unwrap_or(0),
        }
    }
}

impl From<&str> for ResourceRequirement {
    fn from(s: &str) -> Self {
        Self::from(&s.to_string())
    }
}

impl From<String> for ResourceRequirement {
    fn from(s: String) -> Self {
        Self::from(&s)
    }
}

impl From<&String> for ResourceRequirement {
    fn from(s: &String) -> Self {
        let parts = s.split(',');
        let mut cpu = 0;
        let mut memory = 0;
        let mut gpu = 0;
        for p in parts {
            let mut parts = p.split('=').map(|s| s.trim());
            let key = parts.next();
            let value = parts.next();
            match (key, value) {
                (Some("cpu"), Some(value)) => cpu = value.parse::<u64>().unwrap_or(0),
                (Some("memory"), Some(value)) => memory = Self::parse_memory(value),
                (Some("mem"), Some(value)) => memory = Self::parse_memory(value),
                (Some("gpu"), Some(value)) => gpu = value.parse::<i32>().unwrap_or(0),
                _ => {
                    tracing::error!("Invalid resource requirement: {s}");
                }
            }
        }
        Self { cpu, memory, gpu }
    }
}

impl From<&ResourceRequirement> for rpc::ResourceRequirement {
    fn from(r: &ResourceRequirement) -> Self {
        Self {
            cpu: r.cpu,
            memory: r.memory,
            gpu: r.gpu,
        }
    }
}

impl From<rpc::ResourceRequirement> for ResourceRequirement {
    fn from(r: rpc::ResourceRequirement) -> Self {
        Self {
            cpu: r.cpu,
            memory: r.memory,
            gpu: r.gpu,
        }
    }
}

#[derive(Clone, Serialize, Deserialize)]
pub struct ApplicationSchema {
    pub input: Option<String>,
    pub output: Option<String>,
    pub common_data: Option<String>,
}

#[derive(Clone, Serialize, Deserialize)]
pub struct ApplicationAttributes {
    pub shim: Option<Shim>,
    pub image: Option<String>,
    pub description: Option<String>,
    pub labels: Vec<String>,
    pub command: Option<String>,
    pub arguments: Vec<String>,
    pub environments: HashMap<String, String>,
    pub working_directory: Option<String>,
    pub max_instances: Option<u32>,
    #[serde(with = "serde_duration")]
    pub delay_release: Option<Duration>,
    pub schema: Option<ApplicationSchema>,
    pub url: Option<String>,
    pub installer: Option<String>,
}

#[derive(Clone, Serialize, Deserialize)]
pub struct Application {
    pub name: ApplicationID,

    pub attributes: ApplicationAttributes,

    pub state: ApplicationState,
    #[serde(with = "serde_utc")]
    pub creation_time: DateTime<Utc>,
}

#[derive(Clone, Serialize, Deserialize)]
pub struct Executor {
    pub id: String,
    pub state: ExecutorState,
    pub session_id: Option<String>,
    pub node: String,
}

#[derive(Clone, Serialize, Deserialize)]
pub struct Node {
    pub name: String,
    pub hostname: String,
    pub state: NodeState,
    pub cpu: u64,
    pub memory: u64,
    pub arch: String,
    pub os: String,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Hash, Serialize, Deserialize)]
pub enum NodeState {
    #[default]
    Unknown = 0,
    Ready = 1,
    NotReady = 2,
}

#[derive(Clone, Serialize, Deserialize)]
pub struct Session {
    #[serde(skip)]
    pub(crate) client: Option<FlameClient>,

    pub id: SessionID,
    pub application: String,
    #[serde(with = "serde_message")]
    pub common_data: Option<CommonData>,
    #[serde(with = "serde_utc")]
    pub creation_time: DateTime<Utc>,

    pub state: SessionState,
    pub pending: i32,
    pub running: i32,
    pub succeed: i32,
    pub failed: i32,

    pub events: Vec<Event>,
    pub tasks: Option<Vec<Task>>,
    pub priority: u32,
    #[serde(default)]
    pub resreq: Option<ResourceRequirement>,
}

#[derive(Clone, Serialize, Deserialize)]
pub struct Task {
    pub id: TaskID,
    pub ssn_id: SessionID,

    pub state: TaskState,

    #[serde(with = "serde_message")]
    pub input: Option<TaskInput>,
    #[serde(with = "serde_message")]
    pub output: Option<TaskOutput>,

    pub events: Vec<Event>,
}

pub type TaskInformerPtr = Arc<Mutex<dyn TaskInformer>>;

pub trait TaskInformer: Send + Sync + 'static {
    fn on_update(&mut self, task: Task);
    fn on_error(&mut self, e: FlameError);
}

pub struct TaskHandle<O> {
    task_id: TaskID,
    future: TaskHandleInner<O>,
}

pub struct TaskFuture<O> {
    task_id: TaskID,
    future: TaskFutureInner<O>,
}

#[derive(Clone, Debug)]
pub struct TaskResult<O> {
    pub task_id: TaskID,
    pub session_id: SessionID,
    pub state: TaskState,
    pub output: Option<O>,
    pub error_code: Option<i32>,
    pub error_message: Option<String>,
}

impl Task {
    pub fn is_completed(&self) -> bool {
        self.state.is_terminal()
    }

    pub fn is_succeed(&self) -> bool {
        self.state == TaskState::Succeed
    }

    pub fn is_failed(&self) -> bool {
        self.state == TaskState::Failed
    }

    pub fn is_cancelled(&self) -> bool {
        self.state == TaskState::Cancelled
    }
}

impl<O> TaskResult<O> {
    pub fn is_succeed(&self) -> bool {
        self.state == TaskState::Succeed
    }

    pub fn is_failed(&self) -> bool {
        self.state == TaskState::Failed
    }

    pub fn is_cancelled(&self) -> bool {
        self.state == TaskState::Cancelled
    }
}

impl<O> TaskResult<O>
where
    O: FromTaskOutput,
{
    pub fn from_task(task: Task) -> Result<Self, FlameError> {
        let is_succeed = task.is_succeed();
        let (error_code, error_message) = if is_succeed {
            (None, None)
        } else {
            (task_error_code(&task), task_error_message(&task))
        };
        let output = if is_succeed {
            O::from_task_output(task.output)?
        } else {
            None
        };

        Ok(Self {
            task_id: task.id,
            session_id: task.ssn_id,
            state: task.state,
            output,
            error_code,
            error_message,
        })
    }
}

impl Connection {
    pub async fn create_session_with(
        &self,
        options: impl Into<SessionOptions>,
    ) -> Result<Session, FlameError> {
        let attrs = options.into().into_session_attributes()?;
        self.create_session(&attrs).await
    }

    pub async fn create_session(&self, attrs: &SessionAttributes) -> Result<Session, FlameError> {
        trace_fn!("Connection::create_session");

        let create_ssn_req = CreateSessionRequest {
            session_id: attrs.id.clone(),
            session: Some(SessionSpec {
                application: attrs.application.clone(),
                common_data: attrs.common_data.clone().map(CommonData::into),
                min_instances: attrs.min_instances,
                max_instances: attrs.max_instances,
                batch_size: attrs.batch_size.max(1),
                priority: attrs.priority,
                resreq: attrs.resreq.as_ref().map(rpc::ResourceRequirement::from),
            }),
        };

        let mut client = FlameClient::new(self.channel.clone());
        let ssn = client.create_session(create_ssn_req).await?;
        let inner_ssn = ssn.into_inner();
        let mut ssn = Session::try_from(&inner_ssn)?;
        ssn.client = Some(client);
        Ok(ssn)
    }

    pub async fn list_session(&self) -> Result<Vec<Session>, FlameError> {
        let mut client = FlameClient::new(self.channel.clone());
        let ssn_list = client.list_session(ListSessionRequest {}).await?;

        let inner = ssn_list.into_inner();
        inner
            .sessions
            .iter()
            .map(Session::try_from)
            .collect::<Result<Vec<Session>, FlameError>>()
    }

    pub async fn get_session(&self, id: &SessionID) -> Result<Session, FlameError> {
        let mut client = FlameClient::new(self.channel.clone());
        let ssn = client
            .get_session(GetSessionRequest {
                session_id: id.to_string(),
            })
            .await?;

        let inner_ssn = ssn.into_inner();
        let mut ssn = Session::try_from(&inner_ssn)?;
        ssn.client = Some(client);
        Ok(ssn)
    }

    pub async fn open_session(
        &self,
        id: &SessionID,
        spec: Option<&SessionAttributes>,
    ) -> Result<Session, FlameError> {
        let session_spec = spec.map(|attrs| SessionSpec {
            application: attrs.application.clone(),
            common_data: attrs.common_data.clone().map(CommonData::into),
            min_instances: attrs.min_instances,
            max_instances: attrs.max_instances,
            batch_size: attrs.batch_size.max(1),
            priority: attrs.priority,
            resreq: attrs.resreq.as_ref().map(rpc::ResourceRequirement::from),
        });

        let open_ssn_req = OpenSessionRequest {
            session_id: id.clone(),
            session: session_spec,
        };

        let mut client = FlameClient::new(self.channel.clone());
        let ssn = client.open_session(open_ssn_req).await?;
        let inner_ssn = ssn.into_inner();
        let mut ssn = Session::try_from(&inner_ssn)?;
        ssn.client = Some(client);
        Ok(ssn)
    }

    pub async fn open_or_create_session_with(
        &self,
        id: impl Into<SessionID>,
        options: impl Into<SessionOptions>,
    ) -> Result<Session, FlameError> {
        let id = id.into();
        let mut options = options.into();

        if let Some(option_id) = &options.id {
            if option_id != &id {
                return Err(FlameError::InvalidConfig(format!(
                    "session id <{}> does not match options id <{}>",
                    id, option_id
                )));
            }
        }

        options.id = Some(id.clone());
        let attrs = options.into_session_attributes()?;
        self.open_session(&id, Some(&attrs)).await
    }

    pub async fn close_session(&self, id: &str) -> Result<(), FlameError> {
        let mut client = FlameClient::new(self.channel.clone());
        client
            .close_session(CloseSessionRequest {
                session_id: id.to_string(),
            })
            .await?;

        Ok(())
    }

    pub async fn register_application(
        &self,
        name: String,
        app: ApplicationAttributes,
    ) -> Result<(), FlameError> {
        let mut client = FlameClient::new(self.channel.clone());

        let req = RegisterApplicationRequest {
            name,
            application: Some(ApplicationSpec::from(app)),
        };

        let res = client
            .register_application(Request::new(req))
            .await?
            .into_inner();

        if res.return_code < 0 {
            Err(FlameError::Network(res.message.unwrap_or_default()))
        } else {
            Ok(())
        }
    }

    pub async fn update_application(
        &self,
        name: String,
        app: ApplicationAttributes,
    ) -> Result<(), FlameError> {
        let mut client = FlameClient::new(self.channel.clone());

        let req = UpdateApplicationRequest {
            name,
            application: Some(ApplicationSpec::from(app)),
        };

        let res = client
            .update_application(Request::new(req))
            .await?
            .into_inner();

        if res.return_code < 0 {
            Err(FlameError::Network(res.message.unwrap_or_default()))
        } else {
            Ok(())
        }
    }

    pub async fn unregister_application(&self, name: String) -> Result<(), FlameError> {
        let mut client = FlameClient::new(self.channel.clone());

        let req = UnregisterApplicationRequest { name };

        let res = client
            .unregister_application(Request::new(req))
            .await?
            .into_inner();

        if res.return_code < 0 {
            Err(FlameError::Network(res.message.unwrap_or_default()))
        } else {
            Ok(())
        }
    }

    pub async fn list_application(&self) -> Result<Vec<Application>, FlameError> {
        let mut client = FlameClient::new(self.channel.clone());
        let app_list = client.list_application(ListApplicationRequest {}).await?;

        app_list
            .into_inner()
            .applications
            .iter()
            .map(Application::try_from)
            .collect::<Result<Vec<Application>, FlameError>>()
    }

    pub async fn get_application(&self, name: &str) -> Result<Application, FlameError> {
        let mut client = FlameClient::new(self.channel.clone());
        let app = client
            .get_application(GetApplicationRequest {
                name: name.to_string(),
            })
            .await?;
        Application::try_from(&app.into_inner())
    }

    pub async fn list_executor(&self) -> Result<Vec<Executor>, FlameError> {
        let mut client = FlameClient::new(self.channel.clone());
        let executor_list = client.list_executor(ListExecutorRequest {}).await?;
        let inner = executor_list.into_inner();
        inner
            .executors
            .iter()
            .map(Executor::try_from)
            .collect::<Result<Vec<Executor>, FlameError>>()
    }

    pub async fn list_node(&self) -> Result<Vec<Node>, FlameError> {
        let mut client = FlameClient::new(self.channel.clone());
        let node_list = client.list_nodes(ListNodesRequest {}).await?;
        Ok(node_list
            .into_inner()
            .nodes
            .iter()
            .map(Node::from)
            .collect())
    }

    pub async fn get_node(&self, name: &str) -> Result<Node, FlameError> {
        let mut client = FlameClient::new(self.channel.clone());
        let node = client
            .get_node(GetNodeRequest {
                name: name.to_string(),
            })
            .await?;
        let node = node
            .into_inner()
            .node
            .ok_or(FlameError::NotFound(format!("node <{}> not found", name)))?;
        Ok(Node::from(&node))
    }
}

impl Session {
    pub async fn invoke<I, O>(&self, input: I) -> Result<TaskHandle<O>, FlameError>
    where
        I: IntoTaskInput,
        O: FromTaskOutput + Send + 'static,
    {
        self.run(input).await.map(TaskHandle::from)
    }

    pub async fn run<I, O>(&self, input: I) -> Result<TaskFuture<O>, FlameError>
    where
        I: IntoTaskInput,
        O: FromTaskOutput + Send + 'static,
    {
        let task = self.create_task(input.into_task_input()?).await?;
        let session = self.clone();
        let session_id = session.id.clone();
        let task_id = task.id;
        let future_task_id = task_id.clone();

        let future = Box::pin(async move {
            let task = session.wait_task(session_id, future_task_id).await?;
            TaskResult::from_task(task)
        });

        Ok(TaskFuture { task_id, future })
    }

    pub fn common_data<T>(&self) -> Result<Option<T>, FlameError>
    where
        T: crate::message::FlameMessage,
    {
        message::decode_common_data(self.common_data.as_ref())
    }

    pub async fn create_task(&self, input: Option<TaskInput>) -> Result<Task, FlameError> {
        trace_fn!("Session::create_task");
        let mut client = self
            .client
            .clone()
            .ok_or(FlameError::Internal("no flame client".to_string()))?;

        let create_task_req = CreateTaskRequest {
            task: Some(TaskSpec {
                session_id: self.id.clone(),
                input: input.map(|input| input.to_vec()),
                output: None,
            }),
        };

        let task = client.create_task(create_task_req).await?;

        let inner = task.into_inner();
        Task::try_from(&inner)
    }

    pub async fn get_task(&self, id: &TaskID) -> Result<Task, FlameError> {
        trace_fn!("Session::get_task");
        let mut client = self
            .client
            .clone()
            .ok_or(FlameError::Internal("no flame client".to_string()))?;

        let get_task_req = GetTaskRequest {
            session_id: self.id.clone(),
            task_id: id.clone(),
        };
        let task = client.get_task(get_task_req).await?;

        let inner = task.into_inner();
        Task::try_from(&inner)
    }

    pub async fn list_tasks(&self) -> Result<Vec<Task>, FlameError> {
        // TODO (k82cn): Add top n tasks to avoid memory overflow.
        trace_fn!("Session::list_task");
        let mut client = self
            .client
            .clone()
            .ok_or(FlameError::Internal("no flame client".to_string()))?;
        let task_stream = client
            .list_task(Request::new(ListTaskRequest {
                session_id: self.id.to_string(),
            }))
            .await?;

        let mut task_list = vec![];

        let mut task_stream = task_stream.into_inner();
        while let Some(task) = task_stream.next().await {
            if let Ok(t) = task {
                task_list.push(Task::try_from(&t)?);
            }
        }

        Ok(task_list)
    }

    pub async fn run_task(
        &self,
        input: Option<TaskInput>,
        informer_ptr: TaskInformerPtr,
    ) -> Result<(), FlameError> {
        trace_fn!("Session::run_task");
        self.create_task(input)
            .and_then(|task| self.watch_task(task.ssn_id.clone(), task.id, informer_ptr))
            .await
    }

    pub async fn watch_task(
        &self,
        session_id: SessionID,
        task_id: TaskID,
        informer_ptr: TaskInformerPtr,
    ) -> Result<(), FlameError> {
        trace_fn!("Session::watch_task");
        let mut client = self
            .client
            .clone()
            .ok_or(FlameError::Internal("no flame client".to_string()))?;

        let watch_task_req = WatchTaskRequest {
            session_id,
            task_id,
        };
        let mut task_stream = client.watch_task(watch_task_req).await?.into_inner();
        while let Some(task) = task_stream.next().await {
            match task {
                Ok(t) => {
                    let mut informer = lock_ptr!(informer_ptr)?;
                    match Task::try_from(&t) {
                        Ok(parsed) => informer.on_update(parsed),
                        Err(err) => informer.on_error(err),
                    }
                }
                Err(e) => {
                    let mut informer = lock_ptr!(informer_ptr)?;
                    informer.on_error(FlameError::from(e.clone()));
                }
            }
        }
        Ok(())
    }

    async fn wait_task(&self, session_id: SessionID, task_id: TaskID) -> Result<Task, FlameError> {
        trace_fn!("Session::wait_task");
        let mut client = self
            .client
            .clone()
            .ok_or(FlameError::Internal("no flame client".to_string()))?;

        let watch_task_req = WatchTaskRequest {
            session_id,
            task_id: task_id.clone(),
        };
        let mut task_stream = client.watch_task(watch_task_req).await?.into_inner();
        let mut last_task = None;

        while let Some(task) = task_stream.next().await {
            let parsed = Task::try_from(&task?)?;
            if parsed.is_completed() {
                return Ok(parsed);
            }
            last_task = Some(parsed);
        }

        match last_task {
            Some(task) if task.is_completed() => Ok(task),
            _ => self.get_task(&task_id).await,
        }
    }

    pub async fn close(&self) -> Result<(), FlameError> {
        trace_fn!("Session::close");
        let mut client = self
            .client
            .clone()
            .ok_or(FlameError::Internal("no flame client".to_string()))?;

        let close_ssn_req = CloseSessionRequest {
            session_id: self.id.clone(),
        };

        client.close_session(close_ssn_req).await?;

        Ok(())
    }
}

impl<O> TaskHandle<O> {
    pub fn id(&self) -> &TaskID {
        &self.task_id
    }
}

impl<O> Unpin for TaskHandle<O> {}

impl<O> Future for TaskHandle<O> {
    type Output = Result<Option<O>, FlameError>;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        self.get_mut().future.as_mut().poll(cx)
    }
}

impl<O> From<TaskFuture<O>> for TaskHandle<O>
where
    O: Send + 'static,
{
    fn from(task_future: TaskFuture<O>) -> Self {
        let task_id = task_future.id().clone();
        let future = Box::pin(async move {
            let result = task_future.await?;
            if result.is_succeed() {
                Ok(result.output)
            } else {
                Err(task_result_error(&result))
            }
        });

        Self { task_id, future }
    }
}

impl<O> TaskFuture<O> {
    pub fn id(&self) -> &TaskID {
        &self.task_id
    }
}

impl<O> Unpin for TaskFuture<O> {}

impl<O> Future for TaskFuture<O> {
    type Output = Result<TaskResult<O>, FlameError>;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        self.get_mut().future.as_mut().poll(cx)
    }
}

fn task_error_code(task: &Task) -> Option<i32> {
    task.events.last().map(|event| event.code)
}

fn task_error_message(task: &Task) -> Option<String> {
    let message = task
        .events
        .iter()
        .filter_map(|event| event.message.as_deref())
        .collect::<Vec<_>>()
        .join("; ");

    if message.is_empty() {
        None
    } else {
        Some(message)
    }
}

fn task_result_error<O>(result: &TaskResult<O>) -> FlameError {
    let details = match (result.error_code, result.error_message.as_deref()) {
        (Some(code), Some(message)) => format!(": code <{}>, {}", code, message),
        (Some(code), None) => format!(": code <{}>", code),
        (None, Some(message)) => format!(": {}", message),
        (None, None) => String::new(),
    };

    FlameError::Internal(format!(
        "task <{}> in session <{}> finished with state <{}>{}",
        result.task_id, result.session_id, result.state, details
    ))
}

impl TryFrom<&rpc::Task> for Task {
    type Error = FlameError;
    fn try_from(task: &rpc::Task) -> Result<Self, FlameError> {
        let metadata = task
            .metadata
            .clone()
            .ok_or_else(|| FlameError::Internal("missing metadata in response".to_string()))?;
        let spec = task
            .spec
            .clone()
            .ok_or_else(|| FlameError::Internal("missing spec in response".to_string()))?;
        let status = task
            .status
            .clone()
            .ok_or_else(|| FlameError::Internal("missing status in response".to_string()))?;

        let events = status
            .events
            .clone()
            .into_iter()
            .map(|ev| Event::try_from(&ev))
            .collect::<Result<Vec<Event>, FlameError>>()?;

        Ok(Task {
            id: metadata.id,
            ssn_id: spec.session_id.clone(),
            input: spec.input.map(TaskInput::from),
            output: spec.output.map(TaskOutput::from),
            state: TaskState::try_from(status.state).unwrap_or(TaskState::default()),
            events,
        })
    }
}

impl TryFrom<&rpc::Session> for Session {
    type Error = FlameError;
    fn try_from(ssn: &rpc::Session) -> Result<Self, FlameError> {
        let metadata = ssn
            .metadata
            .clone()
            .ok_or_else(|| FlameError::Internal("missing metadata in response".to_string()))?;
        let status = ssn
            .status
            .clone()
            .ok_or_else(|| FlameError::Internal("missing status in response".to_string()))?;
        let spec = ssn
            .spec
            .clone()
            .ok_or_else(|| FlameError::Internal("missing spec in response".to_string()))?;

        let creation_time = DateTime::from_timestamp_millis(status.creation_time)
            .ok_or_else(|| FlameError::Internal("invalid timestamp".to_string()))?;

        let events = status
            .events
            .clone()
            .into_iter()
            .map(|ev| Event::try_from(&ev))
            .collect::<Result<Vec<Event>, FlameError>>()?;

        Ok(Session {
            client: None,
            id: metadata.id,
            application: spec.application,
            common_data: spec.common_data.map(CommonData::from),
            creation_time,
            state: SessionState::try_from(status.state).unwrap_or(SessionState::default()),
            pending: status.pending,
            running: status.running,
            succeed: status.succeed,
            failed: status.failed,
            events,
            tasks: None,
            priority: spec.priority,
            resreq: spec.resreq.map(ResourceRequirement::from),
        })
    }
}

impl TryFrom<&rpc::Event> for Event {
    type Error = FlameError;
    fn try_from(event: &rpc::Event) -> Result<Self, FlameError> {
        let creation_time = DateTime::from_timestamp_millis(event.creation_time)
            .ok_or_else(|| FlameError::Internal("invalid timestamp".to_string()))?;
        Ok(Event {
            code: event.code,
            message: event.message.clone(),
            creation_time,
        })
    }
}

impl TryFrom<rpc::Event> for Event {
    type Error = FlameError;
    fn try_from(event: rpc::Event) -> Result<Self, FlameError> {
        Event::try_from(&event)
    }
}

impl TryFrom<&rpc::Application> for Application {
    type Error = FlameError;
    fn try_from(app: &rpc::Application) -> Result<Self, FlameError> {
        let metadata = app
            .metadata
            .clone()
            .ok_or_else(|| FlameError::Internal("missing metadata in application".to_string()))?;
        let spec = app
            .spec
            .clone()
            .ok_or_else(|| FlameError::Internal("missing spec in application".to_string()))?;
        let status = app
            .status
            .ok_or_else(|| FlameError::Internal("missing status in application".to_string()))?;

        let creation_time = DateTime::from_timestamp_millis(status.creation_time)
            .ok_or_else(|| FlameError::Internal("invalid timestamp".to_string()))?;

        Ok(Self {
            name: metadata.name,
            attributes: ApplicationAttributes::from(spec),
            state: ApplicationState::from(status.state()),
            creation_time,
        })
    }
}

impl From<ApplicationAttributes> for ApplicationSpec {
    fn from(app: ApplicationAttributes) -> Self {
        Self {
            shim: app.shim.map(|s| s as i32).unwrap_or(0),
            image: app.image.clone(),
            description: app.description.clone(),
            labels: app.labels.clone(),
            command: app.command.clone(),
            arguments: app.arguments.clone(),
            environments: app
                .environments
                .clone()
                .into_iter()
                .map(|(key, value)| Environment { name: key, value })
                .collect(),
            working_directory: app.working_directory.clone(),
            max_instances: app.max_instances,
            delay_release: app.delay_release.map(|s| s.num_seconds()),
            schema: app.schema.clone().map(rpc::ApplicationSchema::from),
            url: app.url.clone(),
            installer: app.installer.clone(),
        }
    }
}

impl From<ApplicationSpec> for ApplicationAttributes {
    fn from(app: ApplicationSpec) -> Self {
        Self {
            shim: Some(Shim::from(
                rpc::Shim::try_from(app.shim).unwrap_or(rpc::Shim::Host),
            )),
            image: app.image.clone(),
            description: app.description.clone(),
            labels: app.labels.clone(),
            command: app.command.clone(),
            arguments: app.arguments.clone(),
            environments: app
                .environments
                .clone()
                .into_iter()
                .map(|env| (env.name, env.value))
                .collect(),
            working_directory: app.working_directory.clone().filter(|wd| !wd.is_empty()),
            max_instances: app.max_instances,
            delay_release: app.delay_release.map(Duration::seconds),
            schema: app.schema.clone().map(ApplicationSchema::from),
            url: app.url.clone(),
            installer: app.installer.clone(),
        }
    }
}

impl From<ApplicationSchema> for rpc::ApplicationSchema {
    fn from(schema: ApplicationSchema) -> Self {
        Self {
            input: schema.input,
            output: schema.output,
            common_data: schema.common_data,
        }
    }
}

impl From<rpc::ApplicationSchema> for ApplicationSchema {
    fn from(schema: rpc::ApplicationSchema) -> Self {
        Self {
            input: schema.input,
            output: schema.output,
            common_data: schema.common_data,
        }
    }
}

impl TryFrom<&rpc::Executor> for Executor {
    type Error = FlameError;
    fn try_from(e: &rpc::Executor) -> Result<Self, FlameError> {
        let spec = e
            .spec
            .clone()
            .ok_or_else(|| FlameError::Internal("missing spec in executor".to_string()))?;
        let status = e
            .status
            .clone()
            .ok_or_else(|| FlameError::Internal("missing status in executor".to_string()))?;
        let metadata = e
            .metadata
            .clone()
            .ok_or_else(|| FlameError::Internal("missing metadata in executor".to_string()))?;

        let state = rpc::ExecutorState::try_from(status.state)
            .map_err(|_| FlameError::Internal("invalid executor state".to_string()))?
            .into();

        Ok(Executor {
            id: metadata.id,
            session_id: status.session_id,
            node: spec.node,
            state,
        })
    }
}

impl From<&rpc::Node> for Node {
    fn from(n: &rpc::Node) -> Self {
        let metadata = n.metadata.clone().unwrap_or_default();
        let spec = n.spec.clone().unwrap_or_default();
        let status = n.status.clone().unwrap_or_default();

        let state = match rpc::NodeState::try_from(status.state) {
            Ok(rpc::NodeState::Ready) => NodeState::Ready,
            Ok(rpc::NodeState::NotReady) => NodeState::NotReady,
            _ => NodeState::Unknown,
        };

        let capacity = status.capacity.unwrap_or_default();
        let info = status.info.unwrap_or_default();

        Node {
            name: metadata.name,
            hostname: spec.hostname,
            state,
            cpu: capacity.cpu,
            memory: capacity.memory,
            arch: info.arch,
            os: info.os,
        }
    }
}

impl From<rpc::Node> for Node {
    fn from(n: rpc::Node) -> Self {
        Node::from(&n)
    }
}

mod serde_duration {
    use chrono::Duration;
    use serde::{Deserialize, Deserializer, Serializer};

    pub fn serialize<S>(duration: &Option<Duration>, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        match duration {
            Some(duration) => serializer.serialize_i64(duration.num_seconds()),
            None => serializer.serialize_none(),
        }
    }

    pub fn deserialize<'de, D>(deserializer: D) -> Result<Option<Duration>, D::Error>
    where
        D: Deserializer<'de>,
    {
        let seconds = i64::deserialize(deserializer)?;
        Ok(Some(Duration::seconds(seconds)))
    }
}

mod serde_utc {
    use chrono::{DateTime, Utc};
    use serde::{self, Deserialize, Deserializer, Serializer};

    pub fn serialize<S>(date: &DateTime<Utc>, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        serializer.serialize_i64(date.timestamp())
    }

    pub fn deserialize<'de, D>(deserializer: D) -> Result<DateTime<Utc>, D::Error>
    where
        D: Deserializer<'de>,
    {
        let timestamp = i64::deserialize(deserializer)?;
        DateTime::<Utc>::from_timestamp(timestamp, 0)
            .ok_or(serde::de::Error::custom("invalid timestamp"))
    }
}

mod serde_message {
    use bytes::Bytes;
    use prost::Message;
    use serde::{Deserialize, Deserializer, Serializer};

    pub fn serialize<S>(message: &Option<Bytes>, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        match message {
            Some(message) => serializer.serialize_str(String::from_utf8_lossy(message).as_ref()),
            None => serializer.serialize_none(),
        }
    }

    pub fn deserialize<'de, D>(deserializer: D) -> Result<Option<Bytes>, D::Error>
    where
        D: Deserializer<'de>,
    {
        let data: String = String::deserialize(deserializer)?;
        Ok(Some(Bytes::from(data.encode_to_vec())))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::message::FlameMessage;
    use bytes::Bytes;
    use chrono::TimeZone;
    use serde_derive::{Deserialize, Serialize};

    #[derive(Debug, PartialEq, Serialize, Deserialize)]
    struct TestCommonData {
        value: String,
    }

    impl crate::message::FlameMessage for TestCommonData {
        fn encode(&self) -> Result<Bytes, FlameError> {
            serde_json::to_vec(self)
                .map(Bytes::from)
                .map_err(|e| FlameError::Internal(e.to_string()))
        }

        fn decode(bytes: &[u8]) -> Result<Self, FlameError> {
            serde_json::from_slice(bytes).map_err(|e| FlameError::InvalidConfig(e.to_string()))
        }
    }

    fn test_task(id: &str, state: TaskState) -> Task {
        Task {
            id: id.to_string(),
            ssn_id: "ssn-1".to_string(),
            state,
            input: None,
            output: None,
            events: Vec::new(),
        }
    }

    #[test]
    fn session_options_generate_default_attributes() {
        let attrs = SessionOptions::new("model-app")
            .min_instances(1)
            .batch_size(0)
            .priority(7)
            .resreq("cpu=4,mem=16g")
            .into_session_attributes()
            .unwrap();

        assert!(attrs.id.starts_with("model-app-"));
        assert_eq!(attrs.application, "model-app");
        assert_eq!(attrs.min_instances, 1);
        assert_eq!(attrs.batch_size, 1);
        assert_eq!(attrs.priority, 7);
        let resreq = attrs.resreq.unwrap();
        assert_eq!(resreq.cpu, 4);
        assert_eq!(resreq.memory, 16 * 1024 * 1024 * 1024);
    }

    #[test]
    fn session_options_encode_typed_common_data() {
        let common_data = TestCommonData {
            value: "ctx".to_string(),
        };

        let attrs = SessionOptions::new("model-app")
            .id("ssn-1")
            .common_data(&common_data)
            .unwrap()
            .into_session_attributes()
            .unwrap();

        assert_eq!(attrs.id, "ssn-1");
        let decoded = TestCommonData::decode(&attrs.common_data.unwrap()).unwrap();
        assert_eq!(decoded, common_data);
    }

    #[tokio::test]
    async fn task_future_await_returns_task_result() {
        let payload = TestCommonData {
            value: "done".to_string(),
        };
        let mut task = test_task("task-1", TaskState::Succeed);
        task.output = Some(payload.encode().unwrap());
        let task_future = TaskFuture {
            task_id: "task-1".to_string(),
            future: Box::pin(async move { TaskResult::<TestCommonData>::from_task(task) }),
        };

        assert_eq!(task_future.id(), "task-1");
        let result = task_future.await.unwrap();
        assert_eq!(result.task_id, "task-1");
        assert_eq!(result.session_id, "ssn-1");
        assert!(result.is_succeed());
        assert_eq!(result.output.unwrap().value, "done");
        assert_eq!(result.error_code, None);
        assert_eq!(result.error_message, None);
    }

    #[tokio::test]
    async fn task_handle_await_returns_success_output() {
        let payload = TestCommonData {
            value: "done".to_string(),
        };
        let mut task = test_task("task-1", TaskState::Succeed);
        task.output = Some(payload.encode().unwrap());
        let task_future = TaskFuture {
            task_id: "task-1".to_string(),
            future: Box::pin(async move { TaskResult::<TestCommonData>::from_task(task) }),
        };

        let handle = TaskHandle::from(task_future);

        assert_eq!(handle.id(), "task-1");
        let output = handle.await.unwrap().unwrap();
        assert_eq!(output.value, "done");
    }

    #[tokio::test]
    async fn task_handle_await_returns_error_for_failed_task() {
        let mut task = test_task("task-2", TaskState::Failed);
        task.events.push(Event {
            code: 42,
            message: Some("remote failure".to_string()),
            creation_time: Utc.with_ymd_and_hms(2026, 5, 8, 10, 0, 0).unwrap(),
        });
        let task_future = TaskFuture {
            task_id: "task-2".to_string(),
            future: Box::pin(async move { TaskResult::<TestCommonData>::from_task(task) }),
        };

        let err = TaskHandle::from(task_future).await.unwrap_err();

        assert!(err.to_string().contains("task <task-2>"));
        assert!(err.to_string().contains("code <42>"));
        assert!(err.to_string().contains("remote failure"));
    }

    #[test]
    fn task_result_records_failed_task_error_details() {
        let mut task = test_task("task-2", TaskState::Failed);
        task.events.push(Event {
            code: 42,
            message: Some("remote failure".to_string()),
            creation_time: Utc.with_ymd_and_hms(2026, 5, 8, 10, 0, 0).unwrap(),
        });

        let result = TaskResult::<TestCommonData>::from_task(task).unwrap();

        assert_eq!(result.task_id, "task-2");
        assert_eq!(result.session_id, "ssn-1");
        assert!(result.is_failed());
        assert!(result.output.is_none());
        assert_eq!(result.error_code, Some(42));
        assert_eq!(result.error_message.as_deref(), Some("remote failure"));
    }

    /// Regression test for `Session::try_from` creation_time parsing.
    ///
    /// The wire protocol carries `creation_time` as Unix milliseconds (see
    /// `common/src/apis/to_rpc.rs` which calls `.timestamp_millis()`). The SDK
    /// must parse it as milliseconds without an extra `* 1000` multiplication.
    ///
    /// Previously the SDK multiplied by 1000 again, producing a `DateTime`
    /// roughly 1000× off (year ~57000 instead of the expected year). This test
    /// locks the correct behavior.
    #[test]
    fn session_try_from_parses_creation_time_as_millis() {
        let expected = Utc.with_ymd_and_hms(2026, 5, 8, 10, 0, 0).unwrap();
        let wire_millis = expected.timestamp_millis();

        let rpc_session = rpc::Session {
            metadata: Some(rpc::Metadata {
                id: "ssn-1".to_string(),
                name: String::new(),
            }),
            spec: Some(rpc::SessionSpec {
                application: "app".to_string(),
                common_data: None,
                min_instances: 0,
                max_instances: None,
                batch_size: 1,
                priority: 0,
                resreq: None,
            }),
            status: Some(rpc::SessionStatus {
                state: rpc::SessionState::Open as i32,
                creation_time: wire_millis,
                completion_time: None,
                pending: 0,
                running: 0,
                succeed: 0,
                failed: 0,
                cancelled: 0,
                events: vec![],
            }),
        };

        let parsed = Session::try_from(&rpc_session).expect("Session::try_from should succeed");
        assert_eq!(parsed.creation_time, expected);
    }

    /// Verifies that `From<rpc::ResourceRequirement> for ResourceRequirement`
    /// preserves all fields when converting from the wire type into the SDK type.
    #[test]
    fn resource_requirement_from_rpc_preserves_fields() {
        let rpc_rr = rpc::ResourceRequirement {
            cpu: 4,
            memory: 16 * 1024 * 1024 * 1024,
            gpu: 2,
        };
        let sdk_rr = ResourceRequirement::from(rpc_rr);
        assert_eq!(sdk_rr.cpu, 4);
        assert_eq!(sdk_rr.memory, 16 * 1024 * 1024 * 1024);
        assert_eq!(sdk_rr.gpu, 2);
    }

    /// Verifies that `Session::try_from` extracts the optional `resreq` from
    /// the RPC `SessionSpec` so the CLI can display per-session resource
    /// requirements end-to-end.
    #[test]
    fn session_try_from_extracts_resreq() {
        let when = Utc.with_ymd_and_hms(2026, 5, 8, 10, 0, 0).unwrap();
        let rpc_session = rpc::Session {
            metadata: Some(rpc::Metadata {
                id: "ssn-1".to_string(),
                name: String::new(),
            }),
            spec: Some(rpc::SessionSpec {
                application: "app".to_string(),
                common_data: None,
                min_instances: 0,
                max_instances: None,
                batch_size: 1,
                priority: 0,
                resreq: Some(rpc::ResourceRequirement {
                    cpu: 8,
                    memory: 32 * 1024 * 1024 * 1024,
                    gpu: 1,
                }),
            }),
            status: Some(rpc::SessionStatus {
                state: rpc::SessionState::Open as i32,
                creation_time: when.timestamp_millis(),
                completion_time: None,
                pending: 0,
                running: 0,
                succeed: 0,
                failed: 0,
                cancelled: 0,
                events: vec![],
            }),
        };

        let parsed = Session::try_from(&rpc_session).expect("Session::try_from should succeed");
        let resreq = parsed.resreq.expect("resreq should be populated");
        assert_eq!(resreq.cpu, 8);
        assert_eq!(resreq.memory, 32 * 1024 * 1024 * 1024);
        assert_eq!(resreq.gpu, 1);
    }

    /// Regression test for `Application::try_from` creation_time parsing.
    /// See `session_try_from_parses_creation_time_as_millis` for context.
    #[test]
    fn application_try_from_parses_creation_time_as_millis() {
        let expected = Utc.with_ymd_and_hms(2026, 5, 8, 10, 0, 0).unwrap();
        let wire_millis = expected.timestamp_millis();

        let rpc_app = rpc::Application {
            metadata: Some(rpc::Metadata {
                id: String::new(),
                name: "app-1".to_string(),
            }),
            spec: Some(rpc::ApplicationSpec {
                shim: rpc::Shim::Host as i32,
                description: None,
                labels: vec![],
                image: None,
                command: None,
                arguments: vec![],
                environments: vec![],
                working_directory: None,
                max_instances: None,
                delay_release: None,
                schema: None,
                url: None,
                installer: None,
            }),
            status: Some(rpc::ApplicationStatus {
                state: rpc::ApplicationState::Enabled as i32,
                creation_time: wire_millis,
            }),
        };

        let parsed = Application::try_from(&rpc_app).expect("Application::try_from should succeed");
        assert_eq!(parsed.creation_time, expected);
    }
}
