# E2E Test Enhancements

## Overview

This document describes the enhancements made to the e2e test suite to support both `serving.Instance` and `FlameService` implementations with comprehensive context information testing.

## Changes Summary

### 1. File Restructuring

**Before:**
```
e2e/src/e2e/
  - service.py         (FlameInstance-based)
  - api.py
e2e/tests/
  - test_session.py
  - test_application.py
```

**After:**
```
e2e/src/e2e/
  - instance_svc.py    (serving.Instance-based, renamed from service.py)
  - basic_svc.py       (NEW: FlameService-based)
  - api.py             (ENHANCED with context info classes)
e2e/tests/
  - test_service.py     (tests for instance_svc.py)
  - test_core.py        (tests for basic_svc.py)
  - test_app.py
```

### 2. Enhanced API (api.py)

Added new context information classes:

#### ApplicationContextInfo
```python
@dataclass
class ApplicationContextInfo:
    name: Optional[str] = None
    shim: Optional[str] = None
    image: Optional[str] = None
    command: Optional[str] = None
    working_directory: Optional[str] = None
    url: Optional[str] = None
```

#### SessionContextInfo
```python
@dataclass
class SessionContextInfo:
    session_id: Optional[str] = None
    application: Optional[ApplicationContextInfo] = None
    has_common_data: bool = False
    common_data_type: Optional[str] = None
```

#### TaskContextInfo
```python
@dataclass
class TaskContextInfo:
    task_id: Optional[str] = None
    session_id: Optional[str] = None
    has_input: bool = False
    input_type: Optional[str] = None
    input_size: Optional[int] = None
```

#### Enhanced TestRequest
```python
@dataclass
class TestRequest:
    update_common_data: bool = False
    input: Optional[str] = None
    # NEW: Flags to control what context information to return
    request_task_context: bool = False
    request_session_context: bool = False
    request_application_context: bool = False
```

#### Enhanced TestResponse
```python
@dataclass
class TestResponse:
    output: Optional[str] = None
    common_data: Optional[str] = None
    # NEW: Context information fields
    task_context: Optional[TaskContextInfo] = None
    session_context: Optional[SessionContextInfo] = None
    application_context: Optional[ApplicationContextInfo] = None
    # NEW: Service state information
    service_state: Optional[Dict[str, Any]] = None
```

### 3. Services

#### instance_svc.py (serving.Instance-based)
- Renamed from `service.py`
- Uses `serving.Instance` with `@instance.entrypoint` decorator
- Simple implementation for basic request/response testing
- Focuses on common data management
- **No context introspection capabilities**

```python
instance = flamepy.serving.Instance()

@instance.entrypoint
def e2e_service_entrypoint(req: TestRequest) -> TestResponse:
    cxt = instance.context()
    data = cxt.common_data if cxt is not None else None
    
    if req.update_common_data:
        instance.update_context(TestContext(common_data=req.input))
    
    return TestResponse(output=req.input, common_data=data)
```

#### basic_svc.py (FlameService-based)
- NEW implementation directly using `FlameService`
- Full control over session lifecycle
- **Exposes detailed context information**
- Maintains service state across tasks

```python
class BasicTestService(flamepy.FlameService):
    def on_session_enter(self, context: flamepy.SessionContext):
        # Store session context for introspection
        
    def on_task_invoke(self, context: flamepy.TaskContext) -> flamepy.TaskOutput:
        # Return detailed context information
        
    def on_session_leave(self):
        # Clean up state
```

**Key Features:**
- Tracks service state (task_count, session_enter_count, session_leave_count)
- Exposes TaskContext, SessionContext, and ApplicationContext information
- Supports selective context information requests
- Demonstrates stateful service implementation

### 4. Test Coverage

#### test_service.py (Instance Tests)
Tests for `instance_svc.py` focusing on:
- Session creation and closure
- Task invocation
- Common data management
- Multiple concurrent tasks
- Futures-based task execution

#### test_core.py (Service Tests)
NEW comprehensive tests for `basic_svc.py`:

1. **Basic Functionality**
   - `test_basic_service_invoke()`: Basic request/response
   
2. **Context Information**
   - `test_task_context_info()`: TaskContext details (task_id, input info)
   - `test_session_context_info()`: SessionContext details (session_id, common data info)
   - `test_application_context_info()`: ApplicationContext details (name, command, working_directory)
   - `test_all_context_info()`: All contexts together
   - `test_context_info_selective_request()`: Selective context requests

3. **State Management**
   - `test_service_state_tracking()`: Service state across multiple tasks
   - `test_multiple_sessions_isolation()`: State isolation between sessions

4. **Common Data**
   - `test_common_data_without_context_request()`: Basic common data handling
   - `test_common_data_with_session_context()`: Common data in session context
   - `test_update_common_data()`: Updating common data

## Key Differences: serving.Instance vs FlameService

| Feature | serving.Instance | FlameService |
|---------|--------------|--------------|
| **Usage** | Simple decorator-based | Full lifecycle control |
| **Implementation** | `@instance.entrypoint` | Implement 3 methods |
| **Context Access** | Limited via `instance.context()` | Full access to all contexts |
| **State Management** | Built-in | Manual/Custom |
| **Lifecycle Control** | Automatic | Explicit (enter/leave) |
| **Use Case** | Simple services, quick prototypes | Complex services, full control |
| **Context Introspection** | ❌ Limited | ✅ Full access |
| **Service State** | ❌ Not exposed | ✅ Custom tracking |
| **Best For** | Quick testing, simple workflows | Production services, testing frameworks |

## Usage Examples

### Using instance_svc.py (serving.Instance)
```python
from flamepy.serving import open_session

session = open_session(name="flme2e", ctx=TestContext(common_data="initial"))

response = session.run(TestRequest(input="hello"))
print(response.output)  # "hello"
print(response.common_data)  # "initial"

session.close()
```

### Using basic_svc.py (FlameService with Context Info)
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

# Access context information
print(f"Task ID: {response.task_context.task_id}")
print(f"Session ID: {response.session_context.session_id}")
print(f"App Name: {response.application_context.name}")
print(f"Working Dir: {response.application_context.working_directory}")
print(f"Task Count: {response.service_state['task_count']}")

session.close()
```

## Running Tests

```bash
# Run all e2e tests
pytest e2e/tests/ -v

# Run instance-based tests only
pytest e2e/tests/test_service.py -v

# Run service-based tests only
pytest e2e/tests/test_core.py -v

# Run specific test
pytest e2e/tests/test_core.py::test_all_context_info -v
```

## Benefits

1. **Comprehensive Testing**: Both serving.Instance and FlameService implementations are tested
2. **Context Introspection**: Full visibility into Flame's context system
3. **Debugging Aid**: Detailed context information helps debug issues
4. **Documentation**: Demonstrates both service implementation patterns
5. **Validation**: Ensures context information is properly propagated
6. **Flexibility**: Selective context requests reduce overhead when not needed

## Future Enhancements

Potential additions:
- Add performance/timing metrics
- Include resource usage information
- Add error injection for robustness testing
- Support for custom metadata fields
- Add logging/tracing integration examples
