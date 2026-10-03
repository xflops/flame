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

use super::{ApplicationState, ExecutorState, SessionGID, SessionState, TaskState};
use crate::FlameError;
use rpc::flame::v1 as rpc;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SessionPredicate {
    Ready,
}

impl SessionPredicate {
    pub fn matches(self, retry_count: u32, retry_limits: u32) -> bool {
        match self {
            Self::Ready => retry_count < retry_limits,
        }
    }
}

/// Filter for tasks owned by one session.
pub struct TaskFilter {
    /// Owning session, including its workspace.
    pub session: SessionGID,
    /// Task states to include. `None` matches every state.
    pub states: Option<Vec<TaskState>>,
}

impl TaskFilter {
    /// Returns the owning session's scoped name.
    pub fn session(&self) -> &SessionGID {
        &self.session
    }

    /// Creates a filter for every task in one session.
    pub fn new(session: SessionGID) -> Self {
        Self {
            session,
            states: None,
        }
    }

    pub fn by_state(self, state: TaskState) -> Self {
        self.by_states(vec![state])
    }

    pub fn by_states(mut self, states: impl Into<Vec<TaskState>>) -> Self {
        self.states = Some(states.into());
        self
    }
}

/// Filter for listing sessions within one required workspace.
/// Optional criteria are combined with AND; empty names match no sessions.
pub struct SessionFilter {
    pub workspace: String,
    pub application: Option<String>,
    pub state: Option<SessionState>,
    pub names: Option<Vec<String>>,
    pub predicate: Option<SessionPredicate>,
    pub limit: Option<usize>,
}

impl SessionFilter {
    /// Returns explicitly named sessions; an empty vector matches no sessions.
    pub fn session(&self) -> Option<Vec<SessionGID>> {
        Some(
            self.names
                .as_ref()?
                .iter()
                .map(|name| SessionGID::new(&self.workspace, name))
                .collect(),
        )
    }

    /// Creates a filter for all sessions in one workspace.
    pub fn new(workspace: impl Into<String>) -> Self {
        Self {
            workspace: workspace.into(),
            application: None,
            state: None,
            names: None,
            predicate: None,
            limit: None,
        }
    }

    /// Restricts the filter to one state.
    pub fn by_state(mut self, state: SessionState) -> Self {
        self.state = Some(state);
        self
    }

    /// Restricts the filter to the provided local names.
    pub fn by_names(mut self, names: Vec<String>) -> Self {
        self.names = Some(names);
        self
    }

    /// Restricts the filter to one application in its workspace.
    pub fn by_application(mut self, application: impl Into<String>) -> Self {
        self.application = Some(application.into());
        self
    }

    pub const fn with_predicate(mut self, predicate: SessionPredicate) -> Self {
        self.predicate = Some(predicate);
        self
    }

    pub const fn with_limit(mut self, limit: usize) -> Self {
        self.limit = Some(limit);
        self
    }
}

impl TryFrom<rpc::ListSessionsRequest> for SessionFilter {
    type Error = FlameError;

    fn try_from(request: rpc::ListSessionsRequest) -> Result<Self, Self::Error> {
        let workspace = request.workspace.ok_or_else(|| {
            FlameError::InvalidConfig("workspace is required when listing sessions".to_string())
        })?;
        Ok(Self {
            workspace,
            application: request.application,
            state: request.state.map(SessionState::try_from).transpose()?,
            names: None,
            predicate: None,
            limit: None,
        })
    }
}

/// Filter for listing executors.
/// All fields are Option:
/// - `None` = ignore this filter (match all)
/// - `Some(value)` = match exactly (empty vec/string matches nothing)
pub struct ExecutorFilter {
    /// Filter by executor state
    pub state: Option<ExecutorState>,
    /// Filter by executor names
    pub names: Option<Vec<String>>,
    /// Filter by node name
    pub node: Option<String>,
}

impl ExecutorFilter {
    /// Creates a new empty filter (matches all executors).
    pub const fn new() -> Self {
        Self {
            state: None,
            names: None,
            node: None,
        }
    }

    /// Creates a filter for a specific state.
    pub const fn by_state(state: ExecutorState) -> Self {
        Self {
            state: Some(state),
            names: None,
            node: None,
        }
    }

    /// Creates a filter for a specific node.
    pub fn by_node(node: impl Into<String>) -> Self {
        Self {
            state: None,
            names: None,
            node: Some(node.into()),
        }
    }

    /// Creates a filter for specific executor names.
    pub fn by_names(names: Vec<String>) -> Self {
        Self {
            state: None,
            names: Some(names),
            node: None,
        }
    }
}

impl Default for ExecutorFilter {
    fn default() -> Self {
        Self::new()
    }
}

pub const IDLE_EXECUTOR: Option<ExecutorFilter> =
    Some(ExecutorFilter::by_state(ExecutorState::Idle));
pub const VOID_EXECUTOR: Option<ExecutorFilter> =
    Some(ExecutorFilter::by_state(ExecutorState::Void));
pub const UNBINDING_EXECUTOR: Option<ExecutorFilter> =
    Some(ExecutorFilter::by_state(ExecutorState::Unbinding));
pub const BOUND_EXECUTOR: Option<ExecutorFilter> =
    Some(ExecutorFilter::by_state(ExecutorState::Bound));
pub const BINDING_EXECUTOR: Option<ExecutorFilter> =
    Some(ExecutorFilter::by_state(ExecutorState::Binding));

pub const ALL_EXECUTOR: Option<ExecutorFilter> = None;

/// Filter for applications within one required workspace.
pub struct ApplicationFilter {
    pub workspace: String,
    pub state: Option<ApplicationState>,
}

impl ApplicationFilter {
    pub fn new(workspace: impl Into<String>) -> Self {
        Self {
            workspace: workspace.into(),
            state: None,
        }
    }

    pub fn by_state(mut self, state: ApplicationState) -> Self {
        self.state = Some(state);
        self
    }
}

impl TryFrom<rpc::ListApplicationsRequest> for ApplicationFilter {
    type Error = FlameError;

    fn try_from(request: rpc::ListApplicationsRequest) -> Result<Self, Self::Error> {
        let workspace = request.workspace.ok_or_else(|| {
            FlameError::InvalidConfig("workspace is required when listing applications".to_string())
        })?;
        Ok(Self {
            workspace,
            state: request.state.map(ApplicationState::try_from).transpose()?,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn task_and_application_builders_preserve_scope() {
        let session = SessionGID::new("research", "run");
        let task_filter = TaskFilter::new(session.clone()).by_state(TaskState::Pending);
        assert_eq!(task_filter.session(), &session);
        assert_eq!(task_filter.states, Some(vec![TaskState::Pending]));
        let task_filter = task_filter.by_states(vec![TaskState::Running, TaskState::Pending]);
        assert_eq!(task_filter.session(), &session);
        assert_eq!(
            task_filter.states,
            Some(vec![TaskState::Running, TaskState::Pending])
        );
        let app_filter = ApplicationFilter::new("research").by_state(ApplicationState::Disabled);
        assert_eq!(app_filter.workspace, "research");
        assert_eq!(app_filter.state, Some(ApplicationState::Disabled));
        assert!(ApplicationFilter::try_from(rpc::ListApplicationsRequest::default()).is_err());
    }

    #[test]
    fn session_filter_builders_preserve_workspace_and_other_criteria() {
        let filter = SessionFilter::new("research")
            .by_state(SessionState::Open)
            .by_names(vec!["run".to_string()])
            .by_application("service")
            .with_limit(2);
        assert_eq!(filter.workspace, "research");
        assert_eq!(filter.state, Some(SessionState::Open));
        assert_eq!(filter.application.as_deref(), Some("service"));
        assert_eq!(filter.limit, Some(2));
        assert_eq!(
            filter.session(),
            Some(vec![SessionGID::new("research", "run")])
        );
    }

    #[test]
    fn session_filter_rpc_requires_workspace() {
        assert!(SessionFilter::try_from(rpc::ListSessionsRequest::default()).is_err());
        let filter = SessionFilter::try_from(rpc::ListSessionsRequest {
            workspace: Some("research".to_string()),
            ..Default::default()
        })
        .unwrap();
        assert_eq!(filter.workspace, "research");
    }

    #[test]
    fn session_filter_preserves_multiple_names_in_required_workspace() {
        let mut filter = SessionFilter::new("research");
        assert_eq!(filter.session(), None);
        filter.names = Some(Vec::new());
        assert_eq!(filter.session(), Some(Vec::new()));
        filter.names = Some(vec!["first".to_string()]);
        assert_eq!(
            filter.session(),
            Some(vec![SessionGID::new("research", "first")])
        );
        filter.names.as_mut().unwrap().push("second".to_string());
        assert_eq!(
            filter.session(),
            Some(vec![
                SessionGID::new("research", "first"),
                SessionGID::new("research", "second"),
            ])
        );
    }
}
