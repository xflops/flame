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

mod artifact;
mod detect;

use std::collections::HashMap;
use std::path::PathBuf;

use chrono::Duration;
use clap::Args;
use flame_rs as flame;
use flame_rs::apis::{FlameContext, FlameError, Shim};
use flame_rs::client::{ApplicationAttributes, ApplicationSchema};
use serde_derive::Serialize;
use url::Url;

use artifact::{prepare_application, PreparedApplication};
use detect::{detect_application, DetectedApplication};

#[derive(Debug, Clone, Args)]
pub struct Options {
    /// Application name.
    #[arg(long)]
    pub name: String,

    /// Application path. Can be an executable file, .tar.gz/.tgz, or directory.
    #[arg(long)]
    pub application: PathBuf,

    /// Render the deploy plan without uploading or registering the application.
    #[arg(long)]
    pub dry_run: bool,

    /// Output format: summary, yaml, or json.
    #[arg(short = 'o', long, default_value = "summary")]
    pub output: String,

    /// Application shim.
    #[arg(long)]
    pub shim: Option<String>,

    /// Optional runtime image.
    #[arg(long)]
    pub image: Option<String>,

    /// Application description.
    #[arg(long)]
    pub description: Option<String>,

    /// Application label. Repeatable.
    #[arg(long)]
    pub label: Vec<String>,

    /// Application command. Overrides detected command.
    #[arg(long)]
    pub command: Option<String>,

    /// Add one command argument. Repeatable.
    #[arg(long)]
    pub argument: Vec<String>,

    /// Add one environment variable in NAME=VALUE form. Repeatable.
    #[arg(long)]
    pub env: Vec<String>,

    /// Runtime working directory.
    #[arg(long)]
    pub working_directory: Option<String>,

    /// Application maximum instances.
    #[arg(long)]
    pub max_instances: Option<u32>,

    /// Application delay-release value in seconds.
    #[arg(long)]
    pub delay_release: Option<i64>,

    /// Installer type. Overrides detected installer.
    #[arg(long)]
    pub installer: Option<String>,

    /// Optional input schema string.
    #[arg(long)]
    pub schema_input: Option<String>,

    /// Optional output schema string.
    #[arg(long)]
    pub schema_output: Option<String>,

    /// Optional common-data schema string.
    #[arg(long)]
    pub schema_common_data: Option<String>,
}

struct DeployPlan {
    app_name: String,
    cache_endpoint: String,
    prepared: PreparedApplication,
    attributes: ApplicationAttributes,
    output: String,
    dry_run: bool,
}

#[derive(Debug, Clone, Serialize)]
struct DeployResult {
    name: String,
    input_kind: String,
    installer: String,
    command: String,
    arguments: Vec<String>,
    object_key: String,
    url: String,
    sha256: String,
    dry_run: bool,
    application: RenderedApplication,
}

#[derive(Debug, Clone, Serialize)]
struct RenderedApplication {
    metadata: RenderedMetadata,
    spec: RenderedSpec,
}

#[derive(Debug, Clone, Serialize)]
struct RenderedMetadata {
    name: String,
}

#[derive(Debug, Clone, Serialize)]
struct RenderedSpec {
    #[serde(skip_serializing_if = "Option::is_none")]
    shim: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    image: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    description: Option<String>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    labels: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    command: Option<String>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    arguments: Vec<String>,
    #[serde(skip_serializing_if = "HashMap::is_empty")]
    environments: HashMap<String, String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    working_directory: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    max_instances: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    delay_release: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    schema: Option<RenderedSchema>,
    url: String,
    installer: String,
}

#[derive(Debug, Clone, Serialize)]
struct RenderedSchema {
    #[serde(skip_serializing_if = "Option::is_none")]
    input: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    output: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    common_data: Option<String>,
}

pub async fn run(ctx: &FlameContext, options: &Options) -> Result<(), FlameError> {
    let plan = build_plan(ctx, options)?;
    let object_key = plan.prepared.object_key(&plan.app_name);
    let mut uploaded_key = object_key.clone();

    if !plan.dry_run {
        let object_ref = flame::object::upload_object_with_context(
            ctx,
            &object_key,
            &plan.prepared.package_path,
        )
        .await?;
        uploaded_key = object_ref.key;
    }

    let url = object_url(&plan.cache_endpoint, &uploaded_key);

    let mut attributes = plan.attributes.clone();
    attributes.url = Some(url.clone());

    let result = DeployResult {
        name: plan.app_name.clone(),
        input_kind: plan.prepared.kind.to_string(),
        installer: attributes.installer.clone().unwrap_or_default(),
        command: attributes.command.clone().unwrap_or_default(),
        arguments: attributes.arguments.clone(),
        object_key: uploaded_key.clone(),
        url: url.clone(),
        sha256: plan.prepared.sha256.clone(),
        dry_run: plan.dry_run,
        application: render_application(&plan.app_name, &attributes),
    };

    if !plan.dry_run {
        let current_ctx = ctx.get_current_context()?;
        let conn = flame::client::connect_with_tls(
            &current_ctx.cluster.endpoint,
            current_ctx.cluster.tls.as_ref(),
        )
        .await?;
        conn.register_application(plan.app_name.clone(), attributes)
            .await?;
    }

    print_result(&plan.output, &result)?;
    Ok(())
}

fn build_plan(ctx: &FlameContext, options: &Options) -> Result<DeployPlan, FlameError> {
    validate_name(&options.name)?;
    let current_ctx = ctx.get_current_context()?;
    let cache_config = current_ctx
        .cache
        .as_ref()
        .ok_or_else(|| FlameError::InvalidConfig("cache endpoint not configured".to_string()))?;
    let cache_endpoint = cache_config
        .endpoint
        .as_deref()
        .ok_or_else(|| FlameError::InvalidConfig("cache endpoint not configured".to_string()))
        .and_then(normalize_cache_endpoint)?;

    let prepared = prepare_application(&options.application)?;
    let detected = detect_application(&options.name, prepared.kind, &prepared.detection_root)?;
    let attributes = build_attributes(options, &detected)?;

    Ok(DeployPlan {
        app_name: options.name.clone(),
        cache_endpoint,
        prepared,
        attributes,
        output: options.output.clone(),
        dry_run: options.dry_run,
    })
}

fn build_attributes(
    options: &Options,
    detected: &DetectedApplication,
) -> Result<ApplicationAttributes, FlameError> {
    let shim = parse_shim(options.shim.as_deref())?;
    let installer = options
        .installer
        .clone()
        .or_else(|| detected.installer.clone())
        .ok_or_else(|| {
            FlameError::InvalidConfig(
                "unable to detect installer; pass --installer binary or --installer python"
                    .to_string(),
            )
        })?;
    validate_installer(&installer)?;

    let command = options.command.clone().or_else(|| detected.command.clone());
    if command.is_none() {
        return Err(FlameError::InvalidConfig(
            "unable to detect command; pass --command".to_string(),
        ));
    }

    let arguments = if !options.argument.is_empty() {
        options.argument.clone()
    } else {
        detected.arguments.clone()
    };

    let schema = if options.schema_input.is_some()
        || options.schema_output.is_some()
        || options.schema_common_data.is_some()
    {
        Some(ApplicationSchema {
            input: options.schema_input.clone(),
            output: options.schema_output.clone(),
            common_data: options.schema_common_data.clone(),
        })
    } else {
        None
    };

    Ok(ApplicationAttributes {
        shim,
        image: options.image.clone(),
        description: options.description.clone(),
        labels: options.label.clone(),
        command,
        arguments,
        environments: parse_envs(&options.env)?,
        working_directory: options.working_directory.clone(),
        max_instances: options.max_instances,
        delay_release: options.delay_release.map(Duration::seconds),
        schema,
        url: None,
        installer: Some(installer),
    })
}

fn parse_shim(value: Option<&str>) -> Result<Option<Shim>, FlameError> {
    match value {
        None => Ok(Some(Shim::Host)),
        Some("Host") | Some("host") => Ok(Some(Shim::Host)),
        Some("Wasm") | Some("wasm") | Some("WASM") => Ok(Some(Shim::Wasm)),
        Some(other) => Err(FlameError::InvalidConfig(format!(
            "invalid shim value '{}'. Must be Host or Wasm",
            other
        ))),
    }
}

fn validate_installer(installer: &str) -> Result<(), FlameError> {
    match installer {
        "binary" | "python" => Ok(()),
        other => Err(FlameError::InvalidConfig(format!(
            "unsupported installer '{}'. Supported installers: binary, python",
            other
        ))),
    }
}

fn parse_envs(values: &[String]) -> Result<HashMap<String, String>, FlameError> {
    let mut envs = HashMap::new();
    for value in values {
        let Some((key, val)) = value.split_once('=') else {
            return Err(FlameError::InvalidConfig(format!(
                "invalid environment '{}'; expected NAME=VALUE",
                value
            )));
        };
        if key.is_empty() {
            return Err(FlameError::InvalidConfig(
                "environment variable name cannot be empty".to_string(),
            ));
        }
        envs.insert(key.to_string(), val.to_string());
    }
    Ok(envs)
}

fn validate_name(name: &str) -> Result<(), FlameError> {
    common::apis::validate_application_name(name)
        .map_err(|e| FlameError::InvalidConfig(e.to_string()))
}

fn normalize_cache_endpoint(raw: &str) -> Result<String, FlameError> {
    let parsed = Url::parse(raw)
        .map_err(|e| FlameError::InvalidConfig(format!("invalid cache endpoint: {}", e)))?;
    let scheme = parsed.scheme();
    if scheme != "grpc" && scheme != "grpcs" {
        return Err(FlameError::InvalidConfig(format!(
            "unsupported cache endpoint scheme <{}>; expected grpc or grpcs",
            scheme
        )));
    }
    let host = parsed
        .host_str()
        .ok_or_else(|| FlameError::InvalidConfig("cache endpoint missing host".to_string()))?;
    let port = parsed.port().unwrap_or(9090);
    Ok(format!("{}://{}:{}", scheme, host_for_uri(host), port))
}

fn object_url(cache_endpoint: &str, key: &str) -> String {
    format!("{}/{}", cache_endpoint.trim_end_matches('/'), key)
}

fn host_for_uri(host: &str) -> String {
    if host.contains(':') && !host.starts_with('[') {
        format!("[{host}]")
    } else {
        host.to_string()
    }
}

fn render_application(name: &str, attributes: &ApplicationAttributes) -> RenderedApplication {
    RenderedApplication {
        metadata: RenderedMetadata {
            name: name.to_string(),
        },
        spec: RenderedSpec {
            shim: attributes.shim.map(|shim| shim.to_string()),
            image: attributes.image.clone(),
            description: attributes.description.clone(),
            labels: attributes.labels.clone(),
            command: attributes.command.clone(),
            arguments: attributes.arguments.clone(),
            environments: attributes.environments.clone(),
            working_directory: attributes.working_directory.clone(),
            max_instances: attributes.max_instances,
            delay_release: attributes.delay_release.map(|d| d.num_seconds()),
            schema: attributes.schema.clone().map(|schema| RenderedSchema {
                input: schema.input,
                output: schema.output,
                common_data: schema.common_data,
            }),
            url: attributes.url.clone().unwrap_or_default(),
            installer: attributes.installer.clone().unwrap_or_default(),
        },
    }
}

fn print_result(output: &str, result: &DeployResult) -> Result<(), FlameError> {
    match output {
        "summary" => {
            if result.dry_run {
                println!("Application <{}> deploy plan.", result.name);
            } else {
                println!("Application <{}> deployed.", result.name);
            }
            println!("Input Kind: {}", result.input_kind);
            println!("Installer: {}", result.installer);
            println!("Command: {}", result.command);
            if !result.arguments.is_empty() {
                println!("Arguments: {}", result.arguments.join(" "));
            }
            println!("Object: {}", result.object_key);
            println!("SHA256: {}", result.sha256);
            println!("URL: {}", result.url);
            if result.dry_run {
                println!("Dry Run: true");
            }
            Ok(())
        }
        "yaml" => {
            let rendered = serde_yaml::to_string(&result.application)
                .map_err(|e| FlameError::Internal(format!("failed to render yaml: {}", e)))?;
            print!("{rendered}");
            Ok(())
        }
        "json" => {
            let rendered = serde_json::to_string_pretty(result)
                .map_err(|e| FlameError::Internal(format!("failed to render json: {}", e)))?;
            println!("{rendered}");
            Ok(())
        }
        other => Err(FlameError::InvalidConfig(format!(
            "unsupported output format '{}'. Supported: summary, yaml, json",
            other
        ))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_envs_requires_name_value() {
        assert!(parse_envs(&["A=B".to_string()]).is_ok());
        assert!(parse_envs(&["A".to_string()]).is_err());
    }

    #[test]
    fn normalizes_cache_endpoint_and_object_url() {
        let endpoint = normalize_cache_endpoint("grpc://cache").unwrap();
        assert_eq!(endpoint, "grpc://cache:9090");
        assert_eq!(
            object_url(&endpoint, "app/pkg/app.tar.gz"),
            "grpc://cache:9090/app/pkg/app.tar.gz"
        );
    }

    #[test]
    fn normalizes_ipv6_cache_endpoint() {
        let endpoint = normalize_cache_endpoint("grpc://[2001:db8::1]").unwrap();
        assert_eq!(endpoint, "grpc://[2001:db8::1]:9090");
    }

    #[test]
    fn rejects_non_cache_endpoint_scheme() {
        assert!(normalize_cache_endpoint("http://cache:9090").is_err());
    }

    #[test]
    fn validates_names_like_session_manager() {
        assert!(validate_name("demo-app").is_ok());
        assert!(validate_name("app@name").is_err());
        assert!(validate_name("-demo").is_err());
    }

    #[test]
    fn explicit_options_override_detection() {
        let options = Options {
            name: "demo".to_string(),
            application: PathBuf::from("."),
            dry_run: true,
            output: "summary".to_string(),
            shim: None,
            image: None,
            description: None,
            label: vec![],
            command: Some("python".to_string()),
            argument: vec!["-m".to_string(), "demo".to_string()],
            env: vec![],
            working_directory: None,
            max_instances: None,
            delay_release: None,
            installer: Some("python".to_string()),
            schema_input: None,
            schema_output: None,
            schema_common_data: None,
        };
        let detected = DetectedApplication::executable("service".to_string());
        let attributes = build_attributes(&options, &detected).unwrap();
        assert_eq!(attributes.command.as_deref(), Some("python"));
        assert_eq!(attributes.arguments, vec!["-m", "demo"]);
        assert_eq!(attributes.installer.as_deref(), Some("python"));
    }

    #[test]
    fn binary_package_url_uses_three_part_content_addressed_object_key() {
        let temp = tempfile::TempDir::new().unwrap();
        let bin = temp.path().join("service");
        std::fs::write(&bin, b"#!/bin/sh\n").unwrap();
        make_executable(&bin);

        let prepared = prepare_application(&bin).unwrap();
        let object_key = prepared.object_key("demo");
        assert_eq!(
            object_key,
            format!("demo/pkg/demo-{}.tar.gz", &prepared.sha256[..16])
        );
        assert_eq!(
            object_url("grpc://cache:9090", &object_key),
            format!(
                "grpc://cache:9090/demo/pkg/demo-{}.tar.gz",
                &prepared.sha256[..16]
            )
        );
    }

    fn make_executable(path: &std::path::Path) {
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mut permissions = std::fs::metadata(path).unwrap().permissions();
            permissions.set_mode(0o755);
            std::fs::set_permissions(path, permissions).unwrap();
        }
        #[cfg(not(unix))]
        {
            let _ = path;
        }
    }
}
