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

use crate::model::{ExecutorInfo, SessionInfo, SnapShot};
use crate::scheduler::plugins::{Plugin, PluginPtr};
use common::apis::SessionID;
use common::FlameError;

/// Orders eligible Idle executors by coverage of a session's Pending affinity.
///
/// Equal scores, including no match at all, prefer the executor that publishes fewer attributes: the
/// attributes are the keys an executor holds, so that is the emptier cache, and work without a
/// match lands where it evicts the least. Equal counts prefer the executor that has been Idle the
/// longest (it was used least recently), and equal times fall back to the executor ID.
#[derive(Default)]
pub struct DasPlugin {
    affinity: HashMap<SessionID, HashSet<Bytes>>,
}

impl DasPlugin {
    pub fn new_ptr() -> PluginPtr {
        Box::new(Self::default())
    }

    fn score(&self, executor: &ExecutorInfo, session: &SessionInfo) -> usize {
        self.affinity.get(&session.id).map_or(0, |affinity| {
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
            let affinity = self.affinity.entry(session.id.clone()).or_default();
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
                // Equal scores: fewer attributes, an emptier cache, wins ...
                .then_with(|| e2.attributes.len().cmp(&e1.attributes.len()))
                // ... then the executor Idle the longest, which was updated least recently ...
                .then_with(|| {
                    e2.latest_updated_timestamp
                        .cmp(&e1.latest_updated_timestamp)
                })
                // ... and then a smaller ID, so the order is deterministic.
                .then_with(|| e2.id.cmp(&e1.id)),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use common::apis::{Session, Task, TaskState};
    use std::sync::Arc;

    fn executor(id: &str, attributes: &[&'static [u8]]) -> ExecutorInfo {
        ExecutorInfo {
            id: id.to_string(),
            attributes: attributes.iter().copied().map(Bytes::from_static).collect(),
            ..Default::default()
        }
    }

    fn task(session_id: &str, id: i64, keys: &[&'static [u8]]) -> Task {
        Task {
            id,
            ssn_id: session_id.to_string(),
            version: 1,
            state: TaskState::Pending,
            affinity: keys.iter().copied().map(Bytes::from_static).collect(),
            ..Default::default()
        }
    }

    fn session(id: &str, tasks: Vec<Task>) -> SessionInfo {
        let mut session = Session {
            id: id.to_string(),
            ..Default::default()
        };
        for task in tasks {
            session.update_task(&task).unwrap();
        }
        SessionInfo::try_from(&session).unwrap()
    }

    fn setup(session: &SessionInfo) -> DasPlugin {
        let snapshot = SnapShot::new();
        snapshot.add_session(Arc::new(session.clone())).unwrap();
        let mut plugin = DasPlugin::default();
        plugin.setup(&snapshot).unwrap();
        plugin
    }

    #[test]
    fn orders_by_unique_coverage_then_attributes_then_idle_time_then_smaller_id() {
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
    fn retracted_key_gives_no_affinity_advantage() {
        let session = session("session", vec![task("session", 1, &[b"x"])]);
        let plugin = setup(&session);
        let advertising = executor("a", &[b"x", b"y"]);
        let retracted = executor("b", &[b"y"]);
        assert_eq!(plugin.score(&advertising, &session), 1);
        assert_eq!(plugin.score(&retracted, &session), 0);
        assert_eq!(
            plugin.executor_order_fn(&session, &advertising, &retracted),
            Some(Ordering::Greater)
        );

        // With X retracted from A, the smaller id wins instead of stale residency.
        let evicted = executor("a", &[b"y"]);
        let other = executor("b", &[b"y"]);
        assert_eq!(
            plugin.score(&evicted, &session),
            plugin.score(&other, &session)
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
            id: "session".to_string(),
            ..Default::default()
        };
        source
            .update_task(&Task {
                id: 1,
                ssn_id: "session".to_string(),
                version: 1,
                affinity: HashSet::from([Bytes::from_static(b"popped")]),
                ..Default::default()
            })
            .unwrap();
        source
            .update_task(&Task {
                id: 2,
                ssn_id: "session".to_string(),
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
    fn zero_score_prefers_fewer_attributes() {
        let session = session("session", vec![task("session", 1, &[b"wanted"])]);
        let plugin = setup(&session);
        // No match on either: the emptier executor wins whatever the IDs are.
        let fuller_smaller_id = executor("a", &[b"other"]);
        let emptier_larger_id = executor("z", &[]);
        assert_eq!(plugin.score(&fuller_smaller_id, &session), 0);
        assert_eq!(plugin.score(&emptier_larger_id, &session), 0);
        assert_eq!(
            plugin.executor_order_fn(&session, &emptier_larger_id, &fuller_smaller_id),
            Some(Ordering::Greater)
        );
        assert_eq!(
            plugin.executor_order_fn(&session, &fuller_smaller_id, &emptier_larger_id),
            Some(Ordering::Less)
        );

        // The same number of attributes and idle time: the smaller ID wins.
        let smaller = executor("a", &[b"other"]);
        let larger = executor("z", &[b"another"]);
        assert_eq!(
            plugin.executor_order_fn(&session, &smaller, &larger),
            Some(Ordering::Greater)
        );
    }

    #[test]
    fn equal_attributes_prefer_the_executor_idle_the_longest() {
        let session = session("session", vec![task("session", 1, &[b"wanted"])]);
        let plugin = setup(&session);
        let now = chrono::Utc::now();
        let recently_used = ExecutorInfo {
            latest_updated_timestamp: now,
            ..executor("a", &[b"other"])
        };
        let idle_longer = ExecutorInfo {
            latest_updated_timestamp: now - chrono::Duration::minutes(10),
            ..executor("z", &[b"another"])
        };
        // The longer idle time beats the smaller ID.
        assert_eq!(
            plugin.executor_order_fn(&session, &idle_longer, &recently_used),
            Some(Ordering::Greater)
        );

        // Fewer attributes still beat a longer idle time.
        let sparse_but_recent = ExecutorInfo {
            latest_updated_timestamp: now,
            ..executor("m", &[])
        };
        assert_eq!(
            plugin.executor_order_fn(&session, &sparse_but_recent, &idle_longer),
            Some(Ordering::Greater)
        );
    }

    #[test]
    fn equal_match_prefers_the_executor_with_fewer_other_attributes() {
        let session = session("session", vec![task("session", 1, &[b"wanted"])]);
        let plugin = setup(&session);
        let crowded = executor("a", &[b"wanted", b"x", b"y", b"z"]);
        let sparse = executor("z", &[b"wanted"]);
        assert_eq!(plugin.score(&crowded, &session), 1);
        assert_eq!(plugin.score(&sparse, &session), 1);
        assert_eq!(
            plugin.executor_order_fn(&session, &sparse, &crowded),
            Some(Ordering::Greater)
        );

        // A better match still beats a sparser executor.
        let two_keys = session_with_keys(&[b"p", b"q"]);
        let plugin = setup(&two_keys);
        let full = executor("z", &[b"p", b"q", b"x", b"y"]);
        let partial = executor("a", &[b"p"]);
        assert_eq!(
            plugin.executor_order_fn(&two_keys, &full, &partial),
            Some(Ordering::Greater)
        );
    }

    fn session_with_keys(keys: &[&'static [u8]]) -> SessionInfo {
        session("session", vec![task("session", 1, keys)])
    }
}
