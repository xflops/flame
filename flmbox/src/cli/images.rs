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

//! `flmbox images`.

use flame_rs::apis::FlameError;

use super::fmt::{human_age, print_table, row, short_digest};
use crate::images::human_bytes;
use crate::sandbox::Sandbox;

pub async fn run(sandbox: &Sandbox) -> Result<(), FlameError> {
    let now = std::time::SystemTime::now();
    let mut rows = vec![row(["REFERENCE", "DIGEST", "SIZE", "AGE"])];
    for image in sandbox.images().await? {
        let age = image
            .created
            .and_then(|t| now.duration_since(t).ok())
            .map_or("-".to_string(), |d| human_age(d.as_secs()));
        rows.push(row([
            image.reference.as_deref().unwrap_or("<unknown>"),
            short_digest(&image.digest),
            &human_bytes(image.size),
            &age,
        ]));
    }
    print_table(&rows);
    Ok(())
}
