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
use std::path::PathBuf;
use std::process::Command;

use serde_json::Value;

const IDS: [&str; 3] = ["rootfs-first", "rootfs-second", "rootfs-fresh"];
const MARKER: &str = "flmbox-rootfs-private";
const DIRECTORIES: [&str; 6] = ["", "root", "usr/local", "var", "etc", "workspace/project"];

/// All commands share one image cache. Runtime cleanup precedes TempDir cleanup,
/// including when a test assertion panics.
struct Sandboxes {
    state: tempfile::TempDir,
    config: PathBuf,
}

impl Sandboxes {
    fn new() -> Self {
        let state = tempfile::tempdir().unwrap();
        let config = state.path().join("flame-sandbox.yaml");
        fs::write(&config, "sandbox: {}\n").unwrap();
        Self { state, config }
    }

    fn command(&self, args: &[&str]) -> Command {
        let mut command = Command::new(env!("CARGO_BIN_EXE_flmbox"));
        // Avoid picking up the invoking machine's sandbox settings. Preserve
        // PATH and registry/proxy settings needed to run runsc and pull images.
        for (key, _) in std::env::vars_os() {
            if key.to_string_lossy().starts_with("FLMBOX_") {
                command.env_remove(key);
            }
        }
        command
            .arg("--config")
            .arg(&self.config)
            .arg("--network=none")
            .args(args)
            .env("FLMBOX_STATE_DIR", self.state.path())
            .env("FLMBOX_OVERLAY", "root:memory");
        command
    }

    fn run(&self, args: &[&str]) -> Vec<u8> {
        let output = self.command(args).output().unwrap();
        assert!(
            output.status.success(),
            "flmbox {args:?}: {}\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        output.stdout
    }

    fn exec(&self, id: &str, script: &str) -> Vec<u8> {
        self.run(&["exec", id, "--timeout", "30", "--", "sh", "-c", script])
    }

    fn bundle(&self, id: &str) -> PathBuf {
        self.state.path().join("sandboxes").join(id)
    }

    fn rootfs(&self, id: &str) -> PathBuf {
        let spec: Value =
            serde_json::from_slice(&fs::read(self.bundle(id).join("config.json")).unwrap())
                .unwrap();
        assert_eq!(spec["root"]["readonly"], false);
        PathBuf::from(spec["root"]["path"].as_str().unwrap())
    }
}

impl Drop for Sandboxes {
    fn drop(&mut self) {
        for id in IDS {
            let _ = self.command(&["kill", id]).output();
        }
    }
}

#[test]
#[ignore = "requires Linux, root, runsc and image access"]
fn writable_rootfs_is_private_and_discarded_on_delete() {
    let uid = Command::new("id").arg("-u").output().unwrap();
    assert!(uid.status.success());
    assert_eq!(String::from_utf8_lossy(&uid.stdout).trim(), "0");
    let image = std::env::var("FLMBOX_TEST_IMAGE").unwrap_or_else(|_| "ubuntu:26.04".to_string());
    let boxes = Sandboxes::new();
    boxes.run(&["run", "--name", IDS[0], &image]);
    boxes.run(&["run", "--name", IDS[1], &image]);

    // Both live containers use exactly the same cached lower filesystem.
    let lower = boxes.rootfs(IDS[0]);
    assert_eq!(lower, boxes.rootfs(IDS[1]));
    assert!(lower.starts_with(boxes.state.path().join("images")));
    let os_release = fs::canonicalize(lower.join("etc/os-release")).unwrap();
    assert!(os_release.starts_with(&lower));
    let original = fs::read(&os_release).unwrap();
    assert_eq!(boxes.exec(IDS[0], "cat /etc/os-release"), original);
    assert_eq!(boxes.exec(IDS[1], "cat /etc/os-release"), original);

    boxes.exec(
        IDS[0],
        &format!(
            "set -eu; mkdir -p /workspace/project; \
             for dir in / /root /usr/local /var /etc /workspace/project; do \
             printf 'sandbox-one\\n' > \"$dir/{MARKER}\"; done; \
             printf '\\n# sandbox-one\\n' >> /etc/os-release"
        ),
    );
    boxes.exec(
        IDS[0],
        &format!(
            "set -eu; for dir in / /root /usr/local /var /etc /workspace/project; do \
             test \"$(cat \"$dir/{MARKER}\")\" = sandbox-one; done"
        ),
    );
    let mut modified = original.clone();
    modified.extend_from_slice(b"\n# sandbox-one\n");
    assert_eq!(boxes.exec(IDS[0], "cat /etc/os-release"), modified);

    let pristine = format!(
        "set -eu; for dir in / /root /usr/local /var /etc /workspace/project; do \
         test ! -e \"$dir/{MARKER}\"; done; cat /etc/os-release"
    );
    assert_eq!(boxes.exec(IDS[1], &pristine), original);
    assert_eq!(fs::read(&os_release).unwrap(), original);
    for directory in DIRECTORIES {
        assert!(!lower.join(directory).join(MARKER).exists());
    }

    boxes.run(&["kill", IDS[0]]);
    assert!(!boxes.bundle(IDS[0]).exists());
    boxes.run(&["run", "--name", IDS[2], &image]);
    assert_eq!(boxes.rootfs(IDS[2]), lower);
    assert_eq!(boxes.exec(IDS[2], &pristine), original);
    assert_eq!(fs::read(&os_release).unwrap(), original);
}
