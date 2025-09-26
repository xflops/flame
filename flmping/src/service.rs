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
mod apis;

use byte_unit::Byte;
use gethostname::gethostname;
use std::thread;
use std::time::{Duration, Instant};

use flame_rs::{
    self as flame,
    apis::{FlameError, TaskOutput},
    service::{SessionContext, TaskContext},
};

use self::apis::{PingRequest, PingResponse};

#[derive(Clone)]
pub struct FlmpingService {}

#[tonic::async_trait]
impl flame::service::FlameService for FlmpingService {
    async fn on_session_enter(&self, _: SessionContext) -> Result<(), FlameError> {
        Ok(())
    }

    async fn on_task_invoke(&self, ctx: TaskContext) -> Result<Option<TaskOutput>, FlameError> {
        let start_time = Instant::now();

        let input = ctx.input.unwrap_or_default();
        let input: PingRequest = input.try_into()?;

        let mem = match input.memory {
            Some(memory) => {
                let mem = vec![0; memory as usize];
                mem.into_boxed_slice()
            }
            None => Box::new([]),
        };

        if let Some(duration) = input.duration {
            thread::sleep(Duration::from_millis(duration));
        }

        let end_time = Instant::now();
        let duration = end_time.duration_since(start_time).as_millis();

        let mem_size = Byte::from_u64(mem.len() as u64).to_string();

        let output = PingResponse {
            message: format!(
                "Completed on <{}> in <{duration}> milliseconds with <{mem_size}> memory",
                gethostname().to_string_lossy(),
            ),
        };

        Ok(Some(output.try_into()?))
    }

    async fn on_session_leave(&self) -> Result<(), FlameError> {
        Ok(())
    }
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    flame::apis::init_logger()?;

    flame::service::run(FlmpingService {}).await?;

    tracing::debug!("FlmpingService was stopped.");

    Ok(())
}
