"""
Copyright 2025 The Flame Authors.
Licensed under the Apache License, Version 2.0 (the "License");
you may not use this file except in compliance with the License.
You may obtain a copy of the License at
    http://www.apache.org/licenses/LICENSE-2.0
Unless required by applicable law or agreed to in writing, software
distributed under the License is distributed on an "AS IS" BASIS,
WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
See the License for the specific language governing permissions and
limitations under the License.
"""

import inspect
import io
import logging
import os
import sys
import tarfile
import threading
import uuid
from concurrent.futures import Future, as_completed
from enum import Enum, auto
from functools import partial, wraps
from typing import Any, Callable, List, Optional, Protocol

import cloudpickle

from flamepy.app import _context
from flamepy.app.storage import StorageBackend, create_storage_backend
from flamepy.app.types import (
    ServiceContext,
    ServiceRequest,
    _is_function,
)
from flamepy.core import ObjectRef, get_object
from flamepy.core import client as core_client
from flamepy.core import put_object as _core_put_object
from flamepy.core.service import SessionContext
from flamepy.core.types import (
    ApplicationAttributes,
    ApplicationState,
    FlameContext,
    FlameError,
    FlameErrorCode,
    ResourceRequirement,
    SessionAttributes,
    short_name,
)

logger = logging.getLogger(__name__)

_cloudpickle_registry_lock = threading.Lock()


def _execution_module(execution_object: Any) -> Any:
    """Return the user module that must be serialized with an execution object."""
    target = execution_object
    while isinstance(target, partial):
        target = target.func

    module = inspect.getmodule(target)
    if module is None:
        return None

    module_root = module.__name__.partition(".")[0]
    if module_root == "builtins" or module_root in getattr(sys, "stdlib_module_names", ()):
        return None
    return module


def _validate_resreq(resreq: Optional[str]) -> None:
    if resreq is not None and not isinstance(resreq, str):
        raise TypeError(f"resreq must be a string, got {type(resreq).__name__}")


def _validate_service_class(execution_class: type) -> None:
    conflicts = sorted(name for name in dir(execution_class) if not name.startswith("_") and callable(getattr(execution_class, name)) and hasattr(ServiceInstance, name))
    if conflicts:
        names = ", ".join(conflicts)
        raise TypeError(f"service class methods conflict with ServiceInstance API: {names}")


class _ServiceState(Enum):
    OPEN = auto()
    CLOSING = auto()
    CLOSE_FAILED = auto()
    CLOSED = auto()


class ObjectFuture:
    """Encapsulates a future that resolves to an ObjectRef.

    This class manages asynchronous and deferred computation results in app services.
    The underlying future is expected to always yield an ObjectRef instance when resolved.

    Attributes:
        _future: A Future that will resolve to an ObjectRef
    """

    def __init__(self, future: Future):
        """Initialize an ObjectFuture.

        Args:
            future: A Future that resolves to an ObjectRef
        """
        self._future = future

    def ref(self) -> ObjectRef:
        """Get the ObjectRef by waiting for the future to complete.

        This method is primarily intended for internal use within the Flame SDK,
        providing direct access to the encapsulated object reference.

        Returns:
            The ObjectRef from the completed future
        """
        result = self._future.result()
        # The future returns bytes (ObjectRef encoded), decode it to ObjectRef
        if isinstance(result, bytes):
            return ObjectRef.decode(result)
        # If it's already an ObjectRef, return it as-is
        if isinstance(result, ObjectRef):
            return result
        # Otherwise, assume it's bytes and try to decode
        return ObjectRef.decode(result)

    def get(self) -> Any:
        """Retrieve the concrete object that this ObjectFuture represents.

        This method fetches the ObjectRef via the future, then uses cache.get_object
        to retrieve the actual underlying object.

        Returns:
            The deserialized object from the cache
        """
        result = self._future.result()
        # The future returns bytes (ObjectRef encoded), decode it to ObjectRef
        if isinstance(result, bytes):
            object_ref = ObjectRef.decode(result)
        elif isinstance(result, ObjectRef):
            object_ref = result
        else:
            # Otherwise, assume it's bytes and try to decode
            object_ref = ObjectRef.decode(result)
        return get_object(object_ref)

    def wait(self) -> None:
        """Wait for the future to complete without fetching the result."""
        self._future.result()


class ObjectFutureIterator:
    """Iterator wrapper over futures that yields ObjectFuture as they complete."""

    def __init__(self, futures: List[ObjectFuture]):
        self._future_map = {future._future: future for future in futures}

    def __iter__(self):
        for future in as_completed(self._future_map):
            yield self._future_map[future]


class _SessionOwner(Protocol):
    def close(self) -> None: ...


class _NoopSessionOwner:
    """Represent a session owned by another service proxy."""

    def close(self) -> None:
        pass


class _ServiceSessionOwner:
    """Close a session owned by this service proxy."""

    def __init__(self, session: Any):
        self._session = session

    def close(self) -> None:
        self._session.close()


class ServiceInstance:
    """Encapsulates an execution object for remote invocation within Flame.

    This class creates an app session and dynamically generates wrapper methods
    for all public methods of the execution object.
    Each wrapper submits tasks to the session and returns ObjectFuture instances.

    Attributes:
        _app: The name of the application registered in Flame
        _execution_object: The Python execution object being managed
        _session: The Flame session for task execution
    """

    def __init__(
        self,
        app: str,
        execution_object: Any,
        autoscale: Optional[bool] = None,
        warmup: int = 0,
        resreq: Optional[str] = None,
        constructor_args: Optional[tuple[Any, ...]] = None,
        constructor_kwargs: Optional[dict[str, Any]] = None,
    ):
        """Initialize a ServiceInstance.

        Args:
            app: The name of the application registered in Flame.
                 The associated application must support Python app services.
            execution_object: The Python execution object to be managed and
                             exposed as a remote service. Must be a class or
                             function. Running App code can access
                             invocation state through `flamepy.app.session_context()`
                             and `flamepy.app.publish_attributes(attrs)`.
            autoscale: Whether to create instances dynamically based on pending
                      tasks. Defaults to True.
            warmup: Number of instances to pre-create at session start. When
                    autoscale=False, this sets the fixed instance count.
            resreq: Optional resource requirements string, for example
                    ``"cpu=1,mem=1g"``. When omitted, the server applies
                    cluster.resource_requirement (or its fallback).
        """
        self._app = app
        self._execution_object = execution_object
        self._function_wrapper = None  # For callable functions
        self._method_names: List[str] = []
        self._future_lock = threading.Lock()
        self._state_changed = threading.Condition(self._future_lock)
        self._pending_futures: set[Future] = set()
        self._state = _ServiceState.OPEN
        self._session_context: Optional[SessionContext] = None

        _validate_resreq(resreq)
        if inspect.isclass(execution_object):
            _validate_service_class(execution_object)
        resource_requirement = ResourceRequirement.from_string(resreq) if resreq is not None else None

        session_id = short_name(app)

        # Create an app session.
        # For RL module: serialize ServiceContext with cloudpickle, put in cache to get ObjectRef,
        # then encode ObjectRef to bytes for core API
        app_context = ServiceContext(
            execution_object=execution_object,
            constructor_args=constructor_args,
            constructor_kwargs=constructor_kwargs or {},
            service_id=session_id,
            autoscale=autoscale,
            warmup=warmup,
        )
        # Serialize service modules by value. Importing such a module in an
        # executor would re-run its decorator without the client app runtime.
        execution_module = _execution_module(execution_object)
        with _cloudpickle_registry_lock:
            registered_modules = cloudpickle.list_registry_pickle_by_value()
            register_by_value = execution_module is not None and execution_module not in registered_modules
            if register_by_value:
                cloudpickle.register_pickle_by_value(execution_module)
            try:
                serialized_ctx = cloudpickle.dumps(
                    app_context,
                    protocol=cloudpickle.DEFAULT_PROTOCOL,
                )
            finally:
                if register_by_value:
                    cloudpickle.unregister_pickle_by_value(execution_module)
        # Put in cache with <app>/<session_id> key prefix
        key_prefix = f"{app}/{session_id}"
        logger.debug(f"[ServiceInstance] Putting ServiceContext in cache: key_prefix={key_prefix}, autoscale={app_context.autoscale}")
        object_ref = _core_put_object(key_prefix, serialized_ctx)
        logger.debug(f"[ServiceInstance] ServiceContext cached: key={object_ref.key}, version={object_ref.version}")
        # Encode ObjectRef to bytes for core API
        common_data_bytes = object_ref.encode()

        session_spec = SessionAttributes(
            id=session_id,
            application=app,
            common_data=common_data_bytes,
            min_instances=app_context.min_instances,
            max_instances=app_context.max_instances,
            batch_size=1,
            resreq=resource_requirement,
        )
        logger.info(f"[ServiceInstance] Opening session: session_id={session_id}, app={app}")
        try:
            self._session = core_client.open_session(session_id=session_id, spec=session_spec)
        except Exception as e:
            logger.error(f"[ServiceInstance] Failed to open session: {type(e).__name__}: {e}", exc_info=True)
            raise
        self._session_owner: _SessionOwner = _ServiceSessionOwner(self._session)

        logger.info(f"[ServiceInstance] Session opened: id={self._session.id}")

        # Generate wrapper methods for all public methods of the execution object
        self._generate_wrappers()

    def _generate_wrappers(self) -> None:
        """Generate wrapper functions for all public methods of the execution object.

        This method inspects the execution object and creates a wrapper for each
        public method (not starting with '_'). Each wrapper:
        - Converts ObjectFuture arguments to ObjectRef
        - Constructs a ServiceRequest
        - Submits a task via _session.run()
        - Returns an ObjectFuture
        """
        if not hasattr(self, "_method_names"):
            self._method_names = []

        # Determine if execution_object is a function or has methods
        if _is_function(self._execution_object):
            # It's a function, create a wrapper for direct invocation
            self._create_function_wrapper()
        else:
            # It's a class service, wrap all public methods.
            self._create_method_wrappers()

    def __reduce__(self):
        """Serialize this proxy as a reference to its existing session."""
        return (
            _restore_service_instance,
            (
                self._app,
                self._session.id,
                self._function_wrapper is not None,
                tuple(self._method_names),
            ),
        )

    def _create_function_wrapper(self) -> None:
        """Create a wrapper for a callable execution object (function)."""

        def wrapper(*args, **kwargs):
            option = kwargs.pop("option", None)
            # Convert ObjectFuture arguments to ObjectRef
            converted_args = tuple(arg.ref() if isinstance(arg, ObjectFuture) else arg for arg in args)
            converted_kwargs = {key: value.ref() if isinstance(value, ObjectFuture) else value for key, value in kwargs.items()}

            # Create a ServiceRequest with method=None for direct callable invocation
            request = ServiceRequest(
                method=None,
                args=converted_args if converted_args else None,
                kwargs=converted_kwargs if converted_kwargs else None,
            )

            # For RL module: serialize ServiceRequest with cloudpickle, then call core API
            request_bytes = cloudpickle.dumps(request, protocol=cloudpickle.DEFAULT_PROTOCOL)
            # Submit task and return ObjectFuture
            future = self._submit(request_bytes, option)
            return ObjectFuture(future)

        # Store the wrapper so __call__ can use it
        self._function_wrapper = wrapper
        logger.debug("Created callable wrapper for function execution object")

    def _create_method_wrappers(self) -> None:
        """Create wrappers for all public methods of a class/instance."""
        # Get all public methods (not starting with '_')
        for attr_name in dir(self._execution_object):
            if attr_name.startswith("_"):
                continue

            attr = getattr(self._execution_object, attr_name)
            if not callable(attr):
                continue

            if hasattr(type(self), attr_name) or attr_name in self.__dict__:
                raise TypeError(f"service method '{attr_name}' conflicts with ServiceInstance API")

            # Create a wrapper for this method
            wrapper = self._create_method_wrapper(attr_name)
            setattr(self, attr_name, wrapper)
            if attr_name not in self._method_names:
                self._method_names.append(attr_name)
            logger.debug(f"Created wrapper for method '{attr_name}'")

    def _create_method_wrapper(self, method_name: str) -> Callable:
        """Create a wrapper function for a specific method.

        Args:
            method_name: The name of the method to wrap

        Returns:
            A wrapper function that submits tasks and returns ObjectFuture
        """

        def wrapper(*args, **kwargs):
            option = kwargs.pop("option", None)
            # Convert ObjectFuture arguments to ObjectRef
            converted_args = tuple(arg.ref() if isinstance(arg, ObjectFuture) else arg for arg in args)
            converted_kwargs = {key: value.ref() if isinstance(value, ObjectFuture) else value for key, value in kwargs.items()}

            # Create a ServiceRequest for this method
            request = ServiceRequest(
                method=method_name,
                args=converted_args if len(converted_args) > 0 else None,
                kwargs=converted_kwargs if converted_kwargs and len(converted_kwargs) > 0 else None,
            )

            # For RL module: serialize ServiceRequest with cloudpickle, then call core API
            request_bytes = cloudpickle.dumps(request, protocol=cloudpickle.DEFAULT_PROTOCOL)
            logger.info(f"[ServiceInstance] Submitting task: method={method_name}, session={self._session.id}")
            # Submit task and return ObjectFuture
            future = self._submit(request_bytes, option)
            return ObjectFuture(future)

        return wrapper

    def _submit(self, request: bytes, option: Any) -> Future:
        """Submit and retain a task until it finishes so close can drain it."""
        future_lock = getattr(self, "_future_lock", None)
        if future_lock is None:
            # Supports lightweight test doubles built without __init__.
            return self._session.run(request, option=option)

        with future_lock:
            if self._state is not _ServiceState.OPEN:
                raise FlameError(FlameErrorCode.INVALID_STATE, "App service is closed")
            future = self._session.run(request, option=option)
            self._pending_futures.add(future)

        def discard(completed: Future) -> None:
            with self._future_lock:
                self._pending_futures.discard(completed)

        future.add_done_callback(discard)
        return future

    def __call__(self, *args, **kwargs) -> ObjectFuture:
        """Make ServiceInstance callable for function execution objects.

        This method allows calling the service directly when the execution object
        is a function (not a class or instance).

        Args:
            *args: Positional arguments to pass to the function
            **kwargs: Keyword arguments to pass to the function

        Returns:
            ObjectFuture that resolves to the function's result

        Raises:
            TypeError: If the execution object is not a callable function
        """
        if self._function_wrapper is None:
            raise TypeError(f"ServiceInstance for app '{self._app}' is not callable. The execution object is a class or instance, not a function. Call specific methods instead.")
        return self._function_wrapper(*args, **kwargs)

    def close(self) -> None:
        """Gracefully close the ServiceInstance and clean up resources.

        This closes the underlying session.
        """
        logger.debug(f"Closing ServiceInstance for app '{self._app}'")
        future_lock = getattr(self, "_future_lock", None)
        if future_lock is None:
            if getattr(self, "_state", _ServiceState.OPEN) is _ServiceState.CLOSED:
                return
            self._session_owner.close()
            self._state = _ServiceState.CLOSED
            return

        with self._state_changed:
            while self._state is _ServiceState.CLOSING:
                self._state_changed.wait()
            if self._state is _ServiceState.CLOSED:
                return
            self._state = _ServiceState.CLOSING
            pending = tuple(self._pending_futures)

        try:
            for future in pending:
                try:
                    future.result()
                except Exception:
                    # A failed task is complete and no longer prevents closing
                    # its session. Preserve its error on the ObjectFuture.
                    logger.debug("Task completed with an error during service close", exc_info=True)
            self._session_owner.close()
        except Exception:
            with self._state_changed:
                # Keep new submissions blocked after shutdown has begun, while
                # allowing a later close() call to retry transient failures.
                self._state = _ServiceState.CLOSE_FAILED
                self._state_changed.notify_all()
            raise
        else:
            with self._state_changed:
                self._state = _ServiceState.CLOSED
                self._state_changed.notify_all()


def _restore_service_instance(
    app: str,
    session_id: str,
    is_callable: bool,
    method_names: tuple[str, ...],
) -> ServiceInstance:
    """Restore a service proxy without creating or owning another session."""
    instance = object.__new__(ServiceInstance)
    instance._app = app
    instance._execution_object = None
    instance._function_wrapper = None
    instance._method_names = list(method_names)
    instance._future_lock = threading.Lock()
    instance._state_changed = threading.Condition(instance._future_lock)
    instance._pending_futures = set()
    instance._state = _ServiceState.OPEN
    instance._session_context = None
    instance._session = core_client.open_session(session_id=session_id)
    instance._session_owner = _NoopSessionOwner()
    if is_callable:
        instance._create_function_wrapper()
    for method_name in method_names:
        setattr(instance, method_name, instance._create_method_wrapper(method_name))
    return instance


def _nested_service_instance(
    session_context: SessionContext,
    execution_object: Any,
) -> ServiceInstance:
    """Create a proxy for an existing executor-bound session."""
    instance = object.__new__(ServiceInstance)
    instance._app = session_context.application.name
    instance._execution_object = execution_object
    instance._function_wrapper = None
    instance._method_names = []
    instance._future_lock = threading.Lock()
    instance._state_changed = threading.Condition(instance._future_lock)
    instance._pending_futures = set()
    instance._state = _ServiceState.OPEN
    instance._session_context = session_context
    instance._session = core_client.open_session(session_id=session_context.session_id)
    instance._session_owner = _NoopSessionOwner()
    instance._generate_wrappers()
    return instance


def _service_class_factory(
    app_name: str,
    execution_class: type,
    *,
    autoscale: Optional[bool],
    warmup: int,
    resreq: Optional[str],
) -> Callable[..., ServiceInstance]:
    """Return a class-like factory that creates executor-constructed services."""
    _validate_service_class(execution_class)

    @wraps(execution_class, updated=())
    def create_service(*args: Any, **kwargs: Any) -> ServiceInstance:
        with _runtime_lock:
            try:
                session_context = _context.session_context()
            except RuntimeError:
                session_context = None
            if session_context is not None:
                if autoscale is not None or warmup != 0 or resreq is not None:
                    raise FlameError(
                        FlameErrorCode.INVALID_ARGUMENT,
                        "Nested services reuse the current session and do not accept autoscale, warmup, or resreq",
                    )
                if args or kwargs:
                    raise TypeError("Nested class services reuse the current execution object and do not accept constructor arguments")
                return _nested_service_instance(session_context, execution_class)

            runtime = _runtime
            if runtime is None or runtime._name != app_name or runtime._state is not _RuntimeState.ACTIVE:
                raise FlameError(
                    FlameErrorCode.INVALID_STATE,
                    "The Flame app that declared this service class is not active",
                )
            return runtime._create_service_instance(
                execution_class,
                autoscale=autoscale,
                warmup=warmup,
                resreq=resreq,
                constructor_args=args,
                constructor_kwargs=kwargs,
            )

    return create_service


class _RuntimeState(Enum):
    INACTIVE = auto()
    ACTIVE = auto()
    CLOSING = auto()
    CLOSE_FAILED = auto()


class _ApplicationOwner(Protocol):
    def register(self) -> None: ...

    def unregister(self) -> None: ...


class _NoopApplicationOwner:
    """Represent an application registration owned by another process."""

    def register(self) -> None:
        pass

    def unregister(self) -> None:
        pass


class _RuntimeApplicationOwner:
    """Own an application registration created by this runtime."""

    def __init__(self, runtime: "_Runtime"):
        self._runtime = runtime

    def register(self) -> None:
        self._runtime._register_application()

    def unregister(self) -> None:
        self._runtime._unregister_application()


class _Runtime:
    """Manage the lifecycle and deployment of a Python package in Flame.

    This class automates the packaging, uploading, registration, and cleanup of
    Python applications within Flame. It can be used either as a context manager
    or with explicit close() call.

    Attributes:
        _name: The name of the application/package
        _services: List of ServiceInstance objects created within this context
        _package_path: Path to the created package file
        _application_owner: Registration strategy for a reused or owned app
        _storage_backend: Storage backend instance for uploading/deleting packages
        _state: Current runtime lifecycle state
        _fail_if_exists: Whether to raise an exception if the application already exists
        _dependencies: List of pip dependencies for auto-generated pyproject.toml
        _python_version: Optional Python version to use for execution (e.g., "3.12")
    """

    def __init__(
        self,
        name: str,
        fail_if_exists: bool = False,
        dependencies: Optional[List[str]] = None,
        python_version: Optional[str] = None,
    ):
        """Initialize and start an app runtime.

        Args:
            name: The name of the application/package
            fail_if_exists: If True, raise an exception if the application already exists.
                           If False (default), reuse an existing enabled application. Existing
                           disabled applications always produce an error.
            dependencies: List of pip dependencies (e.g., ["numpy", "pandas>=2.0"]).
                         If provided and no pyproject.toml exists, one will be auto-generated.
            python_version: Python version to use for execution.
                           If omitted, the executor uses the latest installed Flame Python SDK.
        """
        self._name = name
        self._services: List[ServiceInstance] = []
        self._package_path: Optional[str] = None
        self._package_filename: Optional[str] = None
        self._application_owner: _ApplicationOwner = _NoopApplicationOwner()
        self._lifecycle_lock = threading.RLock()
        self._context = FlameContext()
        self._storage_backend: Optional[StorageBackend] = None
        self._state = _RuntimeState.INACTIVE
        self._fail_if_exists = fail_if_exists
        self._dependencies = dependencies
        self._python_version = python_version

        logger.debug(f"Initialized app runtime '{name}' (fail_if_exists={fail_if_exists}, dependencies={dependencies}, python_version={python_version})")

        self._start()

    def _start(self) -> None:
        """Internal method to start the app and set up the application environment.

        Steps:
        1. Check if application already exists (skip packaging if reusing)
        2. Package the current working directory into a .tar.gz archive
        3. Upload the package to the storage location
        4. Retrieve the flmrun application template
        5. Register a new application with the package URL

        Raises:
            FlameError: If setup fails at any step
        """
        with self._lifecycle_lock:
            if self._state is _RuntimeState.ACTIVE:
                logger.debug(f"App runtime '{self._name}' already started, skipping")
                return
            if self._state is not _RuntimeState.INACTIVE:
                raise FlameError(
                    FlameErrorCode.INVALID_STATE,
                    f"Flame app '{self._name}' cannot start while {self._state.name.lower()}",
                )

            logger.debug(f"Starting app runtime '{self._name}'")

            application = core_client.get_application(self._name)
            if application is not None:
                if application.state == ApplicationState.DISABLED:
                    raise FlameError(
                        FlameErrorCode.INVALID_STATE,
                        f"Application '{self._name}' is disabled and cannot be reused.",
                    )
                if self._fail_if_exists:
                    raise FlameError(
                        FlameErrorCode.ALREADY_EXISTS,
                        f"Application '{self._name}' already exists. Set fail_if_exists=False to skip registration.",
                    )
                logger.debug(f"Application '{self._name}' already exists, reusing registration")
                self._application_owner = _NoopApplicationOwner()
            else:
                self._application_owner = _RuntimeApplicationOwner(self)

            self._application_owner.register()
            self._state = _RuntimeState.ACTIVE

    def _register_application(self) -> None:
        """Package, upload, and register an application owned by this runtime."""

        # Initialize storage backend (uses cache.endpoint if package.storage not set)
        storage_base = self._context.package.storage if self._context.package else None
        if storage_base is None and self._context.cache is None:
            raise FlameError(FlameErrorCode.INVALID_CONFIG, "Storage not configured. Please set 'cache.endpoint' or 'package.storage' in flame.yaml.")
        self._storage_backend = create_storage_backend(storage_base, app_name=self._name)
        logger.debug(f"Initialized storage backend: {type(self._storage_backend).__name__}")

        # Step 1: Package the current working directory
        self._package_path = self._create_package()
        logger.debug(f"Created package: {self._package_path}")

        # Step 2: Upload the package to storage
        storage_url = self._upload_package()
        logger.debug(f"Uploaded package to: {storage_url}")

        # Step 3: Retrieve the application template
        # Use configured template if available, otherwise default to flmrun
        template_name = self._context.app

        try:
            template_app = core_client.get_application(template_name)
            logger.debug(f"Retrieved application template: {template_name}")
        except Exception as e:
            self._cleanup_package_artifacts()
            raise FlameError(FlameErrorCode.INTERNAL, f"Failed to get application template '{template_name}': {str(e)}")

        # Register the new application
        try:
            working_directory = None
            if template_app.working_directory is not None and template_app.working_directory != "":
                working_directory = f"{template_app.working_directory}/{self._name}"

            logger.debug(f"Working directory: {working_directory}")

            environments = dict(template_app.environments) if template_app.environments else {}
            if self._python_version:
                environments["FLAME_PYTHON_VERSION"] = self._python_version

            app_attrs = ApplicationAttributes(
                shim=template_app.shim,
                image=template_app.image,
                command=template_app.command,
                description=f"App application: {self._name}",
                labels=template_app.labels,
                arguments=template_app.arguments,
                environments=environments,
                working_directory=working_directory,
                max_instances=template_app.max_instances,
                delay_release=template_app.delay_release,
                schema=template_app.schema,
                url=storage_url,
                installer=template_app.installer,
            )

            core_client.register_application(self._name, app_attrs)
            logger.debug(f"Registered application '{self._name}' with working directory: {working_directory}")
        except FlameError:
            self._cleanup_package_artifacts()
            raise
        except Exception as e:
            self._cleanup_package_artifacts()
            raise FlameError(FlameErrorCode.INTERNAL, f"Failed to register application: {str(e)}")

    def __enter__(self) -> "_Runtime":
        """Enter the context manager and set up the application environment.

        Returns:
            self for use in the with statement

        Raises:
            FlameError: If setup fails at any step
        """
        self._start()
        return self

    def __exit__(self, exc_type, exc_val, exc_tb) -> None:
        """Exit the context manager and clean up resources."""
        self.close()

    def close(self) -> None:
        """Close the app runtime and clean up all owned resources.

        This method can be called explicitly or is automatically called when
        exiting the context manager. It performs the following cleanup:
        1. Closes all ServiceInstance objects.
        2. Unregisters an application created by this runtime. Remote package
           garbage collection is owned by Flame Object Cache.

        Existing applications are borrowed; their registration, cache, and
        package are retained. Sessions opened by this runtime are still closed.
        """
        with self._lifecycle_lock:
            if self._state is _RuntimeState.INACTIVE:
                logger.debug(f"App runtime '{self._name}' not started, nothing to close")
                return
            if self._state not in (
                _RuntimeState.ACTIVE,
                _RuntimeState.CLOSE_FAILED,
            ):
                raise FlameError(
                    FlameErrorCode.INVALID_STATE,
                    f"Flame app '{self._name}' cannot close while {self._state.name.lower()}",
                )

            self._state = _RuntimeState.CLOSING
            close_errors = []
            for service in self._services:
                try:
                    service.close()
                except Exception as e:
                    logger.error(f"Error closing service: {e}", exc_info=True)
                    close_errors.append(e)
            if close_errors:
                self._state = _RuntimeState.CLOSE_FAILED
                raise FlameError(
                    FlameErrorCode.INTERNAL,
                    f"Failed to close {len(close_errors)} app service session(s); application cleanup was skipped",
                ) from close_errors[0]

            try:
                self._application_owner.unregister()
            except Exception:
                self._state = _RuntimeState.CLOSE_FAILED
                raise

            self._services.clear()
            self._state = _RuntimeState.INACTIVE

    def _unregister_application(self) -> None:
        """Release the registration owned by this runtime."""
        logger.debug(f"Closing app runtime '{self._name}'")

        core_client.unregister_application(self._name)
        logger.debug(f"Unregistered application '{self._name}'")
        self._cleanup_local_package()

    def service(
        self,
        *,
        autoscale: Optional[bool] = None,
        warmup: int = 0,
        resreq: Optional[str] = None,
    ) -> Callable[[Any], Any]:
        """Decorate an execution object and return its remote service proxy.

        Args:
            autoscale: Functions, builtins, and classes can autoscale; their
                      default is True.
            warmup: Number of instances to pre-create at session start. When
                   autoscale=False, this sets the fixed instance count. With
                   warmup=0, fixed services create one instance and autoscaled
                   services start from zero. Default: 0.
            resreq: Optional resource requirements string, for example
                    ``"cpu=1,mem=1g"``. When omitted, the server applies
                    cluster.resource_requirement (or its fallback).

        Returns:
            A decorator that accepts a function or class. Functions become a
            ServiceInstance; classes become factories for ServiceInstance values.

        Raises:
            TypeError: If the decorated object is not a function or class.
        """
        _validate_resreq(resreq)

        def decorator(execution_object: Any) -> Any:
            logger.debug(f"Creating service for {type(execution_object).__name__} (autoscale={autoscale}, warmup={warmup})")
            if inspect.isclass(execution_object):
                return _service_class_factory(
                    self._name,
                    execution_object,
                    autoscale=autoscale,
                    warmup=warmup,
                    resreq=resreq,
                )
            if not _is_function(execution_object):
                raise TypeError("app.service() supports a function or class only")
            return self._create_service_instance(
                execution_object,
                autoscale=autoscale,
                warmup=warmup,
                resreq=resreq,
            )

        return decorator

    def _create_service_instance(
        self,
        execution_object: Any,
        *,
        autoscale: Optional[bool] = None,
        warmup: int = 0,
        resreq: Optional[str] = None,
        constructor_args: Optional[tuple[Any, ...]] = None,
        constructor_kwargs: Optional[dict[str, Any]] = None,
    ) -> ServiceInstance:
        """Create and retain one owned service proxy."""
        with self._lifecycle_lock:
            if self._state is not _RuntimeState.ACTIVE:
                raise FlameError(
                    FlameErrorCode.INVALID_STATE,
                    f"Flame app '{self._name}' is not active",
                )

            app_service = ServiceInstance(
                self._name,
                execution_object,
                autoscale=autoscale,
                warmup=warmup,
                resreq=resreq,
                constructor_args=constructor_args,
                constructor_kwargs=constructor_kwargs,
            )
            self._services.append(app_service)

        logger.debug(f"Created service for execution object in app '{self._name}'")
        return app_service

    def get(self, futures: List[ObjectFuture]) -> List[Any]:
        """Resolve multiple ObjectFuture values to their concrete results.

        Args:
            futures: List of ObjectFuture instances

        Returns:
            List of concrete results corresponding to each ObjectFuture
        """
        return [future.get() for future in futures]

    def ref(self, futures: List[ObjectFuture]) -> List[ObjectRef]:
        """Resolve multiple ObjectFuture values to their ObjectRef references.

        Args:
            futures: List of ObjectFuture instances

        Returns:
            List of ObjectRef instances corresponding to each ObjectFuture
        """
        return [future.ref() for future in futures]

    def wait(self, futures: List[ObjectFuture]) -> None:
        """Wait for multiple ObjectFuture values to complete.

        Args:
            futures: List of ObjectFuture instances
        """
        for future in futures:
            future.wait()

    def select(self, futures: List[ObjectFuture]) -> ObjectFutureIterator:
        """Return an iterator over futures as they complete.

        Args:
            futures: List of ObjectFuture instances

        Returns:
            ObjectFutureIterator yielding futures in completion order
        """
        return ObjectFutureIterator(futures)

    def put(self, obj: Any) -> ObjectRef:
        """Put an object into the cache with <app_name>/shared key prefix.

        Args:
            obj: The object to cache (will be pickled)

        Returns:
            ObjectRef pointing to the cached object
        """
        from flamepy.core.cache import ObjectKey, put_object

        object_key = ObjectKey.for_shared(self._name)
        return put_object(object_key.to_prefix(), obj)

    def _create_package(self) -> str:
        """Create a .tar.gz package of the current working directory.

        Applies exclusion patterns from FlameContext.package.excludes.

        Returns:
            Path to the created package file

        Raises:
            FlameError: If package creation fails
        """
        cwd = os.getcwd()
        dist_dir = os.path.join(cwd, "dist")

        # Create dist directory if it doesn't exist
        os.makedirs(dist_dir, exist_ok=True)

        generated_pyproject = self._generated_pyproject_toml(cwd)

        package_filename = f"{self._name}-{uuid.uuid4().hex}.tar.gz"
        package_path = os.path.join(dist_dir, package_filename)

        default_excludes = [
            ".venv",
            "venv",
            "__pycache__",
            ".pytest_cache",
            ".ruff_cache",
            ".mypy_cache",
            "*.egg-info",
            ".git",
            ".tox",
            "node_modules",
            "*.pyc",
            "*.pyo",
            ".DS_Store",
        ]

        user_excludes = self._context.package.excludes if self._context.package else []
        excludes = list(set(default_excludes + user_excludes))

        logger.debug(f"Creating package with excludes: {excludes}")

        try:
            with tarfile.open(package_path, "w:gz") as tar:
                # Add files while respecting exclusions
                for item in os.listdir(cwd):
                    # Skip the dist directory (where the package is created)
                    if item == "dist":
                        continue

                    # Check if item matches any exclusion pattern
                    if self._should_exclude(item, excludes):
                        logger.debug(f"Excluding: {item}")
                        continue

                    item_path = os.path.join(cwd, item)
                    tar.add(item_path, arcname=item, recursive=True, filter=lambda tarinfo: None if self._should_exclude(tarinfo.name, excludes) else tarinfo)

                if generated_pyproject is not None:
                    data = generated_pyproject.encode("utf-8")
                    tarinfo = tarfile.TarInfo("pyproject.toml")
                    tarinfo.size = len(data)
                    tarinfo.mode = 0o644
                    tar.addfile(tarinfo, io.BytesIO(data))

            logger.debug(f"Created package: {package_path}")
            return package_path

        except Exception as e:
            if os.path.exists(package_path):
                os.remove(package_path)
            raise FlameError(FlameErrorCode.INTERNAL, f"Failed to create package: {str(e)}")

    def _should_exclude(self, name: str, patterns: List[str]) -> bool:
        import fnmatch

        for pattern in patterns:
            if fnmatch.fnmatch(name, pattern) or fnmatch.fnmatch(os.path.basename(name), pattern):
                return True
        return False

    def _generated_pyproject_toml(self, cwd: str) -> Optional[str]:
        """Return generated package metadata when the source tree needs it."""
        if os.path.exists(os.path.join(cwd, "pyproject.toml")):
            logger.debug("pyproject.toml already exists, skipping generated metadata")
            return None

        has_legacy_metadata = os.path.exists(os.path.join(cwd, "setup.py")) or os.path.exists(os.path.join(cwd, "setup.cfg"))
        if has_legacy_metadata:
            if self._dependencies:
                logger.warning("Python package metadata (setup.py/setup.cfg) already exists. Skipping pyproject.toml generation to avoid conflicting with existing metadata. Please specify dependencies in your setup.py or setup.cfg.")
            else:
                logger.debug("Python package metadata already exists, skipping generated metadata")
            return None

        deps = sorted(self._dependencies or [])
        if deps:
            deps_block = "dependencies = [\n    " + ",\n    ".join(f'"{dep}"' for dep in deps) + ",\n]"
        else:
            deps_block = "dependencies = []"

        content = f'''[build-system]
requires = ["setuptools>=61.0", "wheel"]
build-backend = "setuptools.build_meta"

[project]
name = "{self._name}"
version = "0.1.0"
requires-python = ">=3.9"
{deps_block}

[tool.setuptools]
py-modules = []
'''

        logger.info(f"Generated package pyproject.toml with dependencies: {deps}")
        return content

    def _upload_package(self) -> str:
        """Upload the package to the storage location.

        Uses the configured storage backend to upload the package.

        Returns:
            The full URL to the uploaded package

        Raises:
            FlameError: If upload fails
        """
        if not self._package_path:
            raise FlameError(FlameErrorCode.INVALID_STATE, "Package path is not set")

        if not self._storage_backend:
            raise FlameError(FlameErrorCode.INVALID_STATE, "Storage backend is not initialized")

        package_filename = os.path.basename(self._package_path)
        self._package_filename = package_filename
        try:
            return self._storage_backend.upload(self._package_path, package_filename)
        except Exception:
            self._cleanup_package_artifacts()
            raise

    def _cleanup_package_artifacts(self) -> None:
        """Remove uploaded and local package artifacts owned by this runtime."""
        self._cleanup_storage()
        self._cleanup_local_package()

    def _cleanup_local_package(self) -> None:
        """Remove the local package archive."""
        if self._package_path and os.path.exists(self._package_path):
            try:
                os.remove(self._package_path)
                logger.debug(f"Removed local package: {self._package_path}")
            except Exception as e:
                logger.error(f"Error removing local package: {e}", exc_info=True)

    def _cleanup_storage(self) -> None:
        """Delete the package from storage."""
        if not self._package_filename or not self._storage_backend:
            return

        try:
            self._storage_backend.delete(self._package_filename)
        except Exception as e:
            logger.error(f"Error cleaning up storage: {e}", exc_info=True)


_runtime_lock = threading.RLock()
_runtime: Optional[_Runtime] = None


def init(
    name: str,
    *,
    fail_if_exists: bool = False,
    dependencies: Optional[List[str]] = None,
    python_version: Optional[str] = None,
) -> _Runtime:
    """Initialize the process-wide Flame application runtime.

    Return the active runtime, which owns service declaration and application
    lifecycle. Repeating ``init`` for the active application returns the same
    runtime. Initialize a
    different application only after calling :func:`destroy`. Each module may
    safely repeat initialization with the same name before its own
    ``@service()`` declarations.
    """
    global _runtime

    with _runtime_lock:
        if _runtime is not None:
            if _runtime._name == name:
                if getattr(_runtime, "_state", None) is not _RuntimeState.INACTIVE:
                    return _runtime
                # A handle returned by init() may be closed directly (including
                # by a with block). Do not leave that inactive handle installed
                # as the process-wide runtime: a subsequent init starts a fresh
                # lifecycle with new service sessions and ownership state.
                _runtime = None
            else:
                raise FlameError(
                    FlameErrorCode.INVALID_STATE,
                    f"Flame app '{_runtime._name}' is already initialized; call flamepy.app.destroy() first",
                )

        runtime = _Runtime(
            name,
            fail_if_exists=fail_if_exists,
            dependencies=dependencies,
            python_version=python_version,
        )
        _runtime = runtime
        return runtime


def destroy() -> None:
    """Destroy the process-wide Flame application runtime if initialized."""
    global _runtime

    with _runtime_lock:
        runtime = _runtime
        if runtime is None:
            return
        runtime.close()
        _runtime = None


def _require_runtime() -> _Runtime:
    with _runtime_lock:
        if _runtime is None:
            raise FlameError(FlameErrorCode.INVALID_STATE, "Flame is not initialized; call flamepy.app.init(name) first")
        return _runtime


def service(
    *,
    autoscale: Optional[bool] = None,
    warmup: int = 0,
    resreq: Optional[str] = None,
) -> Callable[[Any], Any]:
    """Return a service decorator for the initialized process-wide app."""
    _validate_resreq(resreq)

    def decorator(execution_object: Any) -> Any:
        with _runtime_lock:
            try:
                session_context = _context.session_context()
            except RuntimeError:
                session_context = None
            if session_context is not None:
                if autoscale is not None or warmup != 0 or resreq is not None:
                    raise FlameError(
                        FlameErrorCode.INVALID_ARGUMENT,
                        "Nested services reuse the current session and do not accept autoscale, warmup, or resreq",
                    )
                if inspect.isclass(execution_object):
                    return _service_class_factory(
                        session_context.application.name,
                        execution_object,
                        autoscale=autoscale,
                        warmup=warmup,
                        resreq=resreq,
                    )
                if not _is_function(execution_object):
                    raise TypeError("app.service() supports a function or class only")
                return _nested_service_instance(session_context, execution_object)
            if _runtime is None:
                raise FlameError(
                    FlameErrorCode.INVALID_STATE,
                    "Flame is not initialized; call flamepy.app.init(name) before declaring services",
                )
            return _runtime.service(autoscale=autoscale, warmup=warmup, resreq=resreq)(execution_object)

    return decorator


def get(futures: List[ObjectFuture]) -> List[Any]:
    """Resolve multiple object futures through the active application."""
    return _require_runtime().get(futures)


def ref(futures: List[ObjectFuture]) -> List[ObjectRef]:
    """Resolve multiple object references through the active application."""
    return _require_runtime().ref(futures)


def wait(futures: List[ObjectFuture]) -> None:
    """Wait for multiple object futures through the active application."""
    _require_runtime().wait(futures)


def select(futures: List[ObjectFuture]) -> ObjectFutureIterator:
    """Iterate over object futures as they complete."""
    return _require_runtime().select(futures)


def put(obj: Any) -> ObjectRef:
    """Store an object under the active application's shared cache prefix."""
    return _require_runtime().put(obj)


__all__ = [
    "ServiceInstance",
    "ObjectFuture",
    "ObjectFutureIterator",
    "destroy",
    "get",
    "init",
    "put",
    "ref",
    "select",
    "service",
    "wait",
]
