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

### `flmrun` Application

To enable running Python applications directly, a new Flame Application called `flmrun` has been introduced.
`flmrun` serves as the default Flame Application and is automatically registered during Flame startup. Users can utilize `flmctl` to view the details of `flmrun`, as well as to monitor the status of their Python applications—which will appear as sessions under the `flmrun` application.


```shell
$ flmctl list -a
 Name     State    Tags  Created   Shim  Command                              
 ...
 flmrun   Enabled        02:34:39  Host  uv 
```

### Client API

The Python SDK introduces two new APIs: `FlameRunner` and `FlameAsyncRunner`. Both are context manager classes designed to simplify the packaging and deployment workflow for your Python project within the Flame ecosystem. As their names suggest, the main difference between these two runners is that `FlameAsyncRunner` supports asynchronous (asyncio-based) operations, while all methods in `FlameRunner` are synchronous.

Upon entering the context, these runners will automatically package your Python project and upload it to the storage location specified by your `FlameContext` configuration. When exiting the context, the runner will attempt to remove (delete) the package from the storage.

Each runner takes a single argument to specify the package name. If a package with the same name already exists in the storage, it will not be uploaded again, avoiding unnecessary duplication. If package cleanup fails due to a fatal error (such as a system power loss), it is the site administrator’s responsibility to manually remove orphaned packages. In future releases, Flame will provide an option to forcibly overwrite an existing package during upload.

```python
from flamepy import FlameRunner, FlameAsyncRunner

async with FlameAsyncRunner("test") as runner:
    pass

with FlameAsyncRunner("test") as runner:
    pass
```

Both runners offer a `service` method, which is used to define the runner’s service. The `service` method takes a single argument—the execution object—and returns a `FlameRunnerService` instance. This instance exposes all callable methods of the execution object, allowing those methods to be executed as remote Flame tasks.

The execution object provided to the `service` method can be a function, a class, or an instance of a class. If a class is given, `service` will instantiate it using its default constructor. The resulting `FlameRunnerService` exposes all callable methods of the execution object, and each of these methods returns a Flame `ObjectExpr`. Applications can then use the `get()` method of `ObjectExpr` to retrieve results from the remote execution. Furthermore, the wrapper methods of `FlameRunnerService` support accepting `ObjectExpr` instances as parameters, minimizing unnecessary data transfer when working with large objects.


```python
from flamepy import FlameRunner, FlameAsyncRunner


def sum(a:int, b:int) -> int:
    return a + b


class Counter:
    def __init__(self, c = 0):
        self._count = c

    def add(self, a: int):
        self._count = self._count + a

    def get_counter(self) -> int:
        return self._count


async with FlameAsyncRunner("test") as runner:
        # Create a Flame Service with function
    sum_s = runner.service(sum)
    res_r = await sum_s(1, 2)
    print(res_r.get()) # The output is 3


with FlameRunner("test") as runner:
    # Create a Flame Service with class
    cnt_s = runner.service(Counter)
    cnt_s.add(1)
    cnt_s.add(3)
    res_r = cnt_s.get_counter()
    print(res_r.get()) # The out is 4


    # Create a Flame Service with instance
    cnt_os = runner.service(Counter(10))
    cnt_os.add(1)
    cnt_os.add(3)
    res_r = cnt_os.get_counter()
    print(res_r.get()) # The out is 14

    cnt_os.add(res_r) # The FlameService also support ObjectExpr for large object
    res_r = cnt_os.get_counter()
    print(res_r.get()) # The out is 28
```


## Implementation

This section provides detailed implementation insights for this feature, covering the relevant Python language mechanisms, core framework components, and design patterns that support the runner functionality. It outlines the architectural decisions, key abstractions, and extensibility points that enable seamless service creation, orchestration, and efficient resource management.

### FlameContext

A new optional field named `package` will be introduced in `FlameContext` to support application package management. This field will be represented internally by a new class called `FlamePackage`, which encapsulates package configuration details. `FlamePackage` has two fields:

- `storage`: The URL specifying where the application package should be persisted. Currently, only the `file://` schema is supported. This field is required.
- `excludes`: A list of custom patterns to exclude from the package. By default, this list includes `.vent`, `__pycache__`, `.gitignore`, and `*.pyc`. This field is optional.
  
With these enhancements, `FlameContext` provides robust and flexible mechanisms for managing application packaging and deployment artifacts.

### `flmrun` Application



### FlameRunner


### Runner.Service


## Use Cases

## References

