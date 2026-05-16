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

pub mod apis;
pub mod client;
pub mod message;
pub mod service;

pub use client::{Connection, Session, SessionOptions, TaskFuture, TaskHandle, TaskResult};
pub use message::{FlameMessage, FromTaskOutput, IntoCommonData, IntoTaskInput};

#[cfg(feature = "macros")]
pub use flame_rs_macros::{entrypoint, instance, FlameMessage};

#[doc(hidden)]
pub mod __private {
    pub use bytes;
    pub use serde;
    pub use serde_json;
}

pub trait IntoFlameInstance {
    type Service: service::FlameService;

    fn into_flame_instance(self) -> Self::Service;
}

impl<T> IntoFlameInstance for T
where
    T: service::FlameService,
{
    type Service = T;

    fn into_flame_instance(self) -> Self::Service {
        self
    }
}

pub async fn run<T>(target: T) -> Result<(), Box<dyn std::error::Error>>
where
    T: IntoFlameInstance,
{
    apis::init_logger()?;
    service::run(target.into_flame_instance()).await
}

pub async fn connect() -> Result<Connection, apis::FlameError> {
    connect_with_config(None).await
}

pub async fn connect_with_context(
    context: apis::FlameContext,
) -> Result<Connection, apis::FlameError> {
    let current = context.get_current_context()?;
    client::connect_with_tls(&current.cluster.endpoint, current.cluster.tls.as_ref()).await
}

pub async fn connect_with_config(path: Option<String>) -> Result<Connection, apis::FlameError> {
    let context = apis::FlameContext::from_file_with_env(path)?;
    connect_with_context(context).await
}

pub async fn create_session(
    options: impl Into<SessionOptions>,
) -> Result<Session, apis::FlameError> {
    connect().await?.create_session_with(options).await
}

pub async fn open_session(id: impl Into<apis::SessionID>) -> Result<Session, apis::FlameError> {
    let id = id.into();
    connect().await?.open_session(&id, None).await
}

pub async fn open_or_create_session(
    id: impl Into<apis::SessionID>,
    options: impl Into<SessionOptions>,
) -> Result<Session, apis::FlameError> {
    connect()
        .await?
        .open_or_create_session_with(id, options)
        .await
}
