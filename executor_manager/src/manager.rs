/*
Copyright 2025 Flame Authors.

Licensed under the Apache License, Version 2.0 (the "License");
you may not use this file except in compliance with the License.
You may obtain a copy of the License at

    http://www.apache.org/licenses/LICENSE-2.0

Unless required by applicable law or agreed to in writing, software
 */

use std::collections::HashMap;
use std::fs;
use std::sync::{Arc, Mutex};

use tokio::sync::mpsc;

use common::apis::ExecutorState;
use common::{ctx::FlameClusterContext, FlameError};
use stdng::{lock_ptr, MutexPtr};

use crate::appmgr::ApplicationManager;
use crate::client::BackendClient;
use crate::devices::DeviceManagers;
use crate::executor::{self, Executor, ExecutorPtr};
use crate::shims;
use crate::stream_handler::StreamHandler;

pub enum ExecutorMessage {
    Update(Executor),
}

pub struct ExecutorManager {
    ctx: FlameClusterContext,
    executors: MutexPtr<HashMap<String, ExecutorPtr>>,
    client: BackendClient,
    app_mgr: Arc<ApplicationManager>,
    dev_mgr: Arc<DeviceManagers>,
}

impl ExecutorManager {
    pub async fn new(ctx: &FlameClusterContext) -> Result<Self, FlameError> {
        let socket_dir = shims::get_socket_dir();
        fs::create_dir_all(&socket_dir).map_err(|e| {
            FlameError::Internal(format!(
                "failed to create shim socket directory {}: {e}",
                socket_dir.display()
            ))
        })?;

        let dev_mgr = Arc::new(DeviceManagers::load(&ctx.cluster.executors.devices)?);
        let client = BackendClient::new(ctx).await?;

        let cache_tls = match &ctx.cache {
            Some(cache) => match &cache.tls {
                Some(tls) => Some(tls.client_tls_config()?),
                None => None,
            },
            None => None,
        };
        let app_mgr = Arc::new(ApplicationManager::new_with_tls(cache_tls)?);

        Ok(Self {
            ctx: ctx.clone(),
            executors: Arc::new(Mutex::new(HashMap::new())),
            client,
            app_mgr,
            dev_mgr,
        })
    }

    pub async fn run(&mut self) -> Result<(), FlameError> {
        let (executor_tx, mut executor_rx) = mpsc::channel(32);

        let client = self.client.clone();
        let executors_for_handler = self.executors.clone();
        let dev_mgr = self.dev_mgr.clone();

        let mut handler = StreamHandler::new(client, executors_for_handler, dev_mgr)?;
        let stream_handle = tokio::spawn(async move {
            handler.run(executor_tx).await;
        });

        tracing::info!(
            "Starting executor manager in streaming mode with shim <{:?}>",
            self.ctx.cluster.executors.shim
        );

        while let Some(ExecutorMessage::Update(executor)) = executor_rx.recv().await {
            if let Err(error) = self.handle_executor_update(executor).await {
                stream_handle.abort();
                let _ = stream_handle.await;
                return Err(error);
            }
        }

        let _ = stream_handle.await;

        Ok(())
    }

    async fn handle_executor_update(&mut self, mut executor: Executor) -> Result<(), FlameError> {
        let executor_id = executor.id.clone();
        let state = executor.state;

        let existing = lock_ptr!(self.executors)?.get(&executor_id).cloned();
        if state == ExecutorState::Released {
            if let Some(existing) = existing {
                {
                    let mut executor = lock_ptr!(existing)?;
                    executor.state = ExecutorState::Released;
                    self.dev_mgr.release(&executor)?;
                }
                lock_ptr!(self.executors)?.remove(&executor_id);
            }
            return Ok(());
        }
        if existing.is_some() {
            tracing::debug!("Executor <{}> already exists", executor_id);
            return Ok(());
        }
        executor.app_mgr = Some(self.app_mgr.clone());
        executor.context = Some(self.ctx.clone());
        executor.shim = self.ctx.cluster.executors.shim;
        executor.assignments = self.dev_mgr.allocate(&executor)?;
        let pointer = Arc::new(Mutex::new(executor));
        lock_ptr!(self.executors)?.insert(executor_id, pointer.clone());
        executor::start(self.client.clone(), pointer)?;

        Ok(())
    }
}

pub async fn run(ctx: &FlameClusterContext) -> Result<(), FlameError> {
    let mut manager = ExecutorManager::new(ctx).await?;
    manager.run().await?;

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::devices::gpu::GpuManager;
    use common::apis::{ResourceRequirement, Shim};

    fn manager(workdir: &std::path::Path) -> ExecutorManager {
        ExecutorManager {
            ctx: FlameClusterContext::default(),
            executors: Arc::new(Mutex::new(HashMap::new())),
            client: BackendClient::default(),
            app_mgr: Arc::new(ApplicationManager::new().unwrap()),
            dev_mgr: Arc::new(DeviceManagers::with_gpu(
                GpuManager::new()
                    .with_workdir(workdir)
                    .with_inventory(vec!["GPU-a".to_string()])
                    .unwrap(),
            )),
        }
    }

    fn executor(id: &str, state: ExecutorState) -> Executor {
        Executor {
            id: id.to_string(),
            application: "test-app".to_string(),
            resreq: ResourceRequirement {
                gpu: 1,
                ..Default::default()
            },
            assignments: Default::default(),
            node: "test-node".to_string(),
            shim: Shim::Host,
            session: None,
            task: None,
            context: None,
            app_mgr: None,
            shim_instance: None,
            state,
        }
    }

    #[tokio::test]
    async fn local_release_does_not_free_fem_owned_devices() {
        let workdir = tempfile::tempdir().unwrap();
        let manager = manager(workdir.path());
        let mut exec = executor("executor-1", ExecutorState::Idle);
        exec.app_mgr = Some(manager.app_mgr.clone());
        exec.assignments = manager.dev_mgr.allocate(&exec).unwrap();
        let retained = exec.clone();
        assert!(Arc::ptr_eq(
            exec.app_mgr.as_ref().unwrap(),
            retained.app_mgr.as_ref().unwrap()
        ));
        exec.release().unwrap();
        let replacement = executor("executor-2", ExecutorState::Idle);
        assert!(manager.dev_mgr.allocate(&replacement).is_err());
        assert!(workdir.path().join("gpu/executor-1.json").exists());
        manager.dev_mgr.release(&exec).unwrap();
        manager.dev_mgr.allocate(&replacement).unwrap();
        manager.dev_mgr.release(&retained).unwrap();
        assert!(workdir.path().join("gpu/executor-2.json").exists());
    }

    #[tokio::test]
    async fn failed_assignment_release_retains_entry_for_retry() {
        let workdir = tempfile::tempdir().unwrap();
        let mut manager = manager(workdir.path());
        let mut exec = executor("executor-1", ExecutorState::Released);
        exec.assignments = manager.dev_mgr.allocate(&exec).unwrap();
        lock_ptr!(manager.executors)
            .unwrap()
            .insert(exec.id.clone(), Arc::new(Mutex::new(exec)));
        let path = workdir.path().join("gpu/executor-1.json");
        std::fs::remove_file(&path).unwrap();
        std::fs::create_dir(&path).unwrap();
        assert!(manager
            .handle_executor_update(executor("executor-1", ExecutorState::Released))
            .await
            .is_err());
        assert!(lock_ptr!(manager.executors)
            .unwrap()
            .contains_key("executor-1"));
        assert!(manager
            .dev_mgr
            .allocate(&executor("executor-2", ExecutorState::Idle))
            .is_err());
        std::fs::remove_dir(&path).unwrap();
        manager
            .handle_executor_update(executor("executor-1", ExecutorState::Released))
            .await
            .unwrap();
        assert!(lock_ptr!(manager.executors).unwrap().is_empty());
        manager
            .dev_mgr
            .allocate(&executor("executor-2", ExecutorState::Idle))
            .unwrap();
    }

    #[tokio::test]
    async fn repeated_stream_update_preserves_assignment() {
        let workdir = tempfile::tempdir().unwrap();
        let mut manager = manager(workdir.path());
        let mut existing = executor("executor-1", ExecutorState::Idle);
        existing.app_mgr = Some(manager.app_mgr.clone());
        existing.assignments = manager.dev_mgr.allocate(&existing).unwrap();
        let pointer = Arc::new(Mutex::new(existing));
        lock_ptr!(manager.executors)
            .unwrap()
            .insert("executor-1".to_string(), pointer.clone());
        manager
            .handle_executor_update(executor("executor-1", ExecutorState::Idle))
            .await
            .unwrap();
        assert_eq!(
            lock_ptr!(pointer)
                .unwrap()
                .assignments
                .get("gpu")
                .unwrap()
                .devices(),
            ["GPU-a"]
        );
        assert!(manager
            .dev_mgr
            .allocate(&crate::devices::test_executor(
                (ResourceRequirement {
                    gpu: 1,
                    ..Default::default()
                })
                .clone()
            ))
            .is_err());
    }

    #[tokio::test]
    async fn exhausted_start_returns_error_without_inserting_executor() {
        let workdir = tempfile::tempdir().unwrap();
        let mut manager = manager(workdir.path());
        let retained = crate::devices::test_executor(ResourceRequirement {
            gpu: 1,
            ..Default::default()
        });
        manager.dev_mgr.allocate(&retained).unwrap();
        let error = manager
            .handle_executor_update(executor("executor-1", ExecutorState::Void))
            .await
            .unwrap_err();
        assert!(matches!(error, FlameError::InvalidState(_)));
        assert!(lock_ptr!(manager.executors).unwrap().is_empty());
        manager
            .handle_executor_update(executor("executor-1", ExecutorState::Released))
            .await
            .unwrap();
        assert!(lock_ptr!(manager.executors).unwrap().is_empty());
        manager.dev_mgr.release(&retained).unwrap();
        assert!(manager
            .dev_mgr
            .allocate(&crate::devices::test_executor(
                (ResourceRequirement {
                    gpu: 1,
                    ..Default::default()
                })
                .clone()
            ))
            .is_ok());
    }

    #[tokio::test]
    async fn start_allocates_before_worker_runs_and_shares_application_manager() {
        let workdir = tempfile::tempdir().unwrap();
        let mut manager = manager(workdir.path());
        manager
            .handle_executor_update(executor("executor-1", ExecutorState::Void))
            .await
            .unwrap();
        let pointer = lock_ptr!(manager.executors).unwrap()["executor-1"].clone();
        let exec = lock_ptr!(pointer).unwrap();
        assert_eq!(exec.assignments["gpu"].devices(), ["GPU-a"]);
        assert!(Arc::ptr_eq(
            exec.app_mgr.as_ref().unwrap(),
            &manager.app_mgr
        ));
        assert!(manager
            .dev_mgr
            .allocate(&executor("executor-2", ExecutorState::Idle))
            .is_err());
    }

    #[tokio::test]
    async fn released_notification_finishes_local_releasing_and_frees_devices() {
        let workdir = tempfile::tempdir().unwrap();
        let mut manager = manager(workdir.path());
        let mut existing = executor("executor-1", ExecutorState::Releasing);
        existing.assignments = manager.dev_mgr.allocate(&existing).unwrap();
        let pointer = Arc::new(Mutex::new(existing));
        lock_ptr!(manager.executors)
            .unwrap()
            .insert("executor-1".into(), pointer.clone());
        manager
            .handle_executor_update(executor("executor-1", ExecutorState::Released))
            .await
            .unwrap();
        assert_eq!(lock_ptr!(pointer).unwrap().state, ExecutorState::Released);
        assert!(lock_ptr!(manager.executors).unwrap().is_empty());
        assert!(!workdir.path().join("gpu/executor-1.json").exists());
        manager
            .dev_mgr
            .allocate(&executor("executor-2", ExecutorState::Idle))
            .unwrap();
    }

    #[tokio::test]
    async fn release_completion_removes_executor_and_ignores_duplicates() {
        let workdir = tempfile::tempdir().unwrap();
        let mut manager = manager(workdir.path());
        let mut completed = executor("executor-1", ExecutorState::Released);
        completed.assignments = manager.dev_mgr.allocate(&completed).unwrap();
        completed.release().unwrap();
        let pointer = Arc::new(Mutex::new(completed));
        lock_ptr!(manager.executors)
            .unwrap()
            .insert("executor-1".into(), pointer);
        manager
            .handle_executor_update(executor("executor-1", ExecutorState::Released))
            .await
            .unwrap();
        assert!(lock_ptr!(manager.executors).unwrap().is_empty());
        assert!(!workdir.path().join("gpu/executor-1.json").exists());
        let replacement = executor("executor-2", ExecutorState::Idle);
        manager.dev_mgr.allocate(&replacement).unwrap();
        manager
            .handle_executor_update(executor("executor-1", ExecutorState::Released))
            .await
            .unwrap();
        assert!(workdir.path().join("gpu/executor-2.json").exists());
    }

    #[tokio::test]
    async fn unknown_released_update_does_not_allocate_or_release_devices() {
        let workdir = tempfile::tempdir().unwrap();
        let mut manager = manager(workdir.path());
        let original = executor("executor-1", ExecutorState::Idle);
        manager.dev_mgr.allocate(&original).unwrap();
        manager
            .handle_executor_update(executor("executor-1", ExecutorState::Released))
            .await
            .unwrap();
        assert!(lock_ptr!(manager.executors).unwrap().is_empty());
        assert!(workdir.path().join("gpu/executor-1.json").exists());
        manager.dev_mgr.release(&original).unwrap();
    }

    #[tokio::test]
    async fn replayed_releasing_executor_reuses_assignment_when_capacity_is_reserved() {
        let workdir = tempfile::tempdir().unwrap();
        let mut manager = manager(workdir.path());
        let original = executor("executor-1", ExecutorState::Idle);
        let assignments = manager.dev_mgr.allocate(&original).unwrap();
        assert!(manager
            .dev_mgr
            .allocate(&executor("executor-2", ExecutorState::Idle))
            .is_err());
        manager
            .handle_executor_update(executor("executor-1", ExecutorState::Releasing))
            .await
            .unwrap();
        let existing = lock_ptr!(manager.executors)
            .unwrap()
            .get("executor-1")
            .unwrap()
            .clone();
        assert_eq!(lock_ptr!(existing).unwrap().assignments, assignments);
    }
}
