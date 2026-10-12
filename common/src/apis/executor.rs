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

use bytes::Bytes;
use chrono::{DateTime, Utc};
use stdng::MutexPtr;

use super::{ExecutorID, ExecutorState, ResourceRequirement, SessionID, Shim, Signal, TaskID};
use rpc::flame::v1 as rpc;

#[derive(Clone, Debug)]
pub struct Executor {
    pub id: ExecutorID,
    pub node: String,
    pub resreq: ResourceRequirement,
    pub shim: Shim,
    /// Persisted owner of the retained service instance, also carried by RPC.
    pub application: String,
    pub task_id: Option<TaskID>,
    pub ssn_id: Option<SessionID>,
    /// Volatile instance attributes, intentionally omitted from storage/RPC.
    pub attributes: HashSet<Bytes>,

    pub creation_time: DateTime<Utc>,
    /// Volatile lifecycle timestamp; intentionally omitted from storage and RPC.
    pub latest_updated_timestamp: DateTime<Utc>,
    pub state: ExecutorState,
    /// Volatile control signal, intentionally omitted from storage and RPC.
    pub signal: Option<Signal>,
}

impl Executor {
    pub fn is_interrupted(&self) -> bool {
        self.signal == Some(Signal::Interrupt)
    }

    /// Whether this executor is leaving or no longer owns the session binding.
    pub fn is_unbinding(&self, ssn_id: &SessionID) -> bool {
        matches!(
            self.state,
            ExecutorState::Unbinding | ExecutorState::Releasing | ExecutorState::Released
        ) || self.ssn_id.as_ref() != Some(ssn_id)
    }

    pub fn set_state(&mut self, state: ExecutorState) {
        self.state = state;
        if state == ExecutorState::Idle {
            // Keep the signal through unbinding, then clear it so the executor
            // can be reused without interrupting its next session binding.
            self.signal = None;
        }
        self.latest_updated_timestamp = Utc::now();
    }
}

impl Default for Executor {
    fn default() -> Self {
        Executor {
            id: String::new(),
            node: String::new(),
            resreq: ResourceRequirement::default(),
            shim: Shim::Host,
            application: String::new(),
            signal: None,
            task_id: None,
            ssn_id: None,
            attributes: HashSet::new(),
            creation_time: Utc::now(),
            latest_updated_timestamp: Utc::now(),
            state: ExecutorState::default(),
        }
    }
}

pub type ExecutorPtr = MutexPtr<Executor>;

impl From<rpc::Executor> for Executor {
    fn from(e: rpc::Executor) -> Self {
        Executor::from(&e)
    }
}

impl From<&rpc::Executor> for Executor {
    fn from(e: &rpc::Executor) -> Self {
        let spec = e.spec.clone().unwrap();
        let status = e.status.clone().unwrap();
        let metadata = e.metadata.clone().unwrap();

        let state = rpc::ExecutorState::try_from(status.state).unwrap().into();

        Executor {
            id: metadata.id.clone(),
            node: spec.node.clone(),
            resreq: spec.resreq.unwrap().into(),
            shim: Shim::from(spec.shim()),
            application: spec.application.clone(),
            signal: None,
            task_id: None,
            ssn_id: None,
            attributes: HashSet::new(),
            creation_time: Utc::now(),
            latest_updated_timestamp: Utc::now(),
            state,
        }
    }
}

impl From<Executor> for rpc::Executor {
    fn from(e: Executor) -> Self {
        rpc::Executor::from(&e)
    }
}

impl From<&Executor> for rpc::Executor {
    fn from(e: &Executor) -> Self {
        let metadata = Some(rpc::Metadata {
            id: e.id.clone(),
            name: e.id.clone(),
        });

        let spec = Some(rpc::ExecutorSpec {
            resreq: Some(e.resreq.clone().into()),
            node: e.node.clone(),
            shim: rpc::Shim::from(e.shim).into(), // Include shim in spec
            application: e.application.clone(),
        });

        let status = Some(rpc::ExecutorStatus {
            state: rpc::ExecutorState::from(e.state).into(),
            session_id: e.ssn_id.clone(),
        });

        rpc::Executor {
            metadata,
            spec,
            status,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unbinding_detects_departure_and_session_ownership_change() {
        let session_id = "session".to_string();
        for (state, departing) in [
            (ExecutorState::Binding, false),
            (ExecutorState::Bound, false),
            (ExecutorState::Unbinding, true),
            (ExecutorState::Releasing, true),
            (ExecutorState::Released, true),
        ] {
            let mut executor = Executor {
                state,
                ssn_id: Some(session_id.clone()),
                ..Default::default()
            };
            assert_eq!(executor.is_unbinding(&session_id), departing);
            executor.ssn_id = Some("other-session".into());
            assert!(executor.is_unbinding(&session_id));
            executor.ssn_id = None;
            assert!(executor.is_unbinding(&session_id));
        }
    }

    #[test]
    fn interrupt_keeps_executor_bound_until_normal_lifecycle_transition() {
        let session_id = "session".to_string();
        let mut executor = Executor {
            state: ExecutorState::Bound,
            ssn_id: Some(session_id.clone()),
            signal: Some(Signal::Interrupt),
            ..Default::default()
        };

        assert!(executor.is_interrupted());
        assert!(!executor.is_unbinding(&session_id));
        assert_eq!(executor.state, ExecutorState::Bound);
        executor.set_state(ExecutorState::Bound);
        assert!(executor.is_interrupted());
        executor.set_state(ExecutorState::Unbinding);
        assert!(executor.is_interrupted());
        assert!(executor.is_unbinding(&session_id));
        executor.set_state(ExecutorState::Idle);
        assert!(!executor.is_interrupted());
        executor.set_state(ExecutorState::Bound);
        assert!(!executor.is_unbinding(&session_id));
    }

    #[test]
    fn rpc_does_not_restore_volatile_signal() {
        let executor = Executor {
            signal: Some(Signal::Interrupt),
            ..Default::default()
        };
        let restored = Executor::from(rpc::Executor::from(executor));
        assert_eq!(restored.signal, None);
    }
}
