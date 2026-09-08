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

use std::collections::HashMap;
use std::sync::Arc;

use anyhow::Context;
use async_trait::async_trait;
use stdng::{logs::TraceFn, trace_fn};
use tokio::sync::Mutex;
use wasmtime::component::{Component, Linker, ResourceTable};
use wasmtime::{Config, Engine, Store};
use wasmtime_wasi::{WasiCtx, WasiCtxBuilder, WasiCtxView, WasiView};

use crate::executor::Executor;
use crate::shims::wasm_shim::exports::component::flame::service;
use crate::shims::{Shim, ShimPtr};
use ::rpc::flame::v1 as rpc;
use common::{self, apis, FlameError};

wasmtime::component::bindgen!({
    path: "wit/flame.wit",
    world: "flame",
});

// Note: We use synchronous Wasm calls here because wasmtime-wasi 43's WasiCtx
// is not Sync-safe, which prevents using async instantiation with the Shim trait's
// Arc<Mutex<dyn Shim>> pattern. The Wasm tasks are expected to be short-lived
// and non-blocking. For long-running Wasm tasks, consider using tokio::task::spawn_blocking.

pub struct WasmShim {
    session_context: Option<apis::SessionContext>,
    instance: Flame,
    store: Store<ServerWasiView>,
}

impl WasmShim {
    pub async fn new_ptr(
        _: &Executor,
        app: &apis::ApplicationContext,
        _install_env_vars: &HashMap<String, String>,
    ) -> Result<ShimPtr, common::FlameError> {
        trace_fn!("WasmShim::new_ptr");

        let mut config = Config::default();
        config.wasm_component_model(true);

        let engine =
            Engine::new(&config).map_err(|e| common::FlameError::Internal(e.to_string()))?;
        let mut linker = Linker::new(&engine);
        wasmtime_wasi::p2::add_to_linker_sync(&mut linker)
            .map_err(|e| common::FlameError::Internal(e.to_string()))?;
        let wasi_view = ServerWasiView::new();
        let mut store = Store::new(&engine, wasi_view);

        let cmd = app
            .command
            .clone()
            .ok_or(FlameError::InvalidConfig("command is empty".to_string()))?;

        let component = Component::from_file(&engine, cmd).map_err(|e| {
            common::FlameError::Internal(format!("Component file not found: {}", e))
        })?;

        let instance = Flame::instantiate(&mut store, &component, &linker).map_err(|e| {
            common::FlameError::Internal(format!("Failed to instantiate the flame world: {}", e))
        })?;

        Ok(Arc::new(Mutex::new(WasmShim {
            store,
            instance,
            session_context: None,
        })))
    }
}

#[async_trait]
impl Shim for WasmShim {
    async fn on_session_enter(
        &mut self,
        ctx: &apis::SessionContext,
    ) -> Result<super::SessionEnterResponse, common::FlameError> {
        trace_fn!("WasmShim::on_session_enter");

        let ssn_ctx = service::SessionContext {
            session_id: ctx.session_id.clone(),
            common_data: ctx.common_data.clone().map(apis::CommonData::into),
        };

        let _ = self
            .instance
            .component_flame_service()
            .call_on_session_enter(&mut self.store, &ssn_ctx)
            .map_err(|e| common::FlameError::Internal(e.to_string()))?
            .map_err(|e| common::FlameError::Internal(e.message))?;

        self.session_context = Some(ctx.clone());

        Ok(super::SessionEnterResponse {
            executor_attributes: None,
        })
    }

    async fn on_task_invoke(
        &mut self,
        ctx: &apis::TaskContext,
    ) -> Result<super::TaskInvokeResponse, common::FlameError> {
        trace_fn!("WasmShim::on_task_invoke");

        let task_ctx = service::TaskContext {
            session_id: ctx.session_id.clone(),
            task_id: ctx.task_id.clone(),
        };

        let result = self
            .instance
            .component_flame_service()
            .call_on_task_invoke(
                &mut self.store,
                &task_ctx,
                ctx.input.clone().map(apis::TaskInput::into).as_ref(),
            )
            .map_err(|e| common::FlameError::Internal(e.to_string()))?;

        match result {
            Ok(output) => Ok(super::TaskInvokeResponse {
                task_result: apis::TaskResult {
                    state: apis::TaskState::Succeed,
                    output: output.map(apis::TaskOutput::from),
                    message: None,
                },
                attributes: None,
            }),
            Err(e) => {
                tracing::error!("Task failed: {}", e.message);
                Ok(super::TaskInvokeResponse {
                    task_result: apis::TaskResult {
                        state: apis::TaskState::Failed,
                        output: None,
                        message: Some(e.message),
                    },
                    attributes: None,
                })
            }
        }
    }

    async fn on_session_leave(&mut self) -> Result<(), common::FlameError> {
        trace_fn!("WasmShim::on_session_leave");

        let session_context = self
            .session_context
            .as_ref()
            .ok_or(FlameError::InvalidState(
                "session context not set".to_string(),
            ))?;

        let ssn_ctx = service::SessionContext {
            session_id: session_context.session_id.clone(),
            common_data: None,
        };

        let _ = self
            .instance
            .component_flame_service()
            .call_on_session_leave(&mut self.store, &ssn_ctx)
            .map_err(|e| common::FlameError::Internal(e.to_string()))?
            .map_err(|e| common::FlameError::Internal(e.message))?;

        Ok(())
    }
}

struct ServerWasiView {
    table: ResourceTable,
    ctx: WasiCtx,
}

impl ServerWasiView {
    fn new() -> Self {
        let table = ResourceTable::new();
        let ctx = WasiCtxBuilder::new().inherit_stdio().build();

        Self { table, ctx }
    }
}

impl WasiView for ServerWasiView {
    fn ctx(&mut self) -> WasiCtxView<'_> {
        WasiCtxView {
            ctx: &mut self.ctx,
            table: &mut self.table,
        }
    }
}
