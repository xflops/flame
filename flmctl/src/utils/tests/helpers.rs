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

fn args(ids: &[&str]) -> Vec<String> {
    ids.iter().map(|id| id.to_string()).collect()
}

#[test]
fn ids_are_trimmed() {
    let a = args(&["id-1", " id-2 "]);
    assert_eq!(trim_ids(&a).unwrap(), ["id-1", "id-2"]);
}

#[test]
fn blank_id_is_rejected() {
    assert!(trim_ids(&args(&[""])).is_err());
    assert!(trim_ids(&args(&["id-1", " "])).is_err());
}

#[test]
fn format_resreq_none_renders_dash() {
    assert_eq!(format_resreq(&None), "-");
}

#[test]
fn format_resreq_typical_values() {
    let rr = ResourceRequirement {
        cpu: 4,
        memory: 16 * 1024 * 1024 * 1024,
        gpu: 2,
    };
    assert_eq!(format_resreq(&Some(rr)), "cpu=4,mem=16Gi,gpu=2");
}

#[test]
fn format_resreq_zero_values() {
    let rr = ResourceRequirement {
        cpu: 0,
        memory: 0,
        gpu: 0,
    };
    assert_eq!(format_resreq(&Some(rr)), "cpu=0,mem=0B,gpu=0");
}

#[test]
fn format_duration_seconds() {
    assert_eq!(format_duration(Duration::seconds(60)), "1m");
}

#[test]
fn format_duration_compound_values() {
    assert_eq!(
        format_duration(Duration::seconds(86_400 + 3_600 + 120 + 3)),
        "1d 1h 2m 3s"
    );
}

#[test]
fn format_duration_zero() {
    assert_eq!(format_duration(Duration::zero()), "0s");
}

#[test]
fn format_duration_negative() {
    assert_eq!(format_duration(Duration::seconds(-65)), "-1m 5s");
}

#[test]
fn format_optional_duration_some() {
    assert_eq!(format_optional_duration(&Some(Duration::seconds(60))), "1m");
}

#[test]
fn format_optional_duration_none_renders_dash() {
    assert_eq!(format_optional_duration(&None), "-");
}
