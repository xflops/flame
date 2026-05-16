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

pub(crate) mod flame {
    pub mod v1 {
        tonic::include_proto!("flame.v1");
    }
}
use flame::v1 as rpc;

use bincode::{config, Decode, Encode};
use bytes::Bytes;
use prost::Enumeration;
use serde_derive::{Deserialize, Serialize};
use thiserror::Error;
use time::macros::format_description;
use tonic::Status;
use tracing_subscriber::filter::{FromEnvError, ParseError};
use tracing_subscriber::fmt::time::LocalTime;

mod ctx;
pub use ctx::FlameClientCache;
pub use ctx::FlameClientTls;
pub use ctx::FlameClusterConfig;
pub use ctx::FlameContext;
pub use ctx::FlameContextEntry;
pub use ctx::FlamePackage;
pub use ctx::FlameRunner;

pub type TaskID = String;
pub type SessionID = String;
pub type ApplicationID = String;

type Message = Bytes;
pub type TaskInput = Message;
pub type TaskOutput = Message;
pub type CommonData = Message;

#[derive(Encode, Decode, PartialEq, Eq)]
pub enum DataSource {
    Local,
    Remote,
}

#[derive(Encode, Decode)]
pub struct DataExpr {
    pub source: DataSource,
    pub endpoint: Option<String>,
    pub data: Option<Vec<u8>>,
}

impl DataExpr {
    pub fn encode(&self) -> Result<Bytes, FlameError> {
        if self.source == DataSource::Local {
            let data = bincode::encode_to_vec(self, config::standard())
                .map_err(|e| FlameError::Internal(e.to_string()))?;
            return Ok(Bytes::from(data));
        }

        let data_expr = DataExpr {
            source: DataSource::Remote,
            endpoint: self.endpoint.clone(),
            data: None,
        };
        let data = bincode::encode_to_vec(data_expr, config::standard())
            .map_err(|e| FlameError::Internal(e.to_string()))?;

        Ok(Bytes::from(data))
    }

    pub fn decode(data: Bytes) -> Result<Self, FlameError> {
        let data = data.to_vec();

        let (data, _): (Self, usize) = bincode::decode_from_slice(&data, config::standard())
            .map_err(|e| FlameError::Internal(e.to_string()))?;
        Ok(data)
    }
}

#[derive(Error, Debug, Clone)]
pub enum FlameError {
    #[error("'{0}' not found")]
    NotFound(String),

    #[error("{0}")]
    Internal(String),

    #[error("{0}")]
    Network(String),

    #[error("{0}")]
    InvalidConfig(String),
}

impl From<stdng::Error> for FlameError {
    fn from(value: stdng::Error) -> Self {
        FlameError::Internal(value.to_string())
    }
}

impl From<ParseError> for FlameError {
    fn from(value: ParseError) -> Self {
        FlameError::InvalidConfig(value.to_string())
    }
}

impl From<FromEnvError> for FlameError {
    fn from(value: FromEnvError) -> Self {
        FlameError::InvalidConfig(value.to_string())
    }
}

#[derive(
    Clone, Copy, Debug, PartialEq, Eq, Enumeration, strum_macros::Display, Serialize, Deserialize,
)]
pub enum SessionState {
    Open = 0,
    Closed = 1,
}

#[derive(
    Clone, Copy, Debug, PartialEq, Eq, Enumeration, strum_macros::Display, Serialize, Deserialize,
)]
pub enum TaskState {
    Pending = 0,
    Running = 1,
    Succeed = 2,
    Failed = 3,
    Cancelled = 4,
}

impl TaskState {
    pub fn is_terminal(&self) -> bool {
        matches!(self, Self::Succeed | Self::Failed | Self::Cancelled)
    }
}

#[derive(
    Clone, Copy, Debug, PartialEq, Eq, Enumeration, strum_macros::Display, Serialize, Deserialize,
)]
pub enum ApplicationState {
    Enabled = 0,
    Disabled = 1,
}

#[derive(
    Clone, Copy, Debug, PartialEq, Eq, Enumeration, strum_macros::Display, Serialize, Deserialize,
)]
pub enum Shim {
    Host = 0,
    Wasm = 1,
}

#[derive(
    Clone, Copy, Debug, PartialEq, Eq, Enumeration, strum_macros::Display, Serialize, Deserialize,
)]
pub enum ExecutorState {
    Unknown = 0,
    Void = 1,
    Idle = 2,
    Binding = 3,
    Bound = 4,
    Unbinding = 5,
    Releasing = 6,
    Released = 7,
}

impl From<FlameError> for Status {
    fn from(value: FlameError) -> Self {
        match value {
            FlameError::NotFound(s) => Status::not_found(s),
            FlameError::Internal(s) => Status::internal(s),
            _ => Status::unknown(value.to_string()),
        }
    }
}

impl From<Status> for FlameError {
    fn from(value: Status) -> Self {
        FlameError::Network(value.message().to_string())
    }
}

impl From<rpc::Shim> for Shim {
    fn from(shim: rpc::Shim) -> Self {
        match shim {
            rpc::Shim::Host => Shim::Host,
            rpc::Shim::Wasm => Shim::Wasm,
        }
    }
}

impl From<rpc::ApplicationState> for ApplicationState {
    fn from(s: rpc::ApplicationState) -> Self {
        match s {
            rpc::ApplicationState::Disabled => Self::Disabled,
            rpc::ApplicationState::Enabled => Self::Enabled,
        }
    }
}

impl From<rpc::ExecutorState> for ExecutorState {
    fn from(s: rpc::ExecutorState) -> Self {
        match s {
            rpc::ExecutorState::ExecutorVoid => Self::Void,
            rpc::ExecutorState::ExecutorIdle => Self::Idle,
            rpc::ExecutorState::ExecutorBinding => Self::Binding,
            rpc::ExecutorState::ExecutorBound => Self::Bound,
            rpc::ExecutorState::ExecutorUnbinding => Self::Unbinding,
            rpc::ExecutorState::ExecutorReleasing => Self::Releasing,
            rpc::ExecutorState::ExecutorReleased => Self::Released,
            rpc::ExecutorState::ExecutorUnknown => Self::Unknown,
        }
    }
}

pub fn init_logger() -> Result<(), FlameError> {
    let filter = tracing_subscriber::EnvFilter::from_default_env()
        .add_directive("h2=error".parse()?)
        .add_directive("hyper_util=error".parse()?)
        .add_directive("tower=error".parse()?);

    let time_format = LocalTime::new(format_description!(
        "[hour repr:24]:[minute]:[second]::[subsecond digits:3]"
    ));

    // Initialize tracing with a custom format
    tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_timer(time_format)
        .with_target(true)
        .with_ansi(false)
        // .with_thread_ids(true)
        // .with_process_ids(true)
        .try_init()
        .ok();

    Ok(())
}
