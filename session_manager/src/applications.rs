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
use std::ffi::OsString;
use std::fs;
use std::future::Future;
use std::path::{Path, PathBuf};

use common::apis::{Application, ApplicationAttributes};
use common::application::parse_application_manifests;
use common::{FlameError, FLAME_HOME};

const DEFAULT_FLAME_HOME: &str = "/usr/local/flame";

#[derive(Debug)]
pub struct ConfiguredApplication {
    pub name: String,
    pub attributes: ApplicationAttributes,
}

#[derive(Debug, PartialEq, Eq)]
pub enum Reconciliation {
    Registered,
    Update,
}

/// Returns the installation-owned application manifest directory.
pub fn manifest_directory() -> PathBuf {
    manifest_directory_from_home(std::env::var_os(FLAME_HOME))
}

fn manifest_directory_from_home(flame_home: Option<OsString>) -> PathBuf {
    PathBuf::from(flame_home.unwrap_or_else(|| OsString::from(DEFAULT_FLAME_HOME)))
        .join("conf/applications")
}

pub async fn reconcile<Register, RegisterFuture, Update, UpdateFuture>(
    applications: Vec<ConfiguredApplication>,
    mut register: Register,
    mut update: Update,
) -> Result<(), FlameError>
where
    Register: FnMut(String, ApplicationAttributes) -> RegisterFuture,
    RegisterFuture: Future<Output = Result<(), FlameError>>,
    Update: FnMut(String, ApplicationAttributes) -> UpdateFuture,
    UpdateFuture: Future<Output = Result<(), FlameError>>,
{
    for application in applications {
        let result = register(application.name.clone(), application.attributes.clone()).await;
        match reconciliation_after_register(result)? {
            Reconciliation::Registered => {
                tracing::info!("Registered configured application <{}>", application.name);
            }
            Reconciliation::Update => {
                update(application.name.clone(), application.attributes).await?;
                tracing::info!("Updated configured application <{}>", application.name);
            }
        }
    }
    Ok(())
}

/// Determines the second phase of reconciling a configured application. A
/// manifest is authoritative for its own name, so an existing definition must
/// be updated while a newly registered definition needs no more work.
pub fn reconciliation_after_register(
    result: Result<(), FlameError>,
) -> Result<Reconciliation, FlameError> {
    match result {
        Ok(()) => Ok(Reconciliation::Registered),
        Err(FlameError::AlreadyExist(_)) => Ok(Reconciliation::Update),
        Err(error) => Err(error),
    }
}

pub(crate) fn matches_attributes(
    application: &Application,
    attributes: &ApplicationAttributes,
) -> bool {
    application.shim == attributes.shim
        && application.image == attributes.image
        && application.description == attributes.description
        && application.labels == attributes.labels
        && application.command == attributes.command
        && application.arguments == attributes.arguments
        && application.environments == attributes.environments
        && application.working_directory == attributes.working_directory
        && application.max_instances == attributes.max_instances
        && application.delay_release == attributes.delay_release
        && match (&application.schema, &attributes.schema) {
            (Some(current), Some(configured)) => {
                current.input == configured.input
                    && current.output == configured.output
                    && current.common_data == configured.common_data
            }
            (None, None) => true,
            _ => false,
        }
        && application.url == attributes.url
        && application.installer == attributes.installer
}

/// Loads configured applications from YAML files in deterministic path and
/// document order. All files and names are validated before the caller
/// registers or updates any application.
pub fn load(directory: &Path) -> Result<Vec<ConfiguredApplication>, FlameError> {
    if !directory.exists() {
        return Ok(Vec::new());
    }
    if !directory.is_dir() {
        return Err(FlameError::InvalidConfig(format!(
            "application manifest path <{}> is not a directory",
            directory.display()
        )));
    }

    let mut paths = fs::read_dir(directory)
        .map_err(|error| path_error(directory, "read directory", error))?
        .map(|entry| {
            entry
                .map(|entry| entry.path())
                .map_err(|error| path_error(directory, "read directory entry", error))
        })
        .collect::<Result<Vec<_>, _>>()?;
    paths.retain(|path| {
        path.is_file()
            && matches!(
                path.extension().and_then(|extension| extension.to_str()),
                Some("yaml" | "yml")
            )
    });
    paths.sort();

    let mut applications = Vec::new();
    let mut names = HashMap::<String, PathBuf>::new();
    for path in paths {
        let contents = fs::read_to_string(&path)
            .map_err(|error| path_error(&path, "read application manifest", error))?;
        let manifests = parse_application_manifests(&contents).map_err(|error| {
            FlameError::InvalidConfig(format!(
                "invalid application manifest <{}>: {}",
                path.display(),
                error
            ))
        })?;

        for manifest in manifests {
            let name = manifest.metadata.name.clone();
            if let Some(previous_path) = names.insert(name.clone(), path.clone()) {
                return Err(FlameError::InvalidConfig(format!(
                    "duplicate application <{}> in <{}> and <{}>",
                    name,
                    previous_path.display(),
                    path.display()
                )));
            }
            let attributes = manifest.attributes().map_err(|error| {
                FlameError::InvalidConfig(format!(
                    "invalid application manifest <{}>: {}",
                    path.display(),
                    error
                ))
            })?;
            applications.push(ConfiguredApplication { name, attributes });
        }
    }

    Ok(applications)
}

fn path_error(path: &Path, action: &str, error: std::io::Error) -> FlameError {
    FlameError::InvalidConfig(format!(
        "failed to {} <{}>: {}",
        action,
        path.display(),
        error
    ))
}

#[cfg(test)]
mod tests {
    use std::ffi::OsString;
    use std::fs;
    use std::sync::{Arc, Mutex};

    use tempfile::tempdir;

    use super::*;

    #[test]
    fn manifest_directory_is_under_flame_home() {
        assert_eq!(
            manifest_directory_from_home(Some(OsString::from("/opt/flame-test"))),
            PathBuf::from("/opt/flame-test/conf/applications")
        );
        assert_eq!(
            manifest_directory_from_home(None),
            PathBuf::from("/usr/local/flame/conf/applications")
        );
    }

    #[test]
    fn missing_directory_is_empty() {
        let root = tempdir().unwrap();
        assert!(load(&root.path().join("missing")).unwrap().is_empty());
    }

    #[test]
    fn loads_yaml_files_in_sorted_order_and_ignores_other_files() {
        let root = tempdir().unwrap();
        fs::write(
            root.path().join("b.yml"),
            "metadata:\n  name: second\nspec: {}\n",
        )
        .unwrap();
        fs::write(
            root.path().join("a.yaml"),
            "metadata:\n  name: first\nspec: {}\n",
        )
        .unwrap();
        fs::write(root.path().join("ignored.txt"), "not yaml").unwrap();

        let names = load(root.path())
            .unwrap()
            .into_iter()
            .map(|application| application.name)
            .collect::<Vec<_>>();
        assert_eq!(names, ["first", "second"]);
    }

    #[test]
    fn rejects_duplicates_before_returning_applications() {
        let root = tempdir().unwrap();
        for file in ["a.yaml", "b.yaml"] {
            fs::write(
                root.path().join(file),
                "metadata:\n  name: duplicate\nspec: {}\n",
            )
            .unwrap();
        }

        let error = load(root.path()).unwrap_err();
        assert!(error.to_string().contains("duplicate application"));
        assert!(error.to_string().contains("a.yaml"));
        assert!(error.to_string().contains("b.yaml"));
    }

    #[test]
    fn malformed_error_includes_path() {
        let root = tempdir().unwrap();
        let path = root.path().join("broken.yaml");
        fs::write(&path, "metadata: [\n").unwrap();

        let error = load(root.path()).unwrap_err();
        assert!(error.to_string().contains(path.to_str().unwrap()));
    }

    #[test]
    fn existing_application_is_reconciled_by_update() {
        assert_eq!(
            reconciliation_after_register(Err(FlameError::AlreadyExist("already exists".into())))
                .unwrap(),
            Reconciliation::Update
        );
    }

    #[test]
    fn newly_registered_application_needs_no_update() {
        assert_eq!(
            reconciliation_after_register(Ok(())).unwrap(),
            Reconciliation::Registered
        );
    }

    #[test]
    fn other_registration_errors_are_propagated() {
        let error =
            reconciliation_after_register(Err(FlameError::Storage("database unavailable".into())))
                .unwrap_err();
        assert!(matches!(error, FlameError::Storage(_)));
    }

    #[tokio::test]
    async fn reconciliation_registers_missing_application_without_update() {
        let updates = Arc::new(Mutex::new(Vec::new()));
        let captured_updates = updates.clone();
        reconcile(
            vec![ConfiguredApplication {
                name: "new".into(),
                attributes: ApplicationAttributes::default(),
            }],
            |_name, _attributes| async { Ok(()) },
            move |name, _attributes| {
                let updates = captured_updates.clone();
                async move {
                    updates.lock().unwrap().push(name);
                    Ok(())
                }
            },
        )
        .await
        .unwrap();

        assert!(updates.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn reconciliation_updates_existing_application_with_manifest() {
        let updates = Arc::new(Mutex::new(Vec::new()));
        let captured_updates = updates.clone();
        let attributes = ApplicationAttributes {
            command: Some("configured-command".into()),
            ..ApplicationAttributes::default()
        };
        reconcile(
            vec![ConfiguredApplication {
                name: "existing".into(),
                attributes,
            }],
            |_name, _attributes| async { Err(FlameError::AlreadyExist("already exists".into())) },
            move |name, attributes| {
                let updates = captured_updates.clone();
                async move {
                    updates.lock().unwrap().push((name, attributes.command));
                    Ok(())
                }
            },
        )
        .await
        .unwrap();

        assert_eq!(
            *updates.lock().unwrap(),
            vec![(
                "existing".to_string(),
                Some("configured-command".to_string())
            )]
        );
    }

    #[test]
    fn attribute_comparison_ignores_runtime_metadata() {
        let attributes = ApplicationAttributes {
            shim: common::apis::Shim::Cri,
            image: Some("registry.example/flmrt:test".into()),
            command: Some("/usr/local/flame/bin/flmping-service".into()),
            ..ApplicationAttributes::default()
        };
        let application = Application {
            name: "configured".into(),
            version: 42,
            creation_time: chrono::Utc::now(),
            shim: attributes.shim,
            image: attributes.image.clone(),
            command: attributes.command.clone(),
            max_instances: attributes.max_instances,
            delay_release: attributes.delay_release,
            schema: attributes.schema.clone(),
            ..Application::default()
        };

        assert!(matches_attributes(&application, &attributes));
    }
}
