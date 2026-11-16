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

use std::collections::HashSet;
use std::error::Error;

use flame_rs as flame;
use flame_rs::apis::{FlameContext, SessionState};
use flame_rs::client::{NodeFilter, NodeState, Paging, SessionFilter};

use crate::utils::trim_ids;

#[derive(Debug, PartialEq)]
pub(crate) enum NodeSelection<'a> {
    AllNotReady,
    Ids(Vec<&'a str>),
}

#[derive(Debug, PartialEq)]
pub(crate) enum SessionSelection<'a> {
    AllClosed,
    Ids(Vec<&'a str>),
}

impl<'a> NodeSelection<'a> {
    pub(crate) fn parse(nodes: &'a [String]) -> Result<Self, Box<dyn Error>> {
        if nodes.is_empty() {
            return Ok(Self::AllNotReady);
        }
        let mut ids = trim_ids(nodes).map_err(|_| "node names must not be empty")?;
        let mut seen = HashSet::new();
        ids.retain(|id| seen.insert(*id));
        Ok(Self::Ids(ids))
    }
}

impl<'a> SessionSelection<'a> {
    pub(crate) fn parse(sessions: &'a [String]) -> Result<Self, Box<dyn Error>> {
        if sessions.is_empty() {
            return Ok(Self::AllClosed);
        }
        Ok(Self::Ids(trim_ids(sessions)?))
    }
}

async fn connect(ctx: &FlameContext) -> Result<flame::Connection, Box<dyn Error>> {
    let current_ctx = ctx.get_current_context()?;
    Ok(flame::client::connect_with_tls(
        &current_ctx.cluster.endpoint,
        current_ctx.cluster.tls.as_ref(),
    )
    .await?)
}

pub(crate) async fn delete_nodes(
    ctx: &FlameContext,
    nodes: &[String],
    paging: Paging,
) -> Result<(), Box<dyn Error>> {
    let selection = NodeSelection::parse(nodes)?;
    let conn = connect(ctx).await?;
    let filter = NodeFilter {
        names: match selection {
            NodeSelection::AllNotReady => Vec::new(),
            NodeSelection::Ids(ids) => ids.into_iter().map(str::to_owned).collect(),
        },
        paging: Some(paging),
        ..Default::default()
    };
    let names = conn
        .list_nodes(Some(&filter))
        .await?
        .items
        .into_iter()
        .filter(|node| node.state == NodeState::NotReady)
        .map(|node| node.name)
        .collect::<Vec<_>>();
    let mut deleted = 0;
    let mut failed = 0;
    for name in names {
        match conn.delete_node(&name).await {
            Ok(()) => deleted += 1,
            Err(error) => {
                failed += 1;
                eprintln!("Failed to delete node <{}>: {}", name, error);
            }
        }
    }
    println!();
    println!("Deleted {} node(s), {} failed.", deleted, failed);
    if failed > 0 {
        return Err(format!("failed to delete {} node(s)", failed).into());
    }
    Ok(())
}

pub(crate) async fn delete_sessions(
    ctx: &FlameContext,
    sessions: &[String],
    paging: Paging,
) -> Result<(), Box<dyn Error>> {
    let selection = SessionSelection::parse(sessions)?;
    let conn = connect(ctx).await?;
    let ids = match &selection {
        SessionSelection::AllClosed => vec![],
        SessionSelection::Ids(ids) => ids.iter().map(|id| id.to_string()).collect(),
    };

    let filter = SessionFilter {
        ids,
        paging: Some(paging),
        ..Default::default()
    };
    let closed = conn
        .list_sessions(Some(&filter))
        .await?
        .items
        .into_iter()
        .filter(|session| session.state == SessionState::Closed)
        .map(|session| session.id)
        .collect::<Vec<_>>();

    let mut deleted = 0;
    let mut failed = 0;

    // A named id that did not come back is not a closed session.
    if let SessionSelection::Ids(ids) = &selection {
        for id in ids.iter().filter(|id| !closed.iter().any(|c| c == **id)) {
            failed += 1;
            eprintln!(
                "Failed to delete session <{}>: not a Closed session on the selected page",
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

pub async fn run(
    ctx: &FlameContext,
    sessions: Option<&[String]>,
    nodes: Option<&[String]>,
    paging: Paging,
) -> Result<(), Box<dyn Error>> {
    match (sessions, nodes) {
        (Some(sessions), None) => delete_sessions(ctx, sessions, paging).await,
        (None, Some(nodes)) => delete_nodes(ctx, nodes, paging).await,
        (Some(_), Some(_)) => Err("--session and --node cannot be combined".into()),
        (None, None) => Err(
            "--session or --node is required; use `-s` for Closed sessions or `-n` for NotReady nodes on the selected page".into(),
        ),
    }
}
