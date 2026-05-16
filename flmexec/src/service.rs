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

mod api;
mod script;

use flame_rs::{self as flame, apis::FlameError};

use api::{Script, ScriptOutput};

#[flame::entrypoint]
async fn exec(script: Script) -> Result<Option<ScriptOutput>, FlameError> {
    tracing::debug!("Try to create engine for script: {:?}", script);
    let engine = script::new(&script)?;
    tracing::debug!("Created engine for language: {}", script.language);
    tracing::debug!("Code:\n{}", script.code);
    let output = engine.run()?;

    Ok(output.map(|data| ScriptOutput { data }))
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    flame::run(exec).await?;

    tracing::debug!("flmexec service was stopped.");

    Ok(())
}
