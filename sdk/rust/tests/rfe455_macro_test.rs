#![cfg(feature = "macros")]

use std::sync::Mutex;

use flame_rs as flame;
use flame_rs::apis::{CommonData, FlameError};
use flame_rs::service::{
    ApplicationContext, FlameInstance, FlameService, SessionContext, TaskContext,
};
use flame_rs::{FlameMessage, IntoFlameInstance};
use serde::Serializer;
use serde_derive::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, FlameMessage)]
struct EchoRequest {
    value: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, FlameMessage)]
struct EchoResponse {
    value: String,
}

#[flame::entrypoint]
async fn echo(req: EchoRequest) -> Result<EchoResponse, FlameError> {
    Ok(EchoResponse { value: req.value })
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, FlameMessage)]
struct Factor {
    value: u32,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, FlameMessage)]
struct Number {
    value: u32,
}

#[derive(Debug, Clone, PartialEq, Deserialize, FlameMessage)]
struct EncodeFailure;

impl serde::Serialize for EncodeFailure {
    fn serialize<S>(&self, _: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        Err(serde::ser::Error::custom("encode failure"))
    }
}

#[derive(Default)]
struct Multiplier {
    factor: Mutex<u32>,
}

#[flame::instance]
impl Multiplier {
    async fn enter(&self, instance: FlameInstance) -> Result<(), FlameError> {
        let factor = instance
            .common_data::<Factor>()?
            .map(|factor| factor.value)
            .unwrap_or(1);
        *self.factor.lock().unwrap() = factor;
        Ok(())
    }

    #[flame::entrypoint]
    async fn multiply(&self, req: Number) -> Result<Number, FlameError> {
        let factor = *self.factor.lock().unwrap();
        Ok(Number {
            value: req.value * factor,
        })
    }

    async fn leave(&self) -> Result<(), FlameError> {
        *self.factor.lock().unwrap() = 1;
        Ok(())
    }
}

fn session_context(common_data: Option<CommonData>) -> SessionContext {
    SessionContext {
        session_id: "ssn-1".to_string(),
        application: ApplicationContext {
            name: "test-app".to_string(),
            image: None,
            command: None,
        },
        common_data,
    }
}

fn task_context(input: Option<flame::apis::TaskInput>) -> TaskContext {
    TaskContext {
        task_id: "task-1".to_string(),
        session_id: "ssn-1".to_string(),
        input,
    }
}

#[tokio::test]
async fn free_function_entrypoint_decodes_and_encodes_typed_payloads() {
    let service = echo.into_flame_instance();
    service
        .on_session_enter(session_context(None))
        .await
        .unwrap();

    let input = EchoRequest {
        value: "hello".to_string(),
    }
    .encode()
    .unwrap();

    let output = service
        .on_task_invoke(task_context(Some(input)))
        .await
        .unwrap()
        .unwrap();
    let decoded = EchoResponse::decode(&output).unwrap();

    assert_eq!(
        decoded,
        EchoResponse {
            value: "hello".to_string()
        }
    );
    service.on_session_leave().await.unwrap();
}

#[tokio::test]
async fn instance_entrypoint_can_use_typed_common_data() {
    let service = Multiplier::default().into_flame_instance();
    let common_data = Factor { value: 3 }.encode().unwrap();
    service
        .on_session_enter(session_context(Some(common_data)))
        .await
        .unwrap();

    let input = Number { value: 7 }.encode().unwrap();
    let output = service
        .on_task_invoke(task_context(Some(input)))
        .await
        .unwrap()
        .unwrap();
    let decoded = Number::decode(&output).unwrap();

    assert_eq!(decoded, Number { value: 21 });
    service.on_session_leave().await.unwrap();
}

#[tokio::test]
async fn required_macro_input_reports_missing_task_input() {
    let service = echo.into_flame_instance();
    service
        .on_session_enter(session_context(None))
        .await
        .unwrap();

    let err = service
        .on_task_invoke(task_context(None))
        .await
        .expect_err("required input should be rejected");

    assert!(err.to_string().contains("missing task input"));
}

#[test]
fn flame_message_decode_errors_are_invalid_config() {
    let err = EchoRequest::decode(b"{").unwrap_err();

    assert!(matches!(err, FlameError::InvalidConfig(_)));
}

#[test]
fn flame_message_encode_errors_are_internal() {
    let err = EncodeFailure.encode().unwrap_err();

    assert!(matches!(err, FlameError::Internal(_)));
}
