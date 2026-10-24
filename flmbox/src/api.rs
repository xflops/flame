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

//! JSON types exchanged between the SDK and the `flmbox` service.

use std::collections::BTreeMap;

use flame_rs::FlameMessage;
use serde_derive::{Deserialize, Serialize};

/// A task request: a script run inside the sandbox.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize, FlameMessage)]
pub struct CommandRequest {
    /// Source code, or a shell command line.
    pub code: String,
    /// Language of the code: `shell` (the default), `python`.
    #[serde(default)]
    pub language: Option<String>,
    /// Working directory inside the sandbox.
    #[serde(default)]
    pub cwd: Option<String>,
    /// Extra environment variables.
    #[serde(default)]
    pub envs: Option<BTreeMap<String, String>>,
    /// Optional stdin for the program.
    #[serde(default)]
    pub input: Option<Vec<u8>>,
    /// Timeout in seconds.
    #[serde(default)]
    pub timeout: Option<f64>,
}

/// The result of a command; byte vectors serialize as JSON integer arrays.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, FlameMessage)]
pub struct CommandResult {
    /// Process exit code.
    pub exit_code: i32,
    /// Captured standard output.
    pub stdout: Vec<u8>,
    /// Captured standard error.
    pub stderr: Vec<u8>,
}

/// Session common data. Unknown fields are ignored so the SDK can evolve.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize, FlameMessage)]
pub struct SandboxConfig {
    /// Image reference; the service default applies when absent.
    #[serde(default)]
    pub image: Option<String>,
}

#[cfg(test)]
mod tests {
    use flame_rs::FlameMessage;

    use super::*;

    #[test]
    fn defaults() {
        let req = CommandRequest::decode(br#"{"code":"ls"}"#).unwrap();
        assert_eq!(
            req,
            CommandRequest {
                code: "ls".to_string(),
                ..Default::default()
            }
        );
    }

    #[test]
    fn round_trip_with_bytes() {
        let req = CommandRequest {
            code: "print(1)".to_string(),
            language: Some("python".to_string()),
            cwd: Some("/tmp".to_string()),
            envs: Some(BTreeMap::from([("A".to_string(), "b".to_string())])),
            input: Some(vec![1, 2, 3]),
            timeout: Some(1.5),
        };
        let bytes = req.encode().unwrap();
        let text = String::from_utf8(bytes.to_vec()).unwrap();
        assert!(text.contains(r#""input":[1,2,3]"#));
        assert_eq!(CommandRequest::decode(&bytes).unwrap(), req);
    }

    #[test]
    fn missing_code_is_rejected() {
        assert!(CommandRequest::decode(br#"{"language":"python"}"#).is_err());
    }

    #[test]
    fn command_result_bytes_are_integer_arrays() {
        let res = CommandResult {
            exit_code: 3,
            stdout: b"ok".to_vec(),
            stderr: vec![],
        };
        let bytes = res.encode().unwrap();
        assert_eq!(
            String::from_utf8(bytes.to_vec()).unwrap(),
            r#"{"exit_code":3,"stdout":[111,107],"stderr":[]}"#
        );
        assert_eq!(CommandResult::decode(&bytes).unwrap(), res);
    }

    #[test]
    fn sandbox_config_defaults_and_unknown_fields() {
        let cfg = SandboxConfig::decode(b"{}").unwrap();
        assert_eq!(cfg, SandboxConfig::default());

        let cfg = SandboxConfig::decode(br#"{"image":"ubuntu:26.04","future":{"x":1}}"#).unwrap();
        assert_eq!(cfg.image.as_deref(), Some("ubuntu:26.04"));
    }
}
