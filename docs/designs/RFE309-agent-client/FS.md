# RFE309: Service session client

## Motivation

Currently, users build AI agents by directly re-using the core APIs. However, this approach can be confusing and inconvenient for end users. In this feature, a dedicated service session client will be introduced to streamline and simplify the process of interacting with service sessions.

## Function Specification

A new class, called `Session`, will be added to the `flamepy.service` module to provide a streamlined interface for interacting with service sessions.

### Initialization

There are two ways to manage the lifecycle of a service session: using the constructor directly or as a context manager.

- **Direct Instantiation:**  
  When you create a service session using the constructor, you are responsible for explicitly closing it after use. This ensures that all resources are properly released.

  ```python
  session = Session("test", ctx)
  # ... use the session ...
  session.close()
  ```

- **Context Manager:**  
  By using the session as a context manager, resource management is handled automatically. The session is closed when execution leaves the context block.

  ```python
  with Session("test", ctx) as session:
      # ... use the session ...
  ```

### Invoking the Service Session

Use the `invoke()` method to interact with the service session. The request and response objects should exactly match the service entrypoint signature defined on the service side.

```python
resp = session.invoke(req)
```

### Accessing the Service Session Context

The current service session context, such as a running conversation or environment state, can be retrieved via the `context()` method.

```python
ctx = session.context()
```

## Implementation detail

1. The `Session` class is essentially a thin wrapper around `core.Session`, where the service session name corresponds to the application's name.
2. The service session's `context()` method returns the session's shared (common) data, accessed directly from `core.Session`.
3. The service session's `invoke()` method delegates its call to `core.Session.invoke`, passing through the invocation to the underlying session mechanism.

## Use Cases

1. Update e2e and integration test accordingly
2. Update example accordingly
