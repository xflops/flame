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
use crate::executor::{self, Executor, ExecutorPtr};
use crate::stream_handler::StreamHandler;

pub enum ExecutorMessage {
    Update(Executor),
}

pub struct ExecutorManager {
    ctx: FlameClusterContext,
    executors: MutexPtr<HashMap<String, ExecutorPtr>>,
    client: BackendClient,
    app_manager: Arc<ApplicationManager>,
}

impl ExecutorManager {
    pub async fn new(ctx: &FlameClusterContext) -> Result<Self, FlameError> {
        fs::create_dir_all("/tmp/flame/shim")
            .map_err(|e| FlameError::Internal(format!("failed to create shim directory: {e}")))?;

        let client = BackendClient::new(ctx).await?;

        let cache_tls = match &ctx.cache {
            Some(cache) => match &cache.tls {
                Some(tls) => Some(tls.client_tls_config()?),
                None => None,
            },
            None => None,
        };
        let app_manager = Arc::new(ApplicationManager::new_with_tls(cache_tls)?);

        Ok(Self {
            ctx: ctx.clone(),
            executors: Arc::new(Mutex::new(HashMap::new())),
            client,
            app_manager,
        })
    }

    pub async fn run(&mut self) -> Result<(), FlameError> {
        let (executor_tx, mut executor_rx) = mpsc::channel::<ExecutorMessage>(32);

        let client = self.client.clone();
        let executors_for_handler = self.executors.clone();

        let stream_handle = tokio::spawn(async move {
            let mut handler = StreamHandler::new(client, executors_for_handler);
            handler.run(executor_tx).await;
        });

        tracing::info!(
            "Starting executor manager in streaming mode with shim <{:?}>",
            self.ctx.cluster.executors.shim
        );

        while let Some(msg) = executor_rx.recv().await {
            match msg {
                ExecutorMessage::Update(executor) => {
                    self.handle_executor_update(executor)?;
                }
            }
        }

        let _ = stream_handle.await;

        Ok(())
    }

    fn handle_executor_update(&mut self, mut executor: Executor) -> Result<(), FlameError> {
        let executor_id = executor.id.clone();
        let state = executor.state;

        let mut executors = lock_ptr!(self.executors)?;

        if state == ExecutorState::Released {
            tracing::info!(
                "Removing executor <{}> from map (state={:?})",
                executor_id,
                state
            );
            executors.remove(&executor_id);
            return Ok(());
        }

        if !executors.contains_key(&executor_id) {
            tracing::info!(
                "Creating executor <{}> (state={:?}, shim={:?})",
                executor_id,
                state,
                self.ctx.cluster.executors.shim
            );
            executor.context = Some(self.ctx.clone());
            executor.shim = self.ctx.cluster.executors.shim;

            let executor_ptr = Arc::new(Mutex::new(executor));
            executors.insert(executor_id.clone(), executor_ptr.clone());
            executor::start(self.client.clone(), executor_ptr, self.app_manager.clone());
            return Ok(());
        }

        if let Some(existing) = executors.get(&executor_id) {
            let existing = lock_ptr!(existing)?;
            tracing::debug!(
                "Executor <{}> already exists (current_state={:?}, received_state={:?})",
                executor_id,
                existing.state,
                state
            );
        }

        Ok(())
    }
}

pub async fn run(ctx: &FlameClusterContext) -> Result<(), FlameError> {
    let mut manager = ExecutorManager::new(ctx).await?;
    manager.run().await?;

    Ok(())
}
