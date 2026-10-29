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

use std::{
    collections::HashMap,
    fs::{self, File},
    io::{Read, Write},
    path::Path,
    process::{Command, Stdio},
    thread,
};

use rand::RngExt;

use flame_rs::apis::FlameError;
use stdng::trace_fn;

use crate::api::Script;
use crate::script::{ScriptEngine, ScriptRuntime};

const DEFAULT_ENTRYPOINT: &str = "main.sh";
const DEFAULT_SHELL_CMD: &str = "bash";
const SUPPORTED_SHELLS: &[&str] = &["sh", "bash", "zsh", "csh", "tcsh", "ksh", "fish"];

pub struct ShellScript {
    runtime: ScriptRuntime,
    shell_cmd: String,
}

impl ShellScript {
    pub fn new(script: &Script) -> Result<Self, FlameError> {
        trace_fn!("ShellScript::new");

        let shell_cmd = shell_cmd(script.runtime.as_deref())?;

        let mut rng = rand::rng();
        let work_dir_path = format!("/tmp/flame/script/shell-{}", rng.random::<u32>());
        let work_dir = Path::new(&work_dir_path);

        fs::create_dir_all(work_dir).map_err(|e| FlameError::Internal(e.to_string()))?;
        tracing::debug!("Created work directory: {work_dir_path}");

        let entrypoint = DEFAULT_ENTRYPOINT;

        let mut file = File::create(work_dir.join(entrypoint))
            .map_err(|e| FlameError::Internal(e.to_string()))?;
        file.write_all(script.code.as_bytes())
            .map_err(|e| FlameError::Internal(e.to_string()))?;

        let full_path = work_dir.join(entrypoint);

        let runtime = ScriptRuntime {
            entrypoint: full_path.to_string_lossy().to_string(),
            work_dir: work_dir.to_string_lossy().to_string(),
            input: script.input.clone(),
            env: HashMap::new(),
        };
        Ok(Self { runtime, shell_cmd })
    }
}

fn shell_cmd(runtime: Option<&str>) -> Result<String, FlameError> {
    let Some(runtime) = runtime.map(str::trim).filter(|value| !value.is_empty()) else {
        return Ok(DEFAULT_SHELL_CMD.to_string());
    };

    let shell_name = Path::new(runtime)
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or(runtime);
    let supported_name = SUPPORTED_SHELLS.contains(&shell_name);
    let supported_path = SUPPORTED_SHELLS.iter().any(|&shell| {
        runtime.strip_prefix("/bin/").is_some_and(|s| s == shell)
            || runtime
                .strip_prefix("/usr/bin/")
                .is_some_and(|s| s == shell)
    });
    if !supported_name || (runtime.contains('/') && !supported_path) {
        return Err(FlameError::InvalidConfig(format!(
            "Unsupported shell runtime: {runtime}"
        )));
    }

    Ok(runtime.to_string())
}

impl ScriptEngine for ShellScript {
    fn run(&self) -> Result<Option<Vec<u8>>, FlameError> {
        trace_fn!("ShellScript::run");

        tracing::debug!("Running script: {}", self.runtime.entrypoint);
        tracing::debug!("Work directory: {}", self.runtime.work_dir);
        tracing::debug!("Using shell runtime: {}", self.shell_cmd);

        let mut child = Command::new(&self.shell_cmd)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .current_dir(&self.runtime.work_dir)
            .args([&self.runtime.entrypoint])
            .envs(self.runtime.env.iter().map(|(k, v)| (k.clone(), v.clone())))
            .spawn()
            .map_err(|e| FlameError::Internal(format!("failed to start subprocess: {e}")))?;

        tracing::debug!("Spawned child process: {}", child.id());
        let mut stdin = child.stdin.take().unwrap();
        if let Some(input) = &self.runtime.input {
            let input = input.clone();
            let _handler = thread::spawn(move || {
                match stdin.write_all(&input) {
                    Ok(_) => {}
                    Err(e) => {
                        tracing::error!("Failed to send input into shim instance: {e}.");
                    }
                };
            });
            tracing::debug!("Sent input into child process.");
        }

        let mut stdout = child.stdout.take().unwrap();
        let mut data = vec![];
        let n = stdout
            .read_to_end(&mut data)
            .map_err(|_| FlameError::Internal("failed to read task output".to_string()))?;

        tracing::debug!("Read <{n}> data from child process.");

        match child.wait() {
            Ok(es) => {
                if !es.success() {
                    tracing::info!("Child process exist with error: {es}");
                }
            }
            Err(e) => {
                tracing::error!("Failed to wait child process: {e}")
            }
        };

        tracing::debug!("Child process exited.");

        Ok(Some(data))
    }
}

impl Drop for ShellScript {
    fn drop(&mut self) {
        trace_fn!("ShellScript::drop");

        fs::remove_dir_all(Path::new(&self.runtime.work_dir)).unwrap();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn uses_default_shell_when_runtime_is_not_requested() {
        assert_eq!(shell_cmd(None).unwrap(), DEFAULT_SHELL_CMD);
        assert_eq!(shell_cmd(Some("")).unwrap(), DEFAULT_SHELL_CMD);
    }

    #[test]
    fn accepts_supported_shell_runtime() {
        assert_eq!(shell_cmd(Some("zsh")).unwrap(), "zsh");
        assert_eq!(shell_cmd(Some("/bin/csh")).unwrap(), "/bin/csh");
    }

    #[test]
    fn rejects_unsupported_shell_runtime() {
        assert!(shell_cmd(Some("python")).is_err());
        assert!(shell_cmd(Some("/tmp/bash")).is_err());
        assert!(shell_cmd(Some("/tmp/custom-shell")).is_err());
    }
}
