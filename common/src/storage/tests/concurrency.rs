/*
Copyright 2025 The xflops Authors.
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

//! Controlled engine failures exercise publication boundaries without timing sleeps.
use super::*;
use crate::storage::engine::Engine;
use bytes::Bytes;

#[derive(Default)]
struct Gate {
    started: tokio::sync::Notify,
    release: tokio::sync::Notify,
}
struct ControlledEngine {
    inner: EnginePtr,
    close_error: bool,
    delete_error: bool,
    resume_gate: Option<Arc<Gate>>,
    detail_gate: Option<Arc<Gate>>,
    delete_gate: Option<Arc<Gate>>,
}
#[async_trait::async_trait]
impl Engine for ControlledEngine {
    async fn register_application(
        &self,
        name: String,
        attr: ApplicationAttributes,
    ) -> Result<Application, FlameError> {
        self.inner.register_application(name, attr).await
    }
    async fn update_application_state(
        &self,
        id: ApplicationID,
        state: ApplicationState,
    ) -> Result<Application, FlameError> {
        self.inner.update_application_state(id, state).await
    }
    async fn delete_application(&self, id: ApplicationID) -> Result<(), FlameError> {
        self.inner.delete_application(id).await
    }
    async fn update_application(
        &self,
        id: String,
        attr: ApplicationAttributes,
    ) -> Result<Application, FlameError> {
        self.inner.update_application(id, attr).await
    }
    async fn get_application(&self, id: ApplicationID) -> Result<Application, FlameError> {
        self.inner.get_application(id).await
    }
    async fn find_applications(
        &self,
        filter: Option<&ApplicationFilter>,
    ) -> Result<Vec<Application>, FlameError> {
        self.inner.find_applications(filter).await
    }
    async fn create_session(&self, attr: SessionAttributes) -> Result<Session, FlameError> {
        self.inner.create_session(attr).await
    }
    async fn get_session(&self, id: SessionID) -> Result<Session, FlameError> {
        if let Some(gate) = self.detail_gate.as_ref() {
            gate.started.notify_one();
            gate.release.notified().await;
        }
        self.inner.get_session(id).await
    }
    async fn open_session(
        &self,
        id: SessionID,
        spec: Option<SessionAttributes>,
    ) -> Result<Session, FlameError> {
        self.inner.open_session(id, spec).await
    }
    async fn close_session(&self, id: SessionID) -> Result<Session, FlameError> {
        let session = self.inner.close_session(id).await?;
        if self.close_error {
            return Err(FlameError::Storage(
                "injected post-commit close failure".into(),
            ));
        }
        Ok(session)
    }
    async fn update_session_state(
        &self,
        id: SessionID,
        state: SessionState,
    ) -> Result<Session, FlameError> {
        if state == SessionState::Open {
            if let Some(gate) = self.resume_gate.as_ref() {
                gate.started.notify_one();
                gate.release.notified().await;
                return Err(FlameError::Storage(
                    "injected pre-commit resume failure".into(),
                ));
            }
        }
        self.inner.update_session_state(id, state).await
    }
    async fn delete_session(&self, id: SessionID) -> Result<Session, FlameError> {
        if let Some(gate) = &self.delete_gate {
            gate.started.notify_one();
            gate.release.notified().await;
        }
        if self.delete_error {
            return Err(FlameError::Storage("injected delete failure".into()));
        }
        self.inner.delete_session(id).await
    }
    async fn stale_session(&self, id: SessionID) -> Result<(), FlameError> {
        if let Some(gate) = &self.delete_gate {
            gate.started.notify_one();
            gate.release.notified().await;
        }
        if self.delete_error {
            return Err(FlameError::Storage("injected delete failure".into()));
        }
        self.inner.stale_session(id).await
    }

    async fn find_session_metadata(&self) -> Result<Vec<Session>, FlameError> {
        self.inner.find_session_metadata().await
    }
    async fn create_task(
        &self,
        ssn_id: SessionID,
        task_input: Option<TaskInput>,
        options: Option<TaskOptions>,
    ) -> Result<Task, FlameError> {
        self.inner.create_task(ssn_id, task_input, options).await
    }
    async fn get_task(&self, gid: TaskGID) -> Result<Task, FlameError> {
        self.inner.get_task(gid).await
    }
    async fn retry_task(&self, gid: TaskGID) -> Result<Task, FlameError> {
        self.inner.retry_task(gid).await
    }
    async fn update_task_state(
        &self,
        current: Task,
        task_state: TaskState,
        message: Option<String>,
    ) -> Result<Task, FlameError> {
        self.inner
            .update_task_state(current, task_state, message)
            .await
    }
    async fn update_task_result(
        &self,
        current: Task,
        task_result: TaskResult,
    ) -> Result<Task, FlameError> {
        self.inner.update_task_result(current, task_result).await
    }
    async fn find_task_metadata(
        &self,
        filter: &TaskFilter,
    ) -> Result<crate::apis::Page<Task>, FlameError> {
        self.inner.find_task_metadata(filter).await
    }
    async fn create_node(&self, node: &Node) -> Result<Node, FlameError> {
        self.inner.create_node(node).await
    }
    async fn get_node(&self, name: &str) -> Result<Option<Node>, FlameError> {
        self.inner.get_node(name).await
    }
    async fn update_node(&self, node: &Node) -> Result<Node, FlameError> {
        self.inner.update_node(node).await
    }
    async fn delete_node(&self, name: &str) -> Result<(), FlameError> {
        self.inner.delete_node(name).await
    }
    async fn find_nodes(&self) -> Result<Vec<Node>, FlameError> {
        self.inner.find_nodes().await
    }

    async fn create_executor(&self, executor: &Executor) -> Result<Executor, FlameError> {
        self.inner.create_executor(executor).await
    }
    async fn get_executor(&self, id: &ExecutorID) -> Result<Option<Executor>, FlameError> {
        self.inner.get_executor(id).await
    }
    async fn update_executor(&self, executor: &Executor) -> Result<Executor, FlameError> {
        self.inner.update_executor(executor).await
    }
    async fn update_executor_state(
        &self,
        id: &ExecutorID,
        state: ExecutorState,
    ) -> Result<Executor, FlameError> {
        self.inner.update_executor_state(id, state).await
    }
    async fn delete_executor(&self, id: &ExecutorID) -> Result<(), FlameError> {
        self.inner.delete_executor(id).await
    }
    async fn find_executors(&self, node: Option<&str>) -> Result<Vec<Executor>, FlameError> {
        self.inner.find_executors(node).await
    }
}
async fn storage() -> Storage {
    let context = FlameClusterContext {
        cluster: crate::ctx::FlameCluster {
            storage: "none".into(),
            ..Default::default()
        },
        ..Default::default()
    };
    (*new_ptr(&context).await.unwrap()).clone()
}
fn attributes(id: &str) -> SessionAttributes {
    SessionAttributes {
        id: id.into(),
        application: "app".into(),
        ..Default::default()
    }
}
#[tokio::test]
async fn post_commit_close_failure_still_publishes_closed_metadata_and_frees_capacity() {
    let mut storage = storage().await;
    storage.max_sessions = Some(1);
    storage.create_session(attributes("old")).await.unwrap();
    storage.create_task("old".into(), None, None).await.unwrap();
    let live = storage.get_session_ptr("old".into()).unwrap();
    storage.engine = Arc::new(ControlledEngine {
        inner: storage.engine.clone(),
        close_error: true,
        delete_error: false,
        resume_gate: None,
        detail_gate: None,
        delete_gate: None,
    });
    assert!(storage.close_session("old".into()).await.is_err());
    let closed = storage.get_session("old".into()).unwrap();
    assert!(closed.is_closed());
    assert_eq!(closed.task_counts.cancelled, 1);
    assert!(lock_ptr!(live).unwrap().is_closed());
    assert!(storage.get_session_ptr("old".into()).is_ok());
    {
        let directory = lock_ptr!(storage.sessions).unwrap();
        assert_eq!(directory.len(), 1);
    }
    storage.create_session(attributes("new")).await.unwrap();
}

#[tokio::test]
async fn resume_is_not_visible_to_launch_observers_before_commit() {
    let mut storage = storage().await;
    storage.max_sessions = Some(1);
    storage.create_session(attributes("paused")).await.unwrap();
    storage
        .update_session_state("paused".into(), SessionState::Suspended)
        .await
        .unwrap();
    let gate = Arc::new(Gate::default());
    storage.engine = Arc::new(ControlledEngine {
        inner: storage.engine.clone(),
        close_error: false,
        delete_error: false,
        resume_gate: Some(gate.clone()),
        detail_gate: None,
        delete_gate: None,
    });
    let storage = Arc::new(storage);
    let resuming = {
        let storage = storage.clone();
        tokio::spawn(async move {
            storage
                .update_session_state("paused".into(), SessionState::Open)
                .await
        })
    };
    gate.started.notified().await;
    assert!(storage.get_session("paused".into()).unwrap().is_suspended());
    assert!(storage.list_sessions(None).await.unwrap()[0].is_suspended());
    assert!(storage.snapshot().unwrap().sessions.is_empty());
    gate.release.notify_one();
    assert!(resuming.await.unwrap().is_err());
    assert!(storage.get_session("paused".into()).unwrap().is_suspended());
    storage
        .create_session(attributes("admitted"))
        .await
        .unwrap();
}

#[tokio::test]
async fn closed_details_keep_deletion_and_recreation_outside_the_counter_merge() {
    let storage = storage().await;
    storage
        .create_session(SessionAttributes {
            common_data: Some(Bytes::from_static(b"old")),
            ..attributes("same")
        })
        .await
        .unwrap();
    storage
        .create_task("same".into(), None, None)
        .await
        .unwrap();
    storage.close_session("same".into()).await.unwrap();
    let detailed = storage.get_session("same".into()).unwrap();
    assert_eq!(detailed.common_data, Some(Bytes::from_static(b"old")));
    assert_eq!(detailed.task_counts.cancelled, 1);
    storage
        .delete_session(&storage.get_session_ptr("same".into()).unwrap())
        .await
        .unwrap();
    storage
        .create_session(SessionAttributes {
            common_data: Some(Bytes::from_static(b"new")),
            ..attributes("same")
        })
        .await
        .unwrap();
    let current = storage.get_session("same".into()).unwrap();
    assert!(current.is_open());
    assert_eq!(current.task_counts.cancelled, 0);
    assert_eq!(current.common_data, Some(Bytes::from_static(b"new")));
}

#[test]
fn exact_owner_filters_deduplicate_ids_without_locking_unrelated_history() {
    let storage = tokio_test::block_on(storage());
    for id in ["wanted", "unrelated"] {
        tokio_test::block_on(storage.create_session(attributes(id))).unwrap();
        tokio_test::block_on(
            storage.register_application(id.into(), ApplicationAttributes::default()),
        )
        .unwrap();
    }
    let unrelated_session = storage.get_session_ptr("unrelated".into()).unwrap();
    let unrelated_application = lock_ptr!(storage.applications).unwrap()["unrelated"].clone();
    std::thread::scope(|scope| {
        let (send, receive) = std::sync::mpsc::channel();
        let result = {
            let _session = lock_ptr!(unrelated_session).unwrap();
            let _application = lock_ptr!(unrelated_application).unwrap();
            let storage = &storage;
            scope.spawn(move || {
                let sessions = tokio_test::block_on(storage.list_sessions(Some(
                    &SessionFilter::by_ids(vec!["wanted".into(), "wanted".into()]),
                )));
                let applications =
                    tokio_test::block_on(storage.list_applications(Some(&ApplicationFilter {
                        names: Some(vec!["wanted".into(), "wanted".into()]),
                        ..Default::default()
                    })));
                let _ = send.send((sessions, applications));
            });
            receive.recv_timeout(std::time::Duration::from_secs(1))
        };
        let (sessions, applications) =
            result.expect("exact owner query waited for an unrelated resource lock");
        let sessions = sessions.unwrap();
        let applications = applications.unwrap();
        assert_eq!(sessions.len(), 1);
        assert_eq!(sessions.paging.total(), Some(1));
        assert_eq!(sessions[0].id, "wanted");
        assert_eq!(applications.len(), 1);
        assert_eq!(applications.paging.total(), Some(1));
        assert_eq!(applications[0].name, "wanted");
    });
}

/// Admission serializes removals and creation while ordinary reads and tasks continue.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn blocked_delete_serializes_removers_and_same_id_recreation() {
    let mut storage = storage().await;
    storage.create_session(attributes("expired")).await.unwrap();
    storage.close_session("expired".into()).await.unwrap();
    storage.create_session(attributes("active")).await.unwrap();
    let pointer = storage.get_session_ptr("expired".into()).unwrap();
    let gate = Arc::new(Gate::default());
    storage.engine = Arc::new(ControlledEngine {
        inner: storage.engine.clone(),
        close_error: false,
        delete_error: false,
        resume_gate: None,
        detail_gate: None,
        delete_gate: Some(gate.clone()),
    });
    let storage = Arc::new(storage);
    let deleting = {
        let storage = storage.clone();
        let pointer = pointer.clone();
        tokio::spawn(async move { storage.delete_session(&pointer).await })
    };
    gate.started.notified().await;
    let admission_locked = storage.session_admission.try_lock().is_err();
    let directory_available = storage.sessions.try_lock().is_some();
    let session_available = pointer.try_lock().is_some();
    let entered = Arc::new(tokio::sync::Notify::new());
    let mut second = {
        let storage = storage.clone();
        let pointer = pointer.clone();
        let entered = entered.clone();
        tokio::spawn(async move {
            entered.notify_one();
            storage.delete_session(&pointer).await
        })
    };
    entered.notified().await;
    let waiting = tokio::time::timeout(std::time::Duration::from_millis(30), &mut second).await;
    let mut recreating = {
        let storage = storage.clone();
        tokio::spawn(async move { storage.create_session(attributes("expired")).await })
    };
    let creation_waiting =
        tokio::time::timeout(std::time::Duration::from_millis(30), &mut recreating).await;
    let mut progress = {
        let storage = storage.clone();
        tokio::spawn(async move {
            assert!(storage.get_session("active".into())?.is_open());
            storage
                .create_task("active".into(), Some(Bytes::from_static(b"work")), None)
                .await?;
            Ok::<_, FlameError>(())
        })
    };
    let progressed = tokio::time::timeout(std::time::Duration::from_secs(2), &mut progress).await;
    // Release before assertions so a regression cannot strand a waiting task.
    gate.release.notify_one();
    let removed = deleting.await.unwrap().unwrap();
    assert!(
        admission_locked,
        "removal did not retain admission during engine I/O"
    );
    assert!(directory_available, "directory mutex crossed engine I/O");
    assert!(session_available, "session mutex crossed engine I/O");
    assert!(waiting.is_err(), "second remover bypassed admission");
    assert!(
        creation_waiting.is_err(),
        "same-ID creation bypassed admission"
    );
    progressed
        .expect("active-session work waited for removal admission")
        .unwrap()
        .unwrap();
    assert!(second.await.unwrap().is_err());
    assert_eq!(removed.id, "expired");
    recreating.await.unwrap().unwrap();
    let replacement = storage.get_session_ptr("expired".into()).unwrap();
    assert!(!Arc::ptr_eq(&pointer, &replacement));
    assert!(storage.get_session("expired".into()).unwrap().is_open());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn cancelled_removal_releases_admission_and_retains_cache_for_retry() {
    let mut storage = storage().await;
    storage
        .create_session(attributes("cancelled"))
        .await
        .unwrap();
    storage.close_session("cancelled".into()).await.unwrap();
    let pointer = storage.get_session_ptr("cancelled".into()).unwrap();
    let gate = Arc::new(Gate::default());
    storage.engine = Arc::new(ControlledEngine {
        inner: storage.engine.clone(),
        close_error: false,
        delete_error: false,
        resume_gate: None,
        detail_gate: None,
        delete_gate: Some(gate.clone()),
    });
    let storage = Arc::new(storage);
    let removing = {
        let storage = storage.clone();
        let pointer = pointer.clone();
        tokio::spawn(async move { storage.stale_session(&pointer).await })
    };
    gate.started.notified().await;
    removing.abort();
    assert!(removing.await.unwrap_err().is_cancelled());
    let admission_available = storage.session_admission.try_lock().is_ok();
    let directory_available = storage.sessions.try_lock().is_some();
    let session_available = pointer.try_lock().is_some();
    gate.release.notify_one();
    assert!(admission_available, "cancelled removal retained admission");
    assert!(
        directory_available && session_available,
        "cancelled removal retained cache locks"
    );
    assert!(Arc::ptr_eq(
        &pointer,
        &storage.get_session_ptr("cancelled".into()).unwrap()
    ));
    assert!(storage
        .create_session(attributes("cancelled"))
        .await
        .is_err());
    tokio::time::timeout(
        std::time::Duration::from_secs(3),
        storage.delete_session(&pointer),
    )
    .await
    .expect("cancelled removal left a background operation or lock")
    .unwrap();
    assert!(storage.get_session("cancelled".into()).is_err());
}

#[tokio::test]
async fn failed_removal_releases_locks_and_old_pointer_cannot_delete_recreation() {
    let mut storage = storage().await;
    storage.create_session(attributes("retry")).await.unwrap();
    storage.close_session("retry".into()).await.unwrap();
    let pointer = storage.get_session_ptr("retry".into()).unwrap();
    let original = storage.engine.clone();
    storage.engine = Arc::new(ControlledEngine {
        inner: original.clone(),
        close_error: false,
        delete_error: true,
        resume_gate: None,
        detail_gate: None,
        delete_gate: None,
    });
    assert!(storage.delete_session(&pointer).await.is_err());
    assert!(Arc::ptr_eq(
        &pointer,
        &storage.get_session_ptr("retry".into()).unwrap()
    ));
    storage.engine = original;
    storage.stale_session(&pointer).await.unwrap();
    storage.create_session(attributes("retry")).await.unwrap();
    storage.close_session("retry".into()).await.unwrap();
    assert!(storage.delete_session(&pointer).await.is_err());
    assert!(storage.stale_session(&pointer).await.is_err());
    assert!(storage.get_session("retry".into()).is_ok());
}
