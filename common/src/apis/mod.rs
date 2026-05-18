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

mod from_rpc;
mod session;
mod to_rpc;
mod types;

pub use types::*;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_resreq_from_string() {
        let cases = vec![
            ("cpu=1,mem=256", (1, 256)),
            ("cpu=1,mem=1k", (1, 1024)),
            ("cpu=1,memory=1m", (1, 1024 * 1024)),
            ("cpu=1,memory=1g", (1, 1024 * 1024 * 1024)),
        ];

        for (input, expected) in cases {
            let resreq = ResourceRequirement::from(input);
            assert_eq!(resreq.cpu, expected.0);
            assert_eq!(resreq.memory, expected.1);
        }
    }

    #[test]
    fn test_resreq_parse_rejects_malformed_input() {
        for input in ["cpu=abc", "mem=bogus", "gpu=", "foo=1", "cpu=1,"] {
            assert!(
                ResourceRequirement::parse(input).is_err(),
                "expected invalid resreq to fail: {input}"
            );
        }
    }

    #[test]
    fn test_shim_default() {
        let shim = Shim::default();
        assert_eq!(shim, Shim::Host);
    }

    #[test]
    fn test_shim_from_string() {
        assert_eq!(Shim::try_from("host".to_string()).unwrap(), Shim::Host);
        assert_eq!(Shim::try_from("Host".to_string()).unwrap(), Shim::Host);
        assert_eq!(Shim::try_from("HOST".to_string()).unwrap(), Shim::Host);
        assert_eq!(Shim::try_from("wasm".to_string()).unwrap(), Shim::Wasm);
        assert_eq!(Shim::try_from("Wasm".to_string()).unwrap(), Shim::Wasm);
        assert_eq!(Shim::try_from("WASM".to_string()).unwrap(), Shim::Wasm);
        assert!(Shim::try_from("invalid".to_string()).is_err());
    }

    #[test]
    fn test_application_attributes_default_shim() {
        let attrs = ApplicationAttributes::default();
        assert_eq!(attrs.shim, Shim::Host);
    }

    mod validate_application_name {
        use super::*;

        #[test]
        fn valid_names() {
            assert!(validate_application_name("my-app").is_ok());
            assert!(validate_application_name("my_app").is_ok());
            assert!(validate_application_name("my.app").is_ok());
            assert!(validate_application_name("myapp123").is_ok());
            assert!(validate_application_name("MyApp").is_ok());
            assert!(validate_application_name("a").is_ok());
        }

        #[test]
        fn empty_name() {
            assert!(validate_application_name("").is_err());
        }

        #[test]
        fn path_traversal() {
            assert!(validate_application_name("..").is_err());
            assert!(validate_application_name("../etc").is_err());
            assert!(validate_application_name("app/../../etc").is_err());
            assert!(validate_application_name("app\\..\\etc").is_err());
        }

        #[test]
        fn starts_with_dot_or_dash() {
            assert!(validate_application_name(".hidden").is_err());
            assert!(validate_application_name("-invalid").is_err());
        }

        #[test]
        fn invalid_characters() {
            assert!(validate_application_name("app name").is_err());
            assert!(validate_application_name("app@name").is_err());
            assert!(validate_application_name("app#name").is_err());
            assert!(validate_application_name("app$name").is_err());
        }

        #[test]
        fn too_long() {
            let long_name = "a".repeat(254);
            assert!(validate_application_name(&long_name).is_err());
            let ok_name = "a".repeat(253);
            assert!(validate_application_name(&ok_name).is_ok());
        }
    }
}
