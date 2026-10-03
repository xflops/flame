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

use stdng::lock_ptr;

use super::types::*;
use crate::FlameError;

impl Session {
    pub fn gid(&self) -> SessionGID {
        SessionGID::new(&self.workspace, &self.name)
    }

    pub fn is_closed(&self) -> bool {
        self.status.state == SessionState::Closed
    }

    pub fn is_ready(&self, retry_limits: u32) -> bool {
        self.retry_count < retry_limits
    }

    pub fn update_task(&mut self, task: &Task) -> Result<(), FlameError> {
        if task.workspace != self.workspace || task.session != self.name {
            return Err(FlameError::InvalidConfig(format!(
                "task <{}/{}/{}> does not belong to session <{}/{}>",
                task.workspace, task.session, task.name, self.workspace, self.name
            )));
        }
        if task.name == 0 {
            return Err(FlameError::InvalidConfig(
                "task name must be a positive number".into(),
            ));
        }
        let task_ptr = TaskPtr::new(task.clone().into());

        let old_task_ptr = self.tasks.get(&task.name);
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
                .get(&task.name)
                .and_then(|t| lock_ptr!(t).ok())
                .map(|t| t.state),
            task.state,
            task.version
        );

        self.tasks.insert(task.name, task_ptr.clone());
        self.tasks_index.entry(task.state).or_default();

        for state in self.tasks_index.values_mut() {
            state.remove(&task.name);
        }

        self.tasks_index
            .get_mut(&task.state)
            .unwrap()
            .insert(task.name, task_ptr);

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

    /// Remove the oldest pending task.
    pub fn pop_pending_task(&mut self) -> Option<TaskPtr> {
        let pending_tasks = self.tasks_index.get_mut(&TaskState::Pending)?;
        pending_tasks.pop_first().map(|(_, task)| task)
    }

    pub fn validate_spec(&self, attr: &SessionAttributes) -> Result<(), FlameError> {
        if self.workspace != attr.workspace || self.name != attr.name {
            return Err(FlameError::InvalidConfig(format!(
                "session <{}/{}> spec mismatch: expected <{}/{}>",
                self.workspace, self.name, attr.workspace, attr.name
            )));
        }
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
    fn rejects_tasks_with_wrong_parent_or_zero_name() {
        let mut session = Session {
            workspace: "team-a".into(),
            name: "run".into(),
            ..Default::default()
        };
        for (workspace, parent) in [("team-b", "run"), ("team-a", "other")] {
            let task = Task {
                workspace: workspace.into(),
                session: parent.into(),
                name: 1,
                version: 1,
                ..Default::default()
            };
            assert!(session.update_task(&task).is_err());
        }
        assert!(session
            .update_task(&Task {
                workspace: "team-a".into(),
                session: "run".into(),
                name: 0,
                ..Default::default()
            })
            .is_err());
        assert!(session.tasks.is_empty());
    }

    #[test]
    fn pop_pending_task_is_fifo() {
        let mut session = Session {
            workspace: "default".into(),
            ..Default::default()
        };
        for name in [10, 2, 1] {
            session
                .update_task(&Task {
                    id: crate::apis::new_metadata_id(),
                    name,
                    version: 1,
                    ..Default::default()
                })
                .unwrap();
        }
        assert_eq!(
            session.tasks_index[&TaskState::Pending]
                .keys()
                .copied()
                .collect::<Vec<_>>(),
            [1, 2, 10]
        );
        for expected in [1, 2, 10] {
            let task = session.pop_pending_task().unwrap();
            assert_eq!(lock_ptr!(task).unwrap().name, expected);
        }
    }
}
