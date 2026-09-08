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

use std::collections::HashSet;

use stdng::lock_ptr;

use super::types::*;
use crate::FlameError;

impl Session {
    pub fn is_closed(&self) -> bool {
        self.status.state == SessionState::Closed
    }

    pub fn is_ready(&self, retry_limits: u32) -> bool {
        self.retry_count < retry_limits
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

    /// Remove the most-local pending task for an executor.
    ///
    /// A task's score is the number of its opaque affinity keys in the
    /// executor's published attributes. Task IDs are monotonic, so the lower
    /// ID wins ties and preserves FIFO behavior when locality is equal.
    pub fn pop_pending_task(
        &mut self,
        attributes: &HashSet<Vec<u8>>,
    ) -> Result<Option<TaskPtr>, FlameError> {
        let pending_tasks = match self.tasks_index.get(&TaskState::Pending) {
            Some(tasks) => tasks,
            None => return Ok(None),
        };

        let mut selected: Option<(TaskID, usize)> = None;
        for (task_id, task_ptr) in pending_tasks {
            let task = lock_ptr!(task_ptr)?;
            let task_score = task
                .affinity
                .iter()
                .filter(|key| attributes.contains(key.as_ref()))
                .count();
            if selected.is_none_or(|(selected_id, selected_score)| {
                task_score > selected_score
                    || (task_score == selected_score && *task_id < selected_id)
            }) {
                selected = Some((*task_id, task_score));
            }
        }

        Ok(selected.and_then(|(task_id, _)| {
            self.tasks_index
                .get_mut(&TaskState::Pending)
                .and_then(|tasks| tasks.remove(&task_id))
        }))
    }

    pub fn validate_spec(&self, attr: &SessionAttributes) -> Result<(), FlameError> {
        if self.application != attr.application {
            return Err(FlameError::InvalidConfig(format!(
                "session <{}> spec mismatch: application differs (expected '{}', got '{}')",
                self.id, self.application, attr.application
            )));
        }
        if self.resreq != attr.resreq {
            return Err(FlameError::InvalidConfig(format!(
                "session <{}> spec mismatch: resreq differs (expected {:?}, got {:?})",
                self.id, self.resreq, attr.resreq
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
        if self.priority != attr.priority {
            return Err(FlameError::InvalidConfig(format!(
                "session <{}> spec mismatch: priority differs (expected {}, got {})",
                self.id, self.priority, attr.priority
            )));
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn is_ready_uses_transient_retry_count() {
        assert!(Session {
            retry_count: 1,
            ..Default::default()
        }
        .is_ready(2));
        assert!(!Session {
            retry_count: 2,
            ..Default::default()
        }
        .is_ready(2));
        assert!(!Session {
            retry_count: 3,
            ..Default::default()
        }
        .is_ready(2));
    }

    #[test]
    fn pop_pending_task_prefers_matching_affinity() {
        let mut session = Session::default();
        session
            .update_task(&Task {
                id: 1,
                affinity: HashSet::from([bytes::Bytes::from_static(b"cold")]),
                ..Default::default()
            })
            .unwrap();
        session
            .update_task(&Task {
                id: 2,
                affinity: HashSet::from([bytes::Bytes::from_static(b"warm")]),
                ..Default::default()
            })
            .unwrap();

        let attributes = HashSet::from([b"warm".to_vec()]);
        let task = session.pop_pending_task(&attributes).unwrap().unwrap();
        assert_eq!(lock_ptr!(task).unwrap().id, 2);
    }
}
