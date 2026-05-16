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

use flame_rs::{self as flame, apis::FlameError};

use self::apis::{PingRequest, PingResponse};

#[flame::entrypoint]
async fn ping(input: Option<PingRequest>) -> Result<PingResponse, FlameError> {
    let start_time = Instant::now();
    let input = input.unwrap_or_default();

    let mem = match input.memory {
        Some(memory) => vec![0; memory as usize].into_boxed_slice(),
        None => Box::new([]),
    };

    if let Some(duration) = input.duration {
        thread::sleep(Duration::from_millis(duration));
    }

    let end_time = Instant::now();
    let duration = end_time.duration_since(start_time).as_millis();

    let mem_size = Byte::from_u64(mem.len() as u64).to_string();

    Ok(PingResponse {
        message: format!(
            "Completed on <{}> in <{duration}> milliseconds with <{mem_size}> memory",
            gethostname().to_string_lossy(),
        ),
    })
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    flame::run(ping).await?;

    tracing::debug!("FlmpingService was stopped.");

    Ok(())
}
