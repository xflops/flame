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
    use super::super::events_path;

    #[test]
    fn test_stores_have_isolated_stable_event_directories() {
        let root = tempfile::TempDir::new().unwrap();
        let first = events_path("sqlite:///first.db", Some(root.path().as_os_str()));
        assert_eq!(
            first,
            events_path("sqlite:///first.db", Some(root.path().as_os_str()))
        );
        assert_ne!(
            first,
            events_path("sqlite:///second.db", Some(root.path().as_os_str()))
        );
        assert!(std::path::Path::new(&first).starts_with(root.path().join("events")));
    }

    #[test]
    fn test_default_events_dir() {
        assert_eq!(events_path("any", None), "events");
    }
}
