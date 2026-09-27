# E2E Testing Quick Reference

## File Structure

```
e2e/
├── src/e2e/
│   ├── api.py              # Shared API definitions with context info classes
│   ├── instance_svc.py     # serving.Instance-based service (simple)
│   └── basic_svc.py        # FlameService-based service (with context info)
│
└── tests/
    ├── test_app.py          # Application management tests
    ├── test_service.py      # Tests for instance_svc.py
    └── test_core.py         # Tests for basic_svc.py (with context info)
```

## Service Comparison

### instance_svc.py (serving.Instance)
```python
# Simple decorator-based approach
instance = flamepy.serving.Instance()

@instance.entrypoint
def e2e_service_entrypoint(req: TestRequest) -> TestResponse:
    return TestResponse(output=req.input, common_data=...)

if __name__ == "__main__":
    instance.run()
```

**Application Name:** `flme2e`
**Test File:** `test_service.py`
**Features:**
- ✅ Simple request/response
- ✅ Common data management
- ❌ No context introspection
- ❌ No service state tracking

### basic_svc.py (FlameService)
```python
# Full lifecycle control
class BasicTestService(flamepy.FlameService):
    def on_session_enter(self, context: flamepy.SessionContext):
        # Store context for later use
        
    def on_task_invoke(self, context: flamepy.TaskContext):
        # Return detailed context information
        
    def on_session_leave(self):
        # Cleanup

if __name__ == "__main__":
    flamepy.run(BasicTestService())
```

**Application Name:** `flme2e-core-svc`
**Test File:** `test_core.py`
**Features:**
- ✅ Full context access (Task, Session, Application)
- ✅ Service state tracking
- ✅ Selective context requests
- ✅ Demonstrates stateful service

## Context Information Classes

### TaskContextInfo
```python
task_context = TaskContextInfo(
    task_id="...",          # Unique task identifier
    session_id="...",       # Parent session ID
    has_input=True,         # Whether task has input
    input_type="bytes",     # Type of input data
    input_size=128          # Size in bytes
)
```

### SessionContextInfo
```python
session_context = SessionContextInfo(
    session_id="...",              # Unique session ID
    application=app_info,          # Application context
    has_common_data=True,          # Has common data?
    common_data_type="TestContext" # Type of common data
)
```

### ApplicationContextInfo
```python
app_context = ApplicationContextInfo(
    name="flme2esvc",              # Application name
    shim="Shim.Host",              # Shim type
    image=None,                    # Container image
    command="uv",                  # Command
    working_directory=None,       # Executor chooses a work directory
    url="grpcs://cache/...",      # Deployed package URL
)
```

## Request/Response Example

### Request with Context Information
```python
request = TestRequest(
    input="test data",
    update_common_data=False,
    # Context flags (only in basic_svc.py)
    request_task_context=True,
    request_session_context=True,
    request_application_context=True,
)
```

### Response with Context Information
```python
response = TestResponse(
    output="test data",            # Echoed input
    common_data="...",             # Session common data
    task_context=TaskContextInfo(...),
    session_context=SessionContextInfo(...),
    application_context=ApplicationContextInfo(...),
    service_state={                # Service internal state
        "task_count": 1,
        "session_enter_count": 1,
        "session_leave_count": 0
    }
)
```

## Running Tests

```bash
# All e2e tests
pytest e2e/tests/ -v

# Instance tests only (serving.Instance-based)
pytest e2e/tests/test_service.py -v

# Service tests only (FlameService-based with context)
pytest e2e/tests/test_core.py -v

# Specific test
pytest e2e/tests/test_core.py::test_all_context_info -v
```

## Test Categories in test_core.py

| Category | Tests |
|----------|-------|
| **Basic** | `test_basic_service_invoke` |
| **Task Context** | `test_task_context_info` |
| **Session Context** | `test_session_context_info` |
| **Application Context** | `test_application_context_info` |
| **All Contexts** | `test_all_context_info` |
| **State Management** | `test_service_state_tracking`, `test_multiple_sessions_isolation` |
| **Common Data** | `test_common_data_without_context_request`, `test_common_data_with_session_context`, `test_update_common_data` |
| **Selective Requests** | `test_context_info_selective_request` |

## When to Use Which?

### Use instance_svc.py (serving.Instance) when:
- Building simple request/response services
- Prototyping quickly
- Don't need context introspection
- Want minimal boilerplate

### Use basic_svc.py (FlameService) when:
- Need full control over lifecycle
- Require context introspection
- Building stateful services
- Need detailed debugging information
- Writing testing frameworks

## Code Examples

### Example 1: Simple Instance-based Service
```python
from flamepy.serving import open_session

session = open_session(name="flme2e", ctx=TestContext(common_data="data"))

response = session.run(TestRequest(input="hello"))
print(response.output)       # "hello"
print(response.common_data)  # "data"

session.close()
```

### Example 2: Service-based with Full Context
```python
from e2e.helpers import invoke_task, serialize_common_data

session = flamepy.create_session(
    application="flme2e-core-svc",
    common_data=serialize_common_data(TestContext(common_data="data"), "flme2e-core-svc")
)

response = invoke_task(session, TestRequest(
    input="hello",
    request_task_context=True,
    request_session_context=True,
    request_application_context=True,
))

# Access all context information
print(f"Task: {response.task_context.task_id}")
print(f"Session: {response.session_context.session_id}")
print(f"App: {response.application_context.name}")
print(f"Working Dir: {response.application_context.working_directory}")
print(f"Tasks Processed: {response.service_state['task_count']}")

session.close()
```

### Example 3: Selective Context Request
```python
# Only request what you need to reduce overhead
response = invoke_task(session, TestRequest(
    input="data",
    request_application_context=True,  # Only app context
))

assert response.application_context is not None
assert response.task_context is None
assert response.session_context is None
```
