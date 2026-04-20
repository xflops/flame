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

use stdng::lock_ptr;

use super::types::*;
use crate::FlameError;

impl Session {
    pub fn is_closed(&self) -> bool {
        self.status.state == SessionState::Closed
    }

    pub fn update_task(&mut self, task: &Task) -> Result<(), FlameError> {
        let task_ptr = TaskPtr::new(task.clone().into());

        let old_task_ptr = self.tasks.get(&task.id);
        if let Some(old_task_ptr) = old_task_ptr {
            let old_task = lock_ptr!(old_task_ptr)?;
            if old_task.version >= task.version {
                tracing::debug!(
                    "Update task: <{task_id}> with an old version (old={old_version}, new={new_version}), ignore it.",
                    task_id = task.id,
                    old_version = old_task.version,
                    new_version = task.version
                );
                return Ok(());
            }
        }

        tracing::debug!(
            "Updating task <{}> from state {:?} to {:?} (version {})",
            task.id,
            self.tasks
                .get(&task.id)
                .and_then(|t| lock_ptr!(t).ok())
                .map(|t| t.state),
            task.state,
            task.version
        );

        self.tasks.insert(task.id, task_ptr.clone());
        self.tasks_index.entry(task.state).or_default();

        for state in self.tasks_index.values_mut() {
            state.remove(&task.id);
        }

        self.tasks_index
            .get_mut(&task.state)
            .unwrap()
            .insert(task.id, task_ptr);

        let pending_count = self
            .tasks_index
            .get(&TaskState::Pending)
            .map(|m| m.len())
            .unwrap_or(0);
        let running_count = self
            .tasks_index
            .get(&TaskState::Running)
            .map(|m| m.len())
            .unwrap_or(0);
        tracing::debug!(
            "Session <{}> tasks_index after update: pending={}, running={}",
            self.id,
            pending_count,
            running_count
        );

        Ok(())
    }

    pub fn pop_pending_task(&mut self) -> Option<TaskPtr> {
        let pending_tasks = self.tasks_index.get_mut(&TaskState::Pending)?;
        let task_id = *pending_tasks.keys().next()?;
        pending_tasks.remove(&task_id)
    }

    pub fn validate_spec(&self, attr: &SessionAttributes) -> Result<(), FlameError> {
        if self.application != attr.application {
            return Err(FlameError::InvalidConfig(format!(
                "session <{}> spec mismatch: application differs (expected '{}', got '{}')",
                self.id, self.application, attr.application
            )));
        }
        if self.slots != attr.slots {
            return Err(FlameError::InvalidConfig(format!(
                "session <{}> spec mismatch: slots differs (expected {}, got {})",
                self.id, self.slots, attr.slots
            )));
        }
        if self.min_instances != attr.min_instances {
            return Err(FlameError::InvalidConfig(format!(
                "session <{}> spec mismatch: min_instances differs (expected {}, got {})",
                self.id, self.min_instances, attr.min_instances
            )));
        }
        if self.max_instances != attr.max_instances {
            return Err(FlameError::InvalidConfig(format!(
                "session <{}> spec mismatch: max_instances differs (expected {:?}, got {:?})",
                self.id, self.max_instances, attr.max_instances
            )));
        }
        if self.batch_size != attr.batch_size {
            return Err(FlameError::InvalidConfig(format!(
                "session <{}> spec mismatch: batch_size differs (expected {}, got {})",
                self.id, self.batch_size, attr.batch_size
            )));
        }
        Ok(())
    }
}

impl Clone for Session {
    fn clone(&self) -> Self {
        let mut ssn = Session {
            id: self.id.clone(),
            application: self.application.clone(),
            slots: self.slots,
            version: self.version,
            common_data: self.common_data.clone(),
            tasks: HashMap::new(),
            tasks_index: HashMap::new(),
            creation_time: self.creation_time,
            completion_time: self.completion_time,
            events: self.events.clone(),
            status: self.status.clone(),
            min_instances: self.min_instances,
            max_instances: self.max_instances,
            batch_size: self.batch_size,
        };

        for (id, t) in &self.tasks {
            match t.lock() {
                Ok(t) => {
                    if let Err(e) = ssn.update_task(&t) {
                        tracing::error!("Failed to update task: <{id}> for session: <{ssn_id}>, ignore it during clone: {e}", ssn_id = self.id);
                    }
                }
                Err(_) => {
                    tracing::error!("Failed to lock task: <{id}> for session: <{ssn_id}>, ignore it during clone.", ssn_id = self.id);
                }
            }
        }

        ssn
    }
}
