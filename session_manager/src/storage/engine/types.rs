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

use chrono::{DateTime, Duration, Utc};
use serde::{Deserialize, Serialize};
use sqlx::{types::Json, FromRow};
use std::collections::HashMap;

use crate::FlameError;
use bytes::Bytes;
use common::apis::{
    Application, ApplicationSchema, ApplicationState, ExecutorState, Node, NodeInfo, NodeState,
    ResourceRequirement, Session, SessionStatus, Shim, Task,
};
use common::apis::{ApplicationID, Event, ExecutorID, SessionID, TaskID};

use crate::model::Executor;

#[derive(Clone, FromRow, Debug)]
pub struct EventDao {
    pub owner: String,
    pub parent: Option<String>,
    pub code: i32,
    pub message: Option<String>,
    pub creation_time: i64,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct AppSchemaDao {
    pub input: Option<String>,
    pub output: Option<String>,
    pub common_data: Option<String>,
}

#[derive(Clone, FromRow, Debug)]
pub struct ApplicationDao {
    pub name: ApplicationID,
    pub version: u32,
    pub shim: i32,
    pub image: Option<String>,
    pub description: Option<String>,
    pub labels: Option<Json<Vec<String>>>,
    pub command: Option<String>,
    pub arguments: Option<Json<Vec<String>>>,
    pub environments: Option<Json<HashMap<String, String>>>,
    pub working_directory: Option<String>,
    pub max_instances: i64,
    pub delay_release: i64,
    pub schema: Option<Json<AppSchemaDao>>,
    pub url: Option<String>,
    pub creation_time: i64,
    pub state: i32,
}

#[derive(Clone, FromRow, Debug)]
pub struct SessionDao {
    pub id: SessionID,
    pub application: String,
    pub slots: i64,
    pub version: u32,

    pub common_data: Option<Vec<u8>>,
    pub creation_time: i64,
    pub completion_time: Option<i64>,

    pub state: i32,
    pub min_instances: i64,
    pub max_instances: Option<i64>,
    pub batch_size: i64,
}

#[derive(Clone, FromRow, Debug)]
pub struct TaskDao {
    pub id: TaskID,
    pub ssn_id: SessionID,
    pub version: u32,
    pub input: Option<Vec<u8>>,
    pub output: Option<Vec<u8>>,

    pub creation_time: i64,
    pub completion_time: Option<i64>,

    pub state: i32,
}

#[derive(Clone, FromRow, Debug)]
pub struct NodeDao {
    pub name: String,
    pub state: i32,

    // Capacity resources
    pub capacity_cpu: i64,
    pub capacity_memory: i64,

    // Allocatable resources
    pub allocatable_cpu: i64,
    pub allocatable_memory: i64,

    // Node info
    pub info_arch: String,
    pub info_os: String,

    pub creation_time: i64,
    pub last_heartbeat: i64,
}

#[derive(Clone, FromRow, Debug)]
pub struct ExecutorDao {
    pub id: ExecutorID,
    pub node: String,

    pub resreq_cpu: i64,
    pub resreq_memory: i64,

    pub slots: i64,
    pub shim: i32,

    pub task_id: Option<TaskID>,
    pub ssn_id: Option<SessionID>,

    pub creation_time: i64,
    pub state: i32,
}

impl TryFrom<&SessionDao> for Session {
    type Error = FlameError;

    fn try_from(ssn: &SessionDao) -> Result<Self, Self::Error> {
        Ok(Self {
            id: ssn.id.clone(),
            application: ssn.application.clone(),
            slots: ssn.slots as u32,
            version: ssn.version,
            common_data: ssn.common_data.clone().map(Bytes::from),
            creation_time: DateTime::<Utc>::from_timestamp(ssn.creation_time, 0)
                .ok_or(FlameError::Storage("invalid creation time".to_string()))?,
            completion_time: ssn
                .completion_time
                .map(|t| {
                    DateTime::<Utc>::from_timestamp(t, 0)
                        .ok_or(FlameError::Storage("invalid completion time".to_string()))
                })
                .transpose()?,
            tasks: HashMap::new(),
            tasks_index: HashMap::new(),
            status: SessionStatus {
                state: ssn.state.try_into()?,
            },
            events: vec![],
            min_instances: ssn.min_instances as u32,
            max_instances: ssn.max_instances.map(|v| v as u32),
            batch_size: ssn.batch_size.max(1) as u32,
        })
    }
}

impl TryFrom<SessionDao> for Session {
    type Error = FlameError;

    fn try_from(ssn: SessionDao) -> Result<Self, Self::Error> {
        Session::try_from(&ssn)
    }
}

impl TryFrom<&TaskDao> for Task {
    type Error = FlameError;

    fn try_from(task: &TaskDao) -> Result<Self, Self::Error> {
        Ok(Self {
            id: task.id,
            ssn_id: task.ssn_id.clone(),
            version: task.version,
            input: task.input.clone().map(Bytes::from),
            output: task.output.clone().map(Bytes::from),

            creation_time: DateTime::<Utc>::from_timestamp(task.creation_time, 0)
                .ok_or(FlameError::Storage("invalid creation time".to_string()))?,
            completion_time: task
                .completion_time
                .map(|t| {
                    DateTime::<Utc>::from_timestamp(t, 0)
                        .ok_or(FlameError::Storage("invalid completion time".to_string()))
                })
                .transpose()?,

            state: task.state.try_into()?,
            events: vec![],
        })
    }
}

impl TryFrom<TaskDao> for Task {
    type Error = FlameError;

    fn try_from(ssn: TaskDao) -> Result<Self, Self::Error> {
        Task::try_from(&ssn)
    }
}

impl TryFrom<&ApplicationDao> for Application {
    type Error = FlameError;

    fn try_from(app: &ApplicationDao) -> Result<Self, Self::Error> {
        Ok(Self {
            name: app.name.clone(),
            version: app.version,
            state: ApplicationState::try_from(app.state)?,
            creation_time: DateTime::<Utc>::from_timestamp(app.creation_time, 0)
                .ok_or(FlameError::Storage("invalid creation time".to_string()))?,
            shim: Shim::try_from(app.shim).unwrap_or_default(),
            image: app.image.clone(),
            description: app.description.clone(),
            labels: app
                .labels
                .clone()
                .map(|labels| labels.0)
                .unwrap_or_default(),
            command: app.command.clone(),
            arguments: app.arguments.clone().map(|args| args.0).unwrap_or_default(),
            environments: app
                .environments
                .clone()
                .map(|envs| envs.0)
                .unwrap_or_default(),
            working_directory: app.working_directory.clone(),
            max_instances: app.max_instances as u32,
            delay_release: Duration::seconds(app.delay_release),
            schema: app.schema.clone().map(|arg| arg.0.into()),
            url: app.url.clone(),
        })
    }
}

impl TryFrom<ApplicationDao> for Application {
    type Error = FlameError;

    fn try_from(ssn: ApplicationDao) -> Result<Self, Self::Error> {
        Application::try_from(&ssn)
    }
}

impl From<ApplicationSchema> for AppSchemaDao {
    fn from(schema: ApplicationSchema) -> Self {
        Self {
            input: schema.input,
            output: schema.output,
            common_data: schema.common_data,
        }
    }
}

impl From<AppSchemaDao> for ApplicationSchema {
    fn from(schema: AppSchemaDao) -> Self {
        Self {
            input: schema.input,
            output: schema.output,
            common_data: schema.common_data,
        }
    }
}

impl TryFrom<EventDao> for Event {
    type Error = FlameError;

    fn try_from(event: EventDao) -> Result<Self, Self::Error> {
        Ok(Self {
            code: event.code,
            message: event.message.clone(),
            creation_time: DateTime::<Utc>::from_timestamp(event.creation_time, 0)
                .ok_or(FlameError::Storage("invalid creation time".to_string()))?,
        })
    }
}

// Node DAO conversions

impl TryFrom<&NodeDao> for Node {
    type Error = FlameError;

    fn try_from(dao: &NodeDao) -> Result<Self, Self::Error> {
        Ok(Self {
            name: dao.name.clone(),
            state: NodeState::from(dao.state),
            capacity: ResourceRequirement {
                cpu: dao.capacity_cpu as u64,
                memory: dao.capacity_memory as u64,
            },
            allocatable: ResourceRequirement {
                cpu: dao.allocatable_cpu as u64,
                memory: dao.allocatable_memory as u64,
            },
            info: NodeInfo {
                arch: dao.info_arch.clone(),
                os: dao.info_os.clone(),
            },
        })
    }
}

impl TryFrom<NodeDao> for Node {
    type Error = FlameError;

    fn try_from(dao: NodeDao) -> Result<Self, Self::Error> {
        Node::try_from(&dao)
    }
}

impl From<&Node> for NodeDao {
    fn from(node: &Node) -> Self {
        Self {
            name: node.name.clone(),
            state: i32::from(node.state),
            capacity_cpu: node.capacity.cpu as i64,
            capacity_memory: node.capacity.memory as i64,
            allocatable_cpu: node.allocatable.cpu as i64,
            allocatable_memory: node.allocatable.memory as i64,
            info_arch: node.info.arch.clone(),
            info_os: node.info.os.clone(),
            creation_time: Utc::now().timestamp(),
            last_heartbeat: Utc::now().timestamp(),
        }
    }
}

// Executor DAO conversions

impl TryFrom<&ExecutorDao> for Executor {
    type Error = FlameError;

    fn try_from(dao: &ExecutorDao) -> Result<Self, Self::Error> {
        Ok(Self {
            id: dao.id.clone(),
            node: dao.node.clone(),
            resreq: ResourceRequirement {
                cpu: dao.resreq_cpu as u64,
                memory: dao.resreq_memory as u64,
            },
            slots: dao.slots as u32,
            shim: Shim::try_from(dao.shim).unwrap_or_default(),
            task_id: dao.task_id,
            ssn_id: dao.ssn_id.clone(),
            creation_time: DateTime::<Utc>::from_timestamp(dao.creation_time, 0)
                .ok_or(FlameError::Storage("invalid creation time".to_string()))?,
            state: ExecutorState::from(dao.state),
        })
    }
}

impl TryFrom<ExecutorDao> for Executor {
    type Error = FlameError;

    fn try_from(dao: ExecutorDao) -> Result<Self, Self::Error> {
        Executor::try_from(&dao)
    }
}

impl From<&Executor> for ExecutorDao {
    fn from(exec: &Executor) -> Self {
        Self {
            id: exec.id.clone(),
            node: exec.node.clone(),
            resreq_cpu: exec.resreq.cpu as i64,
            resreq_memory: exec.resreq.memory as i64,
            slots: exec.slots as i64,
            shim: i32::from(exec.shim),
            task_id: exec.task_id,
            ssn_id: exec.ssn_id.clone(),
            creation_time: exec.creation_time.timestamp(),
            state: i32::from(exec.state),
        }
    }
}
