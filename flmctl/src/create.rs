/*
Copyright 2023 The Flame Authors.
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
use flame_rs::{apis::FlameContext, client::SessionAttributes};

pub async fn run(
    ctx: &FlameContext,
    app: &str,
    slots: &u32,
    batch_size: &u32,
    priority: &u32,
) -> Result<(), Box<dyn Error>> {
    let current_ctx = ctx.get_current_context()?;
    let conn = flame::client::connect_with_tls(
        &current_ctx.cluster.endpoint,
        current_ctx.cluster.tls.as_ref(),
    )
    .await?;
    let attr = SessionAttributes {
        id: format!("{app}-{}", stdng::rand::short_name()),
        application: app.to_owned(),
        slots: *slots,
        common_data: None,
        min_instances: 0,
        max_instances: None,
        batch_size: *batch_size,
        priority: *priority,
    };

    let ssn = conn.create_session(&attr).await?;

    println!("Session <{}> was created.", ssn.id);

    Ok(())
}
