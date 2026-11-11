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
use common::{FlameError, PACKAGE_SESSION, SHARED_SESSION};
use rpc::flame::v1::frontend_client::FrontendClient;
use rpc::flame::v1::{self as flame_rpc, ListApplicationsRequest, ListSessionsRequest};
use tokio::time::{interval, MissedTickBehavior};
use tonic::transport::{Channel, ClientTlsConfig, Endpoint};
use tonic::Request;

use super::Controller;
use crate::model::{ObjectFilter, ObjectKey, ObjectMetadata};

const LIST_TIMEOUT: Duration = Duration::from_secs(5);
const STALE_GRACE_MILLIS: i64 = 10_000;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum ApplicationState {
    Enabled,
    Disabled,
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) struct ApplicationSnapshot {
    pub(super) state: ApplicationState,
    pub(super) creation_time: i64,
}
type ApplicationMap = HashMap<String, ApplicationSnapshot>;
type SessionMap = HashMap<String, String>;
#[derive(Clone, Default)]
pub(super) struct LivenessSnapshot {
    pub(super) applications: ApplicationMap,
    pub(super) sessions: SessionMap,
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum StaleReason {
    MissingSession,
    PreviousApplication,
    MissingApplication,
}
struct Candidate {
    key: ObjectKey,
    metadata: ObjectMetadata,
    reason: StaleReason,
}
#[derive(Default)]
pub(super) struct ReconcileStats {
    pub(super) candidates: usize,
    pub(super) deleted: usize,
    pub(super) failures: usize,
}

#[async_trait]
pub(super) trait LivenessProvider: Send {
    /// Return validated application and session identities, including every session state.
    async fn snapshot(&mut self) -> Result<LivenessSnapshot, FlameError>;
}
struct FrontendLivenessProvider {
    client: FrontendClient<Channel>,
}
impl FrontendLivenessProvider {
    /// Build one lazy frontend client for application and session liveness queries.
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
        Ok(Self {
            client: FrontendClient::new(endpoint.connect_lazy()),
        })
    }
}
#[async_trait]
impl LivenessProvider for FrontendLivenessProvider {
    /// Fetch unfiltered application and session lists, failing closed if either RPC or validation fails.
    async fn snapshot(&mut self) -> Result<LivenessSnapshot, FlameError> {
        let mut request = Request::new(ListApplicationsRequest { state: None });
        request.set_timeout(LIST_TIMEOUT);
        let applications = validate_applications(
            self.client
                .list_applications(request)
                .await
                .map_err(|error| FlameError::Network(error.to_string()))?
                .into_inner()
                .applications,
        )?;
        let mut request = Request::new(ListSessionsRequest::default());
        request.set_timeout(LIST_TIMEOUT);
        let sessions = validate_sessions(
            self.client
                .list_sessions(request)
                .await
                .map_err(|error| FlameError::Network(error.to_string()))?
                .into_inner()
                .sessions,
        )?;
        Ok(LivenessSnapshot {
            applications,
            sessions,
        })
    }
}

#[async_trait]
pub(super) trait GarbageCollectableStorage: Send + Sync {
    /// List the filesystem object metadata eligible for garbage-collection classification.
    async fn list(&self, filter: &ObjectFilter) -> Result<Vec<ObjectMetadata>, FlameError>;
    /// Delete one exact stale object without waiting for readers or accepted writes.
    async fn delete(&self, key: &ObjectKey) -> Result<(), FlameError>;
}
#[async_trait]
impl GarbageCollectableStorage for Controller {
    /// List the filesystem object metadata through the controller's storage adapter.
    async fn list(&self, filter: &ObjectFilter) -> Result<Vec<ObjectMetadata>, FlameError> {
        Controller::list(self, filter).await
    }
    /// Remove stale session data internally without exposing a public delete RPC.
    async fn delete(&self, key: &ObjectKey) -> Result<(), FlameError> {
        Controller::delete(self, key).await
    }
}

pub(crate) struct GarbageCollector {
    storage: Arc<dyn GarbageCollectableStorage>,
    liveness: Box<dyn LivenessProvider>,
    interval: Duration,
}
impl GarbageCollector {
    /// Configure session-aware filesystem cleanup without requiring FSM availability during startup.
    pub(crate) fn new(
        storage: Arc<Controller>,
        cluster: &FlameCluster,
        interval: Duration,
    ) -> Result<Self, FlameError> {
        Ok(Self {
            storage,
            liveness: Box::new(FrontendLivenessProvider::new(cluster)?),
            interval,
        })
    }
    /// Reconcile periodically and keep later cycles available after transient listing or deletion failures.
    pub(crate) async fn run(mut self) {
        let mut ticker = interval(self.interval);
        ticker.set_missed_tick_behavior(MissedTickBehavior::Skip);
        loop {
            ticker.tick().await;
            let started = std::time::Instant::now();
            match self.reconcile(now_millis()).await {
                Ok(stats) => tracing::info!(
                    candidates = stats.candidates,
                    deleted = stats.deleted,
                    failures = stats.failures,
                    elapsed_ms = started.elapsed().as_millis(),
                    "object storage garbage collection completed"
                ),
                Err(error) => {
                    tracing::warn!(error = %error, elapsed_ms = started.elapsed().as_millis(), "object storage garbage collection skipped")
                }
            }
        }
    }
    /// Recheck a complete validated liveness snapshot before deleting confirmed absent-session or stale-package data.
    pub(super) async fn reconcile(&mut self, now: i64) -> Result<ReconcileStats, FlameError> {
        let objects = self.storage.list(&ObjectFilter::AllObject).await?;
        if objects.is_empty() {
            return Ok(ReconcileStats::default());
        }
        let snapshot = self.liveness.snapshot().await?;
        let mut candidates = Vec::new();
        for metadata in objects {
            let key = match ObjectKey::try_from(metadata.key.as_str()) {
                Ok(key) => key,
                Err(error) => {
                    tracing::warn!(key = %metadata.key, error = %error, "ignoring invalid object key during garbage collection");
                    continue;
                }
            };
            if let Some(reason) = classify_stale(&snapshot, &key, metadata.creation_time, now) {
                candidates.push(Candidate {
                    key,
                    metadata,
                    reason,
                });
            }
        }
        let mut stats = ReconcileStats {
            candidates: candidates.len(),
            ..Default::default()
        };
        if !candidates.is_empty() {
            // Neither RPC list is atomic with deletion. A fresh validated pair is required before any destructive work.
            let current = self.liveness.snapshot().await?;
            candidates.retain_mut(|candidate| {
                if let Some(reason) = classify_stale(
                    &current,
                    &candidate.key,
                    candidate.metadata.creation_time,
                    now,
                ) {
                    candidate.reason = reason;
                    true
                } else {
                    false
                }
            });
        }
        for candidate in candidates {
            match self.storage.delete(&candidate.key).await {
                Ok(()) => {
                    stats.deleted += 1;
                    tracing::debug!(key = %candidate.metadata.key, reason = ?candidate.reason, "deleted stale object");
                }
                Err(error) => {
                    stats.failures += 1;
                    tracing::warn!(key = %candidate.metadata.key, error = %error, "failed to delete stale object");
                }
            }
        }
        Ok(stats)
    }
    #[cfg(test)]
    /// Inject validated liveness snapshots and observable storage for deterministic collection tests.
    pub(super) fn with_dependencies(
        storage: Arc<dyn GarbageCollectableStorage>,
        liveness: Box<dyn LivenessProvider>,
    ) -> Self {
        Self {
            storage,
            liveness,
            interval: Duration::from_secs(60),
        }
    }
}

/// Reject malformed application records before they can authorize application-owned object deletion.
pub(super) fn validate_applications(
    applications: Vec<flame_rpc::Application>,
) -> Result<ApplicationMap, FlameError> {
    let mut result = HashMap::with_capacity(applications.len());
    for application in applications {
        let metadata = application
            .metadata
            .ok_or_else(|| invalid("ListApplications returned an application without metadata"))?;
        let key = ObjectKey::from_path(&format!("{}/{PACKAGE_SESSION}", metadata.name))
            .map_err(|_| invalid("ListApplications returned an invalid application name"))?;
        if key.app_name != metadata.name
            || key.session_id != PACKAGE_SESSION
            || key.object_id.is_some()
        {
            return Err(invalid(
                "ListApplications returned an invalid application name",
            ));
        }
        let status = application
            .status
            .ok_or_else(|| invalid("ListApplications returned an application without status"))?;
        if status.creation_time <= 0 {
            return Err(invalid(
                "ListApplications returned an invalid creation time",
            ));
        }
        let state = match flame_rpc::ApplicationState::try_from(status.state)
            .map_err(|_| invalid("ListApplications returned an unknown state"))?
        {
            flame_rpc::ApplicationState::Enabled => ApplicationState::Enabled,
            flame_rpc::ApplicationState::Disabled => ApplicationState::Disabled,
        };
        if result
            .insert(
                metadata.name,
                ApplicationSnapshot {
                    state,
                    creation_time: status.creation_time,
                },
            )
            .is_some()
        {
            return Err(invalid("ListApplications returned a duplicate application"));
        }
    }
    Ok(result)
}

/// Validate every session identity and application owner while retaining open, closed, and suspended sessions alike.
pub(super) fn validate_sessions(
    sessions: Vec<flame_rpc::Session>,
) -> Result<SessionMap, FlameError> {
    let mut result = HashMap::with_capacity(sessions.len());
    for session in sessions {
        let metadata = session
            .metadata
            .ok_or_else(|| invalid("ListSessions returned a session without metadata"))?;
        let spec = session
            .spec
            .ok_or_else(|| invalid("ListSessions returned a session without spec"))?;
        let status = session
            .status
            .ok_or_else(|| invalid("ListSessions returned a session without status"))?;
        let key = ObjectKey::from_path(&format!("{}/{}", spec.application, metadata.id))
            .map_err(|_| invalid("ListSessions returned an invalid session identity"))?;
        if key.app_name != spec.application
            || key.session_id != metadata.id
            || key.object_id.is_some()
            || key.is_all_sessions()
        {
            return Err(invalid("ListSessions returned an invalid session identity"));
        }
        flame_rpc::SessionState::try_from(status.state)
            .map_err(|_| invalid("ListSessions returned an unknown state"))?;
        if status.creation_time <= 0 {
            return Err(invalid("ListSessions returned an invalid creation time"));
        }
        if result.insert(metadata.id, spec.application).is_some() {
            return Err(invalid("ListSessions returned a duplicate session id"));
        }
    }
    Ok(result)
}

/// Classify session data by absence only; package and shared namespaces follow application ownership.
pub(super) fn classify_stale(
    snapshot: &LivenessSnapshot,
    key: &ObjectKey,
    creation_time: i64,
    now: i64,
) -> Option<StaleReason> {
    if matches!(key.session_id.as_str(), PACKAGE_SESSION | SHARED_SESSION) {
        return match snapshot.applications.get(&key.app_name) {
            Some(ApplicationSnapshot {
                state: ApplicationState::Enabled,
                creation_time: application_time,
            }) if creation_time.saturating_add(STALE_GRACE_MILLIS) < *application_time => {
                Some(StaleReason::PreviousApplication)
            }
            Some(_) => None,
            None if creation_time.saturating_add(STALE_GRACE_MILLIS) < now => {
                Some(StaleReason::MissingApplication)
            }
            None => None,
        };
    }
    if !snapshot
        .sessions
        .get(&key.session_id)
        .is_some_and(|application| application == &key.app_name)
        && creation_time.saturating_add(STALE_GRACE_MILLIS) < now
    {
        Some(StaleReason::MissingSession)
    } else {
        None
    }
}
/// Describe an invalid liveness response without authorizing any deletion.
fn invalid(message: &str) -> FlameError {
    FlameError::InvalidState(message.into())
}
/// Return the current Unix timestamp in milliseconds for the creation grace period.
fn now_millis() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
        .min(i64::MAX as u128) as i64
}
