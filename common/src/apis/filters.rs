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
    ApplicationID, ApplicationState, ExecutorID, ExecutorState, NodeState, Paging, SessionID,
    SessionState, TaskID, TaskState,
};
use crate::FlameError;
use rpc::flame::v1 as rpc;

/// One page of selected metadata and its filtered total.
#[derive(Clone, Debug)]
pub struct Page<T> {
    pub items: Vec<T>,
    pub paging: Paging,
}

impl<T> std::ops::Deref for Page<T> {
    type Target = [T];
    fn deref(&self) -> &Self::Target {
        &self.items
    }
}
impl<T> IntoIterator for Page<T> {
    type Item = T;
    type IntoIter = std::vec::IntoIter<T>;
    fn into_iter(self) -> Self::IntoIter {
        self.items.into_iter()
    }
}
impl<'a, T> IntoIterator for &'a Page<T> {
    type Item = &'a T;
    type IntoIter = std::slice::Iter<'a, T>;
    fn into_iter(self) -> Self::IntoIter {
        self.items.iter()
    }
}

/// Select a page of an already filtered and ordered metadata view.
/// Without paging, internal callers retain all matches.
pub(crate) fn paginate<T>(items: Vec<T>, paging: Option<Paging>) -> Result<Page<T>, FlameError> {
    let total = items.len() as u64;
    if let Some(paging) = paging {
        let offset = paging.offset();
        let limit = paging.limit();
        Ok(Page {
            items: items.into_iter().skip(offset).take(limit).collect(),
            paging: paging.with_total(total),
        })
    } else {
        Ok(Page {
            items,
            paging: Paging::all(total),
        })
    }
}

#[cfg(test)]
#[path = "tests/mod.rs"]
mod paging_tests;

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
    /// Exact task identities; None matches all.
    pub ids: Option<Vec<TaskID>>,
    /// Owning session.
    pub session: SessionID,
    /// Task states to include. `None` matches every state; an empty list matches none.
    pub states: Option<Vec<TaskState>>,
    /// Selects one ordered page. `None` returns all matching task metadata.
    pub paging: Option<Paging>,
}

impl TaskFilter {
    /// Creates a filter for every task in a session.
    pub fn by_session(session: impl Into<SessionID>) -> Self {
        Self {
            session: session.into(),
            ids: None,
            states: None,
            paging: None,
        }
    }

    /// Creates a filter for tasks in any of the provided states.
    pub fn by_session_states(
        session: impl Into<SessionID>,
        states: impl Into<Vec<TaskState>>,
    ) -> Self {
        Self {
            session: session.into(),
            ids: None,
            states: Some(states.into()),
            paging: None,
        }
    }

    /// Returns one ordered page of this filter.
    pub const fn with_paging(mut self, page: Paging) -> Self {
        self.paging = Some(page);
        self
    }

    /// Creates a filter for finished tasks in a session.
    pub fn terminal(session: impl Into<SessionID>) -> Self {
        Self::by_session_states(
            session,
            vec![TaskState::Succeed, TaskState::Failed, TaskState::Cancelled],
        )
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
#[derive(Clone)]
pub struct SessionFilter {
    /// Filter by owning application
    pub application: Option<ApplicationID>,
    /// Filter by session state
    pub state: Option<SessionState>,
    /// Filter by session IDs
    pub ids: Option<Vec<SessionID>>,
    /// Additional in-memory predicate filter.
    pub predicate: Option<SessionPredicate>,
    /// Selects one ordered page. `None` returns all matching records.
    pub paging: Option<Paging>,
}

impl SessionFilter {
    /// Creates a new empty filter (matches all sessions).
    pub const fn new() -> Self {
        Self {
            application: None,
            state: None,
            ids: None,
            predicate: None,
            paging: None,
        }
    }

    /// Creates a filter for a specific state.
    pub const fn by_state(state: SessionState) -> Self {
        Self {
            application: None,
            state: Some(state),
            ids: None,
            predicate: None,
            paging: None,
        }
    }

    /// Creates a filter for specific session IDs.
    pub fn by_ids(ids: Vec<SessionID>) -> Self {
        Self {
            application: None,
            state: None,
            ids: Some(ids),
            predicate: None,
            paging: None,
        }
    }

    /// Creates a filter for an application's sessions.
    pub fn by_application(application: impl Into<ApplicationID>) -> Self {
        Self {
            application: Some(application.into()),
            state: None,
            ids: None,
            predicate: None,
            paging: None,
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
            paging: None,
        }
    }

    /// Adds an in-memory predicate filter.
    pub const fn with_predicate(mut self, predicate: SessionPredicate) -> Self {
        self.predicate = Some(predicate);
        self
    }

    /// Returns one ordered page of matching metadata.
    pub const fn with_paging(mut self, page: Paging) -> Self {
        self.paging = Some(page);
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
            ids: (!request.ids.is_empty()).then_some(request.ids),
            predicate: None,
            paging: Some(Paging::try_from(request.paging)?),
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
    /// Owning application.
    pub application: Option<String>,
    /// Assigned session.
    pub session_id: Option<String>,
    /// Filter by executor state
    pub state: Option<ExecutorState>,
    /// Filter by executor IDs
    pub ids: Option<Vec<ExecutorID>>,
    /// Filter by node name
    pub node: Option<String>,
    /// Selects one ordered page. `None` returns all matching records.
    pub paging: Option<Paging>,
}

impl ExecutorFilter {
    /// Creates a new empty filter (matches all executors).
    pub const fn new() -> Self {
        Self {
            application: None,
            session_id: None,
            state: None,
            ids: None,
            node: None,
            paging: None,
        }
    }

    /// Creates a filter for a specific state.
    pub const fn by_state(state: ExecutorState) -> Self {
        Self {
            application: None,
            session_id: None,
            state: Some(state),
            ids: None,
            node: None,
            paging: None,
        }
    }

    /// Creates a filter for a specific node.
    pub fn by_node(node: impl Into<String>) -> Self {
        Self {
            application: None,
            session_id: None,
            state: None,
            ids: None,
            node: Some(node.into()),
            paging: None,
        }
    }

    /// Creates a filter for specific executor IDs.
    pub fn by_ids(ids: Vec<ExecutorID>) -> Self {
        Self {
            application: None,
            session_id: None,
            state: None,
            ids: Some(ids),
            node: None,
            paging: None,
        }
    }

    /// Returns one ordered page of matching metadata.
    pub const fn with_paging(mut self, page: Paging) -> Self {
        self.paging = Some(page);
        self
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
    /// Exact application names.
    pub names: Option<Vec<String>>,
    /// Filter by application state
    pub state: Option<ApplicationState>,
    /// Selects one ordered page. `None` returns all matching records.
    pub paging: Option<Paging>,
}

impl ApplicationFilter {
    /// Creates a new empty filter (matches all applications).
    pub const fn new() -> Self {
        Self {
            names: None,
            state: None,
            paging: None,
        }
    }

    /// Creates a filter for a specific application state.
    pub const fn by_state(state: ApplicationState) -> Self {
        Self {
            names: None,
            state: Some(state),
            paging: None,
        }
    }

    /// Returns one ordered page of matching metadata.
    pub const fn with_paging(mut self, page: Paging) -> Self {
        self.paging = Some(page);
        self
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
            names: (!request.names.is_empty()).then_some(request.names),
            state: request.state.map(ApplicationState::try_from).transpose()?,
            paging: Some(Paging::try_from(request.paging)?),
        })
    }
}

/// Filter for listing nodes. Nodes have no other selectors.
pub struct NodeFilter {
    /// Exact node names.
    pub names: Option<Vec<String>>,
    /// Node readiness state.
    pub state: Option<NodeState>,
    /// Selects one ordered page. `None` returns all matching records.
    pub paging: Option<Paging>,
}

impl NodeFilter {
    /// Creates a filter that returns every node.
    pub const fn new() -> Self {
        Self {
            names: None,
            state: None,
            paging: None,
        }
    }

    /// Returns one ordered page of matching metadata.
    pub const fn with_paging(mut self, page: Paging) -> Self {
        self.paging = Some(page);
        self
    }
}

impl Default for NodeFilter {
    fn default() -> Self {
        Self::new()
    }
}

pub const ALL_APPLICATION: Option<ApplicationFilter> = None;
