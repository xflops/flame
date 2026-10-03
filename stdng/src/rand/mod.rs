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

use rand::{Rng, distr::Alphanumeric};

pub fn short_name() -> String {
    rand::rng()
        .sample_iter(Alphanumeric)
        .take(6)
        .map(char::from)
        .map(|character| character.to_ascii_lowercase())
        .collect()
}

#[cfg(test)]
mod tests {
    #[test]
    fn short_name_uses_lowercase_alphanumeric_characters() {
        for _ in 0..100 {
            let name = super::short_name();
            assert_eq!(name.len(), 6);
            assert!(
                name.chars()
                    .all(|ch| ch.is_ascii_lowercase() || ch.is_ascii_digit())
            );
        }
    }
}
