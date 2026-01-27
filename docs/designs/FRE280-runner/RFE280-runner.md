# RFE280 - Simplify the Python API of Flame

## Motivation

Flame currently provides both Python and Rust SDKs for building distributed applications. However, both SDKs require users to manually build service packages and deploy them to the cluster, which adds unnecessary complexity to the application development workflow. To address this, we propose introducing a new set of Python APIs that can automatically manage service packaging and deployment. This will enable users to build and run applications directly from the client, significantly simplifying the development process.

Given Python's rich language features and dynamic capabilities, this proposal will center on streamlining and simplifying the Python API.

## Function Specification

### FlameContext

With this feature, Flame will automatically handle packaging and storage of your application, based on the configuration provided in the `FlameContext`—which defaults to using `flame.yaml`. 

At present, Flame supports storage via shared file systems only. Therefore, the storage location must begin with `file://` and reference a network-accessible (shared) file system, such as NFS. Support for additional protocols—including object stores like S3 (by specifying `s3://`), http (by specifying `http://`)—is planned for future releases.

Additionally, `FlameContext` supports excluding unnecessary files when building the application package. By default, it ignores common patterns such as `.vent`, `__pycache__`, `.gitignore`, and all `*.pyc` files. Users can further customize exclusion filters to omit additional files or directories as needed, for example excluding log files or other artifacts.

```yaml
---
current-cluster: flame
clusters:
  - name: flame
    endpoint: "http://flame-session-manager:8080"
    cache: "http://flame-executor-manager:9090"
    package:
      - storage: "file:///opt/example/"
        excludes:
          - "*.log"
          - "*.pkl"
```

### Client API

The Python SDK introduces a new API, named `Runner`, which is a context manager class designed to simplify the packaging and deployment workflow for your Python project within the Flame ecosystem. Upon entering the context, these runners will automatically package your Python project and upload it to the storage location specified by your `FlameContext` configuration. When exiting the context, the runner will attempt to remove (delete) the package from the storage.

Each runner takes a single argument to specify the package name. If a package with the same name already exists in the storage, it will not be uploaded again, avoiding unnecessary duplication. If package cleanup fails due to a fatal error (such as a system power loss), it is the site administrator’s responsibility to manually remove orphaned packages. In future releases, Flame will provide an option to forcibly overwrite an existing package during upload.

```python
from flamepy.rl import Runner

with Runner("test") as rr:
    pass
```

The runner offers a `service` method, which is used to define the runner’s service. The `service` method takes a single argument—the execution object—and returns a `RunnerService` instance. This instance exposes all callable methods of the execution object, allowing those methods to be executed as remote Flame tasks.

The execution object provided to the `service` method can be a function, a class, or an instance of a class. If a class is given, `service` will instantiate it using its default constructor. The resulting `RunnerService` exposes all callable methods of the execution object, and each of these methods returns a Flame `ObjectFuture`. Applications can then use the `get()` method of `ObjectFuture` to retrieve results from the remote execution. Furthermore, the wrapper methods of `RunnerService` support accepting `ObjectFuture` instances as parameters, minimizing unnecessary data transfer when working with large objects.


```python
from flamepy import Runner

def sum(a:int, b:int) -> int:
    return a + b


class Counter:
    def __init__(self, c = 0):
        self._count = c

    def add(self, a: int):
        self._count = self._count + a

    def get_counter(self) -> int:
        return self._count


with Runner("test") as rr:
    # Create a Flame Service with class
    cnt_s = rr.service(Counter)
    cnt_s.add(1)
    cnt_s.add(3)
    res_r = cnt_s.get_counter()
    print(res_r.get()) # The out is 4


    # Create a Flame Service with instance
    cnt_os = rr.service(Counter(10))
    cnt_os.add(1)
    cnt_os.add(3)
    res_r = cnt_os.get_counter()
    print(res_r.get()) # The out is 14

    cnt_os.add(res_r) # The FlameService also support ObjectRef for large object
    res_r = cnt_os.get_counter()
    print(res_r.get()) # The out is 28
```

## Implementation

This section provides detailed implementation insights for this feature, covering the relevant Python language mechanisms, core framework components, and design patterns that support the runner functionality. It outlines the architectural decisions, key abstractions, and extensibility points that enable seamless service creation, orchestration, and efficient resource management.

### FlameContext

A new optional field named `package` will be introduced in `FlameContext` to support application package management. This field will be represented internally by a new class called `FlamePackage`, which encapsulates package configuration details. `FlamePackage` has two fields:

- `storage`: The URL specifying where the application package should be persisted. Currently, only the `file://` schema is supported. This field is required.
- `excludes`: A list of custom patterns to exclude from the package. By default, this list includes `.vent`, `__pycache__`, `.gitignore`, and `*.pyc`. This field is optional.


### ObjectFuture

To enable efficient management of asynchronous and deferred computation results in runner services, the `ObjectFuture` class is introduced. This class encapsulates a single field, `_future`, which represents a pending computation or its eventual result. The `result()` method of `_future` is expected to always yield an `ObjectRef` instance.

`ObjectFuture` exposes two key methods:

- `ref()`: Returns the `ObjectRef` by invoking `_future.result()`. This method is primarily intended for internal use within the Flame SDK, providing direct access to the encapsulated object expression.
- `get()`: Retrieves the concrete object that `ObjectFuture` represents. It does so by fetching the `ObjectRef` via `_future.result()`, and then utilizing `cache.get_object` to asynchronously or lazily fetch the actual underlying object as needed.

This abstraction streamlines the interaction with service-based computations, ensuring seamless integration with both cached and remote results while maintaining a unified interface for accessing computed objects.


### RunnerService

`RunnerService` is a new class designed to encapsulate an execution object for remote invocation within Flame. Its constructor, `__init__`, takes two parameters:

- `app`: The name of the application registered in Flame. Note that the associated service must be `flamepy.runpy`.
- `execution_object`: The Python execution object to be managed and exposed as a remote service.

Upon instantiation, `RunnerService` performs the following steps:

1. In `__init__`, a new session is established by invoking `flamepy.create_session`. Because the service is `flamepy.runpy`, the session's common data is set using a `RunnerContext` that includes the provided `execution_object`.

2. Next, `__init__` inspects all public instance methods of the `execution_object` and, for each, generates a corresponding wrapper function. Each wrapper is responsible for:
    - Submitting a task to the session via `_session.run(...)`. The task input must be constructed as a `RunnerRequest`, as required by the `flamepy.runpy` service.
    - If any of the wrapper’s arguments are instances of `ObjectFuture`, these arguments are converted to their respective `ObjectRef` representations by calling their `ref()` method before constructing the `RunnerRequest`.
    - The wrapper returns an `ObjectFuture` that is constructed from the `Future` object returned by `_session.run(...)`.

Additionally, `RunnerService` provides a `close()` method for gracefully cleaning up resources, such as closing the underlying session.


### Runner

The `Runner` class provides a streamlined interface for managing lifecycle and deployment of customized Python packages within Flame. It leverages both its constructor and context manager protocols to ensure that package environments are set up and torn down automatically. This encapsulation allows users to easily register, execute, and manage application components without manual resource management, resulting in a more robust and user-friendly experience when working with custom Python packages.

#### Constructor

The `Runner` constructor accept only one parameter (`name`) for the name of application and packages; it'll also initialize a slice for managing the lifecycle of `RunnerService`, named `_services`.

#### Context Manager

The `Runner` class utilizes Python’s context manager protocol to automate the setup and teardown of customized Python applications within Flame, ensuring robust resource management and a seamless user experience.

**Context Manager Entry (`__enter__`):**

Upon entering the context, `Runner` executes the following steps to prepare the application environment:

1. Packages the current working directory into a `.gz` archive, applying exclusion patterns specified by `FlameContext.package.excludes` to omit unnecessary files.
2. Uploads the resulting package to the storage location defined in `FlameContext.package.storage`.
3. Retrieves the `flmrun` application template using `get_application('flmrun')`. If this retrieval fails, an exception is raised to prevent further setup.
4. Modifies the cloned template by updating its `name` and `url` fields to correspond to the new application, and attempts to register it. If registration fails, a relevant exception is raised.

**Context Manager Exit (`__exit__`):**

When leaving the context, `Runner` ensures a clean teardown by performing these actions:

1. Iterates through all `RunnerService` instances tracked in the `_services` collection, invoking each service’s `close()` method to gracefully terminate resources.
2. Unregisters the deployed application to remove it from the Flame registry.
3. Deletes the application package from storage as specified by the configured URL, fully cleaning up any temporary resources.





#### Runner.Service

The `Runner` class provides a `service(...)` method for instantiating a `RunnerService` within the current `Runner` context. This method accepts a single parameter: the `execution_object` to be managed and exposed as a remote service. When called, `service(...)` automatically creates a new `RunnerService` using the package or application name associated with the `Runner`, and passes along the provided `execution_object`. 

## Use Cases

**How to Use Runner**

1. Initialize a new Python project. For example:
   ```shell
   uv init .
   ```

2. Edit `main.py` to leverage the `Runner` abstraction:
   ```python
   from flamepy import Runner

   def sum_fn(a: int, b: int) -> int:
       return a + b

   # Instantiate the Runner context and register the function as a service
   with Runner("test") as rr:
       sum_service = rr.service(sum_fn)
       result = sum_service(1, 3)
       print(result.get())  # Output: 4
   ```

3. Execute your program:
   ```shell
   uv run ./main.py
   ```

**Expected Behavior**

- The output printed to the console will be `4`.
- After execution, check the applications registered in Flame; no unintended or residual applications should be present.