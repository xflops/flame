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

use tokio::sync::watch;

use common::apis::{ExecutorID, SessionID, TaskID};
use common::FlameError;
use stdng::{lock_ptr, MutexPtr};

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
    channels: MutexPtr<HashMap<SessionID, HashMap<TaskID, Arc<WatchChannel>>>>,
}

impl TaskNotifier {
    pub fn new() -> Self {
        Self {
            channels: stdng::new_ptr(HashMap::new()),
        }
    }

    fn get_or_create_channel(
        &self,
        ssn_id: &SessionID,
        task_id: TaskID,
    ) -> Result<Arc<WatchChannel>, FlameError> {
        let mut channels = lock_ptr!(self.channels)?;
        let session_channels = channels.entry(ssn_id.clone()).or_default();
        Ok(session_channels
            .entry(task_id)
            .or_insert_with(|| Arc::new(WatchChannel::new()))
            .clone())
    }

    pub fn subscribe(
        &self,
        ssn_id: &SessionID,
        task_id: TaskID,
    ) -> Result<watch::Receiver<u64>, FlameError> {
        let channel = self.get_or_create_channel(ssn_id, task_id)?;
        Ok(channel.subscribe())
    }

    pub fn notify(&self, ssn_id: &SessionID, task_id: TaskID) -> Result<(), FlameError> {
        let channel = self.get_or_create_channel(ssn_id, task_id)?;
        channel.notify();
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

pub type NotifyManagerPtr = Arc<NotifyManager>;

pub struct NotifyManager {
    pub tasks: TaskNotifier,
    pub executors: ExecutorNotifier,
}

impl NotifyManager {
    pub fn new() -> Self {
        Self {
            tasks: TaskNotifier::new(),
            executors: ExecutorNotifier::new(),
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

    mod task_notifier_tests {
        use super::*;

        #[test]
        fn test_subscribe_creates_entry() {
            let notifier = TaskNotifier::new();
            let ssn_id = "session-1".to_string();

            let rx = notifier.subscribe(&ssn_id, 1).unwrap();
            assert_eq!(*rx.borrow(), 0);
        }

        #[test]
        fn test_subscribe_session_level() {
            let notifier = TaskNotifier::new();
            let ssn_id = "session-1".to_string();

            let session_rx = notifier.subscribe(&ssn_id, 0).unwrap();
            let task_rx = notifier.subscribe(&ssn_id, 1).unwrap();

            assert_eq!(*session_rx.borrow(), 0);
            assert_eq!(*task_rx.borrow(), 0);
        }

        #[test]
        fn test_notify_increments_version() {
            let notifier = TaskNotifier::new();
            let ssn_id = "session-1".to_string();

            let rx = notifier.subscribe(&ssn_id, 1).unwrap();
            assert_eq!(*rx.borrow(), 0);

            notifier.notify(&ssn_id, 1).unwrap();

            assert_eq!(*rx.borrow(), 1);
        }

        #[test]
        fn test_remove_clears_session() {
            let notifier = TaskNotifier::new();
            let ssn_id = "session-1".to_string();

            notifier.subscribe(&ssn_id, 0).unwrap();
            notifier.subscribe(&ssn_id, 1).unwrap();

            notifier.remove(&ssn_id).unwrap();

            let rx = notifier.subscribe(&ssn_id, 1).unwrap();
            assert_eq!(*rx.borrow(), 0);
        }

        #[tokio::test]
        async fn test_notify_wakes_waiter() {
            let notifier = TaskNotifier::new();
            let ssn_id = "session-1".to_string();

            let mut rx = notifier.subscribe(&ssn_id, 1).unwrap();

            let ssn_id_clone = ssn_id.clone();
            let notifier_clone = notifier.clone();
            tokio::spawn(async move {
                tokio::time::sleep(tokio::time::Duration::from_millis(10)).await;
                notifier_clone.notify(&ssn_id_clone, 1).unwrap();
            });

            tokio::time::timeout(tokio::time::Duration::from_millis(100), rx.changed())
                .await
                .expect("should complete")
                .expect("channel should not close");

            assert_eq!(*rx.borrow(), 1);
        }

        #[tokio::test]
        async fn test_notify_before_subscribe_not_lost() {
            let notifier = TaskNotifier::new();
            let ssn_id = "session-1".to_string();

            notifier.notify(&ssn_id, 1).unwrap();

            let rx = notifier.subscribe(&ssn_id, 1).unwrap();
            assert_eq!(*rx.borrow(), 1);
        }

        #[tokio::test]
        async fn test_multiple_subscribers_all_notified() {
            let notifier = TaskNotifier::new();
            let ssn_id = "session-1".to_string();

            let mut rx1 = notifier.subscribe(&ssn_id, 0).unwrap();
            let mut rx2 = notifier.subscribe(&ssn_id, 0).unwrap();
            let mut rx3 = notifier.subscribe(&ssn_id, 0).unwrap();

            notifier.notify(&ssn_id, 0).unwrap();

            rx1.changed().await.unwrap();
            rx2.changed().await.unwrap();
            rx3.changed().await.unwrap();

            assert_eq!(*rx1.borrow(), 1);
            assert_eq!(*rx2.borrow(), 1);
            assert_eq!(*rx3.borrow(), 1);
        }

        #[tokio::test]
        async fn test_fresh_subscribe_changed_blocks_without_notify() {
            let notifier = TaskNotifier::new();
            let ssn_id = "session-1".to_string();

            let mut rx = notifier.subscribe(&ssn_id, 1).unwrap();

            // NO notify - just call changed() immediately after subscribe
            // This should BLOCK (timeout) because no notification was sent
            let result =
                tokio::time::timeout(tokio::time::Duration::from_millis(50), rx.changed()).await;
            assert!(
                result.is_err(),
                "changed() should block/timeout without any notify"
            );
        }

        #[tokio::test]
        async fn test_changed_twice_blocks_second_time() {
            let notifier = TaskNotifier::new();
            let ssn_id = "session-1".to_string();

            let mut rx = notifier.subscribe(&ssn_id, 1).unwrap();

            notifier.notify(&ssn_id, 1).unwrap();

            // First changed() should return immediately
            let result1 =
                tokio::time::timeout(tokio::time::Duration::from_millis(50), rx.changed()).await;
            assert!(
                result1.is_ok(),
                "First changed() should complete immediately"
            );

            // Second changed() should block (no new notification)
            let result2 =
                tokio::time::timeout(tokio::time::Duration::from_millis(50), rx.changed()).await;
            assert!(result2.is_err(), "Second changed() should timeout (block)");
        }

        #[tokio::test]
        async fn test_multiple_consumers_wait_one_notify() {
            let notifier = TaskNotifier::new();
            let ssn_id = "session-1".to_string();

            let mut rx1 = notifier.subscribe(&ssn_id, 0).unwrap();
            let mut rx2 = notifier.subscribe(&ssn_id, 0).unwrap();
            let mut rx3 = notifier.subscribe(&ssn_id, 0).unwrap();

            let notifier_clone = notifier.clone();
            let ssn_id_clone = ssn_id.clone();

            // Spawn 3 waiters
            let h1 = tokio::spawn(async move { rx1.changed().await });
            let h2 = tokio::spawn(async move { rx2.changed().await });
            let h3 = tokio::spawn(async move { rx3.changed().await });

            // Give waiters time to start
            tokio::time::sleep(tokio::time::Duration::from_millis(10)).await;

            // Single notify
            notifier_clone.notify(&ssn_id_clone, 0).unwrap();

            // All 3 should wake up
            let results = tokio::time::timeout(tokio::time::Duration::from_millis(100), async {
                let r1 = h1.await.unwrap();
                let r2 = h2.await.unwrap();
                let r3 = h3.await.unwrap();
                (r1, r2, r3)
            })
            .await;

            assert!(
                results.is_ok(),
                "All 3 consumers should wake up from single notify"
            );
            let (r1, r2, r3) = results.unwrap();
            assert!(
                r1.is_ok() && r2.is_ok() && r3.is_ok(),
                "All changed() should succeed"
            );
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

            manager
                .tasks
                .subscribe(&"session-1".to_string(), 1)
                .unwrap();
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
