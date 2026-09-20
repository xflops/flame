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

use std::collections::HashMap;

use chrono::Duration;
use flame_rs::{
    apis::{FlameError, Shim},
    client::{ApplicationAttributes, ApplicationSchema},
};

use serde_derive::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MetadataYaml {
    pub name: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SchemaYaml {
    pub input: Option<String>,
    pub output: Option<String>,
    pub common_data: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SpecYaml {
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
    pub schema: Option<SchemaYaml>,
    pub url: Option<String>,
    pub installer: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ApplicationYaml {
    pub metadata: MetadataYaml,
    pub spec: SpecYaml,
}

impl TryFrom<&ApplicationYaml> for ApplicationAttributes {
    type Error = FlameError;

    fn try_from(yaml: &ApplicationYaml) -> Result<Self, Self::Error> {
        let shim = match yaml.spec.shim.as_deref() {
            Some("Host") | Some("host") | None => Some(Shim::Host),
            Some("Wasm") | Some("wasm") | Some("WASM") => Some(Shim::Wasm),
            Some("Cri") | Some("cri") | Some("CRI") => Some(Shim::Cri),
            Some(other) => {
                return Err(FlameError::InvalidConfig(format!(
                    "Invalid shim value '{}'. Must be 'Host', 'Wasm', or 'Cri'.",
                    other
                )))
            }
        };

        Ok(Self {
            shim,
            image: yaml.spec.image.clone(),
            description: yaml.spec.description.clone(),
            labels: yaml.spec.labels.clone().unwrap_or_default(),
            command: yaml.spec.command.clone(),
            arguments: yaml.spec.arguments.clone().unwrap_or_default(),
            environments: yaml.spec.environments.clone().unwrap_or_default(),
            working_directory: yaml.spec.working_directory.clone(),
            max_instances: yaml.spec.max_instances,
            delay_release: yaml.spec.delay_release.map(Duration::seconds),
            schema: yaml.spec.schema.clone().map(ApplicationSchema::from),
            url: yaml.spec.url.clone(),
            installer: yaml.spec.installer.clone(),
        })
    }
}

impl From<SchemaYaml> for ApplicationSchema {
    fn from(schema: SchemaYaml) -> Self {
        Self {
            input: schema.input.clone(),
            output: schema.output.clone(),
            common_data: schema.common_data.clone(),
        }
    }
}
