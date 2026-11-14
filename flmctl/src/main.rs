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
use std::io;

use clap::{CommandFactory, Parser, Subcommand};
use clap_complete::{generate, Shell};
use flame_rs::apis::FlameContext;

mod close;
mod create;
mod delete;
mod deploy;
mod helper;
mod list;
mod register;
mod resume;
mod suspend;
mod unregister;
mod update;
mod utils;
mod view;

#[derive(Parser)]
#[command(name = "flmctl")]
#[command(author = "XFLOPS <support@xflops.io>")]
#[command(version)]
#[command(about = "Flame command line", long_about = None)]
pub struct Cli {
    /// The configuration of flmctl
    #[arg(long)]
    config: Option<String>,

    #[command(subcommand)]
    command: Option<Commands>,
}

#[derive(Subcommand)]
enum Commands {
    /// View the object of Flame
    View {
        /// The name of application
        #[arg(short, long)]
        application: Option<String>,

        /// The id of session
        #[arg(short, long)]
        session: Option<String>,

        /// The id of task
        #[arg(short, long)]
        task: Option<String>,

        /// The name of node
        #[arg(short, long)]
        node: Option<String>,

        /// The output format of the view
        #[arg(short, long, value_parser = ["table", "json"])]
        output_format: Option<String>,
    },
    /// Update the object of Flame
    Update {
        /// The yaml file of the application
        #[arg(short, long)]
        application: Option<String>,
    },
    /// List the objects of Flame
    List {
        /// List the applications of Flame
        #[arg(short, long)]
        application: bool,
        /// List the sessions of Flame
        #[arg(short, long)]
        session: bool,
        /// List the executors of Flame
        #[arg(short, long)]
        executor: bool,
        /// List the nodes of Flame
        #[arg(short, long)]
        node: bool,
        /// Output format for the list
        #[arg(short, long, default_value = "table", value_parser = ["table", "json"])]
        output_format: String,
    },
    /// Close sessions in Flame
    Close {
        /// Comma-separated session ids
        #[arg(short, long, required = true, num_args = 1.., value_delimiter = ',')]
        session: Vec<String>,
    },
    /// Suspend an open session: running tasks finish, executors unbind, then the session is suspended
    Suspend {
        /// The id of the session
        session: String,
    },
    /// Resume a suspended session (an open session is left as is)
    Resume {
        /// The id of the session
        session: String,
    },
    /// Delete closed sessions or inactive nodes in Flame
    Delete {
        /// Comma-separated session ids; with no value, every closed session
        #[arg(short, long, num_args = 0.., value_delimiter = ',', required_unless_present = "node", conflicts_with = "node")]
        session: Option<Vec<String>>,
        /// Comma-separated node names; with no value, every NotReady node
        #[arg(short, long, num_args = 0.., value_delimiter = ',')]
        node: Option<Vec<String>>,
    },
    /// Create a session in Flame
    Create {
        /// The name of Application
        #[arg(short, long)]
        app: String,
        /// Session priority (higher = more important, default: 0)
        #[arg(short = 'p', long, default_value = "0")]
        priority: u32,
        /// Explicit resource request (format: "cpu=N,mem=SIZE,gpu=N").
        /// If omitted, server applies `cluster.resreq` from
        /// flame-cluster.yaml (or a hardcoded fallback when that is unset).
        #[arg(short, long)]
        resreq: Option<String>,
    },
    /// Register an application
    Register {
        /// The yaml file of the application
        #[arg(short, long)]
        file: String,
    },
    /// Deploy an application package to object cache and register it
    Deploy(Box<deploy::Options>),
    /// Unregister the application from Flame
    Unregister {
        /// The name of the application
        #[arg(short, long)]
        application: String,
    },
    /// Generate shell completion scripts
    Completion {
        /// Shell to generate completions for
        #[arg(value_enum)]
        shell: Shell,
    },
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn Error>> {
    flame_rs::apis::init_logger()?;

    let cli = Cli::parse();
    let ctx = FlameContext::from_file(cli.config)?;

    match &cli.command {
        Some(Commands::List {
            application,
            session,
            executor,
            node,
            output_format,
        }) => {
            list::run(
                &ctx,
                *application,
                *session,
                *executor,
                *node,
                output_format,
            )
            .await?
        }
        Some(Commands::Close { session }) => close::run(&ctx, session).await?,
        Some(Commands::Delete { session, node }) => {
            delete::run(&ctx, session.as_deref(), node.as_deref()).await?
        }
        Some(Commands::Suspend { session }) => suspend::run(&ctx, session).await?,
        Some(Commands::Resume { session }) => resume::run(&ctx, session).await?,
        Some(Commands::Create {
            app,
            priority,
            resreq,
        }) => create::run(&ctx, app, priority, resreq).await?,
        Some(Commands::View {
            application,
            session,
            task,
            node,
            output_format,
        }) => view::run(&ctx, output_format, application, session, task, node).await?,
        Some(Commands::Register { file }) => register::run(&ctx, file).await?,
        Some(Commands::Deploy(options)) => deploy::run(&ctx, options).await?,
        Some(Commands::Unregister { application }) => unregister::run(&ctx, application).await?,
        Some(Commands::Update { application }) => update::run(&ctx, application).await?,
        Some(Commands::Completion { shell }) => {
            generate(*shell, &mut Cli::command(), "flmctl", &mut io::stdout());
        }
        _ => helper::run().await?,
    };

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn close_ids(args: &[&str]) -> Vec<String> {
        let cli =
            Cli::try_parse_from(std::iter::once("flmctl").chain(args.iter().copied())).unwrap();
        match cli.command {
            Some(Commands::Close { session }) => session,
            _ => panic!("not a close command"),
        }
    }

    #[test]
    fn close_takes_ids_separated_by_commas_spaces_or_repeated_flags() {
        let expected = ["a", "b", "c"];
        assert_eq!(close_ids(&["close", "-s", "a,b,c"]), expected);
        assert_eq!(close_ids(&["close", "-s", "a", "b", "c"]), expected);
        assert_eq!(close_ids(&["close", "-s", "a", "-s", "b,c"]), expected);
    }

    #[test]
    fn close_requires_a_session() {
        assert!(Cli::try_parse_from(["flmctl", "close"]).is_err());
        assert!(Cli::try_parse_from(["flmctl", "close", "-s"]).is_err());
    }

    fn delete_selection(args: &[&str]) -> (Option<Vec<String>>, Option<Vec<String>>) {
        let cli =
            Cli::try_parse_from(std::iter::once("flmctl").chain(args.iter().copied())).unwrap();
        match cli.command {
            Some(Commands::Delete { session, node }) => (session, node),
            _ => panic!("not a delete command"),
        }
    }

    #[test]
    fn delete_requires_one_kind_of_selection() {
        assert!(Cli::try_parse_from(["flmctl", "delete"]).is_err());
        for args in [
            vec!["flmctl", "delete", "-s", "-n"],
            vec!["flmctl", "delete", "-s", "session-a", "-n", "worker-a"],
            vec!["flmctl", "delete", "--node", "worker-a", "--session"],
        ] {
            assert!(Cli::try_parse_from(args).is_err());
        }
    }

    #[test]
    fn delete_selects_all_closed_sessions_or_all_not_ready_nodes() {
        assert_eq!(delete_selection(&["delete", "-s"]), (Some(vec![]), None));
        assert_eq!(
            delete_selection(&["delete", "--session"]),
            (Some(vec![]), None)
        );
        assert_eq!(delete_selection(&["delete", "-n"]), (None, Some(vec![])));
        assert_eq!(
            delete_selection(&["delete", "--node"]),
            (None, Some(vec![]))
        );
    }

    #[test]
    fn delete_accepts_session_and_node_ids_with_the_same_syntax() {
        for (short, long) in [("-s", "--session"), ("-n", "--node")] {
            let expected: Option<Vec<String>> = Some(vec!["a".into(), "b".into(), "c".into()]);
            for values in [
                vec!["delete", short, "a,b,c"],
                vec!["delete", long, "a", "b", "c"],
                vec!["delete", short, "a", short, "b,c"],
            ] {
                let (sessions, nodes) = delete_selection(&values);
                if short == "-s" {
                    assert_eq!((sessions, nodes), (expected.clone(), None));
                } else {
                    assert_eq!((sessions, nodes), (None, expected.clone()));
                }
            }
        }
    }
}
