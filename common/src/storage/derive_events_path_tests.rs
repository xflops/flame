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

#[cfg(test)]
mod tests {
    use super::super::derive_events_path;
    use tempfile::TempDir;

    #[test]
    fn test_flame_test_dir_set() {
        let temp_dir = TempDir::new().unwrap();
        let temp_path = temp_dir.path().to_string_lossy().to_string();
        let expected = format!("{}/events", temp_path);

        std::env::set_var("FLAME_TEST_DIR", &temp_path);
        assert_eq!(derive_events_path("any"), expected);
        std::env::remove_var("FLAME_TEST_DIR");
    }

    #[test]
    fn test_default_events_dir() {
        if std::env::var("FLAME_TEST_DIR").is_ok() {
            return;
        }
        assert_eq!(derive_events_path("any"), "events");
    }
}
