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

mod lang;

use std::collections::HashMap;

use flame_rs::apis::FlameError;

use crate::api::Script;
use lang::{python::PythonScript, shell::ShellScript};

#[derive(Debug, Clone)]
pub struct ScriptRuntime {
    pub entrypoint: String,
    pub work_dir: String,
    pub input: Option<Vec<u8>>,
    pub env: HashMap<String, String>,
}

pub trait ScriptEngine {
    fn run(&self) -> Result<Option<Vec<u8>>, FlameError>;
}

pub fn new(script: &Script) -> Result<Box<dyn ScriptEngine>, FlameError> {
    match script.language.to_lowercase().as_str() {
        "shell" => Ok(Box::new(ShellScript::new(script)?)),
        "python" => Ok(Box::new(PythonScript::new(script)?)),
        _ => Err(FlameError::InvalidConfig(format!(
            "Unsupported language: {}",
            script.language
        ))),
    }
}
