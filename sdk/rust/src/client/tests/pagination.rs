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

#[test]
fn page_requires_effective_bounds_and_total() {
    for (limit, number, total) in [
        (100, 1, None),
        (0, 1, Some(0)),
        (1001, 1, Some(0)),
        (100, 0, Some(0)),
        (100, -1, Some(0)),
    ] {
        assert!(Page::<()>::new(
            Vec::new(),
            Some(rpc::Paging {
                limit,
                number,
                total
            }),
        )
        .is_err());
    }
    assert!(Page::<()>::new(Vec::new(), None).is_err());
}

/// A valid server offset can have an exclusive page end beyond i64::MAX.
#[test]
fn empty_page_beyond_signed_integer_boundary_decodes() {
    let page = Page::<()>::new(
        Vec::new(),
        Some(rpc::Paging {
            limit: 2,
            number: 1_i64 << 62,
            total: Some(0),
        }),
    )
    .unwrap();
    assert!(page.items.is_empty());
    assert_eq!(page.paging.number, 1_i64 << 62);
}

#[test]
fn page_serialization_exposes_counts_and_no_discarded_envelope() {
    let page = Page {
        items: vec![1],
        paging: Paging {
            limit: 2,
            number: 1,
            total: Some(1),
        },
    };
    let json = serde_json::to_value(&page).unwrap();
    assert_eq!(json["items"][0], 1);
    assert_eq!(json["paging"]["total"], 1);
}
