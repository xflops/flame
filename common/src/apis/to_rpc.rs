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

use rpc::flame::v1 as rpc;

use super::types::*;

impl From<ResourceRequirement> for rpc::ResourceRequirement {
    fn from(req: ResourceRequirement) -> Self {
        Self {
            cpu: req.cpu,
            memory: req.memory,
            gpu: 0,
        }
    }
}

impl From<NodeInfo> for rpc::NodeInfo {
    fn from(info: NodeInfo) -> Self {
        Self {
            arch: info.arch,
            os: info.os,
        }
    }
}

impl From<NodeState> for rpc::NodeState {
    fn from(state: NodeState) -> Self {
        match state {
            NodeState::Unknown => rpc::NodeState::Unknown,
            NodeState::Ready => rpc::NodeState::Ready,
            NodeState::NotReady => rpc::NodeState::NotReady,
        }
    }
}

impl From<NodeState> for i32 {
    fn from(state: NodeState) -> Self {
        match state {
            NodeState::Unknown => 0,
            NodeState::Ready => 1,
            NodeState::NotReady => 2,
        }
    }
}

impl From<Node> for rpc::Node {
    fn from(node: Node) -> Self {
        let status = Some(rpc::NodeStatus {
            state: node.state.into(),
            capacity: Some(node.capacity.into()),
            allocatable: Some(node.allocatable.into()),
            info: Some(node.info.into()),
            addresses: vec![],
            last_heartbeat_time: 0,
        });

        Self {
            metadata: Some(rpc::Metadata {
                id: node.name.clone(),
                name: node.name.clone(),
            }),
            spec: Some(rpc::NodeSpec {
                hostname: node.name.clone(),
            }),
            status,
        }
    }
}

impl From<&Node> for rpc::Node {
    fn from(node: &Node) -> Self {
        rpc::Node::from(node.clone())
    }
}

impl From<Event> for rpc::Event {
    fn from(event: Event) -> Self {
        Self {
            code: event.code,
            message: event.message,
            creation_time: event.creation_time.timestamp_millis(),
        }
    }
}

impl From<TaskContext> for rpc::TaskContext {
    fn from(ctx: TaskContext) -> Self {
        Self {
            task_id: ctx.task_id.clone(),
            session_id: ctx.session_id.clone(),
            input: ctx.input.map(|d| d.into()),
        }
    }
}

impl From<SessionContext> for rpc::SessionContext {
    fn from(ctx: SessionContext) -> Self {
        Self {
            session_id: ctx.session_id.clone(),
            application: Some(ctx.application.into()),
            common_data: ctx.common_data.map(|d| d.into()),
        }
    }
}

impl From<ApplicationContext> for rpc::ApplicationContext {
    fn from(ctx: ApplicationContext) -> Self {
        Self {
            name: ctx.name.clone(),
            shim: rpc::Shim::from(ctx.shim).into(),
            image: ctx.image.clone(),
            command: ctx.command.clone(),
            working_directory: ctx.working_directory.clone(),
            url: ctx.url.clone(),
        }
    }
}

impl From<Task> for rpc::Task {
    fn from(task: Task) -> Self {
        rpc::Task::from(&task)
    }
}

impl From<&Task> for rpc::Task {
    fn from(task: &Task) -> Self {
        let metadata = Some(rpc::Metadata {
            id: task.id.to_string(),
            name: task.id.to_string(),
        });

        let spec = Some(rpc::TaskSpec {
            session_id: task.ssn_id.to_string(),
            input: task.input.clone().map(TaskInput::into),
            output: task.output.clone().map(TaskOutput::into),
        });
        let status = Some(rpc::TaskStatus {
            state: task.state as i32,
            creation_time: task.creation_time.timestamp_millis(),
            completion_time: task.completion_time.map(|s| s.timestamp_millis()),
            events: task.events.clone().into_iter().map(Event::into).collect(),
        });
        rpc::Task {
            metadata,
            spec,
            status,
        }
    }
}

impl From<Session> for rpc::Session {
    fn from(ssn: Session) -> Self {
        rpc::Session::from(&ssn)
    }
}

impl From<&Session> for rpc::Session {
    fn from(ssn: &Session) -> Self {
        let mut status = rpc::SessionStatus {
            state: ssn.status.state as i32,
            creation_time: ssn.creation_time.timestamp_millis(),
            completion_time: ssn.completion_time.map(|s| s.timestamp_millis()),
            failed: 0,
            pending: 0,
            running: 0,
            succeed: 0,
            cancelled: 0,
            events: ssn.events.clone().into_iter().map(Event::into).collect(),
        };
        for (s, v) in &ssn.tasks_index {
            match s {
                TaskState::Pending => status.pending = v.len() as i32,
                TaskState::Running => status.running = v.len() as i32,
                TaskState::Succeed => status.succeed = v.len() as i32,
                TaskState::Failed => status.failed = v.len() as i32,
                TaskState::Cancelled => status.cancelled = v.len() as i32,
            }
        }

        rpc::Session {
            metadata: Some(rpc::Metadata {
                id: ssn.id.to_string(),
                name: ssn.id.to_string(),
            }),
            spec: Some(rpc::SessionSpec {
                application: ssn.application.clone(),
                slots: ssn.slots,
                common_data: ssn.common_data.clone().map(CommonData::into),
                min_instances: ssn.min_instances,
                max_instances: ssn.max_instances,
                batch_size: ssn.batch_size,
            }),
            status: Some(status),
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

impl From<Application> for rpc::Application {
    fn from(app: Application) -> Self {
        rpc::Application::from(&app)
    }
}

impl From<&Application> for rpc::Application {
    fn from(app: &Application) -> Self {
        let spec = Some(rpc::ApplicationSpec {
            shim: rpc::Shim::from(app.shim).into(),
            image: app.image.clone(),
            description: app.description.clone(),
            labels: app.labels.clone(),
            command: app.command.clone(),
            arguments: app.arguments.to_vec(),
            environments: app
                .environments
                .clone()
                .into_iter()
                .map(|(k, v)| rpc::Environment { name: k, value: v })
                .collect(),
            working_directory: app.working_directory.clone(),
            max_instances: Some(app.max_instances),
            delay_release: Some(app.delay_release.num_seconds()),
            schema: app.schema.clone().map(rpc::ApplicationSchema::from),
            url: app.url.clone(),
        });
        let metadata = Some(rpc::Metadata {
            id: app.name.clone(),
            name: app.name.clone(),
        });

        let status = Some(rpc::ApplicationStatus {
            state: app.state.into(),
            creation_time: app.creation_time.timestamp_millis(),
        });
        rpc::Application {
            metadata,
            spec,
            status,
        }
    }
}

impl From<ApplicationState> for rpc::ApplicationState {
    fn from(s: ApplicationState) -> Self {
        match s {
            ApplicationState::Disabled => Self::Disabled,
            ApplicationState::Enabled => Self::Enabled,
        }
    }
}

impl From<ApplicationState> for i32 {
    fn from(s: ApplicationState) -> Self {
        s as i32
    }
}

impl From<SessionState> for rpc::SessionState {
    fn from(state: SessionState) -> Self {
        match state {
            SessionState::Open => rpc::SessionState::Open,
            SessionState::Closed => rpc::SessionState::Closed,
        }
    }
}

impl From<SessionState> for i32 {
    fn from(s: SessionState) -> Self {
        s as i32
    }
}

impl From<TaskState> for rpc::TaskState {
    fn from(state: TaskState) -> Self {
        match state {
            TaskState::Pending => rpc::TaskState::Pending,
            TaskState::Running => rpc::TaskState::Running,
            TaskState::Succeed => rpc::TaskState::Succeed,
            TaskState::Failed => rpc::TaskState::Failed,
            TaskState::Cancelled => rpc::TaskState::Cancelled,
        }
    }
}

impl From<TaskState> for i32 {
    fn from(s: TaskState) -> Self {
        s as i32
    }
}

impl From<Shim> for rpc::Shim {
    fn from(s: Shim) -> Self {
        match s {
            Shim::Host => Self::Host,
            Shim::Wasm => Self::Wasm,
        }
    }
}

impl From<Shim> for i32 {
    fn from(s: Shim) -> Self {
        s as i32
    }
}

impl From<ExecutorState> for rpc::ExecutorState {
    fn from(s: ExecutorState) -> Self {
        match s {
            ExecutorState::Void => rpc::ExecutorState::ExecutorVoid,
            ExecutorState::Idle => rpc::ExecutorState::ExecutorIdle,
            ExecutorState::Binding => rpc::ExecutorState::ExecutorBinding,
            ExecutorState::Bound => rpc::ExecutorState::ExecutorBound,
            ExecutorState::Unbinding => rpc::ExecutorState::ExecutorUnbinding,
            ExecutorState::Releasing => rpc::ExecutorState::ExecutorReleasing,
            ExecutorState::Released => rpc::ExecutorState::ExecutorReleased,
            _ => rpc::ExecutorState::ExecutorUnknown,
        }
    }
}

impl From<ExecutorState> for i32 {
    fn from(s: ExecutorState) -> Self {
        s as i32
    }
}

impl From<&Task> for EventOwner {
    fn from(task: &Task) -> Self {
        Self {
            task_id: task.id,
            session_id: task.ssn_id.clone(),
        }
    }
}

impl From<&TaskGID> for EventOwner {
    fn from(gid: &TaskGID) -> Self {
        Self {
            task_id: gid.task_id,
            session_id: gid.ssn_id.clone(),
        }
    }
}

impl From<Task> for EventOwner {
    fn from(task: Task) -> Self {
        Self::from(&task)
    }
}

impl From<TaskGID> for EventOwner {
    fn from(gid: TaskGID) -> Self {
        Self::from(&gid)
    }
}
