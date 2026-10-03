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
fn deploy_dry_run_python_file_through_cli() {
    let temp = TempDir::new().unwrap();
    let config = write_config(temp.path());
    let script = temp.path().join("main.py");
    fs::write(&script, "print('hello')\n").unwrap();

    let json = run_deploy_json(&config, &script, "demo-app");

    assert_eq!(json_string(&json, "/input_kind"), "file");
    assert_eq!(json_string(&json, "/installer"), "binary");
    assert_eq!(json_string(&json, "/command"), "python3");
    assert_eq!(
        json.pointer("/arguments/0").and_then(Value::as_str),
        Some("main.py")
    );
    assert_content_addressed_package_key(json_string(&json, "/object_key"), "demo-app");
}

#[test]
fn deploy_uses_directory_profile_and_cli_overrides() {
    let temp = TempDir::new().unwrap();
    let config = write_config(temp.path());
    let app_dir = temp.path().join("app");
    fs::create_dir(&app_dir).unwrap();
    fs::write(
        app_dir.join("pyproject.toml"),
        "[project]\nname = 'demo'\n[project.scripts]\ndemo = 'demo:main'\n",
    )
    .unwrap();
    fs::write(
        app_dir.join("flm.yaml"),
        "metadata:\n  name: fallback\nspec:\n  command: fallback-command\n",
    )
    .unwrap();
    fs::write(
        app_dir.join("flame.yaml"),
        r#"metadata:
  name: demo
spec:
  shim: cri
  image: example/runner:v1
  description: profile description
  labels: [profile]
  installer: binary
  command: profile-command
  arguments: [profile-arg]
  environments:
    KEEP: from-profile
    CHANGE: from-profile
  working_directory: /profile
  max_instances: 3
  delay_release: 30
  schema:
    input: profile-input
    output: profile-output
  url: grpc://old-cache/old
"#,
    )
    .unwrap();

    let output = Command::new(env!("CARGO_BIN_EXE_flmctl"))
        .arg("--config")
        .arg(config)
        .arg("deploy")
        .arg("--application")
        .arg(&app_dir)
        .arg("--dry-run")
        .arg("-o")
        .arg("json")
        .arg("--command")
        .arg("cli-command")
        .arg("--argument")
        .arg("cli-arg")
        .arg("--env")
        .arg("CHANGE=from-cli")
        .arg("--schema-output")
        .arg("cli-output")
        .env_remove("FLAME_ENDPOINT")
        .env_remove("FLAME_CACHE_ENDPOINT")
        .env_remove("FLAME_CA_FILE")
        .output()
        .unwrap();
    let json = assert_success(output);
    assert_eq!(json_string(&json, "/name"), "demo");
    assert_eq!(json_string(&json, "/installer"), "binary");
    assert_eq!(json_string(&json, "/command"), "cli-command");
    assert_eq!(json_string(&json, "/application/spec/shim"), "Cri");
    assert_eq!(
        json_string(&json, "/application/spec/image"),
        "example/runner:v1"
    );
    assert_eq!(
        json_string(&json, "/application/spec/description"),
        "profile description"
    );
    assert_eq!(
        json_string(&json, "/application/spec/environments/KEEP"),
        "from-profile"
    );
    assert_eq!(
        json_string(&json, "/application/spec/environments/CHANGE"),
        "from-cli"
    );
    assert_eq!(
        json_string(&json, "/application/spec/schema/input"),
        "profile-input"
    );
    assert_eq!(
        json_string(&json, "/application/spec/schema/output"),
        "cli-output"
    );
    assert_eq!(
        json_string(&json, "/application/spec/working_directory"),
        "/profile"
    );
    assert_eq!(
        json.pointer("/application/spec/max_instances")
            .and_then(Value::as_u64),
        Some(3)
    );
    assert_eq!(
        json.pointer("/application/spec/delay_release")
            .and_then(Value::as_i64),
        Some(30)
    );
    assert_eq!(
        json.pointer("/arguments/0").and_then(Value::as_str),
        Some("cli-arg")
    );
    assert_eq!(
        json_string(&json, "/application/spec/url"),
        json_string(&json, "/url")
    );

    let output = Command::new(env!("CARGO_BIN_EXE_flmctl"))
        .arg("--config")
        .arg(temp.path().join("flame.yaml"))
        .arg("deploy")
        .arg("--application")
        .arg(&app_dir)
        .arg("--name")
        .arg("cli-name")
        .arg("--installer")
        .arg("python")
        .arg("--shim")
        .arg("host")
        .arg("--dry-run")
        .arg("-o")
        .arg("json")
        .env_remove("FLAME_ENDPOINT")
        .env_remove("FLAME_CACHE_ENDPOINT")
        .env_remove("FLAME_CA_FILE")
        .output()
        .unwrap();
    let overridden = assert_success(output);
    assert_eq!(json_string(&overridden, "/name"), "cli-name");
    assert_eq!(json_string(&overridden, "/installer"), "python");
    assert_eq!(json_string(&overridden, "/application/spec/shim"), "Host");
}

#[test]
fn deploy_uses_flm_yaml_when_flame_yaml_absent() {
    let temp = TempDir::new().unwrap();
    let config = write_config(temp.path());
    let app_dir = temp.path().join("app");
    fs::create_dir(&app_dir).unwrap();
    fs::write(app_dir.join("script.py"), "print('hello')\n").unwrap();
    fs::write(
        app_dir.join("flm.yaml"),
        "metadata:\n  name: from-flm\nspec:\n  command: python3\n  arguments: [-m, script]\n",
    )
    .unwrap();
    let output = Command::new(env!("CARGO_BIN_EXE_flmctl"))
        .arg("--config")
        .arg(config)
        .arg("deploy")
        .arg("--application")
        .arg(app_dir)
        .arg("--dry-run")
        .arg("-o")
        .arg("json")
        .env_remove("FLAME_ENDPOINT")
        .env_remove("FLAME_CACHE_ENDPOINT")
        .env_remove("FLAME_CA_FILE")
        .output()
        .unwrap();
    let json = assert_success(output);
    assert_eq!(json_string(&json, "/name"), "from-flm");
    assert_eq!(json_string(&json, "/command"), "python3");
}

#[test]
fn deploy_rejects_missing_name_and_invalid_profile() {
    let temp = TempDir::new().unwrap();
    let config = write_config(temp.path());
    let app_dir = temp.path().join("app");
    fs::create_dir(&app_dir).unwrap();
    fs::write(app_dir.join("script.py"), "print('hello')\n").unwrap();
    let command = || {
        let mut command = Command::new(env!("CARGO_BIN_EXE_flmctl"));
        command
            .arg("--config")
            .arg(&config)
            .arg("deploy")
            .arg("--application")
            .arg(&app_dir)
            .arg("--dry-run")
            .env_remove("FLAME_ENDPOINT")
            .env_remove("FLAME_CACHE_ENDPOINT")
            .env_remove("FLAME_CA_FILE");
        command
    };
    let output = command().output().unwrap();
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("application name required"));

    fs::write(
        app_dir.join("flame.yaml"),
        "metadata:\n  name: demo\nspec:\n  commmand: typo\n",
    )
    .unwrap();
    let output = command().output().unwrap();
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("commmand"));
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
    let prefix = format!("default/{}/pkg/{}-", app_name, app_name);
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
        4,
        "object key {} should have <workspace>/<app>/<session>/<object> shape",
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
