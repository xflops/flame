/*
Copyright 2026 The Flame Authors.
Licensed under the Apache License, Version 2.0 (the "License");
you may not use this file except in compliance with the License.
You may obtain a copy of the License at http://www.apache.org/licenses/LICENSE-2.0
Unless required by applicable law or agreed to in writing, software distributed
under the License is distributed on an "AS IS" BASIS, WITHOUT WARRANTIES OR
CONDITIONS OF ANY KIND, either express or implied. See the License for the
specific language governing permissions and limitations under the License.
*/

use serde_derive::{Deserialize, Serialize};

use super::{
    rpc, Application, Connection, Executor, FlameClient, Node, NodeState, Session, SessionFilter,
    Task,
};
use crate::apis::{ApplicationState, ExecutorState, FlameError, SessionID, TaskState};

/// Request one page; zero limit/number select the defaults. Responses include total.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Paging {
    pub limit: i32,
    pub number: i64,
    pub total: Option<u64>,
}

impl From<Paging> for rpc::Paging {
    fn from(value: Paging) -> Self {
        Self {
            limit: value.limit,
            number: value.number,
            total: value.total,
        }
    }
}
impl From<rpc::Paging> for Paging {
    fn from(value: rpc::Paging) -> Self {
        Self {
            limit: value.limit,
            number: value.number,
            total: value.total,
        }
    }
}
/// A bounded list of resource metadata with its effective page and filtered count.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Page<T> {
    pub items: Vec<T>,
    pub paging: Paging,
}
impl<T> std::ops::Deref for Page<T> {
    type Target = [T];
    fn deref(&self) -> &[T] {
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
impl<T> Page<T> {
    fn new(items: Vec<T>, paging: Option<rpc::Paging>) -> Result<Self, FlameError> {
        let paging: Paging = paging
            .ok_or_else(|| invalid_paging("missing paging"))?
            .into();
        if paging.total.is_none() {
            return Err(invalid_paging("missing total"));
        }
        if !(1..=1000).contains(&paging.limit) || paging.number < 1 {
            return Err(invalid_paging("invalid effective limit or number"));
        }
        Ok(Self { items, paging })
    }
}
fn invalid_paging(message: &str) -> FlameError {
    FlameError::Network(format!("invalid list response paging: {message}"))
}

/// Exact application filters; empty names select every name.
#[derive(Clone, Debug, Default)]
pub struct ApplicationFilter {
    pub paging: Option<Paging>,
    pub names: Vec<String>,
    pub state: Option<ApplicationState>,
}
/// Exact node filters; empty names select every name.
#[derive(Clone, Debug, Default)]
pub struct NodeFilter {
    pub paging: Option<Paging>,
    pub names: Vec<String>,
    pub state: Option<NodeState>,
}
/// Exact executor assignment and lifecycle filters.
#[derive(Clone, Debug, Default)]
pub struct ExecutorFilter {
    pub paging: Option<Paging>,
    pub ids: Vec<String>,
    pub node: Option<String>,
    pub application: Option<String>,
    pub session_id: Option<String>,
    pub state: Option<ExecutorState>,
}
/// Task metadata filters within one session; empty vectors are unrestricted.
#[derive(Clone, Debug, Default)]
pub struct TaskFilter {
    pub paging: Option<Paging>,
    pub ids: Vec<i64>,
    pub states: Vec<TaskState>,
}

impl Connection {
    /// List one applications metadata page using reusable exact filters and paging.
    pub async fn list_applications(
        &self,
        filter: Option<&ApplicationFilter>,
    ) -> Result<Page<Application>, FlameError> {
        let filter = filter.cloned().unwrap_or_default();
        let response = FlameClient::new(self.channel.clone())
            .list_applications(rpc::ListApplicationsRequest {
                paging: filter.paging.map(Into::into),
                names: filter.names.clone(),
                state: filter.state.map(|s| s as i32),
            })
            .await?
            .into_inner();
        Page::new(
            response
                .applications
                .iter()
                .map(Application::try_from)
                .collect::<Result<_, _>>()?,
            response.paging,
        )
    }
    /// List one nodes metadata page using reusable exact filters and paging.
    pub async fn list_nodes(&self, filter: Option<&NodeFilter>) -> Result<Page<Node>, FlameError> {
        let filter = filter.cloned().unwrap_or_default();
        let response = FlameClient::new(self.channel.clone())
            .list_nodes(rpc::ListNodesRequest {
                paging: filter.paging.map(Into::into),
                names: filter.names.clone(),
                state: filter.state.map(|s| s as i32),
            })
            .await?
            .into_inner();
        Page::new(
            response.nodes.iter().map(Node::from).collect(),
            response.paging,
        )
    }
    /// List one executors metadata page using reusable exact filters and paging.
    pub async fn list_executors(
        &self,
        filter: Option<&ExecutorFilter>,
    ) -> Result<Page<Executor>, FlameError> {
        let filter = filter.cloned().unwrap_or_default();
        let response = FlameClient::new(self.channel.clone())
            .list_executors(rpc::ListExecutorsRequest {
                paging: filter.paging.map(Into::into),
                ids: filter.ids.clone(),
                node: filter.node.clone(),
                application: filter.application.clone(),
                session_id: filter.session_id.clone(),
                state: filter.state.map(|s| s as i32),
            })
            .await?
            .into_inner();
        Page::new(
            response
                .executors
                .iter()
                .map(Executor::try_from)
                .collect::<Result<_, _>>()?,
            response.paging,
        )
    }
    /// List one session metadata page; authoritative detail operations remain available on the handles.
    pub async fn list_sessions(
        &self,
        filter: Option<&SessionFilter>,
    ) -> Result<Page<Session>, FlameError> {
        let mut client = FlameClient::new(self.channel.clone());
        let response = client
            .list_sessions(filter.map(SessionFilter::to_request).unwrap_or_default())
            .await?
            .into_inner();
        Page::new(
            response
                .sessions
                .iter()
                .map(|s| self.attach_session(s, client.clone()))
                .collect::<Result<_, _>>()?,
            response.paging,
        )
    }
    /// List task metadata by session ID without a separate GetSession round trip.
    pub async fn list_tasks(
        &self,
        session_id: &SessionID,
        filter: Option<&TaskFilter>,
    ) -> Result<Page<Task>, FlameError> {
        task_page(
            &mut FlameClient::new(self.channel.clone()),
            session_id,
            filter,
        )
        .await
    }
}

impl Session {
    /// List one task metadata page using reusable filters; getters and watches retain payload access.
    pub async fn list_tasks(&self, filter: Option<&TaskFilter>) -> Result<Page<Task>, FlameError> {
        let mut client = self
            .client
            .clone()
            .ok_or_else(|| FlameError::Internal("no flame client".into()))?;
        task_page(&mut client, &self.id, filter).await
    }
}
async fn task_page(
    client: &mut FlameClient,
    session_id: &SessionID,
    filter: Option<&TaskFilter>,
) -> Result<Page<Task>, FlameError> {
    let filter = filter.cloned().unwrap_or_default();
    let response = client
        .list_tasks(rpc::ListTasksRequest {
            session_id: session_id.clone(),
            paging: filter.paging.map(Into::into),
            ids: filter.ids.clone(),
            states: filter.states.iter().map(|s| *s as i32).collect(),
        })
        .await?
        .into_inner();
    Page::new(
        response
            .tasks
            .iter()
            .map(Task::try_from)
            .collect::<Result<_, _>>()?,
        response.paging,
    )
}
#[cfg(test)]
#[path = "tests/pagination.rs"]
mod tests;
