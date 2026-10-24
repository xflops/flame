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

//! `flmbox ps`.

use flame_rs::apis::FlameError;

use super::fmt::{print_table, row};
use crate::sandbox::Sandbox;

pub async fn run(sandbox: &Sandbox) -> Result<(), FlameError> {
    let mut rows = vec![row(["ID", "STATUS", "CREATED", "IMAGE"])];
    for sb in sandbox.ps().await? {
        rows.push(row([
            &sb.info.id,
            &sb.info.status,
            sb.info.created.as_deref().unwrap_or("-"),
            sb.image.as_deref().unwrap_or("-"),
        ]));
    }
    print_table(&rows);
    Ok(())
}
