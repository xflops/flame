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

use super::{
    ApplicationID, ApplicationState, ExecutorID, ExecutorState, SessionID, SessionState, TaskState,
};
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
    /// Owning session.
    pub session: SessionID,
    /// Task states to include. `None` matches every state.
    pub states: Option<Vec<TaskState>>,
}

impl TaskFilter {
    /// Creates a filter for every task in a session.
    pub fn by_session(session: impl Into<SessionID>) -> Self {
        Self {
            session: session.into(),
            states: None,
        }
    }

    /// Creates a filter for tasks in any of the provided states.
    pub fn by_session_states(
        session: impl Into<SessionID>,
        states: impl Into<Vec<TaskState>>,
    ) -> Self {
        Self {
            session: session.into(),
            states: Some(states.into()),
        }
    }

    /// Creates a filter for non-terminal tasks in a session.
    pub fn non_terminal(session: impl Into<SessionID>) -> Self {
        Self::by_session_states(session, vec![TaskState::Pending, TaskState::Running])
    }
}

/// Filter for listing sessions.
/// All fields are Option:
/// - `None` = ignore this filter (match all)
/// - `Some(value)` = match exactly (empty vec matches nothing)
pub struct SessionFilter {
    /// Filter by owning application
    pub application: Option<ApplicationID>,
    /// Filter by session state
    pub state: Option<SessionState>,
    /// Filter by session IDs
    pub ids: Option<Vec<SessionID>>,
    /// Additional in-memory predicate filter.
    pub predicate: Option<SessionPredicate>,
    /// Maximum number of matching sessions to return.
    pub limit: Option<usize>,
}

impl SessionFilter {
    /// Creates a new empty filter (matches all sessions).
    pub const fn new() -> Self {
        Self {
            application: None,
            state: None,
            ids: None,
            predicate: None,
            limit: None,
        }
    }

    /// Creates a filter for a specific state.
    pub const fn by_state(state: SessionState) -> Self {
        Self {
            application: None,
            state: Some(state),
            ids: None,
            predicate: None,
            limit: None,
        }
    }

    /// Creates a filter for specific session IDs.
    pub fn by_ids(ids: Vec<SessionID>) -> Self {
        Self {
            application: None,
            state: None,
            ids: Some(ids),
            predicate: None,
            limit: None,
        }
    }

    /// Creates a filter for an application's sessions.
    pub fn by_application(application: impl Into<ApplicationID>) -> Self {
        Self {
            application: Some(application.into()),
            state: None,
            ids: None,
            predicate: None,
            limit: None,
        }
    }

    /// Creates a filter for an application's sessions in a specific state.
    pub fn by_application_state(
        application: impl Into<ApplicationID>,
        state: SessionState,
    ) -> Self {
        Self {
            application: Some(application.into()),
            state: Some(state),
            ids: None,
            predicate: None,
            limit: None,
        }
    }

    /// Adds an in-memory predicate filter.
    pub const fn with_predicate(mut self, predicate: SessionPredicate) -> Self {
        self.predicate = Some(predicate);
        self
    }

    /// Limits the number of matching sessions returned.
    pub const fn with_limit(mut self, limit: usize) -> Self {
        self.limit = Some(limit);
        self
    }
}

impl Default for SessionFilter {
    fn default() -> Self {
        Self::new()
    }
}

impl TryFrom<rpc::ListSessionsRequest> for SessionFilter {
    type Error = FlameError;

    fn try_from(request: rpc::ListSessionsRequest) -> Result<Self, Self::Error> {
        Ok(Self {
            application: request.application,
            state: request.state.map(SessionState::try_from).transpose()?,
            ids: None,
            predicate: None,
            limit: None,
        })
    }
}

pub const OPEN_SESSION: Option<SessionFilter> = Some(SessionFilter::by_state(SessionState::Open));
pub const READY_SESSION: Option<SessionFilter> =
    Some(SessionFilter::by_state(SessionState::Open).with_predicate(SessionPredicate::Ready));

/// Filter for listing executors.
/// All fields are Option:
/// - `None` = ignore this filter (match all)
/// - `Some(value)` = match exactly (empty vec/string matches nothing)
pub struct ExecutorFilter {
    /// Filter by executor state
    pub state: Option<ExecutorState>,
    /// Filter by executor IDs
    pub ids: Option<Vec<ExecutorID>>,
    /// Filter by node name
    pub node: Option<String>,
}

impl ExecutorFilter {
    /// Creates a new empty filter (matches all executors).
    pub const fn new() -> Self {
        Self {
            state: None,
            ids: None,
            node: None,
        }
    }

    /// Creates a filter for a specific state.
    pub const fn by_state(state: ExecutorState) -> Self {
        Self {
            state: Some(state),
            ids: None,
            node: None,
        }
    }

    /// Creates a filter for a specific node.
    pub fn by_node(node: impl Into<String>) -> Self {
        Self {
            state: None,
            ids: None,
            node: Some(node.into()),
        }
    }

    /// Creates a filter for specific executor IDs.
    pub fn by_ids(ids: Vec<ExecutorID>) -> Self {
        Self {
            state: None,
            ids: Some(ids),
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

/// Filter for listing applications.
/// All fields are Option:
/// - `None` = ignore this filter (match all)
/// - `Some(value)` = match exactly
pub struct ApplicationFilter {
    /// Filter by application state
    pub state: Option<ApplicationState>,
}

impl ApplicationFilter {
    /// Creates a new empty filter (matches all applications).
    pub const fn new() -> Self {
        Self { state: None }
    }

    /// Creates a filter for a specific application state.
    pub const fn by_state(state: ApplicationState) -> Self {
        Self { state: Some(state) }
    }
}

impl Default for ApplicationFilter {
    fn default() -> Self {
        Self::new()
    }
}

impl TryFrom<rpc::ListApplicationsRequest> for ApplicationFilter {
    type Error = FlameError;

    fn try_from(request: rpc::ListApplicationsRequest) -> Result<Self, Self::Error> {
        Ok(Self {
            state: request.state.map(ApplicationState::try_from).transpose()?,
        })
    }
}

pub const ALL_APPLICATION: Option<ApplicationFilter> = None;
