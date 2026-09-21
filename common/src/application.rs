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

use chrono::Duration;
use serde::Deserialize as _;
use serde_derive::{Deserialize, Serialize};

use crate::apis::{validate_application_name, ApplicationAttributes, ApplicationSchema, Shim};
use crate::FlameError;

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ApplicationMetadataManifest {
    pub name: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ApplicationSchemaManifest {
    pub input: Option<String>,
    pub output: Option<String>,
    pub common_data: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ApplicationSpecManifest {
    #[serde(default)]
    pub shim: Option<String>,
    pub image: Option<String>,
    pub description: Option<String>,
    pub labels: Option<Vec<String>>,
    pub command: Option<String>,
    pub arguments: Option<Vec<String>>,
    pub environments: Option<HashMap<String, String>>,
    pub working_directory: Option<String>,
    pub max_instances: Option<u32>,
    pub delay_release: Option<i64>,
    pub schema: Option<ApplicationSchemaManifest>,
    pub url: Option<String>,
    pub installer: Option<String>,
}

/// The application manifest accepted by `flmctl` and by the session manager's
/// `<config-dir>/applications` seed directory.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ApplicationManifest {
    pub metadata: ApplicationMetadataManifest,
    pub spec: ApplicationSpecManifest,
}

impl ApplicationManifest {
    pub fn attributes(&self) -> Result<ApplicationAttributes, FlameError> {
        let defaults = ApplicationAttributes::default();
        let shim = self
            .spec
            .shim
            .as_ref()
            .map(|shim| Shim::try_from(shim.clone()))
            .transpose()?
            .unwrap_or(defaults.shim);

        Ok(ApplicationAttributes {
            shim,
            image: self.spec.image.clone(),
            description: self.spec.description.clone(),
            labels: self.spec.labels.clone().unwrap_or_default(),
            command: self.spec.command.clone(),
            arguments: self.spec.arguments.clone().unwrap_or_default(),
            environments: self.spec.environments.clone().unwrap_or_default(),
            working_directory: self.spec.working_directory.clone(),
            max_instances: self.spec.max_instances.unwrap_or(defaults.max_instances),
            delay_release: self
                .spec
                .delay_release
                .map(Duration::seconds)
                .unwrap_or(defaults.delay_release),
            schema: self.spec.schema.clone().map(ApplicationSchema::from),
            url: self.spec.url.clone(),
            installer: self.spec.installer.clone(),
        })
    }

    pub fn validate(&self) -> Result<(), FlameError> {
        validate_application_name(&self.metadata.name)?;
        self.attributes()?;
        Ok(())
    }
}

impl From<ApplicationSchemaManifest> for ApplicationSchema {
    fn from(schema: ApplicationSchemaManifest) -> Self {
        Self {
            input: schema.input,
            output: schema.output,
            common_data: schema.common_data,
        }
    }
}

/// Parses all YAML documents in an application manifest stream and validates
/// each document before returning any of them.
pub fn parse_application_manifests(contents: &str) -> Result<Vec<ApplicationManifest>, FlameError> {
    let mut manifests = Vec::new();

    for (index, document) in serde_yaml::Deserializer::from_str(contents).enumerate() {
        let manifest = ApplicationManifest::deserialize(document).map_err(|error| {
            FlameError::InvalidConfig(format!(
                "invalid application manifest document {}: {}",
                index + 1,
                error
            ))
        })?;
        manifest.validate().map_err(|error| {
            FlameError::InvalidConfig(format!(
                "invalid application manifest document {}: {}",
                index + 1,
                error
            ))
        })?;
        manifests.push(manifest);
    }

    Ok(manifests)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_multiple_documents_and_applies_defaults() {
        let manifests = parse_application_manifests(
            r#"
metadata:
  name: first
spec:
  command: first-command
---
metadata:
  name: second
spec:
  shim: cri
  image: example/flmrt:latest
"#,
        )
        .unwrap();

        assert_eq!(manifests.len(), 2);
        assert_eq!(manifests[0].attributes().unwrap().shim, Shim::Host);
        assert_eq!(manifests[1].attributes().unwrap().shim, Shim::Cri);
    }

    #[test]
    fn reports_document_number_for_invalid_manifest() {
        let error = parse_application_manifests(
            r#"
metadata:
  name: valid
spec: {}
---
metadata:
  name: invalid
spec:
  shim: unknown
"#,
        )
        .unwrap_err();

        assert!(error.to_string().contains("document 2"));
        assert!(error.to_string().contains("unknown"));
    }

    #[test]
    fn rejects_unknown_fields() {
        let error = parse_application_manifests(
            r#"
metadata:
  name: typo
spec:
  commmand: executable
"#,
        )
        .unwrap_err();

        assert!(error.to_string().contains("commmand"));
        assert!(error.to_string().contains("unknown field"));
    }
}
