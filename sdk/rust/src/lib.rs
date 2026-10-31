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
pub mod object;
pub mod service;

pub const FLAME_PYTHON_VERSION_ENV: &str = "FLAME_PYTHON_VERSION";

pub use client::{
    Connection, Session, SessionOptions, TaskFuture, TaskHandle, TaskOption, TaskOptions,
    TaskResult,
};
#[cfg(feature = "macros")]
pub use flame_rs_macros::{entrypoint, instance, FlameMessage};
pub use message::{FlameMessage, FromTaskOutput, IntoCommonData, IntoTaskInput};
pub use object::{
    delete_objects, get_object, package_key, patch_object, put_object, upload_package,
    ObjectFuture, ObjectKey, ObjectRef,
};

/// Logs a point on the performance timeline: a debug event on the `flame::perf` target with the
/// `phase` field first, followed by any further `field = value` pairs, e.g.
/// `perf_log!("task_invoke_end", executor = %id, elapsed_ms = start.elapsed().as_secs_f64() * 1e3)`.
///
/// The points are off unless a log filter turns the target on, e.g. `RUST_LOG=flame::perf=debug`.
#[macro_export]
macro_rules! perf_log {
    ($phase:expr $(, $($fields:tt)+)?) => {
        $crate::__private::tracing::debug!(target: "flame::perf", phase = $phase $(, $($fields)+)?)
    };
}

#[doc(hidden)]
pub mod __private {
    pub use bytes;
    pub use serde;
    pub use serde_json;
    pub use tracing;
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
    let context = apis::FlameContext::load(path)?;
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

#[cfg(test)]
mod perf_log_tests {
    use std::io::Write;
    use std::sync::{Arc, Mutex};

    use tracing_subscriber::EnvFilter;

    #[derive(Clone, Default)]
    struct Capture(Arc<Mutex<Vec<u8>>>);

    impl Write for Capture {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            self.0.lock().unwrap().extend_from_slice(buf);
            Ok(buf.len())
        }

        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    fn captured(filter: &str) -> String {
        let capture = Capture::default();
        let writer = capture.clone();
        let subscriber = tracing_subscriber::fmt()
            .with_writer(move || writer.clone())
            .with_env_filter(EnvFilter::new(filter))
            .with_ansi(false)
            .finish();
        tracing::subscriber::with_default(subscriber, || {
            crate::perf_log!("task_invoke_end", executor = %"e1", elapsed_ms = 2.5);
            crate::perf_log!("process_spawned");
        });
        let bytes = capture.0.lock().unwrap().clone();
        String::from_utf8(bytes).unwrap()
    }

    #[test]
    fn perf_log_is_a_debug_event_on_the_perf_target_with_the_phase() {
        let out = captured("flame::perf=debug");

        assert!(out.contains("flame::perf"), "{out}");
        assert!(out.contains("phase=\"task_invoke_end\""), "{out}");
        assert!(out.contains("executor=e1"), "{out}");
        assert!(out.contains("elapsed_ms=2.5"), "{out}");
        assert!(out.contains("phase=\"process_spawned\""), "{out}");
    }

    #[test]
    fn perf_log_is_silent_unless_the_perf_target_is_enabled() {
        assert_eq!(captured("info"), "");
    }
}
