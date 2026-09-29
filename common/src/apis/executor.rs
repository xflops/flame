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

use super::{ExecutorID, ExecutorState, ResourceRequirement, SessionID, Shim, TaskID};
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
}

impl Executor {
    pub fn set_state(&mut self, state: ExecutorState) {
        self.state = state;
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
