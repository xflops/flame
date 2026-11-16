/*
Copyright 2025 The xflops Authors.
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
use bytes::Bytes;

/// Metadata paths omit execution data while authoritative history retains it.
#[tokio::test]
async fn metadata_reads_preserve_full_history_and_closed_counters() {
    let directory = tempfile::tempdir().unwrap();
    for url in [
        "none".to_string(),
        format!("fs://{}", directory.path().display()),
    ] {
        let engine = connect(&url).await.unwrap();
        engine
            .register_application("app".into(), ApplicationAttributes::default())
            .await
            .unwrap();
        engine
            .create_session(SessionAttributes {
                id: "session".into(),
                application: "app".into(),
                common_data: Some(Bytes::from_static(b"common payload")),
                tokens: [("credential".into(), "secret".into())].into(),
                ..Default::default()
            })
            .await
            .unwrap();
        for _ in 0..3 {
            let task = engine
                .create_task(
                    "session".into(),
                    Some(Bytes::from_static(b"input")),
                    Some(TaskOptions {
                        affinity: [Bytes::from_static(b"affinity")].into(),
                    }),
                )
                .await
                .unwrap();
            engine
                .update_task_result(
                    task,
                    TaskResult {
                        state: TaskState::Succeed,
                        output: Some(Bytes::from_static(b"output")),
                        message: None,
                    },
                )
                .await
                .unwrap();
        }
        engine.close_session("session".into()).await.unwrap();
        let sessions = engine.find_session_metadata().await.unwrap();
        assert_eq!(sessions.len(), 1);
        let session = &sessions[0];
        assert!(session.is_closed());
        assert_eq!(session.task_counts.succeed, 3);
        assert!(session.common_data.is_none() && session.tokens.is_empty());
        assert!(
            session.tasks.is_empty() && session.tasks_index.is_empty() && session.events.is_empty()
        );
        let mut filter = TaskFilter::by_session_states("session", vec![TaskState::Succeed]);
        filter.ids = Some(vec![1, 3]);
        let tasks = engine.find_task_metadata(&filter).await.unwrap();
        assert_eq!(tasks.len(), 2);
        assert!(tasks.iter().all(|task| task.input.is_none()
            && task.output.is_none()
            && task.affinity.is_empty()
            && task.events.is_empty()));
        assert!(tasks.iter().all(|task| [1, 3].contains(&task.id)));
        assert_eq!(tasks.iter().map(|task| task.id).collect::<Vec<_>>(), [3, 1]);
        filter.paging = Some(
            crate::apis::Paging::new()
                .with_limit(1)
                .unwrap()
                .with_number(2)
                .unwrap(),
        );
        let page = engine.find_task_metadata(&filter).await.unwrap();
        assert_eq!(page.iter().map(|task| task.id).collect::<Vec<_>>(), [1]);
        assert_eq!(page.paging.total(), Some(2));
        filter.paging = None;
        filter.ids = Some(vec![]);
        assert!(engine.find_task_metadata(&filter).await.unwrap().is_empty());
        let recovered = engine
            .find_task_metadata(&TaskFilter::by_session("session"))
            .await
            .unwrap();
        assert_eq!(recovered.len(), 3);
        for header in recovered {
            assert!(header.input.is_none() && header.output.is_none());
            let task = engine.get_task(header.gid()).await.unwrap();
            assert_eq!(task.input.as_deref(), Some(b"input".as_slice()));
            assert_eq!(task.output.as_deref(), Some(b"output".as_slice()));
            assert!(!task.affinity.is_empty());
        }
        engine.delete_session("session".into()).await.unwrap();
    }
}
