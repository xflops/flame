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

use bytes::Bytes;

use crate::apis::{CommonData, FlameError, TaskInput, TaskOutput};

pub trait FlameMessage: Sized {
    fn encode(&self) -> Result<Bytes, FlameError>;
    fn decode(bytes: &[u8]) -> Result<Self, FlameError>;
}

pub trait IntoTaskInput {
    fn into_task_input(self) -> Result<Option<TaskInput>, FlameError>;
}

impl<T> IntoTaskInput for &T
where
    T: FlameMessage,
{
    fn into_task_input(self) -> Result<Option<TaskInput>, FlameError> {
        Ok(Some(self.encode()?))
    }
}

impl<T> IntoTaskInput for Option<&T>
where
    T: FlameMessage,
{
    fn into_task_input(self) -> Result<Option<TaskInput>, FlameError> {
        self.map(FlameMessage::encode).transpose()
    }
}

impl IntoTaskInput for () {
    fn into_task_input(self) -> Result<Option<TaskInput>, FlameError> {
        Ok(None)
    }
}

pub trait FromTaskOutput: Sized {
    fn from_task_output(output: Option<TaskOutput>) -> Result<Option<Self>, FlameError>;
}

impl<T> FromTaskOutput for T
where
    T: FlameMessage,
{
    fn from_task_output(output: Option<TaskOutput>) -> Result<Option<Self>, FlameError> {
        output.map(|output| T::decode(&output)).transpose()
    }
}

impl FromTaskOutput for () {
    fn from_task_output(_: Option<TaskOutput>) -> Result<Option<Self>, FlameError> {
        Ok(Some(()))
    }
}

pub trait IntoCommonData {
    fn into_common_data(self) -> Result<CommonData, FlameError>;
}

impl<T> IntoCommonData for &T
where
    T: FlameMessage,
{
    fn into_common_data(self) -> Result<CommonData, FlameError> {
        self.encode()
    }
}

pub fn decode<T>(input: TaskInput) -> Result<T, FlameError>
where
    T: FlameMessage,
{
    T::decode(&input)
}

pub fn decode_optional<T>(input: Option<TaskInput>) -> Result<Option<T>, FlameError>
where
    T: FlameMessage,
{
    input.map(decode).transpose()
}

pub fn encode<T>(output: T) -> Result<Option<TaskOutput>, FlameError>
where
    T: FlameMessage,
{
    Ok(Some(output.encode()?))
}

pub fn encode_optional<T>(output: Option<T>) -> Result<Option<TaskOutput>, FlameError>
where
    T: FlameMessage,
{
    output.map(|output| output.encode()).transpose()
}

pub fn encode_unit() -> Result<Option<TaskOutput>, FlameError> {
    Ok(None)
}

pub fn decode_common_data<T>(common_data: Option<&CommonData>) -> Result<Option<T>, FlameError>
where
    T: FlameMessage,
{
    common_data.map(|data| T::decode(data)).transpose()
}

#[cfg(test)]
mod tests {
    use serde_derive::{Deserialize, Serialize};

    use super::*;

    #[derive(Debug, PartialEq, Serialize, Deserialize)]
    struct TestMessage {
        value: String,
    }

    impl FlameMessage for TestMessage {
        fn encode(&self) -> Result<Bytes, FlameError> {
            serde_json::to_vec(self)
                .map(Bytes::from)
                .map_err(|e| FlameError::Internal(e.to_string()))
        }

        fn decode(bytes: &[u8]) -> Result<Self, FlameError> {
            serde_json::from_slice(bytes).map_err(|e| FlameError::InvalidConfig(e.to_string()))
        }
    }

    #[test]
    fn message_round_trips_task_payloads() {
        let input = TestMessage {
            value: "hello".to_string(),
        };

        let task_input = (&input).into_task_input().unwrap().unwrap();
        let decoded = decode::<TestMessage>(task_input).unwrap();
        assert_eq!(decoded, input);

        let output = encode(input).unwrap();
        let decoded = TestMessage::from_task_output(output).unwrap().unwrap();
        assert_eq!(decoded.value, "hello");
    }

    #[test]
    fn optional_and_unit_payloads_map_to_absent_bytes() {
        let input: Option<&TestMessage> = None;
        assert!(input.into_task_input().unwrap().is_none());
        assert!(decode_optional::<TestMessage>(None).unwrap().is_none());
        assert!(encode_optional::<TestMessage>(None).unwrap().is_none());
        assert!(encode_unit().unwrap().is_none());
        assert_eq!(<()>::from_task_output(None).unwrap(), Some(()));
    }
}
