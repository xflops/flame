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

use super::*;
use flame::apis::TaskState;
use futures::FutureExt;
use std::sync::{Arc, Mutex};
use tokio::sync::oneshot;

#[derive(Default)]
struct RecordedOutput {
    bytes: Vec<u8>,
    flushes: usize,
}

#[derive(Clone, Default)]
struct RecordingWriter(Arc<Mutex<RecordedOutput>>);

impl RecordingWriter {
    fn text(&self) -> String {
        String::from_utf8(self.0.lock().unwrap().bytes.clone()).unwrap()
    }
}

impl Write for RecordingWriter {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        self.0.lock().unwrap().bytes.extend_from_slice(bytes);
        Ok(bytes.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        self.0.lock().unwrap().flushes += 1;
        Ok(())
    }
}

fn succeeded(id: &str) -> TaskResult<PingResponse> {
    TaskResult {
        task_id: id.into(),
        session_id: "session".into(),
        state: TaskState::Succeed,
        output: Some(PingResponse {
            message: format!("completed-{id}"),
        }),
        error_code: None,
        error_message: None,
    }
}

#[tokio::test]
async fn prints_and_flushes_completions_before_other_submissions_or_tasks_finish() {
    let (finish_slow, slow_completion) = oneshot::channel();
    let (submit_last, last_submission) = oneshot::channel();
    let tasks = [
        async move {
            slow_completion.await.unwrap();
            Ok(succeeded("slow"))
        }
        .boxed(),
        async { Ok(succeeded("fast")) }.boxed(),
        async move {
            last_submission.await.unwrap();
            Ok(succeeded("last"))
        }
        .boxed(),
    ];
    let mut writer = RecordingWriter::default();
    let recorded = writer.clone();
    let mut printing = Box::pin(print_outputs(tasks, "session", 3, &mut writer));

    assert!(printing.as_mut().now_or_never().is_none());
    let output = recorded.text();
    assert!(output.contains("completed-fast"));
    assert!(!output.contains("completed-slow"));
    assert!(!output.contains("completed-last"));
    assert_eq!(recorded.0.lock().unwrap().flushes, 2);

    finish_slow.send(()).unwrap();
    assert!(printing.as_mut().now_or_never().is_none());
    assert!(recorded.text().contains("completed-slow"));
    submit_last.send(()).unwrap();
    printing.await.unwrap();

    let output = recorded.text();
    assert!(output.find("completed-fast").unwrap() < output.find("completed-slow").unwrap());
    assert!(output.find("completed-slow").unwrap() < output.find("completed-last").unwrap());
    assert_eq!(output.matches("Output").count(), 1);
    assert_eq!(recorded.0.lock().unwrap().flushes, 4);
}

#[tokio::test]
async fn task_failures_and_cancellations_are_printed_as_rows() {
    let mut failed = succeeded("failed");
    failed.state = TaskState::Failed;
    failed.output = None;
    failed.error_code = Some(42);
    failed.error_message = Some("application failed".into());
    let mut cancelled = succeeded("cancelled");
    cancelled.state = TaskState::Cancelled;
    cancelled.output = None;
    cancelled.error_message = Some("cancelled by user".into());
    let tasks = [
        std::future::ready(Ok(failed)),
        std::future::ready(Ok(cancelled)),
    ];
    let mut writer = RecordingWriter::default();
    print_outputs(tasks, "session", 2, &mut writer)
        .await
        .unwrap();
    let output = writer.text();
    assert!(output.contains("Failed"));
    assert!(output.contains("code <42>: application failed"));
    assert!(output.contains("Cancelled"));
    assert!(output.contains("cancelled by user"));
}

#[tokio::test]
async fn submission_or_watch_errors_are_returned() {
    let tasks = [std::future::ready(Err(FlameError::Network(
        "watch failed".into(),
    )))];
    let mut writer = RecordingWriter::default();
    let error = print_outputs(tasks, "session", 1, &mut writer)
        .await
        .unwrap_err();
    assert!(error.to_string().contains("watch failed"));
}
