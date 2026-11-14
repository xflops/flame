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

use super::*;

fn close_ids(args: &[&str]) -> Vec<String> {
    let cli = Cli::try_parse_from(std::iter::once("flmctl").chain(args.iter().copied())).unwrap();
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
    let cli = Cli::try_parse_from(std::iter::once("flmctl").chain(args.iter().copied())).unwrap();
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
