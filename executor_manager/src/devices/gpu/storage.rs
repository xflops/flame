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
use std::fs::{self, File, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};

use crate::devices::DeviceAssignment;
use common::FlameError;
use uuid::Uuid;

pub(super) fn validate_executor_id(executor_id: &str) -> Result<(), FlameError> {
    if executor_id.is_empty()
        || executor_id.starts_with('.')
        || !executor_id
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.'))
    {
        return Err(FlameError::InvalidConfig(format!(
            "invalid GPU assignment executor ID '{executor_id}'"
        )));
    }
    Ok(())
}

pub(super) fn assignment_path(workdir: &Path, executor_id: &str) -> Result<PathBuf, FlameError> {
    validate_executor_id(executor_id)?;
    Ok(workdir
        .join(crate::devices::GPU)
        .join(format!("{executor_id}.json")))
}

pub(super) fn load(workdir: &Path) -> Result<HashMap<String, DeviceAssignment>, FlameError> {
    let directory = workdir.join(crate::devices::GPU);
    let entries = match fs::read_dir(&directory) {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(HashMap::new()),
        Err(error) => return Err(io_error("read", &directory, error)),
    };
    let mut assignments = HashMap::new();
    for entry in entries {
        let entry = entry.map_err(|error| io_error("read", &directory, error))?;
        let name = entry
            .file_name()
            .into_string()
            .map_err(|_| FlameError::InvalidConfig("non-UTF8 GPU assignment filename".into()))?;
        // Interrupted atomic writes are never committed assignment records.
        if name
            .strip_prefix(".gpu-assignment-")
            .and_then(|name| name.strip_suffix(".tmp"))
            .is_some_and(|id| Uuid::parse_str(id).is_ok())
        {
            continue;
        }
        let executor_id = name.strip_suffix(".json").ok_or_else(|| {
            FlameError::InvalidConfig(format!("unexpected GPU assignment file '{name}'"))
        })?;
        validate_executor_id(executor_id)?;
        if !entry
            .file_type()
            .map_err(|error| io_error("inspect", &entry.path(), error))?
            .is_file()
        {
            return Err(FlameError::InvalidConfig(format!(
                "GPU assignment '{}' is not a regular file",
                entry.path().display()
            )));
        }
        let bytes =
            fs::read(entry.path()).map_err(|error| io_error("read", &entry.path(), error))?;
        let assignment = serde_json::from_slice(&bytes).map_err(|error| {
            FlameError::InvalidConfig(format!(
                "invalid GPU assignment {}: {error}",
                entry.path().display()
            ))
        })?;
        if assignments
            .insert(executor_id.to_string(), assignment)
            .is_some()
        {
            return Err(FlameError::InvalidConfig(
                "duplicate GPU assignment executor ID".into(),
            ));
        }
    }
    Ok(assignments)
}

pub(super) fn save(
    workdir: &Path,
    executor_id: &str,
    assignment: &DeviceAssignment,
) -> Result<(), FlameError> {
    let path = assignment_path(workdir, executor_id)?;
    let directory = workdir.join(crate::devices::GPU);
    let temporary = directory.join(format!(".gpu-assignment-{}.tmp", Uuid::new_v4()));
    let result = (|| -> Result<(), Box<dyn std::error::Error>> {
        fs::create_dir_all(&directory)?;
        File::open(workdir)?.sync_all()?;
        let parent = File::open(&directory)?;
        let mut options = OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        let mut file = options.open(&temporary)?;
        file.write_all(&serde_json::to_vec_pretty(assignment)?)?;
        file.sync_all()?;
        fs::rename(&temporary, &path)?;
        parent.sync_all()?;
        Ok(())
    })();
    if let Err(error) = result {
        let _ = fs::remove_file(&temporary);
        return Err(FlameError::Internal(format!(
            "failed to persist GPU assignment {}: {error}",
            path.display()
        )));
    }
    Ok(())
}

pub(super) fn remove(workdir: &Path, executor_id: &str) -> Result<(), FlameError> {
    let path = assignment_path(workdir, executor_id)?;
    match fs::remove_file(&path) {
        Ok(()) => {}
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => return Err(io_error("remove", &path, error)),
    }
    let directory = workdir.join(crate::devices::GPU);
    match File::open(&directory) {
        Ok(directory_file) => directory_file
            .sync_all()
            .map_err(|error| io_error("sync", &directory, error)),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(io_error("open", &directory, error)),
    }
}

fn io_error(action: &str, path: &Path, error: std::io::Error) -> FlameError {
    FlameError::Internal(format!(
        "failed to {action} GPU assignment {}: {error}",
        path.display()
    ))
}
