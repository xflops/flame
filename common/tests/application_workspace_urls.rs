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
use common::{apis::ApplicationAttributes, ctx::FlameClusterContext, storage};

#[tokio::test]
async fn application_cache_references_stay_in_workspace() {
    let dir = tempfile::tempdir().unwrap();
    std::env::set_var("FLAME_TEST_DIR", dir.path());
    for backend in [
        "none".into(),
        format!("sqlite://{}", dir.path().join("store.db").display()),
        format!("filesystem://{}", dir.path().join("store").display()),
    ] {
        let mut config = FlameClusterContext::default();
        config.cluster.storage = backend;
        let store = storage::new_ptr(&config).await.unwrap();
        for (index, scheme) in ["grpc", "grpcs", "grpc+tls", "grpcs-proxy"]
            .iter()
            .enumerate()
        {
            let name = format!("app{index}");
            let wrong = ApplicationAttributes {
                url: Some(format!("{scheme}://cache:9090/other/app/pkg/archive")),
                ..Default::default()
            };
            assert!(store
                .register_application("default".into(), name.clone(), wrong.clone())
                .await
                .is_err());
            assert!(store.get_application("default", &name).await.is_err());
            let good = ApplicationAttributes {
                url: Some(format!("{scheme}://cache:9090/default/app/pkg/archive")),
                ..Default::default()
            };
            store
                .register_application("default".into(), name.clone(), good.clone())
                .await
                .unwrap();
            assert!(store
                .update_application("default", &name, wrong)
                .await
                .is_err());
            assert_eq!(
                store.get_application("default", &name).await.unwrap().url,
                good.url
            );
            store
                .update_application("default", &name, good)
                .await
                .unwrap();
        }
        for (index, url) in [
            "https://example.com/archive.tar.gz",
            "file:///tmp/archive.tar.gz",
        ]
        .iter()
        .enumerate()
        {
            store
                .register_application(
                    "default".into(),
                    format!("external{index}"),
                    ApplicationAttributes {
                        url: Some(url.to_string()),
                        ..Default::default()
                    },
                )
                .await
                .unwrap();
        }
    }
}
