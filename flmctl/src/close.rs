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

use std::error::Error;

use flame_rs as flame;
use flame_rs::apis::FlameContext;

use crate::utils::trim_ids;

pub async fn run(ctx: &FlameContext, sessions: &[String]) -> Result<(), Box<dyn Error>> {
    let ids = trim_ids(sessions)?;

    let current_ctx = ctx.get_current_context()?;
    let conn = flame::client::connect_with_tls(
        &current_ctx.cluster.endpoint,
        current_ctx.cluster.tls.as_ref(),
    )
    .await?;

    let mut closed = 0;
    let mut failed = 0;
    for id in ids {
        match conn.close_session(id).await {
            Ok(_) => {
                closed += 1;
                println!("Session <{}> was closed.", id);
            }
            Err(e) => {
                failed += 1;
                eprintln!("Failed to close session <{}>: {}", id, e);
            }
        }
    }

    if sessions.len() > 1 || failed > 0 {
        println!();
        println!("Closed {} session(s), {} failed.", closed, failed);
    }
    if failed > 0 {
        return Err(format!("failed to close {} session(s)", failed).into());
    }

    Ok(())
}
