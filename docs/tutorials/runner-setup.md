# Runner API Setup Guide

This guide explains how to set up and use the Runner API for simplified Python application deployment in Flame.

## Overview

The Runner API provides a simplified way to package, deploy, and run Python applications in Flame without manually building and deploying service packages. It consists of three main components:

1. **Runner**: A context manager that handles packaging, uploading, and application registration
2. **RunnerService**: A service wrapper that exposes execution objects as remote Flame services
3. **ObjectFuture**: A future that resolves to cached objects for efficient data handling

**Note**: The Runner API modules (`Runner`, `RunnerService`, `FlameRunpyService`, etc.) are exported at the top-level `flamepy` module. They can be imported directly via `from flamepy import Runner`, or from `flamepy.core` or `flamepy.rl` (for backward compatibility).

## Prerequisites

### 1. Configure Package Storage

The Runner API requires a package storage location to be configured in your `flame.yaml` file. Add the `package` section under your cluster configuration:

```yaml
---
current-cluster: flame
clusters:
  - name: flame
    endpoint: "http://127.0.0.1:30080"
    cache: "http://127.0.0.1:30090"
    package:
      storage: "file:///opt/flame/packages"
      excludes:
        - "*.log"
        - "*.pkl"
        - "*.tmp"
```

**Note**: Currently, only `file://` URLs are supported. The storage location must be:
- A shared filesystem accessible by all Flame executor nodes (e.g., NFS)
- Writable by the client
- Readable by the executor nodes

### 2. Ensure flmrun Application is Registered

The Runner API requires the `flmrun` application to be registered in Flame. This application should be pre-registered by your Flame deployment. Verify it exists:

```python
import flamepy

# Check if flmrun is available
apps = flamepy.list_applications()
print([app.name for app in apps])
```

## Basic Usage

### Example 1: Simple Function

```python
from flamepy import Runner

def sum_fn(a: int, b: int) -> int:
    return a + b

# Use Runner as a context manager
with Runner("my-app") as rr:
    # Create a service from the function
    sum_service = rr.service(sum_fn)
    
    # Call the function remotely
    result = sum_service(1, 3)
    
    # Get the result
    print(result.get())  # Output: 4
```

### Example 2: Class with State

```python
from flamepy import Runner

class Counter:
    def __init__(self, initial: int = 0):
        self._count = initial
    
    def add(self, a: int) -> int:
        self._count = self._count + a
        return self._count
    
    def get_counter(self) -> int:
        return self._count

with Runner("counter-app") as rr:
    # Create a service with a class (auto-instantiated)
    cnt_s = rr.service(Counter)
    cnt_s.add(1)
    cnt_s.add(3)
    result = cnt_s.get_counter()
    print(result.get())  # Output: 4
    
    # Create a service with an instance
    cnt_os = rr.service(Counter(10))
    cnt_os.add(1)
    cnt_os.add(3)
    result = cnt_os.get_counter()
    print(result.get())  # Output: 14
```

### Example 3: Using ObjectFuture as Arguments

For large objects, you can pass `ObjectFuture` instances as arguments to avoid unnecessary data transfer:

```python
from flamepy import Runner

class Counter:
    def __init__(self, initial: int = 0):
        self._count = initial
    
    def add(self, a: int) -> int:
        self._count = self._count + a
        return self._count
    
    def get_counter(self) -> int:
        return self._count

with Runner("counter-app") as rr:
    cnt_os = rr.service(Counter(10))
    cnt_os.add(1)
    cnt_os.add(3)
    res_r = cnt_os.get_counter()
    
    # Pass ObjectFuture as argument (efficient for large objects)
    cnt_os.add(res_r)
    res_r2 = cnt_os.get_counter()
    print(res_r2.get())  # Output: 28
```

## API Reference

### Runner

**Constructor**: `Runner(name: str)`
- `name`: The name of the application/package

**Methods**:
- `service(execution_object: Any) -> RunnerService`: Create a RunnerService for the given execution object
  - If `execution_object` is a class, it will be instantiated using its default constructor
  - If `execution_object` is a function or instance, it will be used as-is

**Context Manager**:
- `__enter__()`: Packages the current directory, uploads to storage, and registers the application
- `__exit__()`: Closes all services, unregisters the application, and cleans up storage

### RunnerService

**Constructor**: `RunnerService(app: str, execution_object: Any)`
- `app`: The name of the application
- `execution_object`: The Python object to expose as a service

**Methods**:
- All public methods of the execution object are exposed as wrapper methods
- Each wrapper method returns an `ObjectFuture`
- `close()`: Close the service and clean up resources

### ObjectFuture

**Constructor**: `ObjectFuture(future: Future)`
- `future`: A Future that resolves to an ObjectRef

**Methods**:
- `ref() -> flamepy.cache.ObjectRef`: Get the ObjectRef (for internal use)
- `get() -> Any`: Retrieve the concrete object from the cache

## Package Exclusion Patterns

By default, the following patterns are excluded from packages:
- `.venv`
- `__pycache__`
- `.gitignore`
- `*.pyc`

You can add custom exclusion patterns in your `flame.yaml`:

```yaml
package:
  storage: "file:///opt/flame/packages"
  excludes:
    - "*.log"
    - "*.pkl"
    - "*.tmp"
    - "data/"
    - "models/"
```

## Troubleshooting

### Error: "Package configuration is not set"

**Solution**: Add the `package` section to your `flame.yaml` file (see Configuration section above).

### Error: "Failed to get flmrun application template"

**Solution**: Ensure the `flmrun` application is registered in Flame. Contact your Flame administrator.

### Error: "Storage directory does not exist"

**Solution**: Create the storage directory specified in your `flame.yaml` and ensure it's accessible:

```bash
sudo mkdir -p /opt/flame/packages
sudo chmod 777 /opt/flame/packages
```

### Error: "Package path not found"

**Solution**: Ensure the storage location is a shared filesystem accessible by all executor nodes.

## Working Directory

When the Runner registers an application, it automatically sets the working directory to `/opt/{name}` where `{name}` is your application name. This means:

- Your application will execute in `/opt/{name}` on the executor nodes
- Archive packages will be extracted to `/opt/{name}/extracted_...`
- Any relative file paths in your code will be relative to `/opt/{name}`

Example:
```python
with Runner("my-app") as rr:
    # Application will run in /opt/my-app
    # Archives extracted to /opt/my-app/extracted_my-app
    service = rr.service(MyClass())
```

## Advanced Usage

### Multiple Services

You can create multiple services within a single Runner context:

```python
with Runner("multi-app") as rr:
    sum_service = rr.service(sum_fn)
    calc_service = rr.service(Calculator())
    
    result1 = sum_service(5, 3)
    result2 = calc_service.multiply(4, 7)
    
    print(result1.get())  # Output: 8
    print(result2.get())  # Output: 28
```

### Using Keyword Arguments

```python
def greet(name: str, greeting: str = "Hello") -> str:
    return f"{greeting}, {name}!"

with Runner("greet-app") as rr:
    greet_service = rr.service(greet)
    
    result = greet_service(name="World", greeting="Hi")
    print(result.get())  # Output: Hi, World!
```

## Best Practices

1. **Use meaningful application names**: Choose descriptive names for your Runner instances
2. **Clean up resources**: Always use Runner as a context manager to ensure proper cleanup
3. **Minimize package size**: Add exclusion patterns for large files that aren't needed
4. **Use ObjectFuture for large objects**: Pass ObjectFuture instances as arguments instead of fetching and re-sending large objects
5. **Test locally first**: Verify your code works locally before deploying with Runner

## Limitations

1. **Storage**: Currently only `file://` URLs are supported (NFS or shared filesystem)
2. **Package size**: Large packages may take time to upload and install
3. **Pickling**: Execution objects must be picklable (lambdas and local functions may not work)
4. **Dependencies**: External dependencies must be available on executor nodes or installed via package

## Future Enhancements

The following features are planned for future releases:

- Support for S3 and HTTP storage backends
- Automatic dependency installation
- Package caching to avoid re-uploading
- Force overwrite option for existing packages
- Package versioning and rollback
