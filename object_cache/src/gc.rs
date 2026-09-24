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

use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use async_trait::async_trait;
use common::ctx::FlameCluster;
use common::FlameError;
use rpc::flame::v1::frontend_client::FrontendClient;
use rpc::flame::v1::{self as flame_rpc, ListApplicationsRequest};
use tokio::time::{interval, MissedTickBehavior};
use tonic::transport::{Channel, ClientTlsConfig, Endpoint};
use tonic::Request;

use crate::cache::{ObjectCache, ObjectKey, ObjectMetadata};

const LIST_APPLICATIONS_TIMEOUT: Duration = Duration::from_secs(5);
const STALE_GRACE_MILLIS: i64 = 10_000;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum ApplicationState {
    Enabled,
    Disabled,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct ApplicationSnapshot {
    state: ApplicationState,
    creation_time: i64,
}

type ApplicationMap = HashMap<String, ApplicationSnapshot>;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum StaleReason {
    PreviousApplication,
    MissingApplication,
}

#[derive(Clone, Debug)]
struct Candidate {
    application: String,
    metadata: ObjectMetadata,
    reason: StaleReason,
}

#[derive(Default)]
struct ReconcileStats {
    candidates: usize,
    deleted: usize,
    failures: usize,
}

#[async_trait]
trait ApplicationLister: Send {
    async fn list_applications(&mut self) -> Result<ApplicationMap, FlameError>;
}

struct FrontendApplicationLister {
    client: FrontendClient<Channel>,
}

impl FrontendApplicationLister {
    fn new(cluster: &FlameCluster) -> Result<Self, FlameError> {
        let mut endpoint = Endpoint::from_shared(cluster.endpoint.clone()).map_err(|error| {
            FlameError::InvalidConfig(format!(
                "invalid FSM frontend endpoint <{}>: {}",
                cluster.endpoint, error
            ))
        })?;

        if cluster.requires_tls() {
            let tls = match cluster.tls.as_ref() {
                Some(tls) => tls.client_tls_config()?,
                None => ClientTlsConfig::new().with_native_roots(),
            };
            endpoint = endpoint.tls_config(tls).map_err(|error| {
                FlameError::InvalidConfig(format!(
                    "invalid TLS configuration for FSM frontend <{}>: {}",
                    cluster.endpoint, error
                ))
            })?;
        }

        // A lazy channel keeps FSM availability out of FOC startup. Tonic
        // reconnects the shared HTTP/2 channel when a later cycle is due.
        let channel = endpoint.connect_lazy();
        Ok(Self {
            client: FrontendClient::new(channel),
        })
    }
}

#[async_trait]
impl ApplicationLister for FrontendApplicationLister {
    async fn list_applications(&mut self) -> Result<ApplicationMap, FlameError> {
        let mut request = Request::new(ListApplicationsRequest { state: None });
        request.set_timeout(LIST_APPLICATIONS_TIMEOUT);

        let applications = self
            .client
            .list_applications(request)
            .await
            .map_err(|error| FlameError::Network(error.to_string()))?
            .into_inner()
            .applications;

        validate_applications(applications)
    }
}

#[async_trait]
trait GarbageCollectableCache: Send + Sync {
    async fn list_all(&self) -> Result<Vec<ObjectMetadata>, FlameError>;

    async fn delete_if_unchanged(&self, expected: &ObjectMetadata) -> Result<bool, FlameError>;
}

#[async_trait]
impl GarbageCollectableCache for ObjectCache {
    async fn list_all(&self) -> Result<Vec<ObjectMetadata>, FlameError> {
        ObjectCache::list_all(self).await
    }

    async fn delete_if_unchanged(&self, expected: &ObjectMetadata) -> Result<bool, FlameError> {
        ObjectCache::delete_if_unchanged(self, expected).await
    }
}

pub(crate) struct ApplicationGarbageCollector {
    cache: Arc<dyn GarbageCollectableCache>,
    applications: Box<dyn ApplicationLister>,
    interval: Duration,
}

impl ApplicationGarbageCollector {
    pub(crate) fn new(
        cache: Arc<ObjectCache>,
        cluster: &FlameCluster,
        interval: Duration,
    ) -> Result<Self, FlameError> {
        Ok(Self {
            cache,
            applications: Box::new(FrontendApplicationLister::new(cluster)?),
            interval,
        })
    }

    pub(crate) async fn run(mut self) {
        let mut ticker = interval(self.interval);
        ticker.set_missed_tick_behavior(MissedTickBehavior::Skip);

        loop {
            // Tokio's first interval tick is immediate, so persisted stale data
            // is considered as soon as cache recovery has completed.
            ticker.tick().await;
            let started = std::time::Instant::now();
            match self.reconcile(now_millis()).await {
                Ok(stats) => tracing::info!(
                    candidates = stats.candidates,
                    deleted = stats.deleted,
                    failures = stats.failures,
                    elapsed_ms = started.elapsed().as_millis(),
                    "application cache garbage collection completed"
                ),
                Err(error) => tracing::warn!(
                    error = %error,
                    elapsed_ms = started.elapsed().as_millis(),
                    "application cache garbage collection skipped"
                ),
            }
        }
    }

    async fn reconcile(&mut self, now: i64) -> Result<ReconcileStats, FlameError> {
        let objects = self.cache.list_all().await?;
        if objects.is_empty() {
            return Ok(ReconcileStats::default());
        }

        let applications = self.applications.list_applications().await?;
        let mut candidates = Vec::new();

        for metadata in objects {
            let key = match ObjectKey::try_from(metadata.key.as_str()) {
                Ok(key) => key,
                Err(error) => {
                    tracing::warn!(key = %metadata.key, error = %error, "ignoring invalid cache key during garbage collection");
                    continue;
                }
            };

            if let Some(reason) =
                classify_stale(applications.get(&key.app_name), metadata.creation_time, now)
            {
                candidates.push(Candidate {
                    application: key.app_name,
                    metadata,
                    reason,
                });
            }
        }

        let mut stats = ReconcileStats {
            candidates: candidates.len(),
            ..ReconcileStats::default()
        };

        if !candidates.is_empty() {
            // Application lifecycle changes are not atomic with cache cleanup.
            // Recheck every candidate against one fresh snapshot before any
            // destructive work. If this request fails, delete nothing from the
            // first snapshot.
            let current_applications = self.applications.list_applications().await?;
            candidates.retain_mut(|candidate| {
                match classify_stale(
                    current_applications.get(&candidate.application),
                    candidate.metadata.creation_time,
                    now,
                ) {
                    Some(reason) => {
                        candidate.reason = reason;
                        true
                    }
                    None => false,
                }
            });
        }

        for candidate in candidates {
            match self.cache.delete_if_unchanged(&candidate.metadata).await {
                Ok(true) => {
                    stats.deleted += 1;
                    tracing::debug!(
                        application = %candidate.application,
                        key = %candidate.metadata.key,
                        reason = ?candidate.reason,
                        "deleted stale cache object"
                    );
                }
                Ok(false) => tracing::debug!(
                    application = %candidate.application,
                    key = %candidate.metadata.key,
                    "stale cache candidate changed before deletion"
                ),
                Err(error) => {
                    stats.failures += 1;
                    tracing::warn!(
                        application = %candidate.application,
                        key = %candidate.metadata.key,
                        error = %error,
                        "failed to delete stale cache object"
                    );
                }
            }
        }

        Ok(stats)
    }

    #[cfg(test)]
    fn with_dependencies(
        cache: Arc<dyn GarbageCollectableCache>,
        applications: Box<dyn ApplicationLister>,
    ) -> Self {
        Self {
            cache,
            applications,
            interval: Duration::from_secs(60),
        }
    }
}

fn validate_applications(
    applications: Vec<flame_rpc::Application>,
) -> Result<ApplicationMap, FlameError> {
    let mut result = HashMap::with_capacity(applications.len());

    for application in applications {
        let metadata = application.metadata.ok_or_else(|| {
            FlameError::InvalidState(
                "ListApplications returned an application without metadata".to_string(),
            )
        })?;
        if metadata.name.is_empty() {
            return Err(FlameError::InvalidState(
                "ListApplications returned an application with an empty name".to_string(),
            ));
        }

        let status = application.status.ok_or_else(|| {
            FlameError::InvalidState(format!(
                "ListApplications returned application <{}> without status",
                metadata.name
            ))
        })?;
        if status.creation_time <= 0 {
            return Err(FlameError::InvalidState(format!(
                "ListApplications returned application <{}> with invalid creation time <{}>",
                metadata.name, status.creation_time
            )));
        }

        let state = match flame_rpc::ApplicationState::try_from(status.state).map_err(|_| {
            FlameError::InvalidState(format!(
                "ListApplications returned application <{}> with unknown state <{}>",
                metadata.name, status.state
            ))
        })? {
            flame_rpc::ApplicationState::Enabled => ApplicationState::Enabled,
            flame_rpc::ApplicationState::Disabled => ApplicationState::Disabled,
        };

        if result
            .insert(
                metadata.name.clone(),
                ApplicationSnapshot {
                    state,
                    creation_time: status.creation_time,
                },
            )
            .is_some()
        {
            return Err(FlameError::InvalidState(format!(
                "ListApplications returned duplicate application <{}>",
                metadata.name
            )));
        }
    }

    Ok(result)
}

fn classify_stale(
    application: Option<&ApplicationSnapshot>,
    object_creation_time: i64,
    now: i64,
) -> Option<StaleReason> {
    match application {
        Some(ApplicationSnapshot {
            state: ApplicationState::Enabled,
            creation_time,
        }) if object_creation_time.saturating_add(STALE_GRACE_MILLIS) < *creation_time => {
            Some(StaleReason::PreviousApplication)
        }
        Some(_) => None,
        None if object_creation_time.saturating_add(STALE_GRACE_MILLIS) < now => {
            Some(StaleReason::MissingApplication)
        }
        None => None,
    }
}

fn now_millis() -> i64 {
    let millis = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis();
    millis.min(i64::MAX as u128) as i64
}

#[cfg(test)]
mod tests {
    use std::collections::{HashSet, VecDeque};
    use std::sync::Mutex;

    use super::*;

    fn application(name: &str, state: i32, creation_time: i64) -> flame_rpc::Application {
        flame_rpc::Application {
            metadata: Some(flame_rpc::Metadata {
                id: name.to_string(),
                name: name.to_string(),
            }),
            spec: None,
            status: Some(flame_rpc::ApplicationStatus {
                state,
                creation_time,
            }),
        }
    }

    fn object(key: &str, creation_time: i64) -> ObjectMetadata {
        ObjectMetadata {
            endpoint: "grpc://cache:9090".to_string(),
            key: key.to_string(),
            version: 1,
            size: 10,
            delta_count: 0,
            creation_time,
            data_type: "raw".to_string(),
        }
    }

    #[test]
    fn classifier_uses_strict_grace_boundary() {
        let enabled = ApplicationSnapshot {
            state: ApplicationState::Enabled,
            creation_time: 20_000,
        };

        assert_eq!(
            classify_stale(Some(&enabled), 9_999, 30_000),
            Some(StaleReason::PreviousApplication)
        );
        assert_eq!(classify_stale(Some(&enabled), 10_000, 30_000), None);
        assert_eq!(
            classify_stale(None, 9_999, 20_000),
            Some(StaleReason::MissingApplication)
        );
        assert_eq!(classify_stale(None, 10_000, 20_000), None);
    }

    #[test]
    fn classifier_retains_disabled_current_and_future_objects() {
        let disabled = ApplicationSnapshot {
            state: ApplicationState::Disabled,
            creation_time: 100_000,
        };
        let enabled = ApplicationSnapshot {
            state: ApplicationState::Enabled,
            creation_time: 100_000,
        };

        assert_eq!(classify_stale(Some(&disabled), 1, 200_000), None);
        assert_eq!(classify_stale(Some(&enabled), 100_001, 200_000), None);
        assert_eq!(classify_stale(None, 200_001, 200_000), None);
        assert_eq!(classify_stale(None, i64::MAX, i64::MAX), None);
    }

    #[test]
    fn application_validation_rejects_malformed_responses() {
        let mut missing_metadata =
            application("app", flame_rpc::ApplicationState::Enabled as i32, 1);
        missing_metadata.metadata = None;
        assert!(validate_applications(vec![missing_metadata]).is_err());

        let mut missing_status = application("app", flame_rpc::ApplicationState::Enabled as i32, 1);
        missing_status.status = None;
        assert!(validate_applications(vec![missing_status]).is_err());

        assert!(validate_applications(vec![application("app", 99, 1)]).is_err());
        assert!(validate_applications(vec![application(
            "app",
            flame_rpc::ApplicationState::Enabled as i32,
            0,
        )])
        .is_err());
        assert!(validate_applications(vec![
            application("app", flame_rpc::ApplicationState::Enabled as i32, 1),
            application("app", flame_rpc::ApplicationState::Disabled as i32, 2),
        ])
        .is_err());
    }

    #[test]
    fn application_validation_preserves_raw_state_and_time() {
        let applications = validate_applications(vec![
            application("enabled", flame_rpc::ApplicationState::Enabled as i32, 11),
            application("disabled", flame_rpc::ApplicationState::Disabled as i32, 22),
        ])
        .unwrap();

        assert_eq!(applications["enabled"].state, ApplicationState::Enabled);
        assert_eq!(applications["enabled"].creation_time, 11);
        assert_eq!(applications["disabled"].state, ApplicationState::Disabled);
        assert_eq!(applications["disabled"].creation_time, 22);
    }

    struct MockLister {
        responses: VecDeque<Result<ApplicationMap, FlameError>>,
        calls: Arc<Mutex<usize>>,
    }

    #[async_trait]
    impl ApplicationLister for MockLister {
        async fn list_applications(&mut self) -> Result<ApplicationMap, FlameError> {
            *self.calls.lock().unwrap() += 1;
            self.responses.pop_front().expect("unexpected list call")
        }
    }

    struct MockCache {
        objects: Vec<ObjectMetadata>,
        deleted: Mutex<Vec<String>>,
        fail_keys: HashSet<String>,
    }

    #[async_trait]
    impl GarbageCollectableCache for MockCache {
        async fn list_all(&self) -> Result<Vec<ObjectMetadata>, FlameError> {
            Ok(self.objects.clone())
        }

        async fn delete_if_unchanged(&self, expected: &ObjectMetadata) -> Result<bool, FlameError> {
            if self.fail_keys.contains(&expected.key) {
                return Err(FlameError::Storage("injected failure".to_string()));
            }
            self.deleted.lock().unwrap().push(expected.key.clone());
            Ok(true)
        }
    }

    fn collector(
        cache: Arc<MockCache>,
        responses: Vec<Result<ApplicationMap, FlameError>>,
        calls: Arc<Mutex<usize>>,
    ) -> ApplicationGarbageCollector {
        ApplicationGarbageCollector::with_dependencies(
            cache,
            Box::new(MockLister {
                responses: responses.into(),
                calls,
            }),
        )
    }

    #[tokio::test]
    async fn missing_candidates_are_rechecked_before_deletion() {
        let cache = Arc::new(MockCache {
            objects: vec![object("app/session/object", 1)],
            deleted: Mutex::new(Vec::new()),
            fail_keys: HashSet::new(),
        });
        let calls = Arc::new(Mutex::new(0));
        let appeared = HashMap::from([(
            "app".to_string(),
            ApplicationSnapshot {
                state: ApplicationState::Enabled,
                creation_time: 5_000,
            },
        )]);
        let mut collector = collector(
            cache.clone(),
            vec![Ok(HashMap::new()), Ok(appeared)],
            calls.clone(),
        );

        let stats = collector.reconcile(20_000).await.unwrap();

        assert_eq!(*calls.lock().unwrap(), 2);
        assert_eq!(stats.candidates, 1);
        assert_eq!(stats.deleted, 0);
        assert!(cache.deleted.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn enabled_candidates_are_rechecked_and_delete_exact_objects() {
        let cache = Arc::new(MockCache {
            objects: vec![
                object("app/session/old", 1),
                object("app/session/current", 15_000),
            ],
            deleted: Mutex::new(Vec::new()),
            fail_keys: HashSet::new(),
        });
        let calls = Arc::new(Mutex::new(0));
        let applications = HashMap::from([(
            "app".to_string(),
            ApplicationSnapshot {
                state: ApplicationState::Enabled,
                creation_time: 15_000,
            },
        )]);
        let mut collector = collector(
            cache.clone(),
            vec![Ok(applications.clone()), Ok(applications)],
            calls.clone(),
        );

        let stats = collector.reconcile(30_000).await.unwrap();

        assert_eq!(*calls.lock().unwrap(), 2);
        assert_eq!(stats.deleted, 1);
        assert_eq!(&*cache.deleted.lock().unwrap(), &["app/session/old"]);
    }

    #[tokio::test]
    async fn failed_second_list_deletes_nothing() {
        let cache = Arc::new(MockCache {
            objects: vec![
                object("missing/session/object", 1),
                object("enabled/session/object", 1),
            ],
            deleted: Mutex::new(Vec::new()),
            fail_keys: HashSet::new(),
        });
        let applications = HashMap::from([(
            "enabled".to_string(),
            ApplicationSnapshot {
                state: ApplicationState::Enabled,
                creation_time: 15_000,
            },
        )]);
        let calls = Arc::new(Mutex::new(0));
        let mut collector = collector(
            cache.clone(),
            vec![
                Ok(applications),
                Err(FlameError::Network("unavailable".to_string())),
            ],
            calls,
        );

        assert!(collector.reconcile(20_000).await.is_err());
        assert!(cache.deleted.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn one_delete_failure_does_not_stop_other_candidates() {
        let cache = Arc::new(MockCache {
            objects: vec![
                object("app/session/fail", 1),
                object("app/session/delete", 2),
            ],
            deleted: Mutex::new(Vec::new()),
            fail_keys: HashSet::from(["app/session/fail".to_string()]),
        });
        let applications = HashMap::from([(
            "app".to_string(),
            ApplicationSnapshot {
                state: ApplicationState::Enabled,
                creation_time: 20_000,
            },
        )]);
        let calls = Arc::new(Mutex::new(0));
        let mut collector = collector(
            cache.clone(),
            vec![Ok(applications.clone()), Ok(applications)],
            calls,
        );

        let stats = collector.reconcile(30_000).await.unwrap();

        assert_eq!(stats.failures, 1);
        assert_eq!(stats.deleted, 1);
        assert_eq!(&*cache.deleted.lock().unwrap(), &["app/session/delete"]);
    }
}
