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

use std::collections::HashMap;

use common::apis::{ExecutorState, ResourceRequirement, SessionState};
use common::ctx::FlameLimits;
use common::FlameError;

use crate::model::{
    ExecutorInfoPtr, NodeInfoPtr, SessionInfoPtr, SnapShot, ALL_APPLICATION, ALL_EXECUTOR,
};
use crate::scheduler::plugins::{batch_size, compute_demand, Plugin, PluginPtr, PluginsOptions};

#[derive(Default)]
pub struct MinMaxPlugin {
    // Configured executor limits for nodes, applications and sessions.
    limits: FlameLimits,
    // Node name -> reserved executor count, including Idle/departing executors.
    // Enforces the node executor limit.
    node_executors: HashMap<String, usize>,
    // Node name -> reserved CPU/memory/GPU totals. Prevents resource overcommit.
    node_resources: HashMap<String, ResourceRequirement>,
    // Application name -> max_instances, combined with the cluster application limit.
    application_limits: HashMap<String, u32>,
    // Application name -> reserved executor count. Enforces application limits.
    application_executors: HashMap<String, usize>,
    // Session ID -> allocated/pipelined executor count, including interrupted ones.
    // Used for readiness and hard session limits.
    session_executors: HashMap<String, usize>,
    // Session ID -> interrupted executor count. Subtracted from session_executors
    // to determine effective usage when checking whether a session is overused.
    session_interrupted: HashMap<String, usize>,
}

impl MinMaxPlugin {
    pub fn new_ptr(options: &PluginsOptions) -> PluginPtr {
        Box::new(Self {
            limits: options.limits.clone(),
            ..Default::default()
        })
    }
}

impl Plugin for MinMaxPlugin {
    fn name(&self) -> &'static str {
        "minmax"
    }

    fn setup(&mut self, snapshot: &SnapShot) -> Result<(), FlameError> {
        self.node_executors.clear();
        self.node_resources.clear();
        self.application_executors.clear();
        self.session_executors.clear();
        self.session_interrupted.clear();
        self.application_limits = snapshot
            .find_applications(ALL_APPLICATION)?
            .values()
            .map(|application| (application.name.clone(), application.max_instances))
            .collect();
        for executor in snapshot.find_executors(ALL_EXECUTOR)?.values() {
            // Retained and departing executors occupy capacity until Released.
            if executor.state == ExecutorState::Released {
                continue;
            }
            self.account_capacity(executor);
            if let Some(session) = &executor.ssn_id {
                *self.session_executors.entry(session.clone()).or_default() += 1;
                if executor.is_interrupted() {
                    *self.session_interrupted.entry(session.clone()).or_default() += 1;
                }
            }
        }
        Ok(())
    }

    fn is_underused(&self, ssn: &SessionInfoPtr) -> Option<bool> {
        let instances = self.session_executors.get(&ssn.id).copied().unwrap_or(0);
        if executor_limit(ssn.max_instances, self.limits.session.max_executors)
            .is_some_and(|maximum| instances >= maximum as usize)
        {
            return Some(false);
        }
        // Below demand, including warmup without pending tasks, the session needs
        // more executors; `is_ready` and the configured policies bound how fast.
        (instances < compute_demand(ssn) as usize).then_some(true)
    }

    fn is_ready(&self, ssn: &SessionInfoPtr) -> Option<bool> {
        let instances = self.session_executors.get(&ssn.id).copied().unwrap_or(0);
        if executor_limit(ssn.max_instances, self.limits.session.max_executors)
            .is_some_and(|maximum| instances >= maximum as usize)
        {
            return Some(true);
        }
        // Below the retained minimum, or while a gang is still short of its size.
        if instances < ssn.min_instances.max(batch_size(ssn)) as usize {
            return Some(false);
        }
        // Count existing and pipelined reservations before another allocation.
        // Below demand, let the configured policy apply fairness constraints.
        (instances >= compute_demand(ssn) as usize).then_some(true)
    }

    fn is_satisfied(&self, ssn: &SessionInfoPtr) -> Option<bool> {
        // A gang kept warm (its `min_instances` is its size, which `validate_instance_limits`
        // enforces) is formed all at once even before its first task exists.
        let kept_warm = if ssn.batch_size > 1 { ssn.min_instances } else { 0 };
        // The gang cannot exceed what the session limit allows.
        let gang = batch_size(ssn).max(kept_warm);
        let minimum = executor_limit(Some(gang), self.limits.session.max_executors)
            .map_or(gang, |limit| limit.min(gang));
        let instances = self.session_executors.get(&ssn.id).copied().unwrap_or(0);
        Some(instances >= minimum as usize)
    }

    fn is_overused(&self, ssn: &SessionInfoPtr) -> Option<bool> {
        let executors = self.session_executors.get(&ssn.id).copied().unwrap_or(0);
        let interrupted = self.session_interrupted.get(&ssn.id).copied().unwrap_or(0);
        let effective = executors.saturating_sub(interrupted);
        let demand = executor_limit(ssn.max_instances, self.limits.session.max_executors)
            .map_or(compute_demand(ssn), |maximum| {
                compute_demand(ssn).min(maximum)
            });
        (effective > demand as usize).then_some(true)
    }

    fn is_allocatable(&self, node: &NodeInfoPtr, ssn: &SessionInfoPtr) -> Option<bool> {
        // Resource limits are admission constraints under every policy. DRF
        // controls fairness, but enabling it must not be required for safety.
        let used = self
            .node_resources
            .get(&node.name)
            .cloned()
            .unwrap_or_default();
        let request = ssn.resreq.clone().unwrap_or_default();
        if request.cpu > node.allocatable.cpu.saturating_sub(used.cpu)
            || request.memory > node.allocatable.memory.saturating_sub(used.memory)
            || request.gpu > node.allocatable.gpu.saturating_sub(used.gpu)
        {
            return Some(false);
        }
        let active = self.node_executors.get(&node.name).copied().unwrap_or(0);
        let allocatable = active < self.limits.node.max_executors as usize;
        if !allocatable {
            tracing::debug!(
                "Node <{}> reached max_executors limit: {} >= {}",
                node.name,
                active,
                self.limits.node.max_executors
            );
        }
        if !allocatable {
            return Some(false);
        }
        let Some(max_instances) = executor_limit(
            self.application_limits.get(&ssn.application).copied(),
            self.limits.application.max_executors,
        ) else {
            return Some(true);
        };
        let instances = self
            .application_executors
            .get(&ssn.application)
            .copied()
            .unwrap_or(0);
        Some(instances < max_instances as usize)
    }

    fn is_available(&self, executor: &ExecutorInfoPtr, _ssn: &SessionInfoPtr) -> Option<bool> {
        executor.is_interrupted().then_some(false)
    }

    fn is_reclaimable(&self, executor: &ExecutorInfoPtr) -> Option<bool> {
        executor.is_interrupted().then_some(false)
    }

    fn on_executor_create(&mut self, executor: ExecutorInfoPtr, ssn: SessionInfoPtr) {
        self.account_capacity(&executor);
        *self.session_executors.entry(ssn.id.clone()).or_default() += 1;
    }

    fn on_executor_pipeline(&mut self, executor: ExecutorInfoPtr, ssn: SessionInfoPtr) {
        // Physical capacity and any existing session owner were counted by setup.
        if executor.ssn_id.as_ref() != Some(&ssn.id) {
            *self.session_executors.entry(ssn.id.clone()).or_default() += 1;
        }
    }

    fn on_session_bind(&mut self, executor: ExecutorInfoPtr, ssn: SessionInfoPtr) {
        // Dispatch binds retained Idle capacity; its node/application usage
        // is already counted. A reservation owned by this session is not new.
        if executor.ssn_id.as_ref() != Some(&ssn.id) {
            *self.session_executors.entry(ssn.id.clone()).or_default() += 1;
        }
    }

    fn on_executor_interrupt(&mut self, _executor: ExecutorInfoPtr, ssn: SessionInfoPtr) {
        // Keep the reservation, but exclude the interrupted executor from overuse.
        *self.session_interrupted.entry(ssn.id.clone()).or_default() += 1;
    }

    fn on_session_unbind(&mut self, executor: ExecutorInfoPtr, ssn: SessionInfoPtr) {
        let instances = self.session_executors.entry(ssn.id.clone()).or_default();
        *instances = instances.saturating_sub(1);
        if executor.is_interrupted() {
            let interrupted = self.session_interrupted.entry(ssn.id.clone()).or_default();
            *interrupted = interrupted.saturating_sub(1);
        }
    }
}

impl MinMaxPlugin {
    fn account_capacity(&mut self, executor: &ExecutorInfoPtr) {
        *self
            .node_executors
            .entry(executor.node.clone())
            .or_default() += 1;
        self.node_resources
            .entry(executor.node.clone())
            .or_default()
            .add(&executor.resreq);
        *self
            .application_executors
            .entry(executor.application.clone())
            .or_default() += 1;
    }
}

fn executor_limit(entity: Option<u32>, cluster: Option<u32>) -> Option<u32> {
    match (entity, cluster) {
        (Some(entity), Some(cluster)) => Some(entity.min(cluster)),
        (entity, cluster) => entity.or(cluster),
    }
}

#[cfg(test)]
mod tests;
