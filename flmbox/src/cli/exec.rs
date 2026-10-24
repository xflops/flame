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

//! `flmbox exec`.

use std::collections::BTreeMap;
use std::io::{Read, Write};
use std::process::ExitCode;

use flame_rs::apis::FlameError;

use super::require;
use crate::api::CommandRequest;
use crate::sandbox::Sandbox;

#[derive(clap::Args)]
pub struct Args {
    /// Sandbox id
    id: String,
    /// Run the words as source code of this language (python or shell)
    /// instead of as a command
    #[arg(short, long)]
    language: Option<String>,
    /// Working directory inside the sandbox
    #[arg(short = 'w', long)]
    cwd: Option<String>,
    /// Environment variable, NAME=VALUE; repeatable
    #[arg(short, long = "env", value_name = "NAME=VALUE")]
    envs: Vec<String>,
    /// Timeout in seconds
    #[arg(short, long)]
    timeout: Option<f64>,
    /// Pass this process's standard input to the command
    #[arg(short = 'i', long)]
    stdin: bool,
    /// The command and its arguments
    #[arg(required = true, trailing_var_arg = true, allow_hyphen_values = true)]
    command: Vec<String>,
}

/// Runs the command, copies its output and exits with its exit code.
pub async fn run(sandbox: &Sandbox, args: Args) -> Result<ExitCode, FlameError> {
    require(sandbox, &args.id).await?;
    let request = CommandRequest {
        code: match args.language {
            Some(_) => args.command.join(" "),
            None => shell_line(&args.command),
        },
        language: args.language,
        cwd: args.cwd,
        envs: Some(parse_envs(&args.envs)?),
        input: args.stdin.then(read_stdin).transpose()?,
        timeout: args.timeout,
    };
    let result = sandbox.exec(&args.id, request).await?;
    write_all(std::io::stdout(), &result.stdout);
    write_all(std::io::stderr(), &result.stderr);
    Ok(ExitCode::from(exit_status(result.exit_code)))
}

/// One word is a command line as given (`'ls | wc -l'`); several words are an
/// argument vector, so each is quoted.
fn shell_line(words: &[String]) -> String {
    match words {
        [line] => line.clone(),
        _ => words
            .iter()
            .map(|w| format!("'{}'", w.replace('\'', "'\\''")))
            .collect::<Vec<_>>()
            .join(" "),
    }
}

fn parse_envs(envs: &[String]) -> Result<BTreeMap<String, String>, FlameError> {
    envs.iter()
        .map(|e| match e.split_once('=') {
            Some((name, value)) if !name.is_empty() => Ok((name.to_string(), value.to_string())),
            _ => Err(FlameError::InvalidConfig(format!(
                "invalid --env '{e}': expected NAME=VALUE"
            ))),
        })
        .collect()
}

fn read_stdin() -> Result<Vec<u8>, FlameError> {
    let mut data = Vec::new();
    std::io::stdin()
        .read_to_end(&mut data)
        .map_err(|e| FlameError::Internal(format!("read stdin: {e}")))?;
    Ok(data)
}

fn write_all(mut stream: impl Write, bytes: &[u8]) {
    // A closed pipe (`| head`) is not an error worth reporting.
    let _ = stream.write_all(bytes).and_then(|()| stream.flush());
}

/// The low eight bits, as the shell reports them.
fn exit_status(code: i32) -> u8 {
    (code & 0xff) as u8
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn one_word_is_a_command_line_several_are_quoted() {
        assert_eq!(shell_line(&["ls | wc -l".to_string()]), "ls | wc -l");
        let words = ["echo", "it's", "a b"].map(String::from);
        assert_eq!(shell_line(&words), r"'echo' 'it'\''s' 'a b'");
    }

    #[test]
    fn envs_need_a_name_and_an_equals_sign() {
        let envs = parse_envs(&["A=1".to_string(), "B=x=y".to_string(), "C=".to_string()]).unwrap();
        assert_eq!(envs["A"], "1");
        assert_eq!(envs["B"], "x=y");
        assert_eq!(envs["C"], "");
        for bad in ["A", "=1", ""] {
            assert!(parse_envs(&[bad.to_string()]).is_err(), "{bad}");
        }
    }

    #[test]
    fn exit_codes_wrap_like_a_shell() {
        assert_eq!(exit_status(3), 3);
        assert_eq!(exit_status(124), 124);
        assert_eq!(exit_status(256), 0);
    }
}
