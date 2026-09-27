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

use flame_rs::apis::FlameError;
use toml::Value;

use super::artifact::{is_executable, ApplicationInputKind};

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct DetectedApplication {
    pub installer: Option<String>,
    pub command: Option<String>,
    pub arguments: Vec<String>,
}

impl DetectedApplication {
    pub fn executable(command: String) -> Self {
        Self {
            installer: Some("binary".to_string()),
            command: Some(command),
            arguments: Vec::new(),
        }
    }
}

pub fn detect_application(
    app_name: &str,
    kind: ApplicationInputKind,
    root: &Path,
) -> Result<DetectedApplication, FlameError> {
    match kind {
        ApplicationInputKind::ExecutableFile => {
            let command = root
                .join("bin")
                .read_dir()
                .map_err(|e| {
                    FlameError::Internal(format!("failed to read binary package dir: {}", e))
                })?
                .filter_map(|entry| entry.ok().map(|entry| entry.path()))
                .find(|path| path.is_file())
                .and_then(|path| {
                    path.file_name()
                        .and_then(|n| n.to_str())
                        .map(|name| name.to_string())
                })
                .ok_or_else(|| {
                    FlameError::InvalidConfig("failed to detect binary command".to_string())
                })?;
            Ok(DetectedApplication::executable(command))
        }
        ApplicationInputKind::File => {
            let file = fs::read_dir(root)
                .map_err(|e| {
                    FlameError::Internal(format!("failed to read file package dir: {}", e))
                })?
                .filter_map(|entry| entry.ok().map(|entry| entry.path()))
                .find(|path| path.is_file())
                .ok_or_else(|| FlameError::InvalidConfig("file package is empty".to_string()))?;
            let name = file.file_name().unwrap().to_string_lossy().into_owned();
            if file.extension().is_some_and(|ext| ext == "py") {
                Ok(DetectedApplication {
                    installer: Some("binary".to_string()),
                    command: Some("python3".to_string()),
                    arguments: vec![name],
                })
            } else {
                Ok(DetectedApplication {
                    installer: Some("binary".to_string()),
                    command: None,
                    arguments: Vec::new(),
                })
            }
        }
        ApplicationInputKind::Directory | ApplicationInputKind::TarGz => {
            detect_from_directory(app_name, root)
        }
    }
}

fn detect_from_directory(app_name: &str, root: &Path) -> Result<DetectedApplication, FlameError> {
    if has_python_marker(root) {
        let (command, arguments) = detect_python_command(app_name, root)?;
        return Ok(DetectedApplication {
            installer: Some("python".to_string()),
            command,
            arguments,
        });
    }

    let command = detect_binary_command(app_name, root)?;
    Ok(DetectedApplication {
        installer: Some("binary".to_string()),
        command,
        arguments: Vec::new(),
    })
}

fn has_python_marker(root: &Path) -> bool {
    root.join("pyproject.toml").exists()
        || root.join("setup.py").exists()
        || root.join("setup.cfg").exists()
}

fn detect_python_command(
    app_name: &str,
    root: &Path,
) -> Result<(Option<String>, Vec<String>), FlameError> {
    if let Some(command) = detect_pyproject_script(app_name, root)? {
        return Ok((Some(command), Vec::new()));
    }

    let module_name = app_name.replace('-', "_");
    if root.join(&module_name).join("__main__.py").exists() {
        return Ok((
            Some("python".to_string()),
            vec!["-m".to_string(), module_name],
        ));
    }

    Ok((None, Vec::new()))
}

fn detect_pyproject_script(app_name: &str, root: &Path) -> Result<Option<String>, FlameError> {
    let pyproject = root.join("pyproject.toml");
    if !pyproject.exists() {
        return Ok(None);
    }

    let contents = fs::read_to_string(&pyproject).map_err(|e| {
        FlameError::Internal(format!("failed to read {}: {}", pyproject.display(), e))
    })?;
    let value: Value = toml::from_str(&contents).map_err(|e| {
        FlameError::InvalidConfig(format!("failed to parse {}: {}", pyproject.display(), e))
    })?;

    let Some(scripts) = value
        .get("project")
        .and_then(|project| project.get("scripts"))
        .and_then(Value::as_table)
    else {
        return Ok(None);
    };

    if let Some((name, _)) = scripts.iter().find(|(name, _)| name.as_str() == app_name) {
        return Ok(Some(name.clone()));
    }

    if scripts.len() == 1 {
        return Ok(scripts.keys().next().cloned());
    }

    Ok(None)
}

fn detect_binary_command(app_name: &str, root: &Path) -> Result<Option<String>, FlameError> {
    let bin = root.join("bin").join(app_name);
    if bin.is_file() && is_executable(&bin)? {
        return Ok(Some(app_name.to_string()));
    }

    let bin_dir = root.join("bin");
    if bin_dir.is_dir() {
        let executables = executable_children(&bin_dir)?;
        if executables.len() == 1 {
            return Ok(file_name(&executables[0]));
        }
        if executables.len() > 1 {
            return Ok(None);
        }
    }

    let executables = executable_children(root)?;
    if executables.len() == 1 {
        return Ok(file_name(&executables[0]));
    }

    Ok(None)
}

fn executable_children(dir: &Path) -> Result<Vec<PathBuf>, FlameError> {
    let mut executables = Vec::new();
    for entry in fs::read_dir(dir).map_err(|e| {
        FlameError::Internal(format!("failed to read directory {}: {}", dir.display(), e))
    })? {
        let entry =
            entry.map_err(|e| FlameError::Internal(format!("failed to read entry: {}", e)))?;
        let path = entry.path();
        if path.is_file() && is_executable(&path)? {
            executables.push(path);
        }
    }
    executables.sort();
    Ok(executables)
}

fn file_name(path: &Path) -> Option<String> {
    path.file_name()
        .and_then(|n| n.to_str())
        .map(|name| name.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn detects_pyproject_script_matching_app_name() {
        let temp = TempDir::new().unwrap();
        fs::write(
            temp.path().join("pyproject.toml"),
            "[project.scripts]\nother = 'pkg:main'\ndemo = 'pkg:demo'\n",
        )
        .unwrap();

        let detected =
            detect_application("demo", ApplicationInputKind::Directory, temp.path()).unwrap();
        assert_eq!(detected.installer.as_deref(), Some("python"));
        assert_eq!(detected.command.as_deref(), Some("demo"));
    }

    #[test]
    fn detects_python_module_main() {
        let temp = TempDir::new().unwrap();
        fs::write(
            temp.path().join("pyproject.toml"),
            "[project]\nname = 'demo'\n",
        )
        .unwrap();
        fs::create_dir(temp.path().join("demo_app")).unwrap();
        fs::write(temp.path().join("demo_app/__main__.py"), "").unwrap();

        let detected =
            detect_application("demo-app", ApplicationInputKind::Directory, temp.path()).unwrap();
        assert_eq!(detected.installer.as_deref(), Some("python"));
        assert_eq!(detected.command.as_deref(), Some("python"));
        assert_eq!(detected.arguments, vec!["-m", "demo_app"]);
    }

    #[test]
    fn detects_binary_in_bin_dir() {
        let temp = TempDir::new().unwrap();
        let bin_dir = temp.path().join("bin");
        fs::create_dir(&bin_dir).unwrap();
        let bin = bin_dir.join("demo");
        fs::write(&bin, "").unwrap();
        make_executable(&bin);

        let detected =
            detect_application("demo", ApplicationInputKind::Directory, temp.path()).unwrap();
        assert_eq!(detected.installer.as_deref(), Some("binary"));
        assert_eq!(detected.command.as_deref(), Some("demo"));
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
}
