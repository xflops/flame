# RFE284 - Run customized Python application remotely via wheel/dist

## Motivation

Currently, the Python SDK requires users to set up both a client and a service when building an application, which adds unnecessary complexity—particularly on the service side, where users must build a container image and push it to a registry. However, for many Python applications, the underlying base images are quite similar (typically the standard Python image). A common Python service, designed to load and execute customized Python applications given a user-provided wheel file, simplifies this workflow and enables remote execution with minimal overhead.

## Function Specification

### Application configuration

A new optional field, `url`, is introduced in the application configuration to specify the location of the customized application package. In this release, only the `file://` schema is supported; additional schemas may be supported in future updates. Users must ensure that the specified location is accessible by the service, for example, via NFS. If no `url` is provided, the service will not attempt to download any package.

```yaml
metadata:
  name: my-app
spec:
  url: file:///opt/example
  ...
```

### APIs

This common Python application introduces two classes to facilitate communication between the client and the service: `RunnerRequest` and `RunnerContext`. The `RunnerRequest` represents the input for each task, while the `RunnerContext` holds session-wide shared data.

#### RunnerContext

The `RunnerContext` encapsulates data shared within the session, including the execution object specific to the session.  

- `execution_object`: The execution object for the customized session. This field is required.

#### RunnerRequest

The `RunnerRequest` defines the input for each task and contains the following fields:

- `method`: The name of the method to invoke within the customized application. This field is optional; it should be `None` if the execution object itself is a function or callable.
- `args`: A tuple containing positional arguments for the method. Optional.
- `kwargs`: A dictionary of keyword arguments for the method. Optional.
- `input_object`: An `ObjectRef` representing the method input, used when the input is large. Optional.

Note: The fields `args`, `kwargs`, and `input_object` may all be `None`, indicating that the method has no input. However, if any of these fields are provided, only one should be non-`None` at a time to avoid ambiguity. This constraint is enforced by a `__post_init__` validation method that raises a `ValueError` if multiple input fields are set.

### Service

The common Python application is implemented as an executable module called `flamepy.runpy`. This module allows users to define and deploy arbitrary Python applications in a consistent and efficient manner. By default, a `flmrun` application is registered to support execution of self-contained Python objects, streamlining the deployment and execution process.

```yaml
metadata:
  name: my-app
spec:
  url: file:///opt/example/my-app.whl
  command: /usr/bin/uv
  arguments:
    - run
    - -n
    - flamepy.runpy
  ...
```

## Implemention detail

This section offers an in-depth look at the technical implementation of this feature, highlighting the underlying Python language constructs, essential framework components, and design patterns that enable robust runner functionality. It discusses key architectural decisions, core abstractions, and extensibility points that empower seamless creation and orchestration of services, as well as efficient resource management. The following details are intended to guide both users and developers in understanding how the runner is structured, how it integrates with the Flame ecosystem, and how it can be extended for future needs.

### Application Configuration

To support the introduction of the new `url` field in the application configuration, `ApplicationSpec` should be updated to include this field. Additionally, this package information must be communicated to the Flame instance via `ApplicationContext`. Therefore, the `ApplicationContext` definition in `shim.proto` should also be updated accordingly. To ensure backward compatibility, both the Rust and Python SDKs need to be revised to reflect these changes.

### APIs

The `RunnerContext` and `RunnerRequest` classes are integral parts of the `flamepy.core` module, and are made available to users developing custom applications via `from flamepy import RunnerContext, RunnerRequest`.

- **RunnerContext** encapsulates the session-wide shared execution object. Its `execution_object` field holds a Python object directly. This allows flexible sharing of state or functions for all tasks within a session, leveraging `ObjectRef` to optimize data transfer.
  
- **RunnerRequest** represents the per-task invocation input. It includes the following fields:
  - `method`: the method name to invoke on the execution object; `None` indicates that the execution object itself is directly callable.
  - `args`: tuple of positional arguments (optional).
  - `kwargs`: dictionary of keyword arguments (optional).
  - `input_object`: an `ObjectRef` to encapsulate large method arguments (optional).

  The `RunnerRequest` provides a `set_object()` method to pickle and cache large objects, updating the `input_object` field with an `ObjectRef`. The service then unpickles and loads the object as input as needed.
  
  A `__post_init__` validation method ensures that only one of `args`, `kwargs`, or `input_object` is set at initialization time, raising a `ValueError` if multiple input fields are provided. The `set_object()` method also validates that `args` or `kwargs` are not already set before updating `input_object`.

**Usage Note:** Only one of `args`, `kwargs`, or `input_object` should be non-`None` to prevent ambiguity. If none are supplied, the method should be called without arguments. This constraint is enforced programmatically through validation.

### Services

`flamepy.runpy` is an executable module implementing the common Python service for Flame. It includes the `FlameRunpyService`, which implements the standard `FlameService` interface.

#### `on_session_enter`

The `on_session_enter` handler in `FlameRunpyService` is responsible for initializing the session environment:

1. If a package URL is specified:
   - Download the package from the `Application.url` field in `SessionContext`.
   - Install the package into the current `.venv`.
   - Ensure the newly installed package is discoverable in the execution environment.
2. Persist the `SessionContext` instance in a `_ssn_ctx` field for downstream use.

#### `on_task_invoke`

Within `on_task_invoke`, the following steps occur:

1. Retrieve the execution object from `_ssn_ctx.common_data().execution_object`.
2. Deserialize the incoming request from `TaskContext.input`, which contains a pickled `RunnerRequest`.
3. Determine invocation input:
   - If `input_object` is set, unpickle its data for method input.
   - If `args` or `kwargs` are set, use them directly as invocation parameters.
4. Execute the requested method on the execution object using the resolved input.
5. Cache the result as needed and return an `ObjectRef` within the `TaskOutput`.

#### `on_session_leave`

This handler is responsible for cleaning up resources at session end, including uninstalling any temporary packages from the environment.

#### Main module entrypoint

The module's `main` entrypoint starts the `FlameRunpyService` as a standard `FlameService`.

#### flmrun

To demonstrate this python common service, a new default application, named `flmrun`, will be registered when Flame Session Manager start, similar to `flmping` and `flmexec`.

The python project of common service is part of Flame root project, it includes `pyproject.toml` to identify the `flamepy` location which is `/usr/local/flame/sdk/python` by default.

```toml
[project]
name = "flmrun"
version = "0.1.0"
description = "Flame Runner Service"
readme = "README.md"
requires-python = ">=3.12"
dependencies = [
  "flamepy",
]

[tool.uv.sources]
flamepy = { path = "/usr/local/flame/sdk/python" }
```

When building the image of `flame-executor-manager` and `flame-console`, the `flmrun` directory should be copy into `/usr/local/flame/work/flmrun` which is the working directory of `flmrun` application. The application should be started with `uv`.

```yaml
metadata:
  name: flmrun
spec:
  working_directory: /usr/local/flame/work/flmrun
  command: /usr/bin/uv
  arguments:
    - run
    - -n
    - flamepy.runpy
```


## Use Cases

### Case 1: Run `sum` function remotely

**Steps**:
1. Define a `sum` to return the summary of two integer
   ```python
   def sum(a: int, b: int) -> int:
        return a+b
   ```
2. Create a session with `RunnerContext` and `sum`
    ```python
    from flamepy import RunnerContext, RunnerRequest, RunnerServiceKind
    
    ctx = RunnerContext(
        execution_object=sum,
        kind=RunnerServiceKind.Stateless,
    )
    ssn = flamepy.create_session("flmrun", ctx)
    ```
3. Invoke the `sum` function remotely
    ```python
    req = RunnerRequest(method=None, args=(1, 2))
    result = ssn.invoke(req)
    ```

4. Print the result
   ```python
   print(result.get())
   ```

**Output**:

The expected result should be `3`.
