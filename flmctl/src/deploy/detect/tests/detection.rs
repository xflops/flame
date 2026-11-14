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
