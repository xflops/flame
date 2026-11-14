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
fn accepts_proxy_endpoint_for_dry_run_url() {
    for scheme in ["grpc-proxy", "grpcs-proxy"] {
        let endpoint =
            normalize_cache_endpoint(&format!("{scheme}://gateway.example.com:9443")).unwrap();
        assert_eq!(endpoint, format!("{scheme}://gateway.example.com:9443"));
        assert_eq!(
            object_url(&endpoint, "app/pkg/app.tar.gz"),
            format!("{scheme}://gateway.example.com:9443/app/pkg/app.tar.gz")
        );
    }
    assert_eq!(
        normalize_cache_endpoint("grpc+tls://cache.example.com").unwrap(),
        "grpcs://cache.example.com:9090"
    );
}

#[test]
fn rejects_non_cache_endpoint_scheme() {
    assert!(normalize_cache_endpoint("http://cache:9090").is_err());
    for scheme in ["grpc-proxy", "grpcs-proxy"] {
        assert!(normalize_cache_endpoint(&format!("{scheme}://gateway.example.com")).is_err());
        assert!(
            normalize_cache_endpoint(&format!("{scheme}://gateway.example.com:9443/path")).is_err()
        );
    }
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
        name: Some("demo".to_string()),
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
    let attributes = build_attributes(&options, None, &detected).unwrap();
    assert_eq!(attributes.command.as_deref(), Some("python"));
    assert_eq!(attributes.arguments, vec!["-m", "demo"]);
    assert_eq!(attributes.installer.as_deref(), Some("python"));
}

/// Use the same prepared package identity in upload keys and advertised deployment URLs.
#[test]
fn binary_package_url_uses_prepared_deployment_object_key() {
    let temp = tempfile::TempDir::new().unwrap();
    let bin = temp.path().join("service");
    std::fs::write(&bin, b"#!/bin/sh\n").unwrap();
    make_executable(&bin);

    let prepared = prepare_application(&bin).unwrap();
    let object_key = prepared.object_key("demo");
    assert_eq!(
        object_key,
        format!("demo/pkg/{}", prepared.package_filename("demo"))
    );
    assert_eq!(
        object_url("grpc://cache:9090", &object_key),
        format!(
            "grpc://cache:9090/demo/pkg/{}",
            prepared.package_filename("demo")
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
