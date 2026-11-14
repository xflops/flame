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
use tempfile::TempDir;

fn executors(yaml: &str) -> Result<FlameExecutors, FlameError> {
    FlameExecutors::try_from(serde_yaml::from_str::<FlameExecutorsYaml>(yaml).unwrap())
}

#[test]
fn executor_shims_default_to_the_host_shim() {
    let executors = executors("devices: []").unwrap();
    assert_eq!(executors.shims, [Shim::Host]);
    assert_eq!(executors.default_shim(), Shim::Host);
    assert_eq!(FlameExecutors::default().shims, [Shim::default()]);
}

#[test]
fn executor_shims_with_one_shim_run_only_that_shim() {
    let executors = executors("shims: [cri]").unwrap();
    assert_eq!(executors.shims, [Shim::Cri]);
    assert_eq!(executors.default_shim(), Shim::Cri);
}

#[test]
fn executor_shims_list_every_supported_shim_and_the_first_is_the_default() {
    let executors = executors("shims: [cri, host]").unwrap();
    assert_eq!(executors.shims, [Shim::Cri, Shim::Host]);
    assert_eq!(executors.default_shim(), Shim::Cri);
}

#[test]
fn executor_keeps_a_supported_shim_and_falls_back_to_the_default() {
    let both = executors("shims: [cri, host]").unwrap();
    assert_eq!(both.shim_for(Shim::Host), Shim::Host);
    assert_eq!(both.shim_for(Shim::Cri), Shim::Cri);

    let cri = executors("shims: [cri]").unwrap();
    assert_eq!(cri.shim_for(Shim::Host), Shim::Cri);
    assert_eq!(cri.shim_for(Shim::Cri), Shim::Cri);
}

#[test]
fn executor_shims_reject_bad_lists() {
    assert!(executors("shims: []").is_err());
    assert!(executors("shims: [host, host]").is_err());
    assert!(executors("shims: [nope]").is_err());
}

#[test]
fn executor_system_resource_defaults_to_none_and_parses() {
    assert_eq!(
        executors("devices: []").unwrap().system_resource,
        ResourceRequirement::default()
    );
    let configured = executors("system_resource: cpu=2,mem=4g").unwrap();
    assert_eq!(
        configured.system_resource,
        ResourceRequirement::from("cpu=2,mem=4g")
    );
    assert!(executors("system_resource: foo=1").is_err());
}

#[test]
fn executor_devices_default_to_gpu() {
    let yaml: FlameExecutorsYaml = serde_yaml::from_str("shims: [host]").unwrap();
    let executors = FlameExecutors::try_from(yaml).unwrap();
    assert_eq!(executors.devices, ["gpu"]);
    assert_eq!(FlameExecutors::default().devices, ["gpu"]);
    assert_eq!(FlameCluster::default().executors.devices, ["gpu"]);
}

#[test]
fn executor_devices_explicit_empty_disables_managers() {
    let yaml: FlameExecutorsYaml = serde_yaml::from_str("devices: []").unwrap();
    assert!(FlameExecutors::try_from(yaml).unwrap().devices.is_empty());
}

#[test]
fn executor_devices_preserve_configured_names() {
    let yaml: FlameExecutorsYaml = serde_yaml::from_str("devices: [gpu, tpu]").unwrap();
    assert_eq!(
        FlameExecutors::try_from(yaml).unwrap().devices,
        ["gpu", "tpu"]
    );
}

#[test]
fn executor_devices_reject_duplicates() {
    let yaml: FlameExecutorsYaml = serde_yaml::from_str("devices: [gpu, gpu]").unwrap();
    let error = FlameExecutors::try_from(yaml).unwrap_err();
    assert!(matches!(error, FlameError::InvalidConfig(_)));
    assert!(error.to_string().contains("duplicate device manager <gpu>"));
}

#[test]
fn test_executor_limit_defaults() {
    let limits = FlameLimits::default();
    assert_eq!(limits.max_sessions, None);
    assert_eq!(limits.application.max_executors, None);
    assert_eq!(limits.session.max_executors, None);
    assert_eq!(limits.node.max_executors, DEFAULT_MAX_EXECUTORS_PER_NODE);
}

#[test]
fn test_executor_limits_partial_scopes() {
    let yaml: FlameLimitsYaml =
        serde_yaml::from_str("max_sessions: 50\napplication:\n  max_executors: 12\n").unwrap();
    let limits = FlameLimits::from(yaml);
    assert_eq!(limits.max_sessions, Some(50));
    assert_eq!(limits.application.max_executors, Some(12));
    assert_eq!(limits.session.max_executors, None);
    assert_eq!(limits.node.max_executors, DEFAULT_MAX_EXECUTORS_PER_NODE);

    let yaml: FlameLimitsYaml =
        serde_yaml::from_str("session:\n  max_executors: 3\nnode:\n  max_executors: 8\n").unwrap();
    let limits = FlameLimits::from(yaml);
    assert_eq!(limits.application.max_executors, None);
    assert_eq!(limits.session.max_executors, Some(3));
    assert_eq!(limits.node.max_executors, 8);
}

#[test]
fn test_executor_limits_preserve_zero() {
    let yaml: FlameLimitsYaml = serde_yaml::from_str(
            "application:\n  max_executors: 0\nsession:\n  max_executors: 0\nnode:\n  max_executors: 0\n",
        )
        .unwrap();
    let limits = FlameLimits::from(yaml);
    assert_eq!(limits.application.max_executors, Some(0));
    assert_eq!(limits.session.max_executors, Some(0));
    assert_eq!(limits.node.max_executors, 0);

    let yaml: FlameLimitsYaml = serde_yaml::from_str("node: {}\n").unwrap();
    assert_eq!(
        FlameLimits::from(yaml).node.max_executors,
        DEFAULT_MAX_EXECUTORS_PER_NODE
    );
}

#[test]
fn cluster_advertise_parses_and_defaults() -> Result<(), FlameError> {
    assert_eq!(FlameCluster::default().advertise, None);
    for (field, expected) in [
        ("", None),
        ("advertise: \"\"", None),
        ("advertise: 127.0.0.1", Some("127.0.0.1")),
        ("advertise: '^eth[01]$'", Some("^eth[01]$")),
    ] {
        let yaml: FlameClusterYaml = serde_yaml::from_str(&format!(
            "name: flame\nendpoint: http://service:8080\n{field}\n"
        ))
        .unwrap();
        let cluster = FlameCluster::try_from(yaml)?;
        assert_eq!(cluster.advertise.as_deref(), expected);
        assert_eq!(cluster.endpoint, "http://service:8080");
    }
    Ok(())
}

#[test]
fn cluster_storage_defaults_to_filesystem() {
    assert_eq!(FlameCluster::default().storage, "fs://data");
    let yaml: FlameClusterYaml =
        serde_yaml::from_str("name: flame\nendpoint: http://127.0.0.1:8080\n").unwrap();
    assert_eq!(FlameCluster::try_from(yaml).unwrap().storage, "fs://data");
}

#[test]
fn test_flame_context_from_file() -> Result<(), FlameError> {
    // Executor limits explicitly identify their scope.
    let context_string = r#"---
cluster:
  name: flame
  endpoint: "http://flame-session-manager:8080"
  resreq: "cpu=1,mem=1g"
  policies:
    - priority
    - drf
  storage: fs://data
  executors:
    shims: [host]
  limits:
    node:
      max_executors: 10
        "#;

    let tmp_dir = TempDir::new().unwrap();
    let tmp_file = tmp_dir.path().join("flame-cluster.yaml");

    fs::write(&tmp_file, context_string).map_err(|e| FlameError::Internal(e.to_string()))?;

    let ctx = FlameClusterContext::from_file(Some(tmp_file.to_string_lossy().to_string()))
        .map_err(|e| FlameError::Internal(e.to_string()))?;
    assert_eq!(ctx.cluster.name, "flame");
    assert_eq!(ctx.cluster.endpoint, "http://flame-session-manager:8080");
    assert_eq!(
        ctx.cluster.resreq,
        Some(ResourceRequirement::from("cpu=1,mem=1g"))
    );
    assert_eq!(ctx.cluster.policies, vec!["priority", "drf"]);
    assert_eq!(ctx.cluster.storage, "fs://data");
    assert_eq!(ctx.cluster.executors.shims, [Shim::Host]);
    assert_eq!(ctx.cluster.limits.node.max_executors, 10);

    Ok(())
}

#[test]
fn test_flame_context_with_session_recovery_retry_limits() -> Result<(), FlameError> {
    let context_string = r#"---
cluster:
  name: flame
  endpoint: "http://flame-session-manager:8080"
  recovery:
    session:
      retry_limits: 2
        "#;

    let tmp_dir = TempDir::new().unwrap();
    let tmp_file = tmp_dir.path().join("flame-cluster.yaml");

    fs::write(&tmp_file, context_string).map_err(|e| FlameError::Internal(e.to_string()))?;

    let ctx = FlameClusterContext::from_file(Some(tmp_file.to_string_lossy().to_string()))?;

    assert_eq!(ctx.cluster.recovery.session.retry_limits, 2);

    Ok(())
}

#[test]
fn test_flame_context_with_filesystem_object_storage() -> Result<(), FlameError> {
    let context_string = r#"---
cluster:
  name: flame
  endpoint: "http://flame-session-manager:8080"
  executors:
    shims: [host]
cache:
  endpoint: "grpc://127.0.0.1:9090"
  storage: "/var/lib/flame/cache"
        "#;

    let tmp_dir = TempDir::new().unwrap();
    let tmp_file = tmp_dir.path().join("flame-cluster.yaml");

    fs::write(&tmp_file, context_string).map_err(|e| FlameError::Internal(e.to_string()))?;

    let ctx = FlameClusterContext::from_file(Some(tmp_file.to_string_lossy().to_string()))?;

    assert!(ctx.cache.is_some());
    let cache = ctx.cache.unwrap();
    assert_eq!(cache.endpoint, "grpc://127.0.0.1:9090");
    assert_eq!(cache.storage, Some("/var/lib/flame/cache".to_string()));

    Ok(())
}

fn cache_context(cache_yaml: &str) -> Result<FlameCache, FlameError> {
    let context_string = format!(
            "---\ncluster:\n  name: flame\n  endpoint: \"http://flame-session-manager:8080\"\ncache:\n{}",
            cache_yaml
        );
    let tmp_dir = TempDir::new().unwrap();
    let tmp_file = tmp_dir.path().join("flame-cluster.yaml");
    fs::write(&tmp_file, context_string).map_err(|e| FlameError::Internal(e.to_string()))?;
    let ctx = FlameClusterContext::from_file(Some(tmp_file.to_string_lossy().to_string()))?;
    Ok(ctx.cache.unwrap())
}

/// Omitted YAML and direct configuration construction use the same part-cache budget.
#[test]
fn test_flame_context_cache_pool_default() -> Result<(), FlameError> {
    let cache = cache_context("  endpoint: \"grpc://127.0.0.1:9090\"\n")?;
    assert_eq!(cache.pool.size, 256 * 1024 * 1024);
    assert_eq!(cache.pool.policy, FlameCachePolicy::Lru);
    assert_eq!(FlameCache::default().pool, cache.pool);
    Ok(())
}

/// Parse human-readable sizes and preserve zero as the cache-disable setting.
#[test]
fn test_flame_context_cache_pool_sizes_and_disabled() -> Result<(), FlameError> {
    for (value, expected) in [
        ("64M", 64 * 1024 * 1024),
        ("1Gi", 1024 * 1024 * 1024),
        ("4096", 4096),
        ("0", 0),
    ] {
        let cache = cache_context(&format!("  pool:\n    size: \"{value}\"\n"))?;
        assert_eq!(cache.pool.size, expected, "pool.size={value}");
        assert_eq!(cache.pool.policy, FlameCachePolicy::Lru);
    }
    Ok(())
}

/// Reject malformed cache budgets instead of silently disabling memory bounds.
#[test]
fn test_flame_context_cache_pool_invalid() {
    for value in ["", "invalid", "-1M", "1X"] {
        assert!(
            cache_context(&format!("  pool:\n    size: \"{value}\"\n")).is_err(),
            "pool.size={value} should be rejected"
        );
    }
}

/// Empty and partial mappings default each omitted pool property independently.
#[test]
fn test_flame_context_cache_pool_partial_configuration() -> Result<(), FlameError> {
    for yaml in ["  pool: {}\n", "  pool:\n    policy: lru\n"] {
        let cache = cache_context(yaml)?;
        assert_eq!(cache.pool, FlameCachePool::default());
    }
    let cache = cache_context("  pool:\n    size: 32M\n    policy: lru\n")?;
    assert_eq!(cache.pool.size, 32 * 1024 * 1024);
    assert_eq!(cache.pool.policy, FlameCachePolicy::Lru);
    Ok(())
}

/// Unsupported policies, unknown fields, and the previous scalar shape are rejected.
#[test]
fn test_flame_context_cache_pool_rejects_invalid_configuration() {
    for yaml in [
        "  pool:\n    policy: fifo\n",
        "  pool:\n    policy: LRU\n",
        "  pool:\n    size: 32M\n    enabled: true\n",
        "  pool: 256M\n",
    ] {
        assert!(
            cache_context(yaml).is_err(),
            "invalid pool accepted: {yaml}"
        );
    }
}

#[test]
fn test_flame_context_cache_advertise_default() -> Result<(), FlameError> {
    let cache = cache_context("  endpoint: \"grpc://127.0.0.1:9090\"\n")?;
    assert_eq!(cache.advertise, None);
    // An empty value counts as unset.
    let cache = cache_context("  advertise: \"\"\n")?;
    assert_eq!(cache.advertise, None);
    Ok(())
}

#[test]
fn test_flame_context_cache_advertise() -> Result<(), FlameError> {
    let cache = cache_context("  advertise: \"grpc://cache-0.cache.flame.svc:9090\"\n")?;
    assert_eq!(
        cache.advertise.as_deref(),
        Some("grpc://cache-0.cache.flame.svc:9090")
    );
    Ok(())
}

#[test]
fn test_flame_context_cache_ignores_legacy_network_interface() -> Result<(), FlameError> {
    // Old configs that still set `network_interface` keep loading.
    let cache =
        cache_context("  endpoint: \"grpc://127.0.0.1:9090\"\n  network_interface: \"eth0\"\n")?;
    assert_eq!(cache.endpoint, "grpc://127.0.0.1:9090");
    assert_eq!(cache.advertise, None);
    Ok(())
}

#[test]
fn test_flame_context_with_cache_gc() -> Result<(), FlameError> {
    let context_string = r#"---
cluster:
  name: flame
  endpoint: "http://flame-session-manager:8080"
cache:
  gc:
    interval: 30s
        "#;

    let tmp_dir = TempDir::new().unwrap();
    let tmp_file = tmp_dir.path().join("flame-cluster.yaml");
    fs::write(&tmp_file, context_string).map_err(|e| FlameError::Internal(e.to_string()))?;

    let ctx = FlameClusterContext::from_file(Some(tmp_file.to_string_lossy().to_string()))?;
    assert_eq!(
        ctx.cache.unwrap().gc.interval,
        std::time::Duration::from_secs(30)
    );

    Ok(())
}

#[test]
fn test_flame_context_defaults_cache_gc_without_interval() {
    let context_string = r#"---
cluster:
  name: flame
  endpoint: "http://flame-session-manager:8080"
cache:
  gc: {}
        "#;

    let tmp_dir = TempDir::new().unwrap();
    let tmp_file = tmp_dir.path().join("flame-cluster.yaml");
    fs::write(&tmp_file, context_string).unwrap();

    let ctx = FlameClusterContext::from_file(Some(tmp_file.to_string_lossy().to_string())).unwrap();
    assert_eq!(
        ctx.cache.unwrap().gc.interval,
        std::time::Duration::from_secs(DEFAULT_CACHE_GC_INTERVAL_SECS)
    );
}

#[test]
fn test_flame_context_rejects_invalid_cache_gc_interval() {
    for interval in ["0s", "invalid"] {
        let context_string = format!(
            r#"---
cluster:
  name: flame
  endpoint: "http://flame-session-manager:8080"
cache:
  gc:
    interval: {interval}
        "#
        );

        let tmp_dir = TempDir::new().unwrap();
        let tmp_file = tmp_dir.path().join("flame-cluster.yaml");
        fs::write(&tmp_file, context_string).unwrap();

        assert!(
            FlameClusterContext::from_file(Some(tmp_file.to_string_lossy().to_string())).is_err(),
            "interval {interval} should be rejected"
        );
    }
}

#[test]
fn test_flame_context_rejects_unknown_cache_gc_fields() {
    let context_string = r#"---
cluster:
  name: flame
  endpoint: "http://flame-session-manager:8080"
cache:
  gc:
    interval: 60s
    enabled: true
        "#;

    let tmp_dir = TempDir::new().unwrap();
    let tmp_file = tmp_dir.path().join("flame-cluster.yaml");
    fs::write(&tmp_file, context_string).unwrap();

    assert!(FlameClusterContext::from_file(Some(tmp_file.to_string_lossy().to_string())).is_err());
}

// ============================================================
// Tests for pool config parsing with units (using bytesize)
// ============================================================

#[test]
fn test_parse_memory_size_gigabytes_uppercase() {
    // Test uppercase G unit
    assert_eq!(parse_memory_size("1G").unwrap(), 1024 * 1024 * 1024);
    assert_eq!(parse_memory_size("2G").unwrap(), 2 * 1024 * 1024 * 1024);
    assert_eq!(parse_memory_size("4G").unwrap(), 4 * 1024 * 1024 * 1024);
}

#[test]
fn test_parse_memory_size_gigabytes_lowercase() {
    // Test lowercase g unit
    assert_eq!(parse_memory_size("1g").unwrap(), 1024 * 1024 * 1024);
    assert_eq!(parse_memory_size("2g").unwrap(), 2 * 1024 * 1024 * 1024);
}

#[test]
fn test_parse_memory_size_gigabytes_with_b() {
    // Test GB unit
    assert_eq!(parse_memory_size("1GB").unwrap(), 1024 * 1024 * 1024);
    assert_eq!(parse_memory_size("1gb").unwrap(), 1024 * 1024 * 1024);
    assert_eq!(parse_memory_size("2GB").unwrap(), 2 * 1024 * 1024 * 1024);
}

#[test]
fn test_parse_memory_size_binary_suffix_without_b() {
    assert_eq!(parse_memory_size("1Gi").unwrap(), 1024 * 1024 * 1024);
    assert_eq!(parse_memory_size("512Mi").unwrap(), 512 * 1024 * 1024);
    assert_eq!(parse_memory_size("1024Ki").unwrap(), 1024 * 1024);
    assert_eq!(parse_memory_size("2Ti").unwrap(), 2 * 1024_u64.pow(4));
    assert_eq!(parse_memory_size("1Pi").unwrap(), 1024_u64.pow(5));
}

#[test]
fn test_parse_memory_size_terabytes_and_petabytes() {
    assert_eq!(parse_memory_size("1T").unwrap(), 1024_u64.pow(4));
    assert_eq!(parse_memory_size("1TB").unwrap(), 1024_u64.pow(4));
    assert_eq!(parse_memory_size("2P").unwrap(), 2 * 1024_u64.pow(5));
    assert_eq!(parse_memory_size("2PB").unwrap(), 2 * 1024_u64.pow(5));
}

#[test]
fn test_parse_memory_size_megabytes_uppercase() {
    // Test uppercase M unit
    assert_eq!(parse_memory_size("512M").unwrap(), 512 * 1024 * 1024);
    assert_eq!(parse_memory_size("1024M").unwrap(), 1024 * 1024 * 1024);
    assert_eq!(parse_memory_size("256M").unwrap(), 256 * 1024 * 1024);
}

#[test]
fn test_parse_memory_size_megabytes_lowercase() {
    // Test lowercase m unit
    assert_eq!(parse_memory_size("512m").unwrap(), 512 * 1024 * 1024);
    assert_eq!(parse_memory_size("1024m").unwrap(), 1024 * 1024 * 1024);
}

#[test]
fn test_parse_memory_size_megabytes_with_b() {
    // Test MB unit
    assert_eq!(parse_memory_size("512MB").unwrap(), 512 * 1024 * 1024);
    assert_eq!(parse_memory_size("512mb").unwrap(), 512 * 1024 * 1024);
}

#[test]
fn test_parse_memory_size_kilobytes_uppercase() {
    // Test uppercase K unit
    assert_eq!(parse_memory_size("1024K").unwrap(), 1024 * 1024);
    assert_eq!(parse_memory_size("2048K").unwrap(), 2048 * 1024);
}

#[test]
fn test_parse_memory_size_kilobytes_lowercase() {
    // Test lowercase k unit
    assert_eq!(parse_memory_size("1024k").unwrap(), 1024 * 1024);
    assert_eq!(parse_memory_size("2048k").unwrap(), 2048 * 1024);
}

#[test]
fn test_parse_memory_size_kilobytes_with_b() {
    // Test KB unit
    assert_eq!(parse_memory_size("1024KB").unwrap(), 1024 * 1024);
    assert_eq!(parse_memory_size("1024kb").unwrap(), 1024 * 1024);
}

#[test]
fn test_parse_memory_size_bytes() {
    // Test B unit and no unit (bytes)
    assert_eq!(parse_memory_size("512B").unwrap(), 512);
    assert_eq!(parse_memory_size("512b").unwrap(), 512);
    assert_eq!(parse_memory_size("1024").unwrap(), 1024);
}

#[test]
fn test_parse_memory_size_with_whitespace() {
    // Test with leading/trailing whitespace
    assert_eq!(parse_memory_size("  512M  ").unwrap(), 512 * 1024 * 1024);
    assert_eq!(parse_memory_size(" 1G ").unwrap(), 1024 * 1024 * 1024);
}

#[test]
fn test_parse_memory_size_invalid_unit() {
    // Test invalid unit
    let result = parse_memory_size("512X");
    assert!(result.is_err());
    let err = result.unwrap_err();
    assert!(matches!(err, FlameError::InvalidConfig(_)));
}

#[test]
fn test_parse_memory_size_invalid_number() {
    // Test invalid number
    let result = parse_memory_size("abcM");
    assert!(result.is_err());
    let err = result.unwrap_err();
    assert!(matches!(err, FlameError::InvalidConfig(_)));
}

#[test]
fn test_parse_memory_size_empty_string() {
    // Test empty string
    let result = parse_memory_size("");
    assert!(result.is_err());
    let err = result.unwrap_err();
    assert!(matches!(err, FlameError::InvalidConfig(_)));
}

#[test]
fn test_parse_memory_size_only_whitespace() {
    // Test only whitespace
    let result = parse_memory_size("   ");
    assert!(result.is_err());
}

#[test]
fn test_parse_memory_size_zero() {
    // Test zero value
    assert_eq!(parse_memory_size("0").unwrap(), 0);
    assert_eq!(parse_memory_size("0M").unwrap(), 0);
    assert_eq!(parse_memory_size("0G").unwrap(), 0);
}

#[test]
fn test_parse_memory_size_large_values() {
    // Test large values
    assert_eq!(parse_memory_size("100G").unwrap(), 100 * 1024 * 1024 * 1024);
}

// ============================================================
// Resource requirement configuration tests
// ============================================================

// ============================================================
// Tests for cluster.resreq (cluster-wide default
// resreq applied server-side when a session spec supplies neither
// `slots` nor `resreq`).
// ============================================================

#[test]
fn cluster_yaml_with_resreq_parses() -> Result<(), FlameError> {
    let context_string = r#"---
cluster:
  name: flame
  endpoint: "http://flame-session-manager:8080"
  resreq: "cpu=2,mem=4g,gpu=1"
  executors:
    shims: [host]
        "#;

    let tmp_dir = TempDir::new().unwrap();
    let tmp_file = tmp_dir.path().join("flame-cluster.yaml");
    fs::write(&tmp_file, context_string).map_err(|e| FlameError::Internal(e.to_string()))?;

    let ctx = FlameClusterContext::from_file(Some(tmp_file.to_string_lossy().to_string()))?;

    let resreq = ctx.cluster.resreq.expect("resreq should be parsed");
    assert_eq!(resreq.cpu, 2);
    assert_eq!(resreq.memory, 4 * 1024 * 1024 * 1024);
    assert_eq!(resreq.gpu, 1);

    Ok(())
}

#[test]
fn cluster_yaml_without_resreq_is_none() -> Result<(), FlameError> {
    let context_string = r#"---
cluster:
  name: flame
  endpoint: "http://flame-session-manager:8080"
  executors:
    shims: [host]
        "#;

    let tmp_dir = TempDir::new().unwrap();
    let tmp_file = tmp_dir.path().join("flame-cluster.yaml");
    fs::write(&tmp_file, context_string).map_err(|e| FlameError::Internal(e.to_string()))?;

    let ctx = FlameClusterContext::from_file(Some(tmp_file.to_string_lossy().to_string()))?;

    assert!(ctx.cluster.resreq.is_none());

    Ok(())
}

#[test]
fn cluster_yaml_with_invalid_resreq_fails() {
    let context_string = r#"---
cluster:
  name: flame
  endpoint: "http://flame-session-manager:8080"
  resreq: "cpu=two,mem=4g"
  executors:
    shims: [host]
        "#;

    let tmp_dir = TempDir::new().unwrap();
    let tmp_file = tmp_dir.path().join("flame-cluster.yaml");
    fs::write(&tmp_file, context_string).unwrap();

    let result = FlameClusterContext::from_file(Some(tmp_file.to_string_lossy().to_string()));
    assert!(result.is_err());
}
