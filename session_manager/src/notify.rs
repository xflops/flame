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

use std::collections::HashMap;
use std::hash::Hash;
use std::sync::Arc;

use tokio::sync::{broadcast, Notify};
use tokio::time::Duration;

use common::apis::{ExecutorID, SessionID, TaskID};
use common::FlameError;
use stdng::{lock_ptr, MutexPtr};

const NOTIFICATION_CAPACITY: usize = 1024;

/// Keyed event fanout. Channels exist only while subscribers are present.
#[derive(Clone)]
pub struct Notifier<K, V> {
    channels: MutexPtr<HashMap<K, broadcast::Sender<V>>>,
}

pub struct Subscription<K: Eq + Hash, V> {
    receiver: Option<broadcast::Receiver<V>>,
    channels: MutexPtr<HashMap<K, broadcast::Sender<V>>>,
    key: K,
}

impl<K: Eq + Hash, V: Clone> Subscription<K, V> {
    pub async fn recv(&mut self) -> Result<V, broadcast::error::RecvError> {
        self.receiver
            .as_mut()
            .expect("notification subscription closed")
            .recv()
            .await
    }

    #[cfg(test)]
    pub fn try_recv(&mut self) -> Result<V, broadcast::error::TryRecvError> {
        self.receiver
            .as_mut()
            .expect("notification subscription closed")
            .try_recv()
    }
}

impl<K: Eq + Hash, V> Drop for Subscription<K, V> {
    fn drop(&mut self) {
        // Dropping the receiver first makes receiver_count reflect this watcher.
        self.receiver.take();
        if let Ok(mut channels) = lock_ptr!(self.channels) {
            if channels
                .get(&self.key)
                .is_some_and(|sender| sender.receiver_count() == 0)
            {
                channels.remove(&self.key);
            }
        }
    }
}

impl<K: Eq + Hash + Clone, V: Clone> Notifier<K, V> {
    pub fn new() -> Self {
        Self {
            channels: stdng::new_ptr(HashMap::new()),
        }
    }

    pub fn subscribe(&self, key: &K) -> Result<Subscription<K, V>, FlameError> {
        let mut channels = lock_ptr!(self.channels)?;
        let sender = channels.entry(key.clone()).or_insert_with(|| {
            let (sender, _) = broadcast::channel(NOTIFICATION_CAPACITY);
            sender
        });
        Ok(Subscription {
            receiver: Some(sender.subscribe()),
            channels: self.channels.clone(),
            key: key.clone(),
        })
    }

    pub fn notify(&self, key: &K, event: V) -> Result<(), FlameError> {
        let channels = lock_ptr!(self.channels)?;
        if let Some(sender) = channels.get(key) {
            let _ = sender.send(event);
        }
        Ok(())
    }

    pub fn remove(&self, key: &K) -> Result<(), FlameError> {
        let mut channels = lock_ptr!(self.channels)?;
        channels.remove(key);
        Ok(())
    }
}

impl<K: Eq + Hash + Clone, V: Clone> Default for Notifier<K, V> {
    fn default() -> Self {
        Self::new()
    }
}

pub type TaskNotifier = Notifier<SessionID, TaskID>;
pub type TaskSubscription = Subscription<SessionID, TaskID>;
pub type ExecutorNotifier = Notifier<ExecutorID, ExecutorID>;

#[derive(Default)]
pub struct SchedulerNotifier {
    notification: Notify,
}

impl SchedulerNotifier {
    pub fn notify(&self) {
        // `Notify` stores at most one permit, which coalesces bursts of state
        // changes into one additional scheduling pass.
        self.notification.notify_one();
    }

    pub async fn wait(&self, interval: Duration) {
        tokio::select! {
            _ = self.notification.notified() => {}
            _ = tokio::time::sleep(interval) => {}
        }
    }
}

pub type NotifyManagerPtr = Arc<NotifyManager>;

pub struct NotifyManager {
    pub tasks: TaskNotifier,
    pub executors: ExecutorNotifier,
    pub scheduler: SchedulerNotifier,
}

impl NotifyManager {
    pub fn new() -> Self {
        Self {
            tasks: TaskNotifier::new(),
            executors: ExecutorNotifier::new(),
            scheduler: SchedulerNotifier::default(),
        }
    }

    pub fn new_ptr() -> NotifyManagerPtr {
        Arc::new(Self::new())
    }
}

impl Default for NotifyManager {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test(start_paused = true)]
    async fn scheduler_notification_wakes_early_and_coalesces() {
        use tokio::time::{Duration, Instant};

        let notifier = SchedulerNotifier::default();
        notifier.notify();
        notifier.notify();

        let before_event = Instant::now();
        notifier.wait(Duration::from_millis(100)).await;
        assert_eq!(Instant::now() - before_event, Duration::ZERO);

        let before_timeout = Instant::now();
        notifier.wait(Duration::from_millis(100)).await;
        assert_eq!(Instant::now() - before_timeout, Duration::from_millis(100));
    }

    mod task_notifier_tests {
        use super::*;

        #[tokio::test]
        async fn scheduler_subscribers_all_wake() {
            let notifier = TaskNotifier::new();
            let session = "session-1".to_string();
            let mut first = notifier.subscribe(&session).unwrap();
            let mut second = notifier.subscribe(&session).unwrap();

            notifier.notify(&session, 42).unwrap();
            assert_eq!(first.recv().await.unwrap(), 42);
            assert_eq!(second.recv().await.unwrap(), 42);
        }

        #[test]
        fn task_watchers_share_one_channel_per_session() {
            let notifier = TaskNotifier::new();
            let session = "session-1".to_string();
            let first = notifier.subscribe(&session).unwrap();
            let second = notifier.subscribe(&session).unwrap();
            let other = notifier.subscribe(&"session-2".to_string()).unwrap();

            let channels = lock_ptr!(notifier.channels).unwrap();
            assert_eq!(channels.len(), 2);
            assert_eq!(channels.get(&session).unwrap().receiver_count(), 2);
            drop(channels);
            drop((first, second, other));
        }

        #[tokio::test]
        async fn session_updates_include_task_ids_and_close_on_removal() {
            let notifier = TaskNotifier::new();
            let session = "session-1".to_string();
            let mut updates = notifier.subscribe(&session).unwrap();

            notifier.notify(&session, 42).unwrap();
            notifier.remove(&session).unwrap();
            assert_eq!(updates.recv().await.unwrap(), 42);
            assert!(matches!(
                updates.recv().await,
                Err(broadcast::error::RecvError::Closed)
            ));
        }

        #[tokio::test]
        async fn session_updates_report_lag_for_reconciliation() {
            let notifier = TaskNotifier::new();
            let session = "session-1".to_string();
            let mut updates = notifier.subscribe(&session).unwrap();

            for _ in 0..=NOTIFICATION_CAPACITY {
                notifier.notify(&session, 42).unwrap();
            }
            assert!(matches!(
                updates.recv().await,
                Err(broadcast::error::RecvError::Lagged(1))
            ));
            assert_eq!(updates.recv().await.unwrap(), 42);
        }

        #[test]
        fn task_updates_without_subscribers_do_not_allocate_channels() {
            let notifier = TaskNotifier::new();
            let session = "session-1".to_string();
            for task_id in 1..=1000 {
                notifier.notify(&session, task_id).unwrap();
            }
            assert!(lock_ptr!(notifier.channels).unwrap().is_empty());
        }

        #[test]
        fn last_subscription_drop_releases_channel() {
            let notifier = TaskNotifier::new();
            let session = "invalid-session".to_string();
            let first = notifier.subscribe(&session).unwrap();
            let second = notifier.subscribe(&session).unwrap();
            drop(first);
            assert!(lock_ptr!(notifier.channels).unwrap().contains_key(&session));
            drop(second);
            assert!(!lock_ptr!(notifier.channels).unwrap().contains_key(&session));
        }

        #[test]
        fn old_subscription_cannot_remove_recreated_channel() {
            let notifier = TaskNotifier::new();
            let session = "recreated-session".to_string();
            let old = notifier.subscribe(&session).unwrap();
            notifier.remove(&session).unwrap();
            let current = notifier.subscribe(&session).unwrap();
            drop(old);
            assert!(lock_ptr!(notifier.channels).unwrap().contains_key(&session));
            drop(current);
            assert!(!lock_ptr!(notifier.channels).unwrap().contains_key(&session));
        }

        #[tokio::test]
        async fn removing_session_closes_its_update_stream() {
            let notifier = TaskNotifier::new();
            let session = "session-1".to_string();
            let mut updates = notifier.subscribe(&session).unwrap();
            notifier.remove(&session).unwrap();
            assert!(matches!(
                updates.recv().await,
                Err(broadcast::error::RecvError::Closed)
            ));
        }
    }

    mod executor_notifier_tests {
        use super::*;

        #[tokio::test]
        async fn executor_subscribers_all_wake() {
            let notifier = ExecutorNotifier::new();
            let id = "executor-1".to_string();
            let mut first = notifier.subscribe(&id).unwrap();
            let mut second = notifier.subscribe(&id).unwrap();
            notifier.notify(&id, id.clone()).unwrap();
            assert_eq!(first.recv().await, Ok(id.clone()));
            assert_eq!(second.recv().await, Ok(id.clone()));
        }

        #[test]
        fn executor_updates_without_subscribers_do_not_allocate_channels() {
            let notifier = ExecutorNotifier::new();
            notifier
                .notify(&"executor-1".to_string(), "executor-1".to_string())
                .unwrap();
            assert!(lock_ptr!(notifier.channels).unwrap().is_empty());
        }

        #[tokio::test]
        async fn executor_removal_closes_stream_and_old_drop_preserves_replacement() {
            let notifier = ExecutorNotifier::new();
            let id = "executor-1".to_string();
            let mut old = notifier.subscribe(&id).unwrap();
            notifier.remove(&id).unwrap();
            assert_eq!(old.recv().await, Err(broadcast::error::RecvError::Closed));
            let mut current = notifier.subscribe(&id).unwrap();
            drop(old);
            notifier.notify(&id, id.clone()).unwrap();
            assert_eq!(current.recv().await, Ok(id.clone()));
            drop(current);
            assert!(lock_ptr!(notifier.channels).unwrap().is_empty());
        }

        #[tokio::test]
        async fn executor_updates_report_lag_and_resume() {
            let notifier = ExecutorNotifier::new();
            let id = "executor-1".to_string();
            let mut updates = notifier.subscribe(&id).unwrap();
            for _ in 0..=NOTIFICATION_CAPACITY {
                notifier.notify(&id, id.clone()).unwrap();
            }
            assert_eq!(
                updates.recv().await,
                Err(broadcast::error::RecvError::Lagged(1))
            );
            assert_eq!(updates.recv().await, Ok(id.clone()));
        }
    }

    mod notify_manager_tests {
        use super::*;

        #[test]
        fn test_new_creates_empty_notifiers() {
            let manager = NotifyManager::new();

            manager.tasks.subscribe(&"session-1".to_string()).unwrap();
            manager
                .executors
                .subscribe(&"executor-1".to_string())
                .unwrap();
        }

        #[test]
        fn test_new_ptr_creates_arc() {
            let manager = NotifyManager::new_ptr();
            assert!(Arc::strong_count(&manager) == 1);
        }
    }
}
