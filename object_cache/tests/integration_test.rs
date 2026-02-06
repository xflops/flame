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

use arrow::array::{BinaryArray, RecordBatch, UInt64Array};
use arrow::datatypes::{DataType, Field, Schema};
use arrow::ipc::writer::{DictionaryTracker, IpcDataGenerator, IpcWriteOptions};
use arrow_flight::{
    flight_service_client::FlightServiceClient, Action, FlightData, FlightDescriptor, Ticket,
};
use arrow_flight::utils::flight_data_to_arrow_batch;
use base64::Engine;
use bson;
use bytes::Bytes;
use futures::StreamExt;
use std::sync::Arc;
use tonic::transport::Channel;
use uuid;

const CACHE_ENDPOINT: &str = "http://127.0.0.1:9090";

fn create_test_batch(data: &[u8]) -> RecordBatch {
    let schema = Schema::new(vec![
        Field::new("version", DataType::UInt64, false),
        Field::new("data", DataType::Binary, false),
    ]);

    let version_array = UInt64Array::from(vec![0u64]);
    let data_array = BinaryArray::from(vec![data]);

    RecordBatch::try_new(
        Arc::new(schema),
        vec![Arc::new(version_array), Arc::new(data_array)],
    )
    .expect("Failed to create RecordBatch")
}

fn parse_object_from_batch(batch: &RecordBatch) -> Vec<u8> {
    if batch.num_rows() != 1 {
        panic!("Expected exactly one row");
    }

    let data_col = batch
        .column(1)
        .as_any()
        .downcast_ref::<BinaryArray>()
        .expect("Invalid data column");

    data_col.value(0).to_vec()
}

async fn create_flight_client() -> FlightServiceClient<Channel> {
    FlightServiceClient::connect(CACHE_ENDPOINT)
        .await
        .expect("Failed to connect to cache server")
}

#[tokio::test]
#[ignore] // Ignore by default - requires running cache server at 127.0.0.1:9090
async fn test_put_and_get_via_grpc() -> Result<(), Box<dyn std::error::Error>> {
    let mut client = create_flight_client().await;

    let application_id = "test_app";
    let session_id = "test_session_grpc";
    let test_data = b"Hello from gRPC client!";

    // Create test batch
    let batch = create_test_batch(test_data);

    // Put object using do_put
    // Format: application_id/session_id/object_id in FlightDescriptor path
    // Generate object_id (or use a fixed one for testing)
    let object_id = uuid::Uuid::new_v4().to_string();
    let descriptor = FlightDescriptor {
        r#type: arrow_flight::flight_descriptor::DescriptorType::Path as i32,
        cmd: Bytes::new(),
        path: vec![format!("{}/{}/{}", application_id, session_id, object_id)],
    };

    // Create FlightData stream from batch
    let options = IpcWriteOptions::default()
        .try_with_compression(None)
        .map_err(|e| format!("Failed to set compression: {}", e))?;
    let data_gen = IpcDataGenerator::default();
    let mut dict_tracker = DictionaryTracker::new(false);

    // Encode schema
    let encoded_schema = data_gen.schema_to_bytes_with_dictionary_tracker(
        batch.schema().as_ref(),
        &mut dict_tracker,
        &options,
    );

    let schema_flight_data = FlightData {
        flight_descriptor: Some(descriptor.clone()),
        app_metadata: vec![].into(),
        data_header: encoded_schema.ipc_message.into(),
        data_body: vec![].into(),
    };

    // Encode batch
    let (encoded_dictionaries, encoded_batch) = data_gen
        .encoded_batch(&batch, &mut dict_tracker, &options)
        .map_err(|e| format!("Failed to encode batch: {}", e))?;

    // Create stream of FlightData
    let mut flight_data_vec = vec![schema_flight_data];
    for dict_batch in encoded_dictionaries {
        let mut flight_data: FlightData = dict_batch.into();
        // Ensure descriptor is included in all messages
        flight_data.flight_descriptor = Some(descriptor.clone());
        flight_data_vec.push(flight_data);
    }
    let mut batch_flight_data: FlightData = encoded_batch.into();
    batch_flight_data.flight_descriptor = Some(descriptor.clone());
    flight_data_vec.push(batch_flight_data);

    // Create stream from vector (do_put expects FlightData, not Result)
    let flight_data_stream = futures::stream::iter(flight_data_vec);

    // Call do_put with the stream
    let put_stream = client.do_put(tonic::Request::new(flight_data_stream)).await?;
    let mut reader = put_stream.into_inner();

    // Read PutResult from the stream
    let put_result = reader.next().await
        .ok_or("No PutResult received")?
        .map_err(|e| format!("Failed to read PutResult: {}", e))?;

    // Parse ObjectRef from PutResult
    let object_ref: bson::Document = bson::from_slice(&put_result.app_metadata)
        .map_err(|e| format!("Failed to parse ObjectRef: {}", e))?;
    let key = object_ref
        .get_str("key")
        .expect("ObjectRef missing key")
        .to_string();

    println!("Put object with key: {}", key);

    // Verify key format
    assert!(key.contains(application_id));
    assert!(key.contains(session_id));

    // Get object using do_get
    let ticket = Ticket {
        ticket: key.as_bytes().to_vec().into(),
    };

    let get_stream = client.do_get(tonic::Request::new(ticket)).await?;
    let mut reader = get_stream.into_inner();

    // Read all flight data and convert to batches
    let mut schema: Option<Arc<Schema>> = None;
    let mut batches = Vec::new();

    while let Some(data) = reader.message().await? {
        // Extract schema from first message
        if schema.is_none() && !data.data_header.is_empty() {
            let message = arrow::ipc::root_as_message(&data.data_header)
                .map_err(|e| format!("Failed to parse IPC message: {}", e))?;
            let ipc_schema = message
                .header_as_schema()
                .ok_or("Message is not a schema")?;
            schema = Some(Arc::new(arrow::ipc::convert::fb_to_schema(ipc_schema)));
        }

        // Decode batch if we have schema and data
        if let Some(ref schema_ref) = schema {
            if !data.data_body.is_empty() {
                let batch = flight_data_to_arrow_batch(
                    &data,
                    schema_ref.clone(),
                    &Default::default(),
                )
                .map_err(|e| format!("Failed to decode batch: {}", e))?;
                batches.push(batch);
            }
        }
    }

    assert!(!batches.is_empty(), "No batches received");
    let retrieved_batch = &batches[0];
    let retrieved_data = parse_object_from_batch(retrieved_batch);

    assert_eq!(retrieved_data, test_data);

    println!("Successfully retrieved object via gRPC");

    Ok(())
}

#[tokio::test]
#[ignore] // Ignore by default - requires running cache server
async fn test_get_flight_info_via_grpc() -> Result<(), Box<dyn std::error::Error>> {
    let mut client = create_flight_client().await;

    let application_id = "test_app";
    let session_id = "test_session_info";
    let object_id = "test_object";

    // First, put an object
    let test_data = b"Test data for flight info";
    let batch = create_test_batch(test_data);

    let descriptor = FlightDescriptor {
        r#type: arrow_flight::flight_descriptor::DescriptorType::Path as i32,
        cmd: Bytes::new(),
        path: vec![format!("{}/{}/{}", application_id, session_id, object_id)],
    };

    // Create FlightData stream from batch (same as above)
    let options = IpcWriteOptions::default()
        .try_with_compression(None)
        .map_err(|e| format!("Failed to set compression: {}", e))?;
    let data_gen = IpcDataGenerator::default();
    let mut dict_tracker = DictionaryTracker::new(false);

    let encoded_schema = data_gen.schema_to_bytes_with_dictionary_tracker(
        batch.schema().as_ref(),
        &mut dict_tracker,
        &options,
    );

    let schema_flight_data = FlightData {
        flight_descriptor: Some(descriptor.clone()),
        app_metadata: vec![].into(),
        data_header: encoded_schema.ipc_message.into(),
        data_body: vec![].into(),
    };

    let (encoded_dictionaries, encoded_batch) = data_gen
        .encoded_batch(&batch, &mut dict_tracker, &options)
        .map_err(|e| format!("Failed to encode batch: {}", e))?;

    let mut flight_data_vec = vec![schema_flight_data];
    for dict_batch in encoded_dictionaries {
        let mut flight_data: FlightData = dict_batch.into();
        // Ensure descriptor is included in all messages
        flight_data.flight_descriptor = Some(descriptor.clone());
        flight_data_vec.push(flight_data);
    }
    let mut batch_flight_data: FlightData = encoded_batch.into();
    batch_flight_data.flight_descriptor = Some(descriptor.clone());
    flight_data_vec.push(batch_flight_data);

    let flight_data_stream = futures::stream::iter(flight_data_vec);

    let put_stream = client.do_put(tonic::Request::new(flight_data_stream)).await?;
    let mut reader = put_stream.into_inner();

    // Read PutResult to get key
    let put_result = reader.next().await
        .ok_or("No PutResult received")?
        .map_err(|e| format!("Failed to read PutResult: {}", e))?;
    let object_ref: bson::Document = bson::from_slice(&put_result.app_metadata)
        .map_err(|e| format!("Failed to parse ObjectRef: {}", e))?;
    let key = object_ref.get_str("key").expect("ObjectRef missing key");

    // Get flight info
    let info_descriptor = FlightDescriptor {
        r#type: arrow_flight::flight_descriptor::DescriptorType::Path as i32,
        cmd: Bytes::new(),
        path: vec![key.to_string()],
    };

    let flight_info = client
        .get_flight_info(tonic::Request::new(info_descriptor))
        .await?;

    let flight_info = flight_info.into_inner();
    assert!(!flight_info.endpoint.is_empty());
    assert_eq!(flight_info.flight_descriptor.as_ref().unwrap().path[0], key);

    println!("Successfully retrieved flight info for key: {}", key);

    Ok(())
}

#[tokio::test]
#[ignore] // Ignore by default - requires running cache server
async fn test_list_flights_via_grpc() -> Result<(), Box<dyn std::error::Error>> {
    let mut client = create_flight_client().await;

    // List all flights
    let criteria = arrow_flight::Criteria {
        expression: vec![].into(),
    };

    let list_stream = client
        .list_flights(tonic::Request::new(criteria))
        .await?;

    let mut flights = Vec::new();
    let mut reader = list_stream.into_inner();
    while let Some(flight_info) = reader.message().await? {
        flights.push(flight_info);
    }

    println!("Found {} flights", flights.len());
    // Note: It's OK if no flights are found (cache might be empty)

    Ok(())
}

#[tokio::test]
#[ignore] // Ignore by default - requires running cache server
async fn test_actions_via_grpc() -> Result<(), Box<dyn std::error::Error>> {
    let mut client = create_flight_client().await;

    let application_id = "test_app";
    let session_id = "test_session_action";
    let test_data = b"Test data for action";

    // Test PUT action
    let data_b64 = base64::engine::general_purpose::STANDARD.encode(test_data);
    let action_body = format!("{}:{}:{}", application_id, session_id, data_b64);

    let action = Action {
        r#type: "PUT".to_string(),
        body: action_body.as_bytes().to_vec().into(),
    };

    let action_stream = client.do_action(tonic::Request::new(action)).await?;
    let mut reader = action_stream.into_inner();

    let result = reader.message().await?;
    let result = result.expect("Expected action result");
    let result_str = String::from_utf8(result.body.to_vec())
        .map_err(|e| format!("Failed to parse result: {}", e))?;

    println!("PUT action result: {}", result_str);

    // Parse ObjectRef from result
    let object_ref: bson::Document = serde_json::from_str(&result_str)
        .map_err(|e| format!("Failed to parse ObjectRef: {}", e))?;
    let _key = object_ref.get_str("key").expect("ObjectRef missing key");

    // Test DELETE action
    let delete_action_body = format!("{}/{}", application_id, session_id);
    let delete_action = Action {
        r#type: "DELETE".to_string(),
        body: delete_action_body.as_bytes().to_vec().into(),
    };

    let delete_stream = client.do_action(tonic::Request::new(delete_action)).await?;
    let mut delete_reader = delete_stream.into_inner();

    let delete_result = delete_reader.message().await?;
    let delete_result = delete_result.expect("Expected delete result");
    let delete_result_str = String::from_utf8(delete_result.body.to_vec())
        .map_err(|e| format!("Failed to parse delete result: {}", e))?;

    assert_eq!(delete_result_str, "OK");
    println!("Successfully deleted session via action");

    Ok(())
}

#[tokio::test]
#[ignore] // Ignore by default - requires running cache server
async fn test_get_schema_via_grpc() -> Result<(), Box<dyn std::error::Error>> {
    let mut client = create_flight_client().await;

    let descriptor = FlightDescriptor {
        r#type: arrow_flight::flight_descriptor::DescriptorType::Path as i32,
        cmd: Bytes::new(),
        path: vec!["test".to_string()],
    };

    let schema_result = client
        .get_schema(tonic::Request::new(descriptor))
        .await?;

    let schema_result = schema_result.into_inner();
    assert!(!schema_result.schema.is_empty());

    // Parse schema
    let message = arrow::ipc::root_as_message(&schema_result.schema)
        .map_err(|e| format!("Failed to parse schema message: {}", e))?;
    let ipc_schema = message
        .header_as_schema()
        .ok_or("Message is not a schema")?;
    let schema = arrow::ipc::convert::fb_to_schema(ipc_schema);

    assert_eq!(schema.fields().len(), 2);
    assert_eq!(schema.field(0).name(), "version");
    assert_eq!(schema.field(1).name(), "data");

    println!("Successfully retrieved schema via gRPC");

    Ok(())
}
