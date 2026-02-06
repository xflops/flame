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
import logging
import os
import tarfile
from abc import ABC, abstractmethod
from concurrent.futures import Future, as_completed
from typing import Any, Callable, List, Optional

import cloudpickle

from flamepy.core import ObjectRef, get_object, put_object
from flamepy.core.client import create_session, get_application, register_application, unregister_application
from flamepy.core.types import (
    ApplicationAttributes,
    FlameContext,
    FlameError,
    FlameErrorCode,
    short_name,
)
from flamepy.runner.storage import StorageBackend, create_storage_backend
from flamepy.runner.types import (
    RunnerContext,
    RunnerRequest,
)

logger = logging.getLogger(__name__)


class ObjectFuture:
    """Encapsulates a future that resolves to an ObjectRef.

    This class manages asynchronous and deferred computation results in runner services.
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
        """Initialize an iterator from a list of ObjectFuture instances."""
        self._future_map = {future._future: future for future in futures}

    def __iter__(self):
        for future in as_completed(self._future_map):
            yield self._future_map[future]


class RunnerService:
    """Encapsulates an execution object for remote invocation within Flame.

    This class creates a session with the flamepy.runner.runpy service and dynamically
    generates wrapper methods for all public methods of the execution object.
    Each wrapper submits tasks to the session and returns ObjectFuture instances.

    Attributes:
        _app: The name of the application registered in Flame
        _execution_object: The Python execution object being managed
        _session: The Flame session for task execution
    """

    def __init__(self, app: str, execution_object: Any, stateful: bool = False, autoscale: bool = True):
        """Initialize a RunnerService.

        Args:
            app: The name of the application registered in Flame.
                 The associated service must be flamepy.runner.runpy.
            execution_object: The Python execution object to be managed and
                             exposed as a remote service.
            stateful: If True, persist the execution object state back to flame-cache
                     after each task. If False, do not persist state.
            autoscale: If True, create instances dynamically based on pending tasks (min=0, max=None).
                      If False, create exactly one instance (min=1, max=1).
        """
        self._app = app
        self._execution_object = execution_object
        self._function_wrapper = None  # For callable functions

        # Create a session with flamepy.runner.runpy service
        # For RL module: serialize RunnerContext with cloudpickle, put in cache to get ObjectRef,
        # then encode ObjectRef to bytes for core API
        runner_context = RunnerContext(execution_object=execution_object, stateful=stateful, autoscale=autoscale)
        # Serialize the context using cloudpickle
        serialized_ctx = cloudpickle.dumps(runner_context, protocol=cloudpickle.DEFAULT_PROTOCOL)
        # Generate a temporary session_id for caching (will be regenerated by create_session if needed)
        temp_session_id = short_name(app)
        # Put in cache to get ObjectRef (use app as application_id)
        object_ref = put_object(app, temp_session_id, serialized_ctx)
        # Encode ObjectRef to bytes for core API
        common_data_bytes = object_ref.encode()
        # Pass min_instances and max_instances from RunnerContext to create_session
        self._session = create_session(application=app, common_data=common_data_bytes, session_id=temp_session_id, min_instances=runner_context.min_instances, max_instances=runner_context.max_instances)

        logger.debug(f"Created RunnerService for app '{app}' with session '{self._session.id}' (stateful={stateful}, autoscale={autoscale})")

        # Generate wrapper methods for all public methods of the execution object
        self._generate_wrappers()

    def _generate_wrappers(self) -> None:
        """Generate wrapper functions for all public methods of the execution object.

        This method inspects the execution object and creates a wrapper for each
        public method (not starting with '_'). Each wrapper:
        - Converts ObjectFuture arguments to ObjectRef
        - Constructs a RunnerRequest
        - Submits a task via _session.run()
        - Returns an ObjectFuture
        """
        # Determine if execution_object is a function or has methods
        if callable(self._execution_object) and not inspect.isclass(self._execution_object):
            # It's a function, create a wrapper for direct invocation
            self._create_function_wrapper()
        else:
            # It's a class or instance, wrap all public methods
            self._create_method_wrappers()

    def _create_function_wrapper(self) -> None:
        """Create a wrapper for a callable execution object (function)."""

        def wrapper(*args, **kwargs):
            # Convert ObjectFuture arguments to ObjectRef
            converted_args = tuple(arg.ref() if isinstance(arg, ObjectFuture) else arg for arg in args)
            converted_kwargs = {key: value.ref() if isinstance(value, ObjectFuture) else value for key, value in kwargs.items()}

            # Create a RunnerRequest with method=None for direct callable invocation
            request = RunnerRequest(
                method=None,
                args=converted_args if converted_args else None,
                kwargs=converted_kwargs if converted_kwargs else None,
            )

            # For RL module: serialize RunnerRequest with cloudpickle, then call core API
            request_bytes = cloudpickle.dumps(request, protocol=cloudpickle.DEFAULT_PROTOCOL)
            # Submit task and return ObjectFuture
            future = self._session.run(request_bytes)
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

            # Create a wrapper for this method
            wrapper = self._create_method_wrapper(attr_name)
            setattr(self, attr_name, wrapper)
            logger.debug(f"Created wrapper for method '{attr_name}'")

    def _create_method_wrapper(self, method_name: str) -> Callable:
        """Create a wrapper function for a specific method.

        Args:
            method_name: The name of the method to wrap

        Returns:
            A wrapper function that submits tasks and returns ObjectFuture
        """

        def wrapper(*args, **kwargs):
            # Convert ObjectFuture arguments to ObjectRef
            converted_args = tuple(arg.ref() if isinstance(arg, ObjectFuture) else arg for arg in args)
            converted_kwargs = {key: value.ref() if isinstance(value, ObjectFuture) else value for key, value in kwargs.items()}

            # Create a RunnerRequest for this method
            request = RunnerRequest(
                method=method_name,
                args=converted_args if len(converted_args) > 0 else None,
                kwargs=converted_kwargs if converted_kwargs and len(converted_kwargs) > 0 else None,
            )

            # For RL module: serialize RunnerRequest with cloudpickle, then call core API
            request_bytes = cloudpickle.dumps(request, protocol=cloudpickle.DEFAULT_PROTOCOL)
            # Submit task and return ObjectFuture
            future = self._session.run(request_bytes)
            return ObjectFuture(future)

        return wrapper

    def __call__(self, *args, **kwargs) -> ObjectFuture:
        """Make RunnerService callable for function execution objects.

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
            raise TypeError(f"RunnerService for app '{self._app}' is not callable. The execution object is a class or instance, not a function. Call specific methods instead.")
        return self._function_wrapper(*args, **kwargs)

    def close(self) -> None:
        """Gracefully close the RunnerService and clean up resources.

        This closes the underlying session.
        """
        logger.debug(f"Closing RunnerService for app '{self._app}'")
        self._session.close()


class Runner:
    """Context manager for managing lifecycle and deployment of Python packages in Flame.

    This class automates the packaging, uploading, registration, and cleanup of
    Python applications within Flame. It can be used either as a context manager
    or with explicit close() call.

    Attributes:
        _name: The name of the application/package
        _services: List of RunnerService instances created within this context
        _package_path: Path to the created package file
        _app_registered: Whether the application was successfully registered
        _storage_backend: Storage backend instance for uploading/deleting packages
        _started: Whether the runner has been started
        _fail_if_exists: Whether to raise an exception if the application already exists
    """

    def __init__(self, name: str, fail_if_exists: bool = False):
        """Initialize and start a Runner.

        Args:
            name: The name of the application/package
            fail_if_exists: If True, raise an exception if the application already exists.
                           If False (default), skip registration if the application already exists.
        """
        self._name = name
        self._services: List[RunnerService] = []
        self._package_path: Optional[str] = None
        self._app_registered = False
        self._context = FlameContext()
        self._storage_backend: Optional[StorageBackend] = None
        self._started = False
        self._fail_if_exists = fail_if_exists

        logger.debug(f"Initialized Runner '{name}' (fail_if_exists={fail_if_exists})")

        self._start()

    def _start(self) -> None:
        """Internal method to start the runner and set up the application environment.

        Steps:
        1. Package the current working directory into a .tar.gz archive
        2. Upload the package to the storage location
        3. Retrieve the flmrun application template
        4. Register a new application with the package URL

        Raises:
            FlameError: If setup fails at any step
        """
        if self._started:
            logger.debug(f"Runner '{self._name}' already started, skipping")
            return

        logger.debug(f"Starting Runner '{self._name}'")

        # Check that package configuration is available
        if self._context.package is None:
            raise FlameError(FlameErrorCode.INVALID_CONFIG, "Package configuration is not set in FlameContext. Please configure the 'package' field in your flame.yaml.")

        # Initialize storage backend
        storage_base = self._context.package.storage
        self._storage_backend = create_storage_backend(storage_base)
        logger.debug(f"Initialized storage backend: {type(self._storage_backend).__name__}")

        # Step 1: Package the current working directory
        self._package_path = self._create_package()
        logger.debug(f"Created package: {self._package_path}")

        # Step 2: Upload the package to storage
        storage_url = self._upload_package()
        logger.debug(f"Uploaded package to: {storage_url}")

        # Step 3: Retrieve the application template
        # Use configured template if available, otherwise default to flmrun
        template_name = self._context.runner.template

        try:
            template_app = get_application(template_name)
            logger.debug(f"Retrieved application template: {template_name}")
        except Exception as e:
            # Clean up the package file
            if self._package_path and os.path.exists(self._package_path):
                os.remove(self._package_path)
            raise FlameError(FlameErrorCode.INTERNAL, f"Failed to get application template '{template_name}': {str(e)}")

        # Check if application already exists
        existing_app = get_application(self._name)
        if existing_app is not None:
            if self._fail_if_exists:
                self._cleanup_storage()
                if self._package_path and os.path.exists(self._package_path):
                    os.remove(self._package_path)
                raise FlameError(FlameErrorCode.ALREADY_EXISTS, f"Application '{self._name}' already exists. Set fail_if_exists=False to skip registration.")
            else:
                logger.debug(f"Application '{self._name}' already exists, skipping registration")
                self._started = True
                return

        # Register the new application
        try:
            working_directory = None
            if template_app.working_directory is not None and template_app.working_directory != "":
                working_directory = f"{template_app.working_directory}/{self._name}"

            logger.debug(f"Working directory: {working_directory}")

            app_attrs = ApplicationAttributes(
                shim=template_app.shim,
                image=template_app.image,
                command=template_app.command,
                description=f"Runner application: {self._name}",
                labels=template_app.labels,
                arguments=template_app.arguments,
                environments=template_app.environments,
                working_directory=working_directory,
                max_instances=template_app.max_instances,
                delay_release=template_app.delay_release,
                schema=template_app.schema,
                url=storage_url,
            )

            register_application(self._name, app_attrs)
            self._app_registered = True
            self._started = True
            logger.debug(f"Registered application '{self._name}' with working directory: {working_directory}")
        except FlameError:
            raise
        except Exception as e:
            self._cleanup_storage()
            if self._package_path and os.path.exists(self._package_path):
                os.remove(self._package_path)
            raise FlameError(FlameErrorCode.INTERNAL, f"Failed to register application: {str(e)}")

    def __enter__(self) -> "Runner":
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
        """Close the Runner and clean up all resources.

        This method can be called explicitly or is automatically called when
        exiting the context manager. It performs the following cleanup:
        1. Closes all RunnerService instances
        2. Unregisters the application (if registered)
        3. Deletes the package from storage
        4. Removes the local package file
        """
        if not self._started:
            logger.debug(f"Runner '{self._name}' not started, nothing to close")
            return

        logger.debug(f"Closing Runner '{self._name}'")

        for service in self._services:
            try:
                service.close()
            except Exception as e:
                logger.error(f"Error closing service: {e}", exc_info=True)

        if self._app_registered:
            try:
                unregister_application(self._name)
                self._app_registered = False
                logger.debug(f"Unregistered application '{self._name}'")
            except Exception as e:
                logger.error(f"Error unregistering application: {e}", exc_info=True)

        self._cleanup_storage()

        if self._package_path and os.path.exists(self._package_path):
            try:
                os.remove(self._package_path)
                logger.debug(f"Removed local package: {self._package_path}")
            except Exception as e:
                logger.error(f"Error removing local package: {e}", exc_info=True)

        self._started = False

    def service(self, execution_object: Any, stateful: Optional[bool] = None, autoscale: Optional[bool] = None) -> RunnerService:
        """Create a RunnerService for the given execution object.

        Args:
            execution_object: A function, class, or class instance to expose as a service
            stateful: If True, persist the execution object state back to flame-cache
                     after each task. If False, do not persist state. If None, use default
                     based on execution_object type (default: False for all types).
            autoscale: If True, create instances dynamically based on pending tasks (min=0, max=None).
                      If False, create exactly one instance (min=1, max=1).
                      If None, use default based on execution_object type.

        Returns:
            A RunnerService instance

        Raises:
            ValueError: If stateful=True is set for a class (only instances can be stateful)
        """
        # Step 1: Determine execution object type
        is_function = callable(execution_object) and not inspect.isclass(execution_object)
        is_class = inspect.isclass(execution_object)

        # Step 2: Apply defaults if not specified
        if stateful is None:
            stateful = False  # All types default to False

        if autoscale is None:
            if is_function:
                autoscale = True  # Functions benefit from autoscaling
            else:  # class or instance
                autoscale = False  # Classes/instances typically want single instance

        # Step 3: Validation
        if stateful and is_class:
            raise ValueError("Cannot set stateful=True for a class. Classes themselves cannot maintain state; only instances can. Pass an instance instead, or set stateful=False.")

        # Step 4: Do NOT instantiate classes (keep as-is)
        # The class will be instantiated on each executor in FlameRunpyService.on_session_enter
        logger.debug(f"Creating service for {type(execution_object).__name__} (stateful={stateful}, autoscale={autoscale})")

        # Step 5: Create the RunnerService
        runner_service = RunnerService(self._name, execution_object, stateful=stateful, autoscale=autoscale)
        self._services.append(runner_service)

        logger.debug(f"Created service for execution object in Runner '{self._name}'")
        return runner_service

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

        package_filename = f"{self._name}.tar.gz"
        package_path = os.path.join(dist_dir, package_filename)

        # Get exclusion patterns
        excludes = self._context.package.excludes if self._context.package else []

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

            logger.debug(f"Created package: {package_path}")
            return package_path

        except Exception as e:
            raise FlameError(FlameErrorCode.INTERNAL, f"Failed to create package: {str(e)}")

    def _should_exclude(self, name: str, patterns: List[str]) -> bool:
        """Check if a file/directory name should be excluded.

        Args:
            name: The file or directory name
            patterns: List of exclusion patterns (supports wildcards)

        Returns:
            True if the name should be excluded
        """
        import fnmatch

        for pattern in patterns:
            if fnmatch.fnmatch(name, pattern) or fnmatch.fnmatch(os.path.basename(name), pattern):
                return True
        return False

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
        return self._storage_backend.upload(self._package_path, package_filename)

    def _cleanup_storage(self) -> None:
        """Delete the package from storage."""
        if not self._package_path or not self._storage_backend:
            return

        try:
            package_filename = os.path.basename(self._package_path)
            self._storage_backend.delete(package_filename)
        except Exception as e:
            logger.error(f"Error cleaning up storage: {e}", exc_info=True)
