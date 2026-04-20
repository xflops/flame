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

pub mod apis;
pub mod ctx;
pub mod storage;

use std::string::FromUtf8Error;

use prost::UnknownEnumValue;
use serde_json::json;
use std::collections::HashMap;
use std::sync::Arc;
use thiserror::Error;
use time::macros::format_description;
use tonic::Status;
use tracing_appender::non_blocking::WorkerGuard;
use tracing_appender::rolling;
use tracing_subscriber::filter::{FromEnvError, ParseError};
use tracing_subscriber::fmt::time::LocalTime;
use tracing_subscriber::fmt::writer::MakeWriterExt;

use crate::apis::{ApplicationAttributes, ApplicationSchema};

#[derive(Error, Debug)]
pub enum FlameError {
    #[error("{0}")]
    NotFound(String),

    #[error("{0}")]
    AlreadyExist(String),

    #[error("{0}")]
    Internal(String),

    #[error("{0}")]
    Network(String),

    #[error("{0}")]
    InvalidConfig(String),

    #[error("{0}")]
    Uninitialized(String),

    #[error("{0}")]
    InvalidState(String),

    #[error("{0}")]
    Storage(String),

    #[error("{0}")]
    VersionMismatch(String),
}

impl From<stdng::Error> for FlameError {
    fn from(value: stdng::Error) -> Self {
        FlameError::Internal(value.to_string())
    }
}

impl From<FlameError> for Status {
    fn from(value: FlameError) -> Self {
        match value {
            FlameError::NotFound(msg) => Status::not_found(msg),
            FlameError::AlreadyExist(msg) => Status::already_exists(msg),
            FlameError::InvalidConfig(msg) | FlameError::InvalidState(msg) => {
                Status::invalid_argument(msg)
            }
            FlameError::Internal(msg)
            | FlameError::Network(msg)
            | FlameError::Uninitialized(msg)
            | FlameError::Storage(msg)
            | FlameError::VersionMismatch(msg) => Status::internal(msg),
        }
    }
}

impl From<Status> for FlameError {
    fn from(value: Status) -> Self {
        FlameError::Network(value.message().to_string())
    }
}

impl From<ParseError> for FlameError {
    fn from(value: ParseError) -> Self {
        FlameError::InvalidConfig(value.to_string())
    }
}

impl From<FromUtf8Error> for FlameError {
    fn from(value: FromUtf8Error) -> Self {
        FlameError::InvalidConfig(value.to_string())
    }
}

impl From<FromEnvError> for FlameError {
    fn from(value: FromEnvError) -> Self {
        FlameError::InvalidConfig(value.to_string())
    }
}

impl From<UnknownEnumValue> for FlameError {
    fn from(value: UnknownEnumValue) -> Self {
        FlameError::InvalidConfig(value.to_string())
    }
}

impl From<std::io::Error> for FlameError {
    fn from(value: std::io::Error) -> Self {
        FlameError::Storage(value.to_string())
    }
}

pub type AsyncMutexPtr<T> = Arc<tokio::sync::Mutex<T>>;

pub fn new_async_ptr<T>(t: T) -> AsyncMutexPtr<T> {
    Arc::new(tokio::sync::Mutex::new(t))
}

pub const FLAME_HOME: &str = "FLAME_HOME";
pub const FLAME_LOG: &str = "FLAME_LOG";
pub const FLAME_WORKING_DIRECTORY: &str = "/tmp/flame";
pub const FLAME_INSTANCE_ENDPOINT: &str = "FLAME_INSTANCE_ENDPOINT";
pub const FLAME_CACHE_ENDPOINT: &str = "FLAME_CACHE_ENDPOINT";
pub const FLAME_ENDPOINT: &str = "FLAME_ENDPOINT";
pub const FLAME_CA_FILE: &str = "FLAME_CA_FILE";

/// Returns the system temporary directory path.
/// This is cross-platform: /tmp on Unix, %TEMP% on Windows.
pub fn temp_dir() -> std::path::PathBuf {
    std::env::temp_dir()
}

/// Creates a SQLite URL pointing to a temporary database file.
/// The path is cross-platform compatible.
///
/// # Example
/// ```
/// let url = common::temp_sqlite_url("flame_test_my_feature");
/// // On Unix: "sqlite:///tmp/flame_test_my_feature_1234567890.db"
/// // On Windows: "sqlite://C:/Users/.../AppData/Local/Temp/flame_test_my_feature_1234567890.db"
/// ```
pub fn temp_sqlite_url(prefix: &str) -> String {
    let timestamp = chrono::Utc::now().timestamp();
    let temp_path = temp_dir().join(format!("{}_{}.db", prefix, timestamp));
    format!("sqlite://{}", temp_path.display())
}

/// Creates a temporary database file path (without the sqlite:// prefix).
/// Returns the path as a String for use with storage config and cleanup.
///
/// # Example
/// ```
/// let path = common::temp_db_path("flame_test_my_feature");
/// let url = format!("sqlite:///{}", path);
/// // Later: std::fs::remove_file(&path).ok();
/// ```
pub fn temp_db_path(prefix: &str) -> String {
    use std::sync::atomic::{AtomicU64, Ordering};
    static COUNTER: AtomicU64 = AtomicU64::new(0);

    let timestamp = chrono::Utc::now().timestamp_micros();
    let counter = COUNTER.fetch_add(1, Ordering::Relaxed);
    let temp_path = temp_dir().join(format!("{}_{}_{}.db", prefix, timestamp, counter));
    temp_path.to_string_lossy().to_string()
}

pub fn init_logger(component: Option<&str>) -> Result<Option<WorkerGuard>, FlameError> {
    let filter = tracing_subscriber::EnvFilter::from_default_env()
        .add_directive("h2=error".parse()?)
        .add_directive("hyper_util=error".parse()?)
        .add_directive("sqlx=warn".parse()?)
        .add_directive("tower=error".parse()?);

    let time_format = LocalTime::new(format_description!(
        "[hour repr:24]:[minute]:[second].[subsecond digits:3]"
    ));

    let fmt_builder = tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_timer(time_format)
        .with_ansi(false)
        .with_target(true);

    match component {
        Some(name) => {
            let hostname = gethostname::gethostname().to_string_lossy().into_owned();
            let log_file = format!("{}-{}", name, hostname);

            let log_dir = std::env::var(FLAME_HOME)
                .map(|h| format!("{}/logs", h))
                .unwrap_or_else(|_| format!("{}/logs", FLAME_WORKING_DIRECTORY));

            std::fs::create_dir_all(&log_dir)?;

            let file_appender = rolling::daily(&log_dir, log_file);
            let (non_blocking_file, guard) = tracing_appender::non_blocking(file_appender);

            fmt_builder
                .with_writer(non_blocking_file.and(std::io::stdout))
                .init();

            Ok(Some(guard))
        }
        None => {
            fmt_builder.init();

            Ok(None)
        }
    }
}

pub fn default_applications() -> HashMap<String, ApplicationAttributes> {
    let script_input_schema = json!({
        "$schema": "http://json-schema.org/draft-07/schema#",
        "type": "object",
        "properties": {
            "language": {
                "type": "string",
                "description": "The language of the script, e.g. python"
            },
            "code": {
                "type": "string",
                "description": "The code of the script to run, e.g. print('Hello, world!')"
            },
            "input": {
                "type": "array",
                "items": {
                    "type": "integer",
                    "description": "The input to the script in bytes, e.g. [0x1, 0x2]"
                }
            }
        },
        "required": [
            "language",
            "code"
        ]
    });

    let script_output_schema = json!({
        "$schema": "http://json-schema.org/draft-07/schema#",
        "type": "string",
        "description": "The output of the script in UTF-8."
    });

    // Use ${FLAME_HOME} variable substitution syntax
    // This will be expanded at runtime by the executor to the actual FLAME_HOME path
    let flmexec_cmd = "${FLAME_HOME}/bin/flmexec-service".to_string();
    let flmping_cmd = "${FLAME_HOME}/bin/flmping-service".to_string();
    let uv_cmd = "${FLAME_HOME}/bin/uv".to_string();
    let flmping_url = "file://${FLAME_HOME}/bin/flmping-service".to_string();
    // Use pre-built wheel from wheels directory to avoid rebuild on every run
    // The wheel is built during installation via "flmadm install"
    let flamepy_wheels_dir = "${FLAME_HOME}/wheels".to_string();
    // Use the same cache directory that was populated during installation
    let uv_cache_dir = "${FLAME_HOME}/data/cache/uv".to_string();

    HashMap::from([
        (
            "flmexec".to_string(),
            ApplicationAttributes {
                // shim removed - now configured in executor-manager
                description: Some(
                    "The Flame Executor application, which is used to run scripts.".to_string(),
                ),
                command: Some(flmexec_cmd),
                schema: Some(ApplicationSchema {
                    input: Some(script_input_schema.to_string()),
                    output: Some(script_output_schema.to_string()),
                    ..ApplicationSchema::default()
                }),
                ..ApplicationAttributes::default()
            },
        ),
        (
            "flmping".to_string(),
            ApplicationAttributes {
                // shim removed - now configured in executor-manager
                url: Some(flmping_url),
                command: Some(flmping_cmd),
                ..ApplicationAttributes::default()
            },
        ),
        (
            "flmrun".to_string(),
            ApplicationAttributes {
                // shim removed - now configured in executor-manager
                description: Some(
                    "The Flame Runner application for executing customized Python applications."
                        .to_string(),
                ),
                command: Some(uv_cmd),
                arguments: vec![
                    "run".to_string(),
                    "--find-links".to_string(),
                    flamepy_wheels_dir,
                    "--with".to_string(),
                    "pip".to_string(),
                    "--with".to_string(),
                    "flamepy".to_string(),
                    "python".to_string(),
                    "-m".to_string(),
                    "flamepy.runner.runpy".to_string(),
                ],
                environments: HashMap::from([("UV_CACHE_DIR".to_string(), uv_cache_dir)]),
                working_directory: None,
                ..ApplicationAttributes::default()
            },
        ),
    ])
}

#[cfg(test)]
mod tests {
    use super::*;
    use tonic::Code;

    #[test]
    fn test_from_flame_error_to_status() {
        let error = FlameError::NotFound("test".to_string());
        let status = Status::from(error);
        assert_eq!(status.code(), Code::NotFound);
        assert_eq!(status.message(), "test");

        let error = FlameError::Internal("test".to_string());
        let status = Status::from(error);
        assert_eq!(status.code(), Code::Internal);
        assert_eq!(status.message(), "test");

        let error = FlameError::Network("test".to_string());
        let status = Status::from(error);
        assert_eq!(status.code(), Code::Internal);
        assert_eq!(status.message(), "test");

        let error = FlameError::InvalidConfig("test".to_string());
        let status = Status::from(error);
        assert_eq!(status.code(), Code::InvalidArgument);
        assert_eq!(status.message(), "test");

        let error = FlameError::InvalidState("test".to_string());
        let status = Status::from(error);
        assert_eq!(status.code(), Code::InvalidArgument);
        assert_eq!(status.message(), "test");
    }
}
