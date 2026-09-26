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
use std::sync::Arc;

use tokio::sync::{broadcast, watch, Notify};
use tokio::time::Duration;

use common::apis::{ExecutorID, SessionID, TaskID};
use common::FlameError;
use stdng::{lock_ptr, MutexPtr};

const SESSION_TASK_UPDATE_CAPACITY: usize = 1024;

struct WatchChannel {
    tx: watch::Sender<u64>,
    rx: watch::Receiver<u64>,
}

impl WatchChannel {
    fn new() -> Self {
        let (tx, rx) = watch::channel(0u64);
        Self { tx, rx }
    }

    fn notify(&self) {
        self.tx.send_modify(|v| *v = v.wrapping_add(1));
    }

    fn subscribe(&self) -> watch::Receiver<u64> {
        self.rx.clone()
    }
}

#[derive(Clone)]
pub struct TaskNotifier {
    channels: MutexPtr<HashMap<SessionID, broadcast::Sender<TaskID>>>,
}

pub struct TaskSubscription {
    receiver: Option<broadcast::Receiver<TaskID>>,
    channels: MutexPtr<HashMap<SessionID, broadcast::Sender<TaskID>>>,
    session_id: SessionID,
}

impl TaskSubscription {
    pub async fn recv(&mut self) -> Result<TaskID, broadcast::error::RecvError> {
        self.receiver
            .as_mut()
            .expect("task subscription closed")
            .recv()
            .await
    }

    #[cfg(test)]
    pub fn try_recv(&mut self) -> Result<TaskID, broadcast::error::TryRecvError> {
        self.receiver
            .as_mut()
            .expect("task subscription closed")
            .try_recv()
    }
}

impl Drop for TaskSubscription {
    fn drop(&mut self) {
        // Dropping the receiver first makes receiver_count reflect this watcher.
        self.receiver.take();
        if let Ok(mut channels) = lock_ptr!(self.channels) {
            if channels
                .get(&self.session_id)
                .is_some_and(|sender| sender.receiver_count() == 0)
            {
                channels.remove(&self.session_id);
            }
        }
    }
}

impl TaskNotifier {
    pub fn new() -> Self {
        Self {
            channels: stdng::new_ptr(HashMap::new()),
        }
    }

    pub fn subscribe(&self, ssn_id: &SessionID) -> Result<TaskSubscription, FlameError> {
        let mut channels = lock_ptr!(self.channels)?;
        let sender = channels.entry(ssn_id.clone()).or_insert_with(|| {
            let (sender, _) = broadcast::channel(SESSION_TASK_UPDATE_CAPACITY);
            sender
        });
        Ok(TaskSubscription {
            receiver: Some(sender.subscribe()),
            channels: self.channels.clone(),
            session_id: ssn_id.clone(),
        })
    }

    pub fn notify(&self, ssn_id: &SessionID, task_id: TaskID) -> Result<(), FlameError> {
        let channels = lock_ptr!(self.channels)?;
        if let Some(sender) = channels.get(ssn_id) {
            let _ = sender.send(task_id);
        }
        Ok(())
    }

    pub fn remove(&self, ssn_id: &SessionID) -> Result<(), FlameError> {
        let mut channels = lock_ptr!(self.channels)?;
        channels.remove(ssn_id);
        Ok(())
    }
}

impl Default for TaskNotifier {
    fn default() -> Self {
        Self::new()
    }
}

#[derive(Clone)]
pub struct ExecutorNotifier {
    channels: MutexPtr<HashMap<ExecutorID, Arc<WatchChannel>>>,
}

impl ExecutorNotifier {
    pub fn new() -> Self {
        Self {
            channels: stdng::new_ptr(HashMap::new()),
        }
    }

    fn get_or_create_channel(&self, id: &ExecutorID) -> Result<Arc<WatchChannel>, FlameError> {
        let mut channels = lock_ptr!(self.channels)?;
        Ok(channels
            .entry(id.clone())
            .or_insert_with(|| Arc::new(WatchChannel::new()))
            .clone())
    }

    pub fn subscribe(&self, id: &ExecutorID) -> Result<watch::Receiver<u64>, FlameError> {
        let channel = self.get_or_create_channel(id)?;
        Ok(channel.subscribe())
    }

    pub fn notify(&self, id: &ExecutorID) -> Result<(), FlameError> {
        let channel = self.get_or_create_channel(id)?;
        channel.notify();
        Ok(())
    }

    pub fn remove(&self, id: &ExecutorID) -> Result<(), FlameError> {
        let mut channels = lock_ptr!(self.channels)?;
        channels.remove(id);
        Ok(())
    }
}

impl Default for ExecutorNotifier {
    fn default() -> Self {
        Self::new()
    }
}

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

        #[tokio::test]
        async fn session_updates_include_task_ids_and_close_wakeup() {
            let notifier = TaskNotifier::new();
            let session = "session-1".to_string();
            let mut updates = notifier.subscribe(&session).unwrap();

            notifier.notify(&session, 42).unwrap();
            notifier.notify(&session, 0).unwrap();
            assert_eq!(updates.recv().await.unwrap(), 42);
            assert_eq!(updates.recv().await.unwrap(), 0);
            assert!(matches!(
                updates.try_recv(),
                Err(broadcast::error::TryRecvError::Empty)
            ));
        }

        #[tokio::test]
        async fn session_updates_report_lag_for_reconciliation() {
            let notifier = TaskNotifier::new();
            let session = "session-1".to_string();
            let mut updates = notifier.subscribe(&session).unwrap();

            for _ in 0..=SESSION_TASK_UPDATE_CAPACITY {
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

        #[test]
        fn test_subscribe_creates_entry() {
            let notifier = ExecutorNotifier::new();
            let id = "executor-1".to_string();

            let rx = notifier.subscribe(&id).unwrap();
            assert_eq!(*rx.borrow(), 0);
        }

        #[test]
        fn test_notify_increments_version() {
            let notifier = ExecutorNotifier::new();
            let id = "executor-1".to_string();

            let rx = notifier.subscribe(&id).unwrap();
            notifier.notify(&id).unwrap();

            assert_eq!(*rx.borrow(), 1);
        }

        #[tokio::test]
        async fn test_notify_wakes_waiter() {
            let notifier = ExecutorNotifier::new();
            let id = "executor-1".to_string();

            let mut rx = notifier.subscribe(&id).unwrap();

            let id_clone = id.clone();
            let notifier_clone = notifier.clone();
            tokio::spawn(async move {
                tokio::time::sleep(tokio::time::Duration::from_millis(10)).await;
                notifier_clone.notify(&id_clone).unwrap();
            });

            tokio::time::timeout(tokio::time::Duration::from_millis(100), rx.changed())
                .await
                .expect("should complete")
                .expect("channel should not close");

            assert_eq!(*rx.borrow(), 1);
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
