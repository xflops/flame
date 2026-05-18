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

use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

use serde_json::Value;
use tempfile::TempDir;

const CACHE_ENDPOINT: &str = "grpc://127.0.0.1:19090";

#[test]
fn deploy_dry_run_executable_file_through_cli() {
    let temp = TempDir::new().unwrap();
    let config = write_config(temp.path());
    let binary = temp.path().join("service");
    fs::write(&binary, b"#!/bin/sh\nexec echo service\n").unwrap();
    make_executable(&binary);

    let json = run_deploy_json(&config, &binary, "demo-app");

    assert_eq!(json_string(&json, "/name"), "demo-app");
    assert_eq!(json_string(&json, "/input_kind"), "executable-file");
    assert_eq!(json_string(&json, "/installer"), "binary");
    assert_eq!(json_string(&json, "/command"), "service");
    assert_eq!(
        json.pointer("/dry_run").and_then(Value::as_bool),
        Some(true)
    );

    let object_key = json_string(&json, "/object_key");
    assert_content_addressed_package_key(object_key, "demo-app");
    assert_eq!(
        json_string(&json, "/url"),
        format!("{}/{}", CACHE_ENDPOINT, object_key)
    );
    assert!(!json_string(&json, "/url").contains("?sha"));

    assert_eq!(json_string(&json, "/application/metadata/name"), "demo-app");
    assert_eq!(json_string(&json, "/application/spec/installer"), "binary");
    assert_eq!(json_string(&json, "/application/spec/command"), "service");
    assert_eq!(
        json_string(&json, "/application/spec/url"),
        json_string(&json, "/url")
    );
}

#[test]
fn deploy_dry_run_python_directory_through_cli() {
    let temp = TempDir::new().unwrap();
    let config = write_config(temp.path());
    let app_dir = temp.path().join("app");
    fs::create_dir(&app_dir).unwrap();
    fs::write(
        app_dir.join("pyproject.toml"),
        "[project]\nname = 'demo-app'\n[project.scripts]\ndemo-app = 'demo:main'\n",
    )
    .unwrap();

    let json = run_deploy_json(&config, &app_dir, "demo-app");

    assert_eq!(json_string(&json, "/input_kind"), "directory");
    assert_eq!(json_string(&json, "/installer"), "python");
    assert_eq!(json_string(&json, "/command"), "demo-app");
    assert_content_addressed_package_key(json_string(&json, "/object_key"), "demo-app");
    assert_eq!(
        json_string(&json, "/application/spec/url"),
        json_string(&json, "/url")
    );
}

#[test]
fn deploy_dry_run_summary_formats_delay_release() {
    let temp = TempDir::new().unwrap();
    let config = write_config(temp.path());
    let binary = temp.path().join("service");
    fs::write(&binary, b"#!/bin/sh\nexec echo service\n").unwrap();
    make_executable(&binary);

    let output = Command::new(env!("CARGO_BIN_EXE_flmctl"))
        .arg("--config")
        .arg(config)
        .arg("deploy")
        .arg("--name")
        .arg("demo-app")
        .arg("--application")
        .arg(binary)
        .arg("--delay-release")
        .arg("60")
        .arg("--dry-run")
        .env_remove("FLAME_ENDPOINT")
        .env_remove("FLAME_CACHE_ENDPOINT")
        .env_remove("FLAME_CA_FILE")
        .output()
        .unwrap();

    let stdout = assert_success_stdout(output);
    assert!(
        stdout.contains("Delay Release: 1m"),
        "summary should format delay release for humans\nstdout:\n{}",
        stdout
    );
}

fn run_deploy_json(config: &Path, application: &Path, name: &str) -> Value {
    let output = Command::new(env!("CARGO_BIN_EXE_flmctl"))
        .arg("--config")
        .arg(config)
        .arg("deploy")
        .arg("--name")
        .arg(name)
        .arg("--application")
        .arg(application)
        .arg("--dry-run")
        .arg("-o")
        .arg("json")
        .env_remove("FLAME_ENDPOINT")
        .env_remove("FLAME_CACHE_ENDPOINT")
        .env_remove("FLAME_CA_FILE")
        .output()
        .unwrap();
    assert_success(output)
}

fn assert_success_stdout(output: Output) -> String {
    let stdout = String::from_utf8_lossy(&output.stdout).into_owned();
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        output.status.success(),
        "flmctl deploy failed\nstdout:\n{}\nstderr:\n{}",
        stdout,
        stderr
    );
    stdout
}

fn assert_success(output: Output) -> Value {
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        output.status.success(),
        "flmctl deploy failed\nstdout:\n{}\nstderr:\n{}",
        stdout,
        stderr
    );

    serde_json::from_slice(&output.stdout).unwrap_or_else(|e| {
        panic!(
            "failed to parse flmctl json output: {}\nstdout:\n{}\nstderr:\n{}",
            e, stdout, stderr
        )
    })
}

fn write_config(root: &Path) -> PathBuf {
    let config = root.join("flame.yaml");
    fs::write(
        &config,
        format!(
            r#"current-context: test
contexts:
  - name: test
    cluster:
      endpoint: http://127.0.0.1:18080
    cache:
      endpoint: {}
"#,
            CACHE_ENDPOINT
        ),
    )
    .unwrap();
    config
}

fn json_string<'a>(json: &'a Value, pointer: &str) -> &'a str {
    json.pointer(pointer)
        .and_then(Value::as_str)
        .unwrap_or_else(|| panic!("missing string at {} in {}", pointer, json))
}

fn assert_content_addressed_package_key(key: &str, app_name: &str) {
    let prefix = format!("{}/pkg/{}-", app_name, app_name);
    assert!(
        key.starts_with(&prefix),
        "object key {} should start with {}",
        key,
        prefix
    );
    assert!(
        key.ends_with(".tar.gz"),
        "object key {} should end with .tar.gz",
        key
    );
    assert_eq!(
        key.split('/').count(),
        3,
        "object key {} should have <app>/<session>/<object> shape",
        key
    );

    let digest = &key[prefix.len()..key.len() - ".tar.gz".len()];
    assert_eq!(digest.len(), 16, "object key {} should use sha16", key);
    assert!(
        digest.chars().all(|c| matches!(c, '0'..='9' | 'a'..='f')),
        "object key {} should use lowercase hex digest",
        key
    );
}

fn make_executable(path: &Path) {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mut permissions = fs::metadata(path).unwrap().permissions();
        permissions.set_mode(0o755);
        fs::set_permissions(path, permissions).unwrap();
    }

    #[cfg(not(unix))]
    {
        let _ = path;
    }
}
