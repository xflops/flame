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

//! The command line of `flmbox`: [`Cli`] and one module per subcommand, each
//! with its arguments and a `run` function over the sandbox lifecycle.

use std::process::ExitCode;

use clap::{Parser, Subcommand};
use flame_rs::apis::FlameError;

use crate::sandbox::{self, Sandbox};

mod exec;
mod fmt;
mod images;
mod kill;
mod ps;
mod pull;
mod run;

#[derive(Parser)]
#[command(
    name = "flmbox",
    version,
    about = "Run commands and code in isolated sandboxes",
    arg_required_else_help = true
)]
pub struct Cli {
    /// Run as a Flame service instance: one sandbox per bound session
    #[arg(short = 'd', long)]
    pub daemon: bool,

    /// Configuration file (default: $FLMBOX_CONF, $FLAME_HOME/conf/flame-sandbox.yaml
    /// or ~/.flame/flame-sandbox.yaml when present)
    #[arg(long, global = true, value_name = "FILE")]
    pub config: Option<std::path::PathBuf>,

    #[command(subcommand)]
    pub command: Option<Command>,
}

impl Cli {
    /// `-d` or one subcommand, not both and not neither.
    pub fn check(&self) -> Result<(), FlameError> {
        match (self.daemon, &self.command) {
            (true, Some(_)) => Err(FlameError::InvalidConfig(
                "-d cannot be combined with a command".to_string(),
            )),
            (false, None) => Err(FlameError::InvalidConfig(
                "nothing to do: use -d or a command (pull, run, exec, kill)".to_string(),
            )),
            _ => Ok(()),
        }
    }
}

/// A `flmbox` subcommand.
#[derive(Subcommand)]
pub enum Command {
    /// Download an image into the node-local cache
    Pull(pull::Args),
    /// List the images in the node-local cache
    Images,
    /// List the sandboxes
    Ps,
    /// Create and start a sandbox, print its id
    Run(run::Args),
    /// Run a command in a sandbox and exit with its exit code
    Exec(exec::Args),
    /// Kill and remove a sandbox
    Kill(kill::Args),
}

impl Command {
    /// Runs the subcommand; the exit code is the process's.
    pub async fn run(self, sandbox: &Sandbox) -> Result<ExitCode, FlameError> {
        match self {
            Command::Pull(args) => pull::run(sandbox, args).await?,
            Command::Images => images::run(sandbox).await?,
            Command::Ps => ps::run(sandbox).await?,
            Command::Run(args) => run::run(sandbox, args).await?,
            Command::Exec(args) => return exec::run(sandbox, args).await,
            Command::Kill(args) => kill::run(sandbox, args).await?,
        }
        Ok(ExitCode::SUCCESS)
    }
}

/// Fails with `NotFound` unless the runtime knows sandbox `id`.
async fn require(sandbox: &Sandbox, id: &str) -> Result<(), FlameError> {
    sandbox::validate_id(id)?;
    if sandbox.exists(id).await? {
        Ok(())
    } else {
        Err(FlameError::NotFound(format!("sandbox {id}")))
    }
}

#[cfg(test)]
mod tests {
    use clap::CommandFactory;

    use super::*;

    #[test]
    fn cli_definition_is_valid() {
        Cli::command().debug_assert();
    }

    #[test]
    fn daemon_flag_and_subcommands_are_exclusive() {
        let parse = |args: &[&str]| Cli::try_parse_from(args).unwrap();
        assert!(parse(&["flmbox", "-d"]).check().is_ok());
        assert!(parse(&["flmbox", "kill", "x"]).check().is_ok());
        assert!(parse(&["flmbox", "-d", "kill", "x"]).check().is_err());
        assert!(parse(&["flmbox", "--config", "/c.yaml"]).check().is_err());
        assert!(Cli::try_parse_from(["flmbox"]).is_err());
    }

    #[test]
    fn config_file_is_a_global_option() {
        let cli = Cli::try_parse_from(["flmbox", "--config", "/etc/s.yaml", "-d"]).unwrap();
        assert_eq!(
            cli.config.as_deref(),
            Some(std::path::Path::new("/etc/s.yaml"))
        );
        let cli = Cli::try_parse_from(["flmbox", "kill", "x", "--config", "/etc/s.yaml"]).unwrap();
        assert!(cli.config.is_some());
    }

    #[test]
    fn subcommands_parse() {
        let parse = |args: &[&str]| Cli::try_parse_from(args).unwrap().command.unwrap();
        assert!(matches!(
            parse(&["flmbox", "run", "--name", "dev", "alpine:3"]),
            Command::Run(_)
        ));
        assert!(matches!(parse(&["flmbox", "run"]), Command::Run(_)));
        assert!(matches!(
            parse(&["flmbox", "exec", "dev", "-e", "A=1", "-w", "/tmp", "--", "ls", "-la"]),
            Command::Exec(_)
        ));
        assert!(matches!(
            parse(&["flmbox", "pull", "ubuntu:26.04"]),
            Command::Pull(_)
        ));
        assert!(matches!(
            parse(&["flmbox", "kill", "dev"]),
            Command::Kill(_)
        ));
        assert!(matches!(parse(&["flmbox", "ps"]), Command::Ps));
        assert!(matches!(parse(&["flmbox", "images"]), Command::Images));
        assert!(Cli::try_parse_from(["flmbox", "exec", "dev"]).is_err());
        assert!(Cli::try_parse_from(["flmbox", "kill"]).is_err());
    }
}
