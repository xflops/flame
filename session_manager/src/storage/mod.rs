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

use chrono::Utc;
use std::collections::HashMap;
use std::ops::Deref;
use std::sync::Arc;
use uuid::Uuid;

use stdng::{lock_ptr, logs::TraceFn, trace_fn, MutexPtr};

use common::apis::{
    Application, ApplicationAttributes, ApplicationID, ApplicationPtr, CommonData, Event,
    EventOwner, ExecutorID, ExecutorState, Node, NodePtr, ResourceRequirement, Session, SessionID,
    SessionPtr, SessionState, Task, TaskGID, TaskID, TaskInput, TaskOutput, TaskPtr, TaskResult,
    TaskState,
};
use common::ctx::FlameContext;
use common::FlameError;

use crate::model::{
    AppInfo, Executor, ExecutorInfo, ExecutorPtr, NodeInfo, NodeInfoPtr, SessionInfo,
    SessionInfoPtr, SnapShot, SnapShotPtr,
};

use crate::events::{EventManager, EventManagerPtr};
use crate::storage::engine::EnginePtr;

mod engine;

pub type StoragePtr = Arc<Storage>;

#[derive(Clone)]
pub struct Storage {
    context: FlameContext,
    engine: EnginePtr,
    sessions: MutexPtr<HashMap<SessionID, SessionPtr>>,
    executors: MutexPtr<HashMap<ExecutorID, ExecutorPtr>>,
    nodes: MutexPtr<HashMap<String, NodePtr>>,
    applications: MutexPtr<HashMap<String, ApplicationPtr>>,
    event_manager: EventManagerPtr,
}

pub async fn new_ptr(config: &FlameContext) -> Result<StoragePtr, FlameError> {
    Ok(Arc::new(Storage {
        context: config.clone(),
        engine: engine::connect(&config.cluster.storage).await?,
        sessions: stdng::new_ptr(HashMap::new()),
        executors: stdng::new_ptr(HashMap::new()),
        nodes: stdng::new_ptr(HashMap::new()),
        applications: stdng::new_ptr(HashMap::new()),
        event_manager: Arc::new(EventManager::new(None)?),
    }))
}

impl Storage {
    pub fn snapshot(&self) -> Result<SnapShotPtr, FlameError> {
        let res = SnapShot::new(self.context.cluster.slot.clone());

        {
            let node_map = lock_ptr!(self.nodes)?;
            tracing::debug!("There are {} nodes in snapshot.", node_map.len());
            for node in node_map.deref().values() {
                let node = lock_ptr!(node)?;
                let info = NodeInfo::from(&(*node));
                res.add_node(Arc::new(info))?;
            }
        }

        {
            let ssn_map = lock_ptr!(self.sessions)?;
            tracing::debug!("There are {} sessions in snapshot.", ssn_map.len());
            for ssn in ssn_map.deref().values() {
                let ssn = lock_ptr!(ssn)?;
                let info = SessionInfo::from(&(*ssn));
                res.add_session(Arc::new(info))?;
            }
        }

        {
            let exe_map = lock_ptr!(self.executors)?;
            tracing::debug!("There are {} executors in snapshot.", exe_map.len());
            for exe in exe_map.deref().values() {
                let exe = lock_ptr!(exe)?;
                let info = ExecutorInfo::from(&(*exe));
                res.add_executor(Arc::new(info))?;
            }
        }

        {
            let app_map = lock_ptr!(self.applications)?;
            tracing::debug!("There are {} applications in snapshot.", app_map.len());
            for app in app_map.deref().values() {
                let app = lock_ptr!(app)?;
                let info = AppInfo::from(&(*app));
                res.add_application(Arc::new(info))?;
            }
        }

        Ok(Arc::new(res))
    }

    pub async fn load_data(&self) -> Result<(), FlameError> {
        let ssn_list = self.engine.find_session().await?;
        for ssn in ssn_list {
            let task_list = self.engine.find_tasks(ssn.id.clone()).await?;
            let mut ssn = ssn.clone();
            for task in task_list {
                let task = match task.state {
                    TaskState::Running => self.engine.retry_task(task.gid()).await?,
                    _ => task,
                };

                ssn.update_task(&task);
            }

            let mut ssn_map = lock_ptr!(self.sessions)?;
            ssn_map.insert(ssn.id.clone(), SessionPtr::new(ssn.into()));
        }

        let app_list = self.engine.find_application().await?;
        for app in app_list {
            let mut app_map = lock_ptr!(self.applications)?;
            app_map.insert(app.name.clone(), ApplicationPtr::new(app.into()));
        }

        Ok(())
    }

    pub async fn register_node(&self, node: &Node) -> Result<(), FlameError> {
        trace_fn!("Storage::register_node");
        let mut node_map = lock_ptr!(self.nodes)?;
        node_map.insert(node.name.clone(), stdng::new_ptr(node.clone()));
        Ok(())
    }

    pub async fn sync_node(
        &self,
        node: &Node,
        _: &Vec<Executor>,
    ) -> Result<Vec<Executor>, FlameError> {
        // trace_fn!("Storage::sync_node");

        let mut node_map = lock_ptr!(self.nodes)?;
        node_map.insert(node.name.clone(), stdng::new_ptr(node.clone()));

        let mut res = vec![];

        let mut exe_map = lock_ptr!(self.executors)?;
        let execs = exe_map.values();
        for exec in execs {
            let exec = lock_ptr!(exec)?;
            if exec.node == node.name {
                res.push(Executor {
                    id: exec.id.clone(),
                    node: exec.node.clone(),
                    resreq: exec.resreq.clone(),
                    slots: exec.slots,
                    task_id: exec.task_id,
                    ssn_id: exec.ssn_id.clone(),
                    creation_time: exec.creation_time,
                    state: exec.state,
                });
            }
        }

        tracing::debug!("There are {} executors in node {}", res.len(), node.name);

        Ok(res)
    }

    pub async fn release_node(&self, node_name: &str) -> Result<(), FlameError> {
        let mut node_map = lock_ptr!(self.nodes)?;
        node_map.remove(node_name);
        Ok(())
    }

    pub async fn create_session(
        &self,
        id: SessionID,
        app: String,
        slots: u32,
        common_data: Option<CommonData>,
    ) -> Result<Session, FlameError> {
        trace_fn!("Storage::create_session");
        let ssn = self
            .engine
            .create_session(id, app, slots, common_data)
            .await?;

        let mut ssn_map = lock_ptr!(self.sessions)?;
        ssn_map.insert(ssn.id.clone(), SessionPtr::new(ssn.clone().into()));

        Ok(ssn)
    }

    pub async fn close_session(&self, id: SessionID) -> Result<Session, FlameError> {
        trace_fn!("Storage::close_session");
        let mut ssn = self.engine.close_session(id).await?;

        let mut ssn_list = lock_ptr!(self.sessions)?;
        let ssn_ptr = ssn_list
            .get(&ssn.id)
            .ok_or(FlameError::NotFound(format!(
                "session <{id}> not found",
                id = ssn.id
            )))?
            .clone();

        let mut old_ssn = lock_ptr!(ssn_ptr)?;

        if old_ssn.version >= ssn.version {
            return Err(FlameError::VersionMismatch(format!(
                "session <{id}> version is not incremented",
                id = ssn.id
            )));
        }

        old_ssn.status.state = ssn.status.state;
        old_ssn.completion_time = ssn.completion_time;
        old_ssn.version = ssn.version;
        old_ssn.events = ssn.events.clone();

        Ok(old_ssn.clone())
    }

    pub fn get_session(&self, id: SessionID) -> Result<Session, FlameError> {
        let ssn_ptr = self.get_session_ptr(id)?;
        let ssn = lock_ptr!(ssn_ptr)?;
        Ok(ssn.clone())
    }

    pub fn get_session_ptr(&self, id: SessionID) -> Result<SessionPtr, FlameError> {
        let ssn_map = lock_ptr!(self.sessions)?;

        ssn_map
            .get(&id)
            .ok_or(FlameError::NotFound(id.to_string()))
            .cloned()
    }

    pub fn get_task_ptr(&self, gid: TaskGID) -> Result<TaskPtr, FlameError> {
        let ssn_map = lock_ptr!(self.sessions)?;
        let ssn_ptr = ssn_map
            .get(&gid.ssn_id)
            .ok_or(FlameError::NotFound(gid.ssn_id.to_string()))?;

        let ssn = lock_ptr!(ssn_ptr)?;
        let task_ptr = ssn
            .tasks
            .get(&gid.task_id)
            .ok_or(FlameError::NotFound(gid.to_string()))?;

        Ok(task_ptr.clone())
    }

    pub async fn delete_session(&self, id: SessionID) -> Result<Session, FlameError> {
        let ssn = self.engine.delete_session(id.clone()).await?;

        let mut ssn_map = lock_ptr!(self.sessions)?;
        ssn_map.remove(&ssn.id);

        self.event_manager.remove_events(id)?;

        Ok(ssn)
    }

    pub fn list_session(&self) -> Result<Vec<Session>, FlameError> {
        let mut ssn_list = vec![];
        let ssn_map = lock_ptr!(self.sessions)?;

        for ssn in ssn_map.deref().values() {
            let ssn = lock_ptr!(ssn)?;
            ssn_list.push(ssn.clone());
        }

        Ok(ssn_list)
    }

    pub fn list_executor(&self) -> Result<Vec<Executor>, FlameError> {
        let mut exe_list = vec![];
        let exe_map = lock_ptr!(self.executors)?;

        for exe in exe_map.deref().values() {
            let exe = lock_ptr!(exe)?;
            exe_list.push(exe.clone());
        }

        Ok(exe_list)
    }

    pub async fn create_task(
        &self,
        ssn_id: SessionID,
        task_input: Option<TaskInput>,
    ) -> Result<Task, FlameError> {
        trace_fn!("Storage::create_task");
        let task = self.engine.create_task(ssn_id.clone(), task_input).await?;

        let ssn = self.get_session_ptr(ssn_id.clone())?;
        let mut ssn = lock_ptr!(ssn)?;
        ssn.update_task(&task)?;

        self.event_manager.record_event(
            EventOwner::from(&task),
            Event {
                code: task.state.into(),
                message: Some(format!("Task was created with state <{:?}>", task.state)),
                creation_time: Utc::now(),
            },
        )?;

        Ok(task)
    }

    pub fn get_task(&self, ssn_id: SessionID, id: TaskID) -> Result<Task, FlameError> {
        let ssn_map = lock_ptr!(self.sessions)?;

        let ssn = ssn_map
            .get(&ssn_id)
            .ok_or(FlameError::NotFound(ssn_id.to_string()))?;

        let ssn = lock_ptr!(ssn)?;
        let task = ssn
            .tasks
            .get(&id)
            .ok_or(FlameError::NotFound(id.to_string()))?;
        let mut task = lock_ptr!(task)?;
        let events = self
            .event_manager
            .find_events(EventOwner::from(task.gid()))?;
        task.events = events;

        Ok(task.clone())
    }

    pub fn list_task(&self, ssn_id: SessionID) -> Result<Vec<Task>, FlameError> {
        let ssn_map = lock_ptr!(self.sessions)?;
        let ssn = ssn_map
            .get(&ssn_id)
            .ok_or(FlameError::NotFound(ssn_id.to_string()))?;

        let ssn = lock_ptr!(ssn)?;
        let task_list = ssn
            .tasks
            .values()
            .map(|task_ptr| {
                let task = lock_ptr!(task_ptr)?;
                Ok(task.clone())
            })
            .collect::<Result<Vec<Task>, FlameError>>()?;

        Ok(task_list)
    }

    pub async fn get_application(&self, id: ApplicationID) -> Result<Application, FlameError> {
        self.engine.get_application(id).await
    }

    pub async fn register_application(
        &self,
        name: String,
        attr: ApplicationAttributes,
    ) -> Result<(), FlameError> {
        let app = self.engine.register_application(name, attr).await?;

        let mut app_map = lock_ptr!(self.applications)?;
        // just lock the sessions to avoid cache mismatch.
        let _unused = lock_ptr!(self.sessions)?;

        app_map.insert(app.name.clone(), stdng::new_ptr(app.clone()));

        Ok(())
    }

    pub async fn unregister_application(&self, name: String) -> Result<(), FlameError> {
        self.engine.unregister_application(name.clone()).await?;

        {
            let mut app_map = lock_ptr!(self.applications)?;
            let mut ssn_map = lock_ptr!(self.sessions)?;

            app_map.remove(&name);

            ssn_map.retain(|_, ssn| {
                let ssn_ptr = lock_ptr!(ssn);
                match ssn_ptr {
                    Ok(ssn) => ssn.application != name,
                    Err(_) => true,
                }
            });
        }

        Ok(())
    }

    pub async fn update_application(
        &self,
        name: String,
        attr: ApplicationAttributes,
    ) -> Result<(), FlameError> {
        let app = self.engine.update_application(name.clone(), attr).await?;

        let mut app_map = lock_ptr!(self.applications)?;
        app_map.insert(name.clone(), stdng::new_ptr(app.clone()));

        Ok(())
    }

    pub async fn list_application(&self) -> Result<Vec<Application>, FlameError> {
        self.engine.find_application().await
    }

    pub async fn update_task_state(
        &self,
        ssn: SessionPtr,
        task: TaskPtr,
        task_state: TaskState,
        message: Option<String>,
    ) -> Result<(), FlameError> {
        trace_fn!("Storage::update_task_state");
        let gid = TaskGID {
            ssn_id: {
                let ssn_ptr = lock_ptr!(ssn)?;
                ssn_ptr.id.clone()
            },
            task_id: {
                let task_ptr = lock_ptr!(task)?;
                task_ptr.id
            },
        };

        let task = self
            .engine
            .update_task_state(gid, task_state, message)
            .await?;

        let mut ssn_ptr = lock_ptr!(ssn)?;
        ssn_ptr.update_task(&task)?;

        self.event_manager.record_event(
            EventOwner::from(task.gid()),
            Event {
                code: task_state.into(),
                message: Some(format!("Task state was updated to <{:?}>", task_state)),
                creation_time: Utc::now(),
            },
        )?;

        Ok(())
    }

    pub async fn update_task_result(
        &self,
        ssn: SessionPtr,
        task: TaskPtr,
        task_result: TaskResult,
    ) -> Result<(), FlameError> {
        trace_fn!("Storage::update_task_result");
        let gid = TaskGID {
            ssn_id: {
                let ssn_ptr = lock_ptr!(ssn)?;
                ssn_ptr.id.clone()
            },
            task_id: {
                let task_ptr = lock_ptr!(task)?;
                task_ptr.id
            },
        };

        let task = self.engine.update_task_result(gid, task_result).await?;

        let mut ssn_ptr = lock_ptr!(ssn)?;
        ssn_ptr.update_task(&task)?;

        self.event_manager.record_event(
            EventOwner::from(task.gid()),
            Event {
                code: task.state.into(),
                message: Some(format!("Task was completed with state <{:?}>", task.state)),
                creation_time: Utc::now(),
            },
        )?;

        Ok(())
    }

    pub async fn create_executor(
        &self,
        node_name: String,
        ssn_id: SessionID,
    ) -> Result<Executor, FlameError> {
        trace_fn!("Storage::create_executor");
        let ssn = self.get_session_ptr(ssn_id.clone())?;

        let (resreq, slots) = {
            let ssn = lock_ptr!(ssn)?;
            (
                ResourceRequirement::new(ssn.slots, &self.context.cluster.slot),
                ssn.slots,
            )
        };

        let e = Executor {
            id: Uuid::new_v4().to_string(),
            node: node_name.clone(),
            resreq,
            slots,
            task_id: None,
            ssn_id: Some(ssn_id.clone()),
            creation_time: Utc::now(),
            state: ExecutorState::Void,
        };

        // TODO: create executor in engine
        // let e = self.engine.create_executor(node, ssn).await?;

        let mut exe_map = lock_ptr!(self.executors)?;
        let exe = ExecutorPtr::new(e.clone().into());
        exe_map.insert(e.id.clone(), exe.clone());

        Ok(e.clone())
    }

    pub fn get_executor_ptr(&self, id: ExecutorID) -> Result<ExecutorPtr, FlameError> {
        let exe_map = lock_ptr!(self.executors)?;
        let exe = exe_map
            .get(&id)
            .ok_or(FlameError::NotFound(id.to_string()))?;

        Ok(exe.clone())
    }

    pub async fn delete_executor(&self, id: ExecutorID) -> Result<(), FlameError> {
        trace_fn!("Storage::delete_executor");
        let mut exe_map = lock_ptr!(self.executors)?;
        exe_map.remove(&id);

        Ok(())
    }

    pub async fn record_event(&self, owner: EventOwner, event: Event) -> Result<(), FlameError> {
        trace_fn!("Storage::record_event");
        self.event_manager.record_event(owner, event)
    }
}
