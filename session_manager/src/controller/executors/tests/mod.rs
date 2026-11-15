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

use super::*;
use crate::model::Executor;
use chrono::Utc;
use common::apis::{ApplicationAttributes, ResourceRequirement, SessionAttributes, Shim};
use common::ctx::{FlameCluster, FlameClusterContext, FlameExecutors, FlameLimits};

fn create_test_executor(id: &str, state: ExecutorState) -> ExecutorPtr {
    new_ptr(Executor {
        id: id.to_string(),
        node: "test-node".to_string(),
        resreq: ResourceRequirement::default(),
        shim: Shim::Host,
        application: String::new(),
        task_id: None,
        signal: None,
        ssn_id: None,
        attributes: Default::default(),
        creation_time: Utc::now(),
        latest_updated_timestamp: Utc::now(),
        state,
    })
}

fn get_state(exe_ptr: &ExecutorPtr) -> Result<ExecutorState, FlameError> {
    let exe = lock_ptr!(exe_ptr)?;
    Ok(exe.state)
}

async fn create_mock_storage() -> StoragePtr {
    let unique_id = format!(
        "{}_{:?}",
        chrono::Utc::now().timestamp_nanos_opt().unwrap_or(0),
        std::thread::current().id()
    );
    let test_dir = std::env::temp_dir().join(format!("flame_exec_test_{}", unique_id));
    std::fs::create_dir_all(&test_dir).unwrap();
    let url = format!("fs://{}", test_dir.display());

    let ctx = FlameClusterContext {
        cluster: FlameCluster {
            name: "test".to_string(),
            endpoint: "http://localhost:8080".to_string(),
            advertise: None,
            storage: url,
            resreq: None,
            policies: vec!["priority".to_string(), "drf".to_string()],
            schedule_interval: 1000,
            executors: FlameExecutors::default(),
            tls: None,
            limits: FlameLimits {
                max_sessions: None,
                node: common::ctx::FlameNodeLimits { max_executors: 10 },
                ..Default::default()
            },
            recovery: Default::default(),
            pprof: None,
        },
        cache: None,
    };

    crate::storage::new_ptr(&ctx).await.unwrap()
}

fn unique_test_id(prefix: &str) -> String {
    format!(
        "{}-{}-{:?}",
        prefix,
        chrono::Utc::now().timestamp_nanos_opt().unwrap_or(0),
        std::thread::current().id()
    )
}

async fn create_stored_test_session(storage: &StoragePtr) -> (String, SessionPtr) {
    let ssn_id = unique_test_id("ssn");
    let app_name = unique_test_id("app");
    storage
        .register_application(
            app_name.clone(),
            ApplicationAttributes {
                shim: Shim::default(),
                ..Default::default()
            },
        )
        .await
        .unwrap();
    storage
        .create_session(SessionAttributes {
            id: ssn_id.clone(),
            application: app_name,
            ..Default::default()
        })
        .await
        .unwrap();
    let ssn_ptr = storage.get_session_ptr(ssn_id.clone()).unwrap();
    (ssn_id, ssn_ptr)
}

mod binding_state;
mod bound_state;
mod factory;
mod idle_state;
mod lifecycle;
mod releasing_state;
mod unbinding_state;
mod void_state;
