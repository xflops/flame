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
use flame_rs::apis::{FlameContext, SessionState};
use flame_rs::client::SessionFilter;

/// Which sessions a `delete -s` argument names.
#[derive(Debug, PartialEq)]
enum Selection<'a> {
    AllClosed,
    Ids(Vec<&'a str>),
}

/// `None` means `-s` was not given, `Some(&[])` means `-s` with no value.
fn parse_selection(sessions: Option<&[String]>) -> Result<Selection<'_>, Box<dyn Error>> {
    let Some(sessions) = sessions else {
        return Err("--session is required; use `-s` alone for every closed session".into());
    };
    if sessions.is_empty() {
        return Ok(Selection::AllClosed);
    }

    // An explicit blank id is a mistake, not a request for every session.
    let ids: Vec<&str> = sessions.iter().map(|id| id.trim()).collect();
    if ids.iter().any(|id| id.is_empty()) {
        return Err("session ids must not be empty".into());
    }

    Ok(Selection::Ids(ids))
}

pub async fn run(ctx: &FlameContext, sessions: Option<&[String]>) -> Result<(), Box<dyn Error>> {
    let selection = parse_selection(sessions)?;

    let current_ctx = ctx.get_current_context()?;
    let conn = flame::client::connect_with_tls(
        &current_ctx.cluster.endpoint,
        current_ctx.cluster.tls.as_ref(),
    )
    .await?;

    // Only closed sessions can be deleted, so ask the server for closed ones
    // only (and for just the named ids), and no other session crosses the wire.
    let filter = SessionFilter {
        state: Some(SessionState::Closed),
        ids: match &selection {
            Selection::AllClosed => vec![],
            Selection::Ids(ids) => ids.iter().map(|id| id.to_string()).collect(),
        },
        ..Default::default()
    };
    // The server returns each matching session once.
    let closed: Vec<String> = conn
        .list_sessions_with(&filter)
        .await?
        .into_iter()
        .map(|ssn| ssn.id)
        .collect();

    let mut deleted = 0;
    let mut failed = 0;

    // A named id that did not come back is not a closed session.
    if let Selection::Ids(ids) = &selection {
        for id in ids.iter().filter(|id| !closed.iter().any(|c| c == **id)) {
            failed += 1;
            eprintln!(
                "Failed to delete session <{}>: not found or not closed (close it first)",
                id
            );
        }
    }

    for id in &closed {
        match conn.delete_session(id).await {
            Ok(()) => deleted += 1,
            Err(e) => {
                failed += 1;
                eprintln!("Failed to delete session <{}>: {}", id, e);
            }
        }
    }

    println!();
    println!("Deleted {} session(s), {} failed.", deleted, failed);
    if failed > 0 {
        return Err(format!("failed to delete {} session(s)", failed).into());
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(ids: &[&str]) -> Vec<String> {
        ids.iter().map(|id| id.to_string()).collect()
    }

    #[test]
    fn no_value_selects_every_closed_session() {
        assert_eq!(parse_selection(Some(&[])).unwrap(), Selection::AllClosed);
    }

    #[test]
    fn ids_select_those_sessions() {
        let a = args(&["id-1", " id-2 "]);
        assert_eq!(
            parse_selection(Some(&a)).unwrap(),
            Selection::Ids(vec!["id-1", "id-2"])
        );
    }

    #[test]
    fn missing_flag_is_rejected() {
        assert!(parse_selection(None).is_err());
    }

    #[test]
    fn blank_id_is_rejected() {
        assert!(parse_selection(Some(&args(&[""]))).is_err());
        assert!(parse_selection(Some(&args(&["id-1", " "]))).is_err());
    }
}
