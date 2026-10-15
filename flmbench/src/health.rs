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

use std::time::Duration;

use flame_rs::apis::FlameError;
use flame_rs::client::{Connection, NodeState};
use serde_derive::Serialize;
use tokio::sync::oneshot;
use tokio::task::JoinHandle;

/// What the cluster looked like while a round ran. Node names are not recorded.
#[derive(Debug, Clone, Default, PartialEq, Serialize)]
pub struct HealthSummary {
    pub samples: u32,
    pub sample_errors: u32,
    pub nodes: usize,
    pub min_ready_nodes: usize,
    pub not_ready_samples: u32,
    pub max_executors: usize,
}

impl HealthSummary {
    fn record(&mut self, ready: usize, nodes: usize, executors: usize) {
        self.min_ready_nodes = if self.samples == 0 {
            ready
        } else {
            self.min_ready_nodes.min(ready)
        };
        self.samples += 1;
        self.nodes = self.nodes.max(nodes);
        if ready < nodes {
            self.not_ready_samples += 1;
        }
        self.max_executors = self.max_executors.max(executors);
    }
}

async fn sample(connection: &Connection) -> Result<(usize, usize, usize), FlameError> {
    let nodes = connection.list_nodes().await?;
    let executors = connection.list_executors().await?;
    let ready = nodes
        .iter()
        .filter(|node| node.state == NodeState::Ready)
        .count();
    Ok((ready, nodes.len(), executors.len()))
}

/// Polls node and executor state in the background until [`finish`](Self::finish).
pub struct HealthSampler {
    stop: oneshot::Sender<()>,
    task: JoinHandle<HealthSummary>,
}

impl HealthSampler {
    pub fn start(connection: Connection, interval: Duration) -> Self {
        let (stop, mut stopped) = oneshot::channel();
        let task = tokio::spawn(async move {
            let mut summary = HealthSummary::default();
            loop {
                match sample(&connection).await {
                    Ok((ready, nodes, executors)) => summary.record(ready, nodes, executors),
                    Err(_) => summary.sample_errors += 1,
                }
                tokio::select! {
                    _ = tokio::time::sleep(interval) => {}
                    _ = &mut stopped => break,
                }
            }
            match sample(&connection).await {
                Ok((ready, nodes, executors)) => summary.record(ready, nodes, executors),
                Err(_) => summary.sample_errors += 1,
            }
            summary
        });
        Self { stop, task }
    }

    pub async fn finish(self) -> HealthSummary {
        let _ = self.stop.send(());
        self.task.await.unwrap_or_default()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn records_extremes_across_samples() {
        let mut summary = HealthSummary::default();
        summary.record(10, 10, 4);
        summary.record(9, 10, 12);
        summary.record(10, 10, 8);
        assert_eq!(summary.samples, 3);
        assert_eq!(summary.nodes, 10);
        assert_eq!(summary.min_ready_nodes, 9);
        assert_eq!(summary.not_ready_samples, 1);
        assert_eq!(summary.max_executors, 12);
    }

    #[test]
    fn first_sample_sets_the_ready_floor() {
        let mut summary = HealthSummary::default();
        summary.record(3, 3, 0);
        assert_eq!(summary.min_ready_nodes, 3);
        assert_eq!(summary.not_ready_samples, 0);
    }
}
