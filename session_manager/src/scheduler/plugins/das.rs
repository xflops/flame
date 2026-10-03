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

use std::cmp::Ordering;
use std::collections::{HashMap, HashSet};

use bytes::Bytes;
use stdng::lock_ptr;

use crate::model::{ExecutorInfo, ScopedName, SessionInfo, SnapShot};
use crate::scheduler::plugins::{Plugin, PluginPtr};
use common::FlameError;

/// Orders eligible Idle executors by coverage of a session's Pending affinity.
#[derive(Default)]
pub struct DasPlugin {
    affinity: HashMap<ScopedName, HashSet<Bytes>>,
}

impl DasPlugin {
    pub fn new_ptr() -> PluginPtr {
        Box::new(Self::default())
    }

    fn score(&self, executor: &ExecutorInfo, session: &SessionInfo) -> usize {
        self.affinity.get(&session.key()).map_or(0, |affinity| {
            executor
                .attributes
                .iter()
                .filter(|key| affinity.contains(*key))
                .count()
        })
    }
}

impl Plugin for DasPlugin {
    fn name(&self) -> &'static str {
        "das"
    }

    fn setup(&mut self, ss: &SnapShot) -> Result<(), FlameError> {
        self.affinity.clear();

        let sessions = lock_ptr!(ss.sessions)?;
        for session in sessions.values() {
            let affinity = self.affinity.entry(session.key()).or_default();
            if let Some(tasks) = session.task_index.get(&common::apis::TaskState::Pending) {
                for task in tasks.values() {
                    affinity.extend(task.affinity.iter().cloned());
                }
            }
        }

        Ok(())
    }

    fn executor_order_fn(
        &self,
        session: &SessionInfo,
        e1: &ExecutorInfo,
        e2: &ExecutorInfo,
    ) -> Option<Ordering> {
        Some(
            self.score(e1, session)
                .cmp(&self.score(e2, session))
                // A smaller name wins equal scores.
                .then_with(|| e2.name.cmp(&e1.name)),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use common::apis::{Session, Task, TaskName, TaskState};
    use std::sync::Arc;

    fn executor(id: &str, attributes: &[&'static [u8]]) -> ExecutorInfo {
        ExecutorInfo {
            id: uuid::Uuid::new_v4().to_string(),
            name: id.to_string(),
            workspace: "default".to_string(),
            attributes: attributes.iter().copied().map(Bytes::from_static).collect(),
            ..Default::default()
        }
    }

    fn task(session_id: &str, id: TaskName, keys: &[&'static [u8]]) -> Task {
        Task {
            id: uuid::Uuid::new_v4().to_string(),
            workspace: "default".to_string(),
            name: id,
            session: session_id.to_string(),
            version: 1,
            state: TaskState::Pending,
            affinity: keys.iter().copied().map(Bytes::from_static).collect(),
            ..Default::default()
        }
    }

    fn session_in(workspace: &str, name: &str, tasks: Vec<Task>) -> SessionInfo {
        let mut session = Session {
            id: uuid::Uuid::new_v4().to_string(),
            workspace: workspace.to_string(),
            name: name.to_string(),
            ..Default::default()
        };
        for mut task in tasks {
            task.workspace = workspace.to_string();
            session.update_task(&task).unwrap();
        }
        SessionInfo::try_from(&session).unwrap()
    }

    fn session(name: &str, tasks: Vec<Task>) -> SessionInfo {
        session_in("default", name, tasks)
    }

    fn setup(session: &SessionInfo) -> DasPlugin {
        let snapshot = SnapShot::new();
        snapshot.add_session(Arc::new(session.clone())).unwrap();
        let mut plugin = DasPlugin::default();
        plugin.setup(&snapshot).unwrap();
        plugin
    }

    #[test]
    fn same_session_name_in_two_workspaces_keeps_affinity_separate() {
        let first = session_in("default", "shared", vec![task("shared", 1, &[b"alpha"])]);
        let second = session_in("other", "shared", vec![task("shared", 1, &[b"beta"])]);
        let snapshot = SnapShot::new();
        snapshot.add_session(Arc::new(first.clone())).unwrap();
        snapshot.add_session(Arc::new(second.clone())).unwrap();
        let mut plugin = DasPlugin::default();
        plugin.setup(&snapshot).unwrap();

        let alpha = executor("alpha", &[b"alpha"]);
        let beta = executor("beta", &[b"beta"]);
        assert_eq!(plugin.score(&alpha, &first), 1);
        assert_eq!(plugin.score(&beta, &first), 0);
        assert_eq!(plugin.score(&alpha, &second), 0);
        assert_eq!(plugin.score(&beta, &second), 1);
        assert_eq!(
            plugin.executor_order_fn(&first, &alpha, &beta),
            Some(Ordering::Greater)
        );
        assert_eq!(
            plugin.executor_order_fn(&second, &beta, &alpha),
            Some(Ordering::Greater)
        );
    }

    #[test]
    fn orders_by_unique_coverage_then_smaller_id() {
        let session = session(
            "session",
            vec![
                task("session", 1, &[b"a", b"b"]),
                task("session", 2, &[b"b", b"c"]),
            ],
        );
        let plugin = setup(&session);
        let full = executor("z", &[b"a", b"b", b"c", b"extra"]);
        let partial = executor("a", &[b"a", b"b"]);
        assert_eq!(plugin.score(&full, &session), 3);
        assert_eq!(plugin.score(&partial, &session), 2);
        assert_eq!(
            plugin.executor_order_fn(&session, &full, &partial),
            Some(Ordering::Greater)
        );

        let same_score_larger_id = executor("z", &[b"a"]);
        let same_score_smaller_id = executor("a", &[b"b"]);
        assert_eq!(
            plugin.executor_order_fn(&session, &same_score_smaller_id, &same_score_larger_id),
            Some(Ordering::Greater)
        );
    }

    #[test]
    fn setup_rebuilds_affinity_without_stale_keys() {
        let first = session("session", vec![task("session", 1, &[b"old"])]);
        let mut plugin = setup(&first);
        assert_eq!(plugin.score(&executor("old", &[b"old"]), &first), 1);

        let second = session("session", vec![task("session", 2, &[b"new"])]);
        let snapshot = SnapShot::new();
        snapshot.add_session(Arc::new(second.clone())).unwrap();
        plugin.setup(&snapshot).unwrap();

        assert_eq!(plugin.score(&executor("old", &[b"old"]), &second), 0);
        assert_eq!(plugin.score(&executor("new", &[b"new"]), &second), 1);
    }

    #[test]
    fn setup_uses_pending_index_after_fifo_pop() {
        let mut source = Session {
            id: uuid::Uuid::new_v4().to_string(),
            name: "session".to_string(),
            workspace: "default".to_string(),
            ..Default::default()
        };
        source
            .update_task(&Task {
                id: uuid::Uuid::new_v4().to_string(),
                workspace: "default".to_string(),
                name: 1,
                session: "session".to_string(),
                version: 1,
                affinity: HashSet::from([Bytes::from_static(b"popped")]),
                ..Default::default()
            })
            .unwrap();
        source
            .update_task(&Task {
                id: uuid::Uuid::new_v4().to_string(),
                workspace: "default".to_string(),
                name: 2,
                session: "session".to_string(),
                version: 1,
                affinity: HashSet::from([Bytes::from_static(b"pending")]),
                ..Default::default()
            })
            .unwrap();

        source.pop_pending_task().unwrap();
        let session = SessionInfo::try_from(&source).unwrap();
        let plugin = setup(&session);

        assert_eq!(plugin.score(&executor("popped", &[b"popped"]), &session), 0);
        assert_eq!(
            plugin.score(&executor("pending", &[b"pending"]), &session),
            1
        );
    }

    #[test]
    fn zero_score_prefers_smaller_id() {
        let session = session("session", vec![task("session", 1, &[b"wanted"])]);
        let plugin = setup(&session);
        let smaller = executor("a", &[b"other"]);
        let larger = executor("z", &[]);
        assert_eq!(plugin.score(&smaller, &session), 0);
        assert_eq!(plugin.score(&larger, &session), 0);
        assert_eq!(
            plugin.executor_order_fn(&session, &smaller, &larger),
            Some(Ordering::Greater)
        );
    }
}
