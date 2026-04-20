/*
Copyright 2025 The Flame Authors.
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

use std::sync::Arc;

use crate::controller::ControllerPtr;
use crate::model::{
    ExecutorInfo, ExecutorInfoPtr, NodeInfoPtr, SessionInfoPtr, SnapShotPtr, ALL_NODE,
};
use crate::scheduler::plugins::PluginManagerPtr;
use common::apis::{ExecutorID, ExecutorState};
use common::FlameError;

struct Allocation {
    node: NodeInfoPtr,
    ssn: SessionInfoPtr,
}

struct Pipeline {
    executor: ExecutorInfoPtr,
    ssn: SessionInfoPtr,
}

struct Binding {
    executor: ExecutorInfoPtr,
    node: NodeInfoPtr,
    ssn: SessionInfoPtr,
}

pub struct Statement {
    allocations: Vec<Allocation>,
    pipelines: Vec<Pipeline>,
    bindings: Vec<Binding>,
    snapshot: SnapShotPtr,
    plugins: PluginManagerPtr,
    controller: ControllerPtr,
}

impl Statement {
    pub fn new(
        snapshot: SnapShotPtr,
        plugins: PluginManagerPtr,
        controller: ControllerPtr,
    ) -> Self {
        Statement {
            allocations: Vec::new(),
            pipelines: Vec::new(),
            bindings: Vec::new(),
            snapshot,
            plugins,
            controller,
        }
    }

    pub fn allocate(&mut self, node: &NodeInfoPtr, ssn: &SessionInfoPtr) -> Result<(), FlameError> {
        self.plugins
            .on_executor_allocate(node.clone(), ssn.clone())?;
        self.allocations.push(Allocation {
            node: node.clone(),
            ssn: ssn.clone(),
        });
        Ok(())
    }

    pub fn pipeline(
        &mut self,
        executor: &ExecutorInfoPtr,
        ssn: &SessionInfoPtr,
    ) -> Result<(), FlameError> {
        self.plugins
            .on_executor_pipeline(executor.clone(), ssn.clone())?;
        self.pipelines.push(Pipeline {
            executor: executor.clone(),
            ssn: ssn.clone(),
        });
        Ok(())
    }

    pub fn bind(
        &mut self,
        executor: &ExecutorInfoPtr,
        ssn: &SessionInfoPtr,
    ) -> Result<(), FlameError> {
        let nodes = self.snapshot.find_nodes(ALL_NODE)?;
        let node = nodes.get(&executor.node).ok_or_else(|| {
            FlameError::Internal(format!(
                "Node {} not found for executor {}",
                executor.node, executor.id
            ))
        })?;

        self.plugins.on_session_bind(ssn.clone())?;
        self.bindings.push(Binding {
            executor: executor.clone(),
            node: node.clone(),
            ssn: ssn.clone(),
        });
        Ok(())
    }

    pub async fn commit(self) -> Result<Vec<ExecutorID>, FlameError> {
        let mut executor_ids = Vec::new();

        for op in self.allocations.into_iter() {
            let executor = self
                .controller
                .create_executor(op.node.name.clone(), op.ssn.id.clone())
                .await?;

            let exec_info = ExecutorInfo::from(&executor);
            self.snapshot.add_executor(Arc::new(exec_info))?;
        }

        for p in self.pipelines.into_iter() {
            executor_ids.push(p.executor.id.clone());
        }

        for binding in self.bindings.into_iter() {
            self.controller
                .bind_session(binding.executor.id.clone(), binding.ssn.id.clone())
                .await?;
            self.snapshot
                .update_executor_state(binding.executor.clone(), ExecutorState::Binding)?;
            executor_ids.push(binding.executor.id.clone());
        }

        Ok(executor_ids)
    }

    pub fn discard(self) -> Result<(), FlameError> {
        for op in self.allocations.into_iter().rev() {
            self.plugins.on_executor_unallocate(op.node, op.ssn)?;
        }
        for p in self.pipelines.into_iter().rev() {
            self.plugins.on_executor_discard(p.executor, p.ssn)?;
        }
        for binding in self.bindings.into_iter().rev() {
            self.plugins.on_session_unbind(binding.ssn)?;
        }
        Ok(())
    }

    pub fn is_empty(&self) -> bool {
        self.allocations.is_empty() && self.pipelines.is_empty() && self.bindings.is_empty()
    }

    pub fn len(&self) -> usize {
        self.allocations.len() + self.pipelines.len() + self.bindings.len()
    }
}
