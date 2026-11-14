/*
Copyright 2026 The Flame Authors.
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

use std::collections::BTreeMap;
use std::future::Future;

use futures::{stream, Stream};
use prost::Message;
use serde_derive::{Deserialize, Serialize};

use super::{
    rpc, Application, Connection, Executor, FlameClient, Node, NodeState, Session, SessionFilter,
    Task,
};
use crate::apis::{ApplicationState, ExecutorState, FlameError, SessionID, TaskState};

/// Select a bounded page. Zero uses the server default of 100; the maximum is 1000.
/// Page numbers start at one; zero selects the first page.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct PageRequest {
    pub page_size: i32,
    pub page_number: i64,
    pub order_by: String,
}

impl From<&PageRequest> for rpc::PageRequest {
    fn from(value: &PageRequest) -> Self {
        Self {
            page_size: value.page_size,
            page_number: value.page_number,
            order_by: value.order_by.clone(),
        }
    }
}

/// Counts refer to the full filtered result and this resource's complete scope,
/// rather than just the returned items. State names use protobuf spelling.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct PageInfo {
    pub page_number: i64,
    pub page_size: i32,
    pub total_pages: u64,
    pub total_count: u64,
    pub resource_count: u64,
    pub state_counts: BTreeMap<String, u64>,
    pub totals: BTreeMap<String, u64>,
}

impl From<rpc::PageInfo> for PageInfo {
    fn from(value: rpc::PageInfo) -> Self {
        Self {
            page_number: value.page_number,
            page_size: value.page_size,
            total_pages: value.total_pages,
            total_count: value.total_count,
            resource_count: value.resource_count,
            state_counts: value.state_counts.into_iter().collect(),
            totals: value.totals.into_iter().collect(),
        }
    }
}

impl PageInfo {
    /// Refuse old servers which ignored the opt-in request and returned an unbounded list.
    fn required(value: Option<rpc::PageInfo>) -> Result<Self, FlameError> {
        value.map(Self::from).ok_or_else(unsupported_pagination)
    }
}

/// One numbered page and its filtered totals.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Page<T> {
    pub items: Vec<T>,
    pub paging: PageInfo,
}

/// Select applications by state and exact names; ordering belongs to PageRequest.
#[derive(Clone, Debug, Default)]
pub struct ApplicationFilter {
    pub state: Option<ApplicationState>,
    pub names: Vec<String>,
}

/// Select nodes by state and exact registered names.
#[derive(Clone, Debug, Default)]
pub struct NodeFilter {
    pub state: Option<NodeState>,
    pub names: Vec<String>,
}

/// Select executors using their current assignment and lifecycle state.
#[derive(Clone, Debug, Default)]
pub struct ExecutorFilter {
    pub state: Option<ExecutorState>,
    pub application: Option<String>,
    pub node: Option<String>,
    pub session_id: Option<SessionID>,
    pub ids: Vec<String>,
}

/// Select tasks within a session. Payloads are omitted unless explicitly requested.
#[derive(Clone, Debug, Default)]
pub struct TaskFilter {
    pub state: Option<TaskState>,
    pub ids: Vec<i64>,
    pub include_payloads: bool,
}

/// An unsupported server must never silently turn a bounded page call into an all-records call.
fn unsupported_pagination() -> FlameError {
    FlameError::Network("server did not return pagination metadata; paged listing requires server pagination support".into())
}

/// Fetch a bounded task stream only after the server has confirmed pagination in initial metadata.
async fn task_page(
    client: &mut FlameClient,
    session_id: &SessionID,
    filter: &TaskFilter,
    paging: &PageRequest,
) -> Result<Page<Task>, FlameError> {
    let response = client
        .list_tasks(rpc::ListTasksRequest {
            session_id: session_id.clone(),
            paging: Some(paging.into()),
            state: filter.state.map(|state| state as i32),
            ids: filter.ids.clone(),
            include_payloads: filter.include_payloads,
        })
        .await?;
    // Returning before into_inner drops/cancels the stream, without reading legacy tasks.
    let metadata = response
        .metadata()
        .get_bin("flame-page-bin")
        .ok_or_else(unsupported_pagination)?;
    let bytes = metadata.to_bytes().map_err(|_| {
        FlameError::Network("server returned invalid task pagination metadata".into())
    })?;
    let info = rpc::PageInfo::decode(bytes).map_err(|_| {
        FlameError::Network("server returned invalid task pagination metadata".into())
    })?;
    let mut tasks = response.into_inner();
    let mut items = Vec::new();
    while let Some(task) = tasks.message().await? {
        items.push(Task::try_from(&task)?);
    }
    Ok(Page {
        items,
        paging: info.into(),
    })
}

impl Connection {
    /// Retrieve one application page, preserving the legacy list method's item type.
    pub async fn list_applications_page(
        &self,
        filter: &ApplicationFilter,
        paging: &PageRequest,
    ) -> Result<Page<Application>, FlameError> {
        let response = FlameClient::new(self.channel.clone())
            .list_applications(rpc::ListApplicationsRequest {
                state: filter.state.map(|state| state as i32),
                names: filter.names.clone(),
                paging: Some(paging.into()),
            })
            .await?
            .into_inner();
        let paging = PageInfo::required(response.paging)?;
        let items = response
            .applications
            .iter()
            .map(Application::try_from)
            .collect::<Result<_, _>>()?;
        Ok(Page { items, paging })
    }

    /// Retrieve one session page, including only sessions that match the server-side filter.
    pub async fn list_sessions_page(
        &self,
        filter: &SessionFilter,
        paging: &PageRequest,
    ) -> Result<Page<Session>, FlameError> {
        let mut client = FlameClient::new(self.channel.clone());
        let mut request = filter.to_request();
        request.paging = Some(paging.into());
        let response = client.list_sessions(request).await?.into_inner();
        let paging = PageInfo::required(response.paging)?;
        let items = response
            .sessions
            .iter()
            .map(|session| self.attach_session(session, client.clone()))
            .collect::<Result<_, _>>()?;
        Ok(Page { items, paging })
    }

    /// Retrieve one node page using the registered name as its default stable order.
    pub async fn list_nodes_page(
        &self,
        filter: &NodeFilter,
        paging: &PageRequest,
    ) -> Result<Page<Node>, FlameError> {
        let response = FlameClient::new(self.channel.clone())
            .list_nodes(rpc::ListNodesRequest {
                state: filter.state.map(|state| state as i32),
                names: filter.names.clone(),
                paging: Some(paging.into()),
            })
            .await?
            .into_inner();
        let paging = PageInfo::required(response.paging)?;
        let items = response.nodes.iter().map(Node::from).collect();
        Ok(Page { items, paging })
    }

    /// Retrieve one executor page without fetching any unrelated resource lists.
    pub async fn list_executors_page(
        &self,
        filter: &ExecutorFilter,
        paging: &PageRequest,
    ) -> Result<Page<Executor>, FlameError> {
        let response = FlameClient::new(self.channel.clone())
            .list_executors(rpc::ListExecutorsRequest {
                paging: Some(paging.into()),
                state: filter.state.map(|state| state as i32),
                application: filter.application.clone(),
                node: filter.node.clone(),
                session_id: filter.session_id.clone(),
                ids: filter.ids.clone(),
            })
            .await?
            .into_inner();
        let paging = PageInfo::required(response.paging)?;
        let items = response
            .executors
            .iter()
            .map(Executor::try_from)
            .collect::<Result<_, _>>()?;
        Ok(Page { items, paging })
    }

    /// Retrieve one task page directly by session ID, without first fetching the session.
    pub async fn list_tasks_page(
        &self,
        session_id: &SessionID,
        filter: &TaskFilter,
        paging: &PageRequest,
    ) -> Result<Page<Task>, FlameError> {
        task_page(
            &mut FlameClient::new(self.channel.clone()),
            session_id,
            filter,
            paging,
        )
        .await
    }

    /// Lazily retrieve application pages; dropping the stream prevents further page requests.
    pub fn iter_application_pages(
        &self,
        filter: ApplicationFilter,
        paging: PageRequest,
    ) -> impl Stream<Item = Result<Page<Application>, FlameError>> + Send + 'static {
        let connection = self.clone();
        pages(paging, move |paging| {
            let connection = connection.clone();
            let filter = filter.clone();
            async move { connection.list_applications_page(&filter, &paging).await }
        })
    }

    /// Lazily retrieve session pages with the same filter and ordering throughout traversal.
    pub fn iter_session_pages(
        &self,
        filter: SessionFilter,
        paging: PageRequest,
    ) -> impl Stream<Item = Result<Page<Session>, FlameError>> + Send + 'static {
        let connection = self.clone();
        pages(paging, move |paging| {
            let connection = connection.clone();
            let filter = filter.clone();
            async move { connection.list_sessions_page(&filter, &paging).await }
        })
    }

    /// Lazily retrieve node pages without materializing the full resource list.
    pub fn iter_node_pages(
        &self,
        filter: NodeFilter,
        paging: PageRequest,
    ) -> impl Stream<Item = Result<Page<Node>, FlameError>> + Send + 'static {
        let connection = self.clone();
        pages(paging, move |paging| {
            let connection = connection.clone();
            let filter = filter.clone();
            async move { connection.list_nodes_page(&filter, &paging).await }
        })
    }

    /// Lazily retrieve executor pages without materializing the full resource list.
    pub fn iter_executor_pages(
        &self,
        filter: ExecutorFilter,
        paging: PageRequest,
    ) -> impl Stream<Item = Result<Page<Executor>, FlameError>> + Send + 'static {
        let connection = self.clone();
        pages(paging, move |paging| {
            let connection = connection.clone();
            let filter = filter.clone();
            async move { connection.list_executors_page(&filter, &paging).await }
        })
    }

    /// Lazily retrieve task pages for a session without an extra GetSession RPC.
    pub fn iter_task_pages(
        &self,
        session_id: SessionID,
        filter: TaskFilter,
        paging: PageRequest,
    ) -> impl Stream<Item = Result<Page<Task>, FlameError>> + Send + 'static {
        let connection = self.clone();
        pages(paging, move |paging| {
            let connection = connection.clone();
            let filter = filter.clone();
            let session_id = session_id.clone();
            async move {
                connection
                    .list_tasks_page(&session_id, &filter, &paging)
                    .await
            }
        })
    }
}

impl Session {
    /// Retrieve a bounded page from this session, rather than collecting the legacy task stream.
    pub async fn list_tasks_page(
        &self,
        filter: &TaskFilter,
        paging: &PageRequest,
    ) -> Result<Page<Task>, FlameError> {
        let mut client = self
            .client
            .clone()
            .ok_or_else(|| FlameError::Internal("no flame client".into()))?;
        task_page(&mut client, &self.id, filter, paging).await
    }
}

/// Keep continuation state in the stream so constructing it performs no network request.
fn pages<T, F, Fut>(
    paging: PageRequest,
    fetch: F,
) -> impl Stream<Item = Result<Page<T>, FlameError>> + Send
where
    T: Send,
    F: FnMut(PageRequest) -> Fut + Send,
    Fut: Future<Output = Result<Page<T>, FlameError>> + Send,
{
    stream::try_unfold((Some(paging), fetch), |(request, mut fetch)| async move {
        let Some(mut request) = request else {
            return Ok(None);
        };
        let page = fetch(request.clone()).await?;
        let next = if page.paging.page_number as u64 >= page.paging.total_pages {
            None
        } else {
            request.page_number = page
                .paging
                .page_number
                .checked_add(1)
                .ok_or_else(|| FlameError::InvalidConfig("page number overflow".into()))?;
            Some(request)
        };
        Ok(Some((page, (next, fetch))))
    })
}

#[cfg(test)]
mod tests;
