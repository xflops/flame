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

use flame_rs::apis::{FlameError, TaskInput, TaskOutput};

use serde_derive::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct PingRequest {
    pub duration: Option<u64>,
    pub memory: Option<u64>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct PingResponse {
    pub message: String,
}

impl TryFrom<TaskOutput> for PingResponse {
    type Error = FlameError;
    fn try_from(output: TaskOutput) -> Result<Self, Self::Error> {
        let response: Self =
            serde_json::from_slice(&output).map_err(|e| FlameError::Internal(e.to_string()))?;
        Ok(response)
    }
}

impl TryFrom<PingResponse> for TaskOutput {
    type Error = FlameError;
    fn try_from(response: PingResponse) -> Result<Self, Self::Error> {
        let bytes = TaskOutput::from(
            serde_json::to_string(&response).map_err(|e| FlameError::Internal(e.to_string()))?,
        );
        Ok(bytes)
    }
}

impl TryFrom<PingRequest> for TaskInput {
    type Error = FlameError;
    fn try_from(request: PingRequest) -> Result<Self, Self::Error> {
        let bytes = TaskInput::from(
            serde_json::to_string(&request).map_err(|e| FlameError::Internal(e.to_string()))?,
        );
        Ok(bytes)
    }
}

impl TryFrom<TaskInput> for PingRequest {
    type Error = FlameError;
    fn try_from(input: TaskInput) -> Result<Self, Self::Error> {
        let request: Self =
            serde_json::from_slice(&input).map_err(|e| FlameError::Internal(e.to_string()))?;
        Ok(request)
    }
}
