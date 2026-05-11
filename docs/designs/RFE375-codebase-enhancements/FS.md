# RFE375: Codebase Enhancement Plan

**Status:** Draft  
**Created:** 2026-03-17  
**Author:** Flame Team  
**Tracking Issue:** [#375](https://github.com/xflops/flame/issues/375)  

## 1. Motivation

### Background

A comprehensive codebase review was conducted across all Flame components to identify technical debt, code quality issues, and enhancement opportunities. The review analyzed:

- **Core Components:** session_manager, executor_manager, object_cache
- **CLI Tools:** flmctl, flmadm, flmexec
- **SDK:** Python SDK (flamepy), Rust SDK
- **Infrastructure:** RPC definitions, common utilities, CI/CD, documentation

### Key Findings Summary

| Category      | Critical | High | Medium | Total Issues |
| ------------- | -------- | ---- | ------ | ------------ |
| Code Safety   | 5        | 8    | 12     | 25           |
| Architecture  | 2        | 4    | 6      | 12           |
| API/RPC       | 1        | 3    | 5      | 9            |
| Testing       | 3        | 5    | 8      | 16           |
| Documentation | 1        | 6    | 10     | 17           |
| CI/CD         | 0        | 4    | 6      | 10           |

### Target

This RFE provides a structured action plan to address the identified issues in priority order, improving:

1. **Reliability** - Eliminate panic risks in production code paths
2. **Maintainability** - Reduce code duplication and improve organization
3. **Developer Experience** - Better documentation, CLI, and testing
4. **Quality Assurance** - Comprehensive test coverage and CI/CD improvements

---

## 2. Function Specification

### Phase 1: Critical Safety Fixes (Week 1)

#### P1.1: Eliminate Production `unwrap()` Panics

**Problem:** 167 `unwrap()` occurrences across 35 files can cause runtime panics.

**Priority Files (by occurrence count):**

| File                                                 | Count | Risk Level                     |
| ---------------------------------------------------- | ----- | ------------------------------ |
| `common/src/ctx.rs`                                  | 47    | High - Config parsing          |
| `common/src/storage/object.rs`                       | 21    | High - Storage ops             |
| `sdk/rust/src/client/mod.rs`                         | 16    | Medium - SDK client            |
| `common/src/storage/data.rs`                         | 13    | High - Data layer              |
| `session_manager/src/events/mod.rs`                  | 9     | Medium - Event handling        |
| `session_manager/src/scheduler/plugins/fairshare.rs` | 5     | High - Scheduler               |
| `session_manager/src/scheduler/actions/*.rs`         | 3     | Critical - Can crash scheduler |
| `executor_manager/src/executor.rs`                   | 5     | High - Proto conversion        |

**Action:**
- Replace `unwrap()` with `ok_or(FlameError::...)` or `?` operator
- Add proper error context with `map_err()`
- Use `expect("reason")` only for truly invariant conditions

**Acceptance Criteria:**
- Zero `unwrap()` in production code paths
- All error cases return proper `FlameError` variants
- Unit tests for error paths

#### P1.2: Fix CLI `todo!()` Panic

**File:** `flmctl/src/helper.rs:17`

```rust
// Current (will panic)
Commands::Helper => {
    todo!()
}

// Fixed
Commands::Helper => {
    println!("{}", args.command().render_help());
    Ok(())
}
```

**Acceptance Criteria:**
- `flmctl` without subcommand shows help instead of panicking
- Help text is useful and formatted correctly

#### P1.3: Fix WasmShim Task Failure Handling

**File:** `executor_manager/src/shims/wasm_shim.rs:131`

```rust
// Current: TODO comment, always returns Succeed
// TODO: Handle task failure

// Fixed: Proper error handling
let task_result = match output {
    Ok(output) => apis::TaskResult {
        state: apis::TaskState::Succeed,
        output: output.map(apis::TaskOutput::from),
        message: None,
    },
    Err(e) => apis::TaskResult {
        state: apis::TaskState::Failed,
        output: None,
        message: Some(e.to_string()),
    },
};
```

**Acceptance Criteria:**
- Failed WASM tasks correctly report `TaskState::Failed`
- Error messages propagate to task result

#### P1.4: Fix Executor Persistence Gap

**File:** `session_manager/src/storage/mod.rs:579`

```rust
// Current
// TODO: create executor in engine
```

**Action:** Implement executor persistence to SQLite engine.

**Acceptance Criteria:**
- Executors survive session_manager restart
- State is correctly restored on startup

---

### Phase 2: Foundation Improvements (Week 2)

#### P2.1: Add API Versioning to Proto Files

**Files:** All `.proto` files in `rpc/protos/`

**Changes:**
```protobuf
// Before
package flame;

// After
package flame.v1;
```

**Scope:**
- Update package name only in all proto files
- No new fields added to messages
- Regenerate code for all languages (Rust, Python, Go)

**Acceptance Criteria:**
- All protos use `flame.v1` package
- Generated code updated in all SDKs
- Backward compatibility documented

#### P2.2: Split `common/src/apis.rs` (1284 lines)

**Current Structure:**
```
common/src/apis.rs (1284 lines)
├── Domain types (Session, Task, Application, etc.)
├── 45 impl From conversions
├── 11 impl TryFrom conversions
├── State enums
└── Helper functions
```

**Target Structure:**
```
common/src/
├── types/
│   ├── mod.rs          # Re-exports
│   ├── session.rs      # Session, SessionSpec, SessionStatus
│   ├── task.rs         # Task, TaskSpec, TaskStatus
│   ├── application.rs  # Application, ApplicationSpec
│   ├── executor.rs     # Executor, ExecutorSpec
│   └── node.rs         # Node, NodeSpec
├── conversions/
│   ├── mod.rs          # Re-exports
│   ├── from_rpc.rs     # rpc::* -> domain types
│   └── to_rpc.rs       # domain types -> rpc::*
└── state.rs            # All state enums with transitions
```

**Acceptance Criteria:**
- No single file > 400 lines
- Clear separation of concerns
- All existing tests pass
- No public API changes

#### P2.3: Add Code Coverage to CI

**File:** `.github/workflows/code-verify.yaml`

**Changes:**
```yaml
- name: Run tests with coverage
  run: cargo tarpaulin --out Xml --output-dir coverage

- name: Upload coverage to Codecov
  uses: codecov/codecov-action@v4
  with:
    files: coverage/cobertura.xml
    fail_ci_if_error: false
```

**Acceptance Criteria:**
- Coverage report generated on each PR
- Coverage badge in README
- Baseline coverage established

---

### Phase 3: Quality Improvements (Week 3-4)

#### P3.1: Add Python SDK Unit Tests

**Directory:** `sdk/python/tests/`

**Test Modules to Create:**
```
sdk/python/tests/
├── __init__.py
├── test_client.py      # Core client tests
├── test_session.py     # Session lifecycle
├── test_task.py        # Task operations
├── test_agent.py       # Agent wrapper
├── test_runner.py      # Runner service
├── test_cache.py       # Cache client
└── conftest.py         # Pytest fixtures
```

**Target Coverage:** 80%+ for core modules

**Acceptance Criteria:**
- Comprehensive unit tests for all public APIs
- Mock-based tests (no live cluster required)
- Tests run in CI

#### P3.2: Add State Machine Tests

**Files:**
- `session_manager/tests/state_machine_test.rs`
- `executor_manager/tests/state_machine_test.rs`

**Test Cases:**
```rust
#[test]
fn test_executor_state_transitions() {
    // Void -> Idle -> Binding -> Bound -> Unbinding -> Idle
    // Void -> Idle -> Releasing -> Released
    // Invalid transitions should error
}

#[test]
fn test_session_state_transitions() {
    // Open -> Closed
    // Task state transitions within session
}
```

**Acceptance Criteria:**
- All valid state transitions tested
- Invalid transitions return proper errors
- Edge cases covered (concurrent transitions, timeouts)

#### P3.3: Add Shell Completion to CLIs

**Files:**
- `flmctl/src/main.rs`
- `flmadm/src/main.rs`

**Implementation:**
```rust
use clap_complete::{generate, Shell};

#[derive(Parser)]
struct Args {
    #[command(subcommand)]
    command: Commands,
}

enum Commands {
    // ... existing commands ...
    
    /// Generate shell completion scripts
    Completion {
        #[arg(value_enum)]
        shell: Shell,
    },
}
```

**Usage:**
```bash
# Bash
flmctl completion bash > /etc/bash_completion.d/flmctl

# Zsh
flmctl completion zsh > ~/.zfunc/_flmctl

# Fish
flmctl completion fish > ~/.config/fish/completions/flmctl.fish
```

**Acceptance Criteria:**
- Completion works for bash, zsh, fish
- All subcommands and flags complete correctly
- Installation instructions in README

#### P3.4: Generate API Documentation

**Tool:** `protoc-gen-doc` or `grpc-gateway`

**Output:** `docs/api/`
```
docs/api/
├── index.md
├── frontend.md     # Frontend service docs
├── backend.md      # Backend service docs
├── shim.md         # Shim service docs
└── types.md        # Type definitions
```

**Acceptance Criteria:**
- All messages and services documented
- Examples for common operations
- Links from main documentation

---

### Phase 4: Polish and Maintenance (Ongoing)

#### P4.1: CI/CD Enhancements

| Enhancement                | File                              | Priority |
| -------------------------- | --------------------------------- | -------- |
| Security scanning (CodeQL) | `.github/workflows/security.yaml` | High     |
| Dependabot                 | `.github/dependabot.yml`          | High     |
| Cargo/pip caching          | All workflow files                | Medium   |
| Release automation         | `.github/workflows/release.yaml`  | Medium   |
| Nightly builds             | `.github/workflows/nightly.yaml`  | Low      |

#### P4.2: Documentation Improvements

| Document                  | Location                            | Priority |
| ------------------------- | ----------------------------------- | -------- |
| Proto file comments       | `rpc/protos/*.proto`                | High     |
| Troubleshooting guide     | `docs/tutorials/troubleshooting.md` | High     |
| Configuration reference   | `docs/tutorials/configuration.md`   | High     |
| Architecture diagrams     | `docs/images/`                      | Medium   |
| Contributor guide         | `CONTRIBUTING.md`                   | Medium   |
| Upgrade/migration guide   | `docs/tutorials/upgrading.md`       | Low      |

**Proto Comments Enhancement:**

Update all `.proto` files with comprehensive documentation comments aligned with design docs and implementation:

```protobuf
// Example: Enhanced message documentation
/**
 * Session represents a group of related tasks.
 * 
 * The Session Scheduler allocates resources to each session based on 
 * scheduling configurations by requesting the resource manager to launch executors.
 * Clients can continuously create tasks until the session is closed.
 *
 * Lifecycle: Created -> Open -> Closed
 */
message Session {
  /** Unique identifier and name for the session */
  Metadata metadata = 1;
  
  /** Session configuration including application, resource request, and instance limits */
  SessionSpec spec = 2;
  
  /** Current session state and task statistics */
  SessionStatus status = 3;
}

/**
 * SessionSpec defines the configuration for a session.
 */
message SessionSpec {
  /** Name of the application to run (must be pre-registered) */
  string application = 2;

  /** Field number 3 (slots) reserved — removed in the slots-cleanup refactor; do not reuse. */
  reserved 3;
  reserved "slots";

  /** Data shared across all tasks in this session (optional) */
  optional bytes common_data = 4;
  
  /** Minimum number of executor instances to maintain (default: 0) */
  uint32 min_instances = 5;
  
  /** Maximum number of executor instances (null means unlimited) */
  optional uint32 max_instances = 6;
}
```

**Files to Update:**
- `rpc/protos/types.proto` - All message and field definitions
- `rpc/protos/frontend.proto` - Frontend service and request/response messages  
- `rpc/protos/backend.proto` - Backend service and executor messages
- `rpc/protos/shim.proto` - Shim service and instance messages

#### P4.3: Code Deduplication

| Target                    | Current Location                         | Action                           |
| ------------------------- | ---------------------------------------- | -------------------------------- |
| State machine boilerplate | `controller/states/`, `executor/states/` | Extract trait with default impls |
| Schema validation         | `apiserver/frontend.rs:99-122, 172-195`  | Extract helper function          |
| Session heap setup        | `scheduler/actions/*.rs`                 | Extract to Context method        |

**Note:** `common/` and `sdk/rust/` cannot share code (different dependency graphs and APIs for internal vs external use).

---

## 3. Implementation Detail

### Architecture Impact

```
┌─────────────────────────────────────────────────────────────┐
│                      Phase 1: Safety                        │
│  ┌─────────────┐  ┌─────────────┐  ┌─────────────────────┐ │
│  │ unwrap()    │  │ CLI todo!() │  │ WasmShim + Storage  │ │
│  │ elimination │  │ fix         │  │ fixes               │ │
│  └─────────────┘  └─────────────┘  └─────────────────────┘ │
└─────────────────────────────────────────────────────────────┘
                              │
                              ▼
┌─────────────────────────────────────────────────────────────┐
│                    Phase 2: Foundation                      │
│  ┌─────────────┐  ┌─────────────┐  ┌─────────────────────┐ │
│  │ API         │  │ apis.rs     │  │ Code                │ │
│  │ versioning  │  │ split       │  │ coverage CI         │ │
│  └─────────────┘  └─────────────┘  └─────────────────────┘ │
└─────────────────────────────────────────────────────────────┘
                              │
                              ▼
┌─────────────────────────────────────────────────────────────┐
│                     Phase 3: Quality                        │
│  ┌─────────────┐  ┌─────────────┐  ┌─────────────────────┐ │
│  │ Python SDK  │  │ State       │  │ Shell completion    │ │
│  │ tests       │  │ machine     │  │ + API docs          │ │
│  │             │  │ tests       │  │                     │ │
│  └─────────────┘  └─────────────┘  └─────────────────────┘ │
└─────────────────────────────────────────────────────────────┘
                              │
                              ▼
┌─────────────────────────────────────────────────────────────┐
│                     Phase 4: Polish                         │
│  ┌─────────────┐  ┌─────────────┐  ┌─────────────────────┐ │
│  │ CI/CD       │  │ Docs        │  │ Code                │ │
│  │ enhance     │  │ improve     │  │ deduplication       │ │
│  └─────────────┘  └─────────────┘  └─────────────────────┘ │
└─────────────────────────────────────────────────────────────┘
```

### Dependencies Between Tasks

```
P1.1 (unwrap) ──────────────────────────────────────────────┐
P1.2 (CLI)    ──────────────────────────────────────────────┤
P1.3 (Wasm)   ──────────────────────────────────────────────┼─► Phase 1 Complete
P1.4 (Storage)──────────────────────────────────────────────┘
                                                            │
P2.1 (API ver) ◄────────────────────────────────────────────┤
P2.2 (apis.rs) ─────────────────────────────────────────────┼─► Phase 2 Complete
P2.3 (Coverage)─────────────────────────────────────────────┘
                                                            │
P3.1 (Py tests)◄────────────────────────────────────────────┤
P3.2 (SM tests)◄────────────────────────────────────────────┤
P3.3 (Shell)   ─────────────────────────────────────────────┼─► Phase 3 Complete
P3.4 (API docs)◄───────────────── P2.1 enables ─────────────┘
```

### Estimated Effort

| Phase     | Tasks  | Estimated Hours | Calendar Time |
| --------- | ------ | --------------- | ------------- |
| Phase 1   | 4      | 16-24           | 1 week        |
| Phase 2   | 3      | 16-24           | 1 week        |
| Phase 3   | 4      | 32-48           | 2 weeks       |
| Phase 4   | 3      | Ongoing         | Continuous    |
| **Total** | **14** | **64-96**       | **4+ weeks**  |

---

## 4. Use Cases

### Use Case 1: Developer Fixing a Bug

**Before Enhancement:**
1. Developer encounters `unwrap()` panic in production
2. No stack trace, unclear which `unwrap()` failed
3. Must add logging, rebuild, redeploy to diagnose

**After Enhancement:**
1. Proper error with context: `FlameError::InvalidState("executor spec missing node field")`
2. Error propagates with full context
3. Immediate diagnosis from error message

### Use Case 2: New Contributor Onboarding

**Before Enhancement:**
1. No API documentation
2. Must read proto files to understand API
3. No troubleshooting guide when things fail

**After Enhancement:**
1. Generated API docs with examples
2. Clear troubleshooting guide
3. Shell completion for CLI exploration

### Use Case 3: CI/CD for Pull Requests

**Before Enhancement:**
1. Only format and clippy checks
2. No coverage visibility
3. No security scanning

**After Enhancement:**
1. Code coverage report on every PR
2. Security scanning catches vulnerabilities
3. Faster builds with caching

---

## 5. References

### Related Documents
- [AGENTS.md](../../../AGENTS.md) - Project conventions
- [RFE318-cache](../RFE318-cache/) - Object cache design
- [RFE333-flmadm](../RFE333-flmadm/) - Admin CLI design

### Files Referenced

**Critical `unwrap()` locations:**
- `common/src/ctx.rs`
- `common/src/storage/object.rs`
- `session_manager/src/scheduler/actions/shuffle.rs:56`
- `session_manager/src/scheduler/actions/allocate.rs:69`
- `session_manager/src/scheduler/actions/dispatch.rs:64`
- `executor_manager/src/executor.rs:52-60`

**TODO comments:**
- `executor_manager/src/shims/wasm_shim.rs:131`
- `session_manager/src/storage/mod.rs:579`
- `session_manager/src/provider/k8s.rs:33`
- `sdk/rust/src/client/mod.rs:410`
- `common/src/apis.rs:420`

### External References
- [Rust Error Handling Best Practices](https://doc.rust-lang.org/book/ch09-00-error-handling.html)
- [Protocol Buffers Style Guide](https://protobuf.dev/programming-guides/style/)
- [Codecov Documentation](https://docs.codecov.com/)
