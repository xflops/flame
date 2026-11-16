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

use super::super::*;

#[test]
fn defaults_and_request_total_do_not_change_selection() {
    let page = paginate(vec![1, 2, 3], Some(Paging::new().with_total(999))).unwrap();
    assert_eq!(page.items, [1, 2, 3]);
    assert_eq!(page.paging, Paging::new().with_total(3));
}

#[test]
fn internal_queries_are_complete_and_pages_keep_exact_totals() {
    assert_eq!(
        paginate((0..150).collect::<Vec<_>>(), None).unwrap().len(),
        150
    );
    let first = paginate(
        (0..150).collect::<Vec<_>>(),
        Some(Paging::new().with_limit(1).unwrap()),
    )
    .unwrap();
    assert_eq!(first.items, [0]);
    assert_eq!(first.paging.number(), 1);
    assert_eq!(first.paging.total(), Some(150));
    let paging = Paging::new().with_limit(2).unwrap().with_number(2).unwrap();
    let page = paginate(vec![1, 2, 3, 4, 5], Some(paging)).unwrap();
    assert_eq!(page.items, [3, 4]);
    assert_eq!(page.paging.total(), Some(5));
    let empty = paginate(vec![1], Some(paging)).unwrap();
    assert!(empty.is_empty());
    assert_eq!(empty.paging.total(), Some(1));
}

#[test]
fn invalid_and_overflowing_windows_are_rejected() {
    for (limit, number) in [(-1, 1), (1001, 1), (1, -1), (1000, i64::MAX)] {
        assert!(Paging::new()
            .with_limit(limit)
            .and_then(|paging| paging.with_number(number))
            .is_err());
    }
}

#[test]
fn paging_builders_and_accessors_apply_defaults() {
    let paging = Paging::new();
    assert_eq!(paging, Paging::default());
    assert_eq!(paging.limit(), 100);
    assert_eq!(paging.offset(), 0);
    let paging = paging
        .with_limit(3)
        .unwrap()
        .with_number(4)
        .unwrap()
        .with_total(20);
    assert_eq!(paging.limit(), 3);
    assert_eq!(paging.offset(), 9);
    assert_eq!(paging.total(), Some(20));
    assert!(Paging::new().with_limit(-1).is_err());
    assert!(Paging::new().with_limit(1001).is_err());
}

#[test]
fn changing_either_bound_rejects_offset_overflow() {
    let number = (usize::MAX as u64).min(i64::MAX as u64) as i64;
    let paging = Paging::new()
        .with_limit(1)
        .unwrap()
        .with_number(number)
        .unwrap();
    assert!(paging.with_limit(1000).is_err());
    assert_eq!(paging.offset(), (number - 1) as usize);
    let wire = rpc::Paging {
        limit: 1,
        number,
        total: Some(20),
    };
    assert_eq!(Paging::try_from(wire).unwrap().total(), Some(20));
    assert_eq!(Paging::try_from(None).unwrap(), Paging::new());
    assert_eq!(
        Paging::new().with_limit(0).unwrap().with_number(0).unwrap(),
        Paging::new()
    );
    let all = paginate((0..1500).collect::<Vec<_>>(), None).unwrap();
    assert_eq!(all.paging.limit(), 1500);
    assert_eq!(all.paging.offset(), 0);
    assert_eq!(all.paging.total(), Some(1500));
}
