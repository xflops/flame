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

//! Validated offset pagination shared by memory and persistent collections.
use super::Storage;
use crate::apis::*;
use crate::FlameError;
use std::cmp::Ordering;
use std::collections::BTreeMap;
use stdng::lock_ptr;

#[derive(Clone, Debug, Eq, PartialEq, Ord, PartialOrd)]
pub(crate) struct PageKey {
    pub time: i64,
    pub number: i64,
    pub id: String,
}
impl PageKey {
    pub fn text(id: &str, time: i64) -> Self {
        Self {
            time,
            number: 0,
            id: id.into(),
        }
    }
    pub fn task(id: TaskID, time: i64) -> Self {
        Self {
            time,
            number: id,
            id: String::new(),
        }
    }
}

#[derive(Clone, Debug)]
pub(crate) struct PageQuery {
    pub limit: usize,
    pub creation: bool,
    pub descending: bool,
    pub offset: i64,
    pub page_number: i64,
}
fn invalid(message: &str) -> FlameError {
    FlameError::InvalidConfig(message.into())
}

impl PageQuery {
    /// Validate settings before taking collection locks or querying an engine.
    fn new(resource: &str, request: &PageRequest) -> Result<Self, FlameError> {
        if request.page_size < 0 || request.page_size > 1000 {
            return Err(invalid("page_size must be between 0 and 1000"));
        }
        let default = match resource {
            "sessions" | "tasks" => "creation_time desc",
            "executors" => "id asc",
            _ => "name asc",
        };
        let input = if request.order_by.trim().is_empty() {
            default
        } else {
            request.order_by.trim()
        };
        let fields = input.split_whitespace().collect::<Vec<_>>();
        if fields.len() > 2 {
            return Err(invalid("invalid order_by"));
        }
        let creation = fields[0] == "creation_time";
        if !(fields[0] == "id"
            || fields[0] == "name"
            || (creation && matches!(resource, "applications" | "sessions" | "tasks")))
        {
            return Err(invalid("unsupported order_by field"));
        }
        let descending = match fields.get(1).copied().unwrap_or("asc") {
            "asc" => false,
            "desc" => true,
            _ => return Err(invalid("order_by direction must be asc or desc")),
        };
        let limit = if request.page_size == 0 {
            100
        } else {
            request.page_size as usize
        };
        if request.page_number < 0 {
            return Err(invalid("page_number must be nonnegative"));
        }
        let page_number = if request.page_number == 0 {
            1
        } else {
            request.page_number
        };
        let offset = (page_number - 1)
            .checked_mul(limit as i64)
            .ok_or_else(|| invalid("page offset is too large"))?;
        Ok(Self {
            limit,
            creation,
            descending,
            offset,
            page_number,
        })
    }
    pub fn text_key(&self, id: &str, timestamp: i64) -> PageKey {
        PageKey::text(id, if self.creation { timestamp } else { 0 })
    }
    pub fn task_key(&self, id: TaskID, timestamp: i64) -> PageKey {
        PageKey::task(id, if self.creation { timestamp } else { 0 })
    }
    pub fn compare(&self, left: &PageKey, right: &PageKey) -> Ordering {
        if self.descending {
            right.cmp(left)
        } else {
            left.cmp(right)
        }
    }
    fn finish<T>(&self, result: PageResult<T>) -> Page<T> {
        let mut paging = result.info;
        paging.page_number = self.page_number;
        paging.page_size = self.limit as i32;
        paging.total_pages = paging.total_count.div_ceil(self.limit as u64);
        Page {
            items: result.items,
            paging,
        }
    }
}

/// Selects the ordered metadata prefix, then skips the offset before payload loading.
pub(crate) struct PageCollector<T> {
    pub info: PageInfo,
    entries: BTreeMap<PageKey, T>,
}
pub(crate) struct PageResult<T> {
    pub items: Vec<T>,
    pub info: PageInfo,
}
impl<T> PageCollector<T> {
    pub fn new(states: &[&str]) -> Self {
        Self {
            info: PageInfo {
                state_counts: states.iter().map(|s| (s.to_string(), 0)).collect(),
                ..Default::default()
            },
            entries: BTreeMap::new(),
        }
    }
    pub fn observe(&mut self, state: impl ToString) {
        self.info.resource_count += 1;
        *self.info.state_counts.entry(state.to_string()).or_default() += 1;
    }
    pub fn offer(&mut self, query: &PageQuery, key: PageKey, item: impl FnOnce() -> T) {
        self.info.total_count += 1;
        let retained = query.offset as u64 + query.limit as u64;
        if self.entries.len() as u64 >= retained {
            let boundary = if query.descending {
                self.entries.first_key_value().unwrap().0
            } else {
                self.entries.last_key_value().unwrap().0
            };
            if query.compare(&key, boundary) != Ordering::Less {
                return;
            }
        }
        self.entries.insert(key, item());
        if self.entries.len() as u64 > retained {
            if query.descending {
                self.entries.pop_first();
            } else {
                self.entries.pop_last();
            }
        }
    }
    pub fn finish(self, query: &PageQuery) -> PageResult<T> {
        let mut pairs = self.entries.into_iter().collect::<Vec<_>>();
        if query.descending {
            pairs.reverse();
        }
        PageResult {
            items: pairs
                .into_iter()
                .skip(usize::try_from(query.offset).unwrap_or(usize::MAX))
                .take(query.limit)
                .map(|(_, item)| item)
                .collect(),
            info: self.info,
        }
    }
}
pub(crate) fn normalized<T: Ord + Clone>(values: &[T]) -> Vec<T> {
    let mut values = values.to_vec();
    values.sort();
    values.dedup();
    values
}
pub(crate) fn session_metadata(value: &Session) -> Session {
    Session {
        id: value.id.clone(),
        application: value.application.clone(),
        version: value.version,
        common_data: value.common_data.clone(),
        tokens: value.tokens.clone(),
        tasks: Default::default(),
        task_counts: value.task_counts,
        tasks_index: Default::default(),
        creation_time: value.creation_time,
        completion_time: value.completion_time,
        events: Vec::new(),
        status: value.status.clone(),
        min_instances: value.min_instances,
        max_instances: value.max_instances,
        batch_size: value.batch_size,
        priority: value.priority,
        resreq: value.resreq.clone(),
        retry_count: value.retry_count,
    }
}
pub(crate) fn task_metadata(value: &Task) -> Task {
    Task {
        id: value.id,
        ssn_id: value.ssn_id.clone(),
        version: value.version,
        input: None,
        output: None,
        affinity: Default::default(),
        creation_time: value.creation_time,
        completion_time: value.completion_time,
        events: Vec::new(),
        state: value.state,
    }
}

impl Storage {
    /// Clone domain records only after bounded metadata selection is complete.
    fn memory_page<T: Clone>(
        &self,
        query: &PageQuery,
        result: PageResult<stdng::MutexPtr<T>>,
    ) -> Result<Page<T>, FlameError> {
        let items = result
            .items
            .iter()
            .map(|ptr| Ok((*lock_ptr!(ptr)?).clone()))
            .collect::<Result<Vec<_>, FlameError>>()?;
        Ok(query.finish(PageResult {
            items,
            info: result.info,
        }))
    }
    /// List live application metadata without cloning records outside the page.
    pub async fn page_applications(
        &self,
        request: &PageRequest,
        filter: &ApplicationPageFilter,
    ) -> Result<Page<Application>, FlameError> {
        let names = normalized(&filter.names);
        let query = PageQuery::new("applications", request)?;
        let mut selected = PageCollector::new(&["Enabled", "Disabled"]);
        let directory = lock_ptr!(self.applications)?;
        for ptr in directory.values() {
            let item = lock_ptr!(ptr)?;
            selected.observe(item.state);
            if filter.state.is_none_or(|state| state == item.state)
                && (names.is_empty() || names.binary_search(&item.name).is_ok())
            {
                selected.offer(
                    &query,
                    query.text_key(&item.name, item.creation_time.timestamp()),
                    || ptr.clone(),
                );
            }
        }
        self.memory_page(&query, selected.finish(&query))
    }
    /// List live node inventory with capacity totals for the complete scope.
    pub async fn page_nodes(
        &self,
        request: &PageRequest,
        filter: &NodePageFilter,
    ) -> Result<Page<Node>, FlameError> {
        let names = normalized(&filter.names);
        let query = PageQuery::new("nodes", request)?;
        let mut selected = PageCollector::new(&["Unknown", "Ready", "NotReady"]);
        let directory = lock_ptr!(self.nodes)?;
        for ptr in directory.values() {
            let item = lock_ptr!(ptr)?;
            selected.observe(item.state);
            *selected.info.totals.entry("cpu".into()).or_default() += item.capacity.cpu;
            *selected.info.totals.entry("memory".into()).or_default() += item.capacity.memory;
            *selected.info.totals.entry("gpu".into()).or_default() +=
                item.capacity.gpu.max(0) as u64;
            if filter.state.is_none_or(|state| state == item.state)
                && (names.is_empty() || names.binary_search(&item.name).is_ok())
            {
                selected.offer(&query, query.text_key(&item.name, 0), || ptr.clone());
            }
        }
        self.memory_page(&query, selected.finish(&query))
    }
    /// List live executors with exact ownership filters.
    pub async fn page_executors(
        &self,
        request: &PageRequest,
        filter: &ExecutorPageFilter,
    ) -> Result<Page<Executor>, FlameError> {
        let ids = normalized(&filter.ids);
        let query = PageQuery::new("executors", request)?;
        let states = (0..=7)
            .map(|state| {
                rpc::flame::v1::ExecutorState::try_from(state).map(|state| state.as_str_name())
            })
            .collect::<Result<Vec<_>, _>>()?;
        let mut selected = PageCollector::new(&states);
        let directory = lock_ptr!(self.executors)?;
        for ptr in directory.values() {
            let item = lock_ptr!(ptr)?;
            selected
                .observe(rpc::flame::v1::ExecutorState::try_from(item.state as i32)?.as_str_name());
            if filter.state.is_none_or(|s| s == item.state)
                && filter
                    .application
                    .as_ref()
                    .is_none_or(|s| s == &item.application)
                && filter.node.as_ref().is_none_or(|s| s == &item.node)
                && filter
                    .session_id
                    .as_ref()
                    .is_none_or(|s| Some(s) == item.ssn_id.as_ref())
                && (ids.is_empty() || ids.binary_search(&item.id).is_ok())
            {
                selected.offer(&query, query.text_key(&item.id, 0), || ptr.clone());
            }
        }
        self.memory_page(&query, selected.finish(&query))
    }
    /// Query persisted session metadata, including closed sessions evicted from memory.
    pub async fn page_sessions(
        &self,
        request: &PageRequest,
        filter: &SessionPageFilter,
    ) -> Result<Page<Session>, FlameError> {
        let mut filter = filter.clone();
        filter.ids = normalized(&filter.ids);
        let query = PageQuery::new("sessions", request)?;
        let mut result = self.engine.page_sessions(&query, &filter).await?;
        for session in &mut result.items {
            session.events = super::event_result(
                self.event_manager
                    .find_events(EventOwner::session(session.id.clone())),
                "load",
                &session.id,
            );
        }
        let mut running = 0;
        {
            let directory = lock_ptr!(self.sessions)?;
            for ptr in directory.values() {
                let session = lock_ptr!(ptr)?;
                if !session.is_closed() {
                    running += session.task_counts.running;
                }
            }
        }
        result.info.totals.insert("running_tasks".into(), running);
        Ok(query.finish(result))
    }
    /// Query authoritative task records and hydrate payloads only when requested.
    pub async fn page_tasks(
        &self,
        request: &PageRequest,
        filter: &TaskPageFilter,
    ) -> Result<Page<Task>, FlameError> {
        let mut filter = filter.clone();
        filter.ids = normalized(&filter.ids);
        let query = PageQuery::new("tasks", request)?;
        let mut result = self.engine.page_tasks(&query, &filter).await?;
        if filter.include_payloads {
            for task in &mut result.items {
                task.events = super::event_result(
                    self.event_manager.find_events(EventOwner::from(task.gid())),
                    "load",
                    &task.ssn_id,
                );
            }
        }
        Ok(query.finish(result))
    }
}

pub(crate) fn session_matches(
    filter: &SessionPageFilter,
    id: &str,
    application: &str,
    state: SessionState,
) -> bool {
    filter.state.is_none_or(|s| s == state)
        && filter.application.as_ref().is_none_or(|s| s == application)
        && (filter.ids.is_empty() || filter.ids.binary_search_by(|s| s.as_str().cmp(id)).is_ok())
}
pub(crate) fn task_matches(filter: &TaskPageFilter, id: TaskID, state: TaskState) -> bool {
    filter.state.is_none_or(|s| s == state)
        && (filter.ids.is_empty() || filter.ids.binary_search(&id).is_ok())
}
