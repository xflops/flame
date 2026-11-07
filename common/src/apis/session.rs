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
    pub fn is_closed(&self) -> bool {
        self.status.state == SessionState::Closed
    }

    pub fn is_open(&self) -> bool {
        self.status.state == SessionState::Open
    }

    pub fn is_suspending(&self) -> bool {
        self.status.state == SessionState::Suspending
    }

    pub fn is_suspended(&self) -> bool {
        self.status.state == SessionState::Suspended
    }

    pub fn is_ready(&self, retry_limits: u32) -> bool {
        self.retry_count < retry_limits
    }

    /// Cache an active task without changing task counters.
    pub(crate) fn cache_task(&mut self, task: &Task) {
        if task.state.is_terminal() {
            return;
        }
        let ptr = TaskPtr::new(task.clone().into());
        self.tasks.insert(task.id, ptr.clone());
        self.tasks_index
            .entry(task.state)
            .or_default()
            .insert(task.id, ptr);
    }

    /// Insert a new task or publish a newer state for an existing task.
    pub fn update_task(&mut self, task: &Task) -> Result<(), FlameError> {
        if let Some(previous) = self.tasks.get(&task.id) {
            let old = {
                let previous = lock_ptr!(previous)?;
                if previous.version >= task.version || previous.is_completed() {
                    return Ok(());
                }
                previous.state
            };
            self.task_counts.transition(old, task.state)?;
        } else {
            self.task_counts.increment(task.state);
        }
        for index in self.tasks_index.values_mut() {
            index.remove(&task.id);
        }
        self.tasks.remove(&task.id);
        self.cache_task(task);
        Ok(())
    }

    /// Remove the oldest pending task.
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
        if self.batch_size != attr.batch_size {
            return Err(FlameError::InvalidConfig(format!(
                "session <{}> spec mismatch: batch_size differs (expected {}, got {})",
                self.id, self.batch_size, attr.batch_size
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
    use std::collections::HashSet;

    #[test]
    fn state_helpers_match_exactly_one_state() {
        for (state, open, suspending, suspended, closed) in [
            (SessionState::Open, true, false, false, false),
            (SessionState::Suspending, false, true, false, false),
            (SessionState::Suspended, false, false, true, false),
            (SessionState::Closed, false, false, false, true),
        ] {
            let ssn = Session {
                status: SessionStatus { state },
                ..Default::default()
            };
            assert_eq!(ssn.is_open(), open, "{state}");
            assert_eq!(ssn.is_suspending(), suspending, "{state}");
            assert_eq!(ssn.is_suspended(), suspended, "{state}");
            assert_eq!(ssn.is_closed(), closed, "{state}");
        }
    }

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
    fn update_task_inserts_then_evicts_finished_tasks() {
        let mut session = Session::default();
        let mut task = Task {
            id: 1,
            version: 1,
            ..Default::default()
        };
        session.update_task(&task).unwrap();
        assert_eq!(session.task_counts.pending, 1);
        for (version, state) in [(2, TaskState::Running), (3, TaskState::Succeed)] {
            task.version = version;
            task.state = state;
            session.update_task(&task).unwrap();
        }
        assert!(session.tasks.is_empty());
        assert!(session.tasks_index.values().all(|index| index.is_empty()));
        assert_eq!(session.task_counts.pending, 0);
        assert_eq!(session.task_counts.running, 0);
        assert_eq!(session.task_counts.succeed, 1);
        assert_eq!(session.task_counts.failed, 0);
    }

    #[test]
    fn pop_pending_task_is_fifo() {
        let mut session = Session::default();
        session
            .update_task(&Task {
                id: 1,
                version: 1,
                affinity: HashSet::from([bytes::Bytes::from_static(b"cold")]),
                ..Default::default()
            })
            .unwrap();
        session
            .update_task(&Task {
                id: 2,
                version: 1,
                affinity: HashSet::from([bytes::Bytes::from_static(b"warm")]),
                ..Default::default()
            })
            .unwrap();

        let task = session.pop_pending_task().unwrap();
        assert_eq!(lock_ptr!(task).unwrap().id, 1);
    }
}
