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

//! Test doubles shared by the unit tests.

use std::path::PathBuf;
use std::sync::{Arc, Mutex, PoisonError};

use async_trait::async_trait;
use flame_rs::apis::FlameError;

use crate::config::{FlameRuntime, FlameSandbox};
use crate::images::Images;
use crate::images::{ImageConfig, ImageInfo, ImageRoot};
use crate::runtime::runsc::runner::{CommandRunner, CommandSpec};
use crate::runtime::runsc::Runsc;
use crate::runtime::{CommandOutput, Runtime};

/// A `runsc` runtime whose commands go to `runner`.
pub fn runsc_runtime(cfg: &FlameSandbox, runner: Arc<FakeRunner>) -> Arc<dyn Runtime> {
    let FlameRuntime::Runsc(runsc) = &cfg.runtime;
    Arc::new(Runsc::with_runner(runsc, &cfg.state_dir, runner))
}

/// Builds a command output.
pub fn out(exit_code: i32, stdout: &str, stderr: &str) -> CommandOutput {
    CommandOutput {
        exit_code,
        stdout: stdout.as_bytes().to_vec(),
        stderr: stderr.as_bytes().to_vec(),
        ..CommandOutput::default()
    }
}

type Responder = Box<dyn Fn(&CommandSpec) -> Result<CommandOutput, FlameError> + Send + Sync>;

/// Records every command and answers with a scripted response (default: exit 0).
#[derive(Default)]
pub struct FakeRunner {
    specs: Mutex<Vec<CommandSpec>>,
    responder: Mutex<Option<Responder>>,
}

impl FakeRunner {
    /// Sets the responder for all following commands.
    pub fn respond(
        &self,
        f: impl Fn(&CommandSpec) -> Result<CommandOutput, FlameError> + Send + Sync + 'static,
    ) {
        *self
            .responder
            .lock()
            .unwrap_or_else(PoisonError::into_inner) = Some(Box::new(f));
    }

    /// The recorded commands.
    pub fn specs(&self) -> Vec<CommandSpec> {
        self.specs
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }

    /// The recorded commands as `program arg arg ...`.
    pub fn lines(&self) -> Vec<String> {
        self.specs().iter().map(line).collect()
    }

    /// The recorded `runsc` subcommands without the program and global flags.
    pub fn subcommands(&self) -> Vec<String> {
        self.specs()
            .iter()
            .map(|s| {
                let mut args = s.args.iter().peekable();
                while let Some(a) = args.peek() {
                    if *a == "--root" {
                        args.next();
                        args.next();
                    } else if a.starts_with("--") {
                        args.next();
                    } else {
                        break;
                    }
                }
                args.cloned().collect::<Vec<_>>().join(" ")
            })
            .collect()
    }

    /// Forgets the recorded commands.
    pub fn clear(&self) {
        self.specs
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clear();
    }
}

fn line(spec: &CommandSpec) -> String {
    let mut parts = vec![spec.program.clone()];
    parts.extend(spec.args.iter().cloned());
    parts.join(" ")
}

#[async_trait]
impl CommandRunner for FakeRunner {
    async fn run(&self, spec: CommandSpec) -> Result<CommandOutput, FlameError> {
        let result = {
            let responder = self
                .responder
                .lock()
                .unwrap_or_else(PoisonError::into_inner);
            match responder.as_ref() {
                Some(f) => f(&spec),
                None => Ok(CommandOutput::default()),
            }
        };
        self.specs
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .push(spec);
        result
    }
}

/// Returns a fixed image root without any registry.
pub struct FakeImages {
    /// The rootfs handed out.
    pub rootfs: PathBuf,
    /// Image config handed out.
    pub config: ImageConfig,
    /// When set, every call fails with this message.
    pub fail: Mutex<Option<String>>,
    /// Requested references.
    pub calls: Mutex<Vec<String>>,
}

impl FakeImages {
    /// A fake serving `rootfs`.
    pub fn new(rootfs: PathBuf) -> Arc<Self> {
        Arc::new(Self {
            rootfs,
            config: ImageConfig::default(),
            fail: Mutex::new(None),
            calls: Mutex::new(Vec::new()),
        })
    }

    /// The requested references.
    pub fn calls(&self) -> Vec<String> {
        self.calls
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }
}

#[async_trait]
impl Images for FakeImages {
    async fn pull_image(&self, reference: &str) -> Result<ImageRoot, FlameError> {
        self.calls
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .push(reference.to_string());
        if let Some(msg) = self
            .fail
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
        {
            return Err(FlameError::Network(msg));
        }
        Ok(ImageRoot {
            digest: "sha256:fake".to_string(),
            rootfs: self.rootfs.clone(),
            config: self.config.clone(),
        })
    }

    async fn list_images(&self) -> Result<Vec<ImageInfo>, FlameError> {
        Ok(self
            .calls()
            .into_iter()
            .map(|reference| ImageInfo {
                digest: "sha256:fake".to_string(),
                reference: Some(reference),
                size: 1,
                created: None,
            })
            .collect())
    }
}
