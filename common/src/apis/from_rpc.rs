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

use chrono::{DateTime, Duration, Utc};
use tonic::Status;

use rpc::flame::v1 as rpc;

use super::types::*;
use crate::apis::{
    object_kind, valid_role_name, valid_role_object, valid_role_subject, Role, RoleRule, ALL,
    APPLICATION_PREFIX, NODE_PREFIX,
};
use crate::FlameError;

impl From<rpc::Package> for Package {
    fn from(package: rpc::Package) -> Self {
        Self {
            url: package.url,
            signature: package.signature,
        }
    }
}

impl From<rpc::ResourceRequirement> for ResourceRequirement {
    fn from(req: rpc::ResourceRequirement) -> Self {
        Self {
            cpu: req.cpu,
            memory: req.memory,
            gpu: req.gpu,
        }
    }
}

impl From<rpc::NodeInfo> for NodeInfo {
    fn from(info: rpc::NodeInfo) -> Self {
        Self {
            arch: info.arch,
            os: info.os,
        }
    }
}

impl From<rpc::NodeState> for NodeState {
    fn from(state: rpc::NodeState) -> Self {
        match state {
            rpc::NodeState::Unknown => NodeState::Unknown,
            rpc::NodeState::Ready => NodeState::Ready,
            rpc::NodeState::NotReady => NodeState::NotReady,
        }
    }
}

impl From<i32> for NodeState {
    fn from(state: i32) -> Self {
        match state {
            0 => NodeState::Unknown,
            1 => NodeState::Ready,
            2 => NodeState::NotReady,
            _ => NodeState::Unknown,
        }
    }
}

impl From<rpc::Node> for Node {
    fn from(node: rpc::Node) -> Self {
        let status = node.status.unwrap_or_default();
        let metadata = node.metadata.unwrap_or_default();
        Self {
            name: metadata.name,
            capacity: status.capacity.unwrap_or_default().into(),
            allocatable: status.allocatable.unwrap_or_default().into(),
            info: status.info.unwrap_or_default().into(),
            state: status.state.into(),
        }
    }
}

impl From<rpc::Event> for Event {
    fn from(event: rpc::Event) -> Self {
        Self {
            code: event.code,
            message: event.message,
            creation_time: DateTime::from_timestamp_millis(event.creation_time).unwrap_or_default(),
        }
    }
}

impl From<rpc::Result> for FlameResult {
    fn from(result: rpc::Result) -> Self {
        Self {
            return_code: result.return_code,
            message: result.message,
        }
    }
}

impl TryFrom<rpc::Task> for TaskContext {
    type Error = FlameError;

    fn try_from(task: rpc::Task) -> Result<Self, Self::Error> {
        let metadata = task
            .metadata
            .ok_or(FlameError::InvalidConfig("metadata".to_string()))?;

        let spec = task
            .spec
            .ok_or(FlameError::InvalidConfig("spec".to_string()))?;

        Ok(TaskContext {
            task_id: metadata.id.clone(),
            session_id: spec.session_id.to_string(),
            input: spec.input.map(TaskInput::from),
        })
    }
}

impl TryFrom<rpc::Application> for ApplicationContext {
    type Error = FlameError;

    fn try_from(app: rpc::Application) -> Result<Self, Self::Error> {
        let metadata = app
            .metadata
            .ok_or(FlameError::InvalidConfig("metadata".to_string()))?;

        let spec = app
            .spec
            .ok_or(FlameError::InvalidConfig("spec".to_string()))?;

        Ok(ApplicationContext {
            name: metadata.name.clone(),
            shim: Shim::from(spec.shim()),
            image: spec.image.clone(),
            command: spec.command.clone(),
            arguments: spec.arguments.clone(),
            working_directory: spec.working_directory.clone(),
            environments: spec
                .environments
                .clone()
                .into_iter()
                .map(|e| (e.name, e.value))
                .collect(),
            package: spec.package.clone().map(Package::from).or_else(|| {
                spec.url.clone().map(|url| Package {
                    url,
                    signature: String::new(),
                })
            }),
            installer: spec.installer.clone(),
        })
    }
}

impl TryFrom<(rpc::Application, rpc::Session)> for SessionContext {
    type Error = FlameError;
    fn try_from((app, ssn): (rpc::Application, rpc::Session)) -> Result<Self, Self::Error> {
        let metadata = ssn
            .metadata
            .ok_or(FlameError::InvalidConfig("metadata".to_string()))?;

        let spec = ssn
            .spec
            .ok_or(FlameError::InvalidConfig("spec".to_string()))?;

        let application = ApplicationContext::try_from(app)?;

        Ok(SessionContext {
            session_id: metadata.id,
            application,
            common_data: spec.common_data.map(CommonData::from),
        })
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

impl TryFrom<rpc::Application> for Application {
    type Error = FlameError;
    fn try_from(app: rpc::Application) -> Result<Self, Self::Error> {
        Application::try_from(&app)
    }
}

impl TryFrom<&rpc::Application> for Application {
    type Error = FlameError;
    fn try_from(app: &rpc::Application) -> Result<Self, Self::Error> {
        let metadata = app.metadata.clone().ok_or(FlameError::InvalidConfig(
            "application metadata is empty".to_string(),
        ))?;

        let spec = app.spec.clone().ok_or(FlameError::InvalidConfig(
            "application spec is empty".to_string(),
        ))?;

        let status = app.status.ok_or(FlameError::InvalidConfig(
            "application status is empty".to_string(),
        ))?;

        Ok(Application {
            name: metadata.name.clone(),
            version: 0,
            state: ApplicationState::from(status.state()),
            creation_time: DateTime::<Utc>::from_timestamp_millis(status.creation_time).ok_or(
                FlameError::InvalidState("invalid creation time".to_string()),
            )?,
            shim: Shim::from(spec.shim()),
            image: spec.image.clone(),
            description: spec.description.clone(),
            labels: spec.labels.clone(),
            command: spec.command.clone(),
            arguments: spec.arguments.to_vec(),
            environments: spec
                .environments
                .clone()
                .into_iter()
                .map(|e| (e.name, e.value))
                .collect(),
            working_directory: spec.working_directory,
            max_instances: spec.max_instances.unwrap_or(DEFAULT_MAX_INSTANCES),
            delay_release: spec
                .delay_release
                .map(Duration::seconds)
                .unwrap_or(DEFAULT_DELAY_RELEASE),
            schema: spec.schema.map(ApplicationSchema::from),
            package: spec.package.clone().map(Package::from).or_else(|| {
                spec.url.clone().map(|url| Package {
                    url,
                    signature: String::new(),
                })
            }),
            installer: spec.installer.clone(),
        })
    }
}

impl From<rpc::ApplicationSpec> for ApplicationAttributes {
    fn from(spec: rpc::ApplicationSpec) -> Self {
        Self {
            shim: Shim::from(spec.shim()),
            image: spec.image.clone(),
            description: spec.description.clone(),
            labels: spec.labels.clone(),
            command: spec.command.clone(),
            arguments: spec.arguments.clone(),
            environments: spec
                .environments
                .clone()
                .into_iter()
                .map(|e| (e.name, e.value))
                .collect(),
            working_directory: spec.working_directory.clone().filter(|wd| !wd.is_empty()),
            max_instances: spec.max_instances.unwrap_or(DEFAULT_MAX_INSTANCES),
            delay_release: spec
                .delay_release
                .map(Duration::seconds)
                .unwrap_or(DEFAULT_DELAY_RELEASE),
            schema: spec.schema.map(ApplicationSchema::from),
            package: spec.package.clone().map(Package::from).or_else(|| {
                spec.url.clone().map(|url| Package {
                    url,
                    signature: String::new(),
                })
            }),
            installer: spec.installer.clone(),
        }
    }
}

impl From<rpc::ApplicationState> for ApplicationState {
    fn from(s: rpc::ApplicationState) -> Self {
        match s {
            rpc::ApplicationState::Disabled => Self::Disabled,
            rpc::ApplicationState::Enabled => Self::Enabled,
        }
    }
}

impl TryFrom<i32> for ApplicationState {
    type Error = FlameError;
    fn try_from(s: i32) -> Result<Self, Self::Error> {
        let state = rpc::ApplicationState::try_from(s)
            .map_err(|_| FlameError::InvalidState("unknown application state".to_string()))?;
        Ok(Self::from(state))
    }
}

impl From<rpc::SessionState> for SessionState {
    fn from(s: rpc::SessionState) -> Self {
        match s {
            rpc::SessionState::Open => SessionState::Open,
            rpc::SessionState::Closed => SessionState::Closed,
        }
    }
}

impl TryFrom<i32> for SessionState {
    type Error = FlameError;
    fn try_from(s: i32) -> Result<Self, Self::Error> {
        let state = rpc::SessionState::try_from(s)
            .map_err(|_| FlameError::InvalidState("invalid session state".to_string()))?;
        Ok(Self::from(state))
    }
}

impl From<rpc::TaskState> for TaskState {
    fn from(s: rpc::TaskState) -> Self {
        match s {
            rpc::TaskState::Pending => TaskState::Pending,
            rpc::TaskState::Running => TaskState::Running,
            rpc::TaskState::Succeed => TaskState::Succeed,
            rpc::TaskState::Failed => TaskState::Failed,
            rpc::TaskState::Cancelled => TaskState::Cancelled,
        }
    }
}

impl TryFrom<i32> for TaskState {
    type Error = FlameError;
    fn try_from(s: i32) -> Result<Self, Self::Error> {
        let state = rpc::TaskState::try_from(s)
            .map_err(|_| FlameError::InvalidState("invalid task state".to_string()))?;
        Ok(Self::from(state))
    }
}

impl TryFrom<String> for Shim {
    type Error = FlameError;
    fn try_from(s: String) -> Result<Self, Self::Error> {
        match s.to_lowercase().as_str() {
            "host" => Ok(Self::Host),
            "wasm" => Ok(Self::Wasm),
            "cri" => Ok(Self::Cri),
            _ => Err(FlameError::InvalidConfig(format!("invalid shim: {s}"))),
        }
    }
}

impl From<rpc::Shim> for Shim {
    fn from(s: rpc::Shim) -> Self {
        match s {
            rpc::Shim::Host => Self::Host,
            rpc::Shim::Wasm => Self::Wasm,
            rpc::Shim::Cri => Self::Cri,
        }
    }
}

impl TryFrom<i32> for Shim {
    type Error = FlameError;

    fn try_from(v: i32) -> Result<Self, Self::Error> {
        let s = rpc::Shim::try_from(v)
            .map_err(|_| FlameError::InvalidState("invalid shim".to_string()))?;
        Ok(Self::from(s))
    }
}

impl From<rpc::ExecutorState> for ExecutorState {
    fn from(s: rpc::ExecutorState) -> Self {
        match s {
            rpc::ExecutorState::ExecutorVoid => ExecutorState::Void,
            rpc::ExecutorState::ExecutorIdle => ExecutorState::Idle,
            rpc::ExecutorState::ExecutorBinding => ExecutorState::Binding,
            rpc::ExecutorState::ExecutorBound => ExecutorState::Bound,
            rpc::ExecutorState::ExecutorUnbinding => ExecutorState::Unbinding,
            rpc::ExecutorState::ExecutorReleasing => ExecutorState::Releasing,
            rpc::ExecutorState::ExecutorReleased => ExecutorState::Released,
            _ => ExecutorState::Unknown,
        }
    }
}

impl From<i32> for ExecutorState {
    fn from(s: i32) -> Self {
        match s {
            1 => ExecutorState::Void,
            2 => ExecutorState::Idle,
            3 => ExecutorState::Binding,
            4 => ExecutorState::Bound,
            5 => ExecutorState::Unbinding,
            6 => ExecutorState::Releasing,
            7 => ExecutorState::Released,
            _ => ExecutorState::Unknown,
        }
    }
}

impl From<rpc::TaskResult> for TaskResult {
    fn from(result: rpc::TaskResult) -> Self {
        let state = if result.return_code != 0 {
            TaskState::Failed
        } else {
            TaskState::Succeed
        };

        Self {
            state,
            output: result.output.map(TaskOutput::from),
            message: result.message,
        }
    }
}

impl TryFrom<TaskResult> for rpc::TaskResult {
    type Error = FlameError;

    fn try_from(result: TaskResult) -> Result<Self, Self::Error> {
        let return_code = match result.state {
            TaskState::Failed => -1,
            TaskState::Succeed => 0,
            _ => {
                return Err(FlameError::InvalidState(format!(
                    "invalid task state: {:?}",
                    result.state
                )))
            }
        };

        Ok(Self {
            return_code,
            output: result.output.map(TaskOutput::into),
            message: result.message,
        })
    }
}

struct RoleObjectSelector<'a> {
    kind: &'static str,
    object_id: &'a str,
}

impl<'a> TryFrom<&'a str> for RoleObjectSelector<'a> {
    type Error = Status;

    fn try_from(value: &'a str) -> Result<Self, Self::Error> {
        let (kind, object_id) = if value == ALL {
            (ALL, ALL)
        } else if let Some(id) = value.strip_prefix(APPLICATION_PREFIX) {
            (object_kind::APPLICATION, id)
        } else if let Some(id) = value.strip_prefix(NODE_PREFIX) {
            (object_kind::NODE, id)
        } else {
            return Err(Status::invalid_argument("invalid role object ID"));
        };
        if !valid_role_object(kind, object_id) {
            return Err(Status::invalid_argument("invalid role object ID"));
        }
        Ok(Self { kind, object_id })
    }
}

impl TryFrom<rpc::Role> for Role {
    type Error = Status;

    fn try_from(value: rpc::Role) -> Result<Self, Self::Error> {
        if !valid_role_name(&value.name) {
            return Err(Status::invalid_argument("invalid role name"));
        }
        if value.users.iter().any(|user| !valid_role_subject(user)) {
            return Err(Status::invalid_argument("invalid role user ID"));
        }
        let mut rules: HashMap<String, Vec<RoleRule>> = HashMap::new();
        for rule in value.rules {
            if rule.objects.is_empty() {
                return Err(Status::invalid_argument("role rule requires an object"));
            }
            for object in rule.objects {
                let selector = RoleObjectSelector::try_from(object.as_str())?;
                rules
                    .entry(selector.kind.to_string())
                    .or_default()
                    .push(RoleRule {
                        object_id: selector.object_id.to_string(),
                        operations: rule.verbs.clone(),
                    });
            }
        }
        Ok(Self {
            name: value.name,
            users: value.users,
            rules,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::apis::operation;

    #[test]
    fn role_object_selector_parses_rpc_forms() {
        for (input, kind, object_id) in [
            (ALL, ALL, ALL),
            ("node:*", object_kind::NODE, ALL),
            ("node:node-a", object_kind::NODE, "node-a"),
            ("application:nodes", object_kind::APPLICATION, "nodes"),
        ] {
            let selector = RoleObjectSelector::try_from(input).unwrap();
            assert_eq!((selector.kind, selector.object_id), (kind, object_id));
        }
        for input in [
            "application:",
            "application:nested/name",
            "app:nodes",
            "plain-app",
            "node:",
            "nodes",
        ] {
            assert!(RoleObjectSelector::try_from(input).is_err());
        }
    }

    #[test]
    fn role_rpc_round_trip_keeps_each_objects_operations() {
        let rpc_role = rpc::Role {
            name: "reader".to_string(),
            users: vec!["alice".to_string()],
            rules: vec![
                rpc::RoleRule {
                    verbs: vec![operation::VIEW.to_string()],
                    objects: vec!["application:app-a".to_string(), "node:node-a".to_string()],
                },
                rpc::RoleRule {
                    verbs: vec![operation::DELETE.to_string()],
                    objects: vec!["application:app-b".to_string(), "node:*".to_string()],
                },
            ],
        };
        let role = Role::try_from(rpc_role).unwrap();
        assert_eq!(role.rules[object_kind::APPLICATION].len(), 2);
        assert_eq!(role.rules[object_kind::NODE].len(), 2);
        assert_eq!(
            role.rules[object_kind::APPLICATION][0],
            RoleRule {
                object_id: "app-a".to_string(),
                operations: vec![operation::VIEW.to_string()],
            }
        );
        assert_eq!(role.rules[object_kind::NODE][1].object_id, ALL);
        assert_eq!(Role::try_from(rpc::Role::from(role.clone())).unwrap(), role);
    }

    #[test]
    fn role_rpc_rejects_invalid_object_selectors() {
        for object in [
            "application:",
            "application:*",
            "app:app-a",
            "nodes",
            "unknown:target",
        ] {
            let role = rpc::Role {
                name: "invalid".to_string(),
                users: vec!["alice".to_string()],
                rules: vec![rpc::RoleRule {
                    verbs: vec![operation::VIEW.to_string()],
                    objects: vec![object.to_string()],
                }],
            };
            assert_eq!(
                Role::try_from(role).unwrap_err().code(),
                tonic::Code::InvalidArgument
            );
        }
    }

    #[test]
    fn role_rpc_rejects_invalid_name_subject_and_empty_rule() {
        let valid = rpc::Role {
            name: "reader".to_string(),
            users: vec!["alice".to_string()],
            rules: vec![rpc::RoleRule {
                verbs: vec![operation::VIEW.to_string()],
                objects: vec!["application:app-a".to_string()],
            }],
        };
        let invalid_roles = [
            rpc::Role {
                name: String::new(),
                ..valid.clone()
            },
            rpc::Role {
                name: "../outside".to_string(),
                ..valid.clone()
            },
            rpc::Role {
                name: ".hidden".to_string(),
                ..valid.clone()
            },
            rpc::Role {
                users: vec!["system:cache:cache-a".to_string()],
                ..valid.clone()
            },
            rpc::Role {
                rules: vec![rpc::RoleRule {
                    objects: vec![],
                    ..valid.rules[0].clone()
                }],
                ..valid
            },
        ];
        for role in invalid_roles {
            assert_eq!(
                Role::try_from(role).unwrap_err().code(),
                tonic::Code::InvalidArgument
            );
        }
    }
}
