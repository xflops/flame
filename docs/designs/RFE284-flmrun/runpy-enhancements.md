# FlameRunpyService Enhancements

## Overview

This document describes the enhancements made to the `FlameRunpyService` to improve efficiency and functionality.

## Enhancements

### 1. Cache-based Output with ObjectRef

**Problem**: Previously, task results were returned directly as pickled objects, which could be inefficient for large objects and didn't leverage Flame's caching infrastructure.

**Solution**: Modified `on_task_invoke` to cache the result and return an `ObjectRef` instead.

**Benefits**:
- **Efficient data transfer**: Large objects are cached once and referenced, avoiding repeated serialization/deserialization
- **Consistent API**: Aligns with the Runner API's ObjectFuture pattern
- **Memory optimization**: Cache can manage object lifecycle and eviction
- **Reference passing**: Enables passing results as arguments to subsequent tasks without fetching

**Implementation**:
```python
# Put the result into cache and return ObjectRef
logger.debug("Putting result into cache")
application_id = self._ssn_ctx.application.name
object_ref = put_object(application_id, context.session_id, result)
logger.info(f"Result cached with ObjectRef: {object_ref}")

# Return the ObjectRef as TaskOutput
return TaskOutput(data=object_ref)
```

**Impact on Existing Code**: Tests and client code need to call `get_object()` on the returned ObjectRef to retrieve the actual value:
```python
# Old way:
result = ssn.invoke(req)

# New way:
from flamepy.cache import get_object
result_ref = ssn.invoke(req)
result = get_object(result_ref)
```

### 2. Archive Extraction Support

**Problem**: Previously, if a package URL pointed to an archive file (`.tar.gz`, `.zip`, etc.), pip would try to install it directly, which might not work for all archive formats.

**Solution**: Enhanced `_install_package_from_url` to detect archive files, extract them to the working directory, and then install from the extracted directory.

**Supported Archive Formats**:
- `.tar.gz` / `.tgz`
- `.tar.bz2` / `.tbz2`
- `.tar.xz` / `.txz`
- `.tar`
- `.zip`

**Benefits**:
- **Flexibility**: Works with various archive formats
- **Reliability**: Ensures proper extraction before installation
- **Compatibility**: Handles packages that require extraction before installation

**Implementation**:

Added helper methods:
```python
def _is_archive(self, file_path: str) -> bool:
    """Check if a file is an archive that needs to be extracted."""
    archive_extensions = ['.tar.gz', '.tgz', '.tar.bz2', '.tbz2', 
                          '.tar.xz', '.txz', '.zip']
    return any(file_path.endswith(ext) for ext in archive_extensions)

def _extract_archive(self, archive_path: str, extract_to: str) -> str:
    """Extract an archive to a directory."""
    # Creates extraction directory
    # Extracts using tarfile or zipfile
    # Returns path to extracted directory
```

Enhanced installation logic:
```python
# If it's an archive file, extract it first
if os.path.isfile(package_path) and self._is_archive(package_path):
    logger.info(f"Package is an archive file, extracting...")
    
    # Get the working directory (set by application config to /opt/{name})
    working_dir = os.getcwd()
    extract_dir = os.path.join(working_dir, 
                               f"extracted_{os.path.basename(package_path).split('.')[0]}")
    
    # Extract the archive
    extracted_dir = self._extract_archive(package_path, extract_dir)
    
    # Use the extracted directory for installation
    install_path = extracted_dir
    logger.info(f"Will install from extracted directory: {install_path}")
```

**Note**: When used with Runner, archives are extracted to `/opt/{name}/extracted_...` since Runner sets the working directory to `/opt/{name}`.

## Example Usage

### Using Cache-based Output

```python
from flamepy.runner import Runner
from flamepy.cache import get_object

class DataProcessor:
    def process_large_data(self, data):
        # Process large dataset
        return processed_data

with Runner("data-app") as rr:
    processor = rr.service(DataProcessor())
    
    # Result is an ObjectFuture (which wraps ObjectRef)
    result_future = processor.process_large_data(large_dataset)
    
    # Get the actual result from cache
    result = result_future.get()  # Internally calls get_object(ObjectRef)
    
    # Can pass ObjectFuture to other methods without fetching
    processor.analyze(result_future)  # Efficient!
```

### Using Archive Package Installation

Create a package structure:
```
my-package/
├── setup.py
├── my_module/
│   ├── __init__.py
│   └── main.py
└── requirements.txt
```

Package it:
```bash
tar -czf my-package.tar.gz my-package/
```

Use with Runner:
```yaml
# flame.yaml
package:
  storage: "file:///opt/flame/packages"
```

```python
# Upload package to /opt/flame/packages/my-package.tar.gz
# Runner will automatically create the tar.gz

# When session starts, FlameRunpyService will:
# 1. Detect it's an archive
# 2. Extract to working directory
# 3. Install from extracted directory
```

## Migration Guide

### For Test Code

Update all `test_flmrun.py` tests to handle ObjectRef:

**Before**:
```python
req = flamepy.runner.RunnerRequest(method="add", args=(5, 3))
result = ssn.invoke(req)
assert result == 8
```

**After**:
```python
from flamepy.cache import get_object

req = flamepy.runner.RunnerRequest(method="add", args=(5, 3))
result = get_object(ssn.invoke(req))
assert result == 8
```

### For Runner API Users

**No changes required!** The Runner API automatically handles ObjectRef through ObjectFuture:

```python
with Runner("my-app") as rr:
    service = rr.service(MyClass())
    result = service.method()
    value = result.get()  # ObjectFuture.get() handles ObjectRef internally
```

### For Direct Session Users

If you're using `create_session` and `invoke` directly (not through Runner):

**Before**:
```python
from flamepy.runner import RunnerContext, RunnerServiceKind

ctx = RunnerContext(execution_object=my_func, kind=RunnerServiceKind.Stateless)
ssn = create_session("flmrun", ctx)
result = ssn.invoke(request)
```

**After**:
```python
from flamepy.runner import RunnerContext, RunnerServiceKind
from flamepy.cache import get_object

ctx = RunnerContext(execution_object=my_func, kind=RunnerServiceKind.Stateless)
ssn = create_session("flmrun", ctx)
result_ref = ssn.invoke(request)
result = get_object(result_ref)
```

## Performance Implications

### Cache-based Output

**Pros**:
- Reduced network transfer for large objects
- Better memory management through cache
- Enables efficient reference passing

**Cons**:
- Additional cache lookup required
- Slight overhead for small objects

**Recommendation**: The benefits outweigh the costs, especially for:
- Large datasets
- Objects passed as arguments to multiple tasks
- Long-running sessions with many tasks

### Archive Extraction

**Pros**:
- More reliable package installation
- Supports all common archive formats

**Cons**:
- Additional extraction step during session initialization
- Temporary disk space usage for extracted files

**Recommendation**: Minimal impact on performance, as extraction is a one-time operation during session setup.

## Technical Details

### Import Changes

Added imports to `runpy.py`:
```python
import tarfile
import zipfile
import shutil
from pathlib import Path
from .cache import put_object  # Added to existing get_object import
```

### Key Functions Modified

1. **`on_task_invoke`**: Now caches result and returns ObjectRef
2. **`_install_package_from_url`**: Enhanced with archive extraction support
3. **Added `_is_archive`**: Detects archive files
4. **Added `_extract_archive`**: Extracts archives to working directory

## Testing

### Updated Tests

All tests in `test_flmrun.py` have been updated to handle ObjectRef:
- test_flmrun_sum_function
- test_flmrun_class_method
- test_flmrun_kwargs
- test_flmrun_no_args
- test_flmrun_multiple_tasks
- test_flmrun_stateful_class
- test_flmrun_lambda_function
- test_flmrun_complex_return_types

### Test Archive Extraction

To test archive extraction, create a test package:

```bash
# Create test package
mkdir test-pkg
cd test-pkg
cat > setup.py << EOF
from setuptools import setup, find_packages

setup(
    name="test-pkg",
    version="0.1.0",
    packages=find_packages(),
)
EOF

mkdir test_module
cat > test_module/__init__.py << EOF
def hello():
    return "Hello from test package!"
EOF

# Package it
cd ..
tar -czf test-pkg.tar.gz test-pkg/

# Copy to storage
cp test-pkg.tar.gz /opt/flame/packages/

# Use with Runner
# Runner will create application with URL: file:///opt/flame/packages/test-pkg.tar.gz
# FlameRunpyService will extract and install automatically
```

## Future Enhancements

Potential improvements for future releases:

1. **Cache Eviction**: Implement cache eviction policies for ObjectRef
2. **Compression**: Add compression for large cached objects
3. **Cleanup**: Automatic cleanup of extracted directories on session exit
4. **Streaming**: Support streaming for very large objects
5. **Remote Archives**: Support HTTP/HTTPS URLs for archive downloads
6. **Selective Extraction**: Extract only required files from archives

## Conclusion

These enhancements make FlameRunpyService more efficient and flexible:
- **ObjectRef output** enables better cache utilization and reference passing
- **Archive extraction** provides robust package installation from various formats

Both enhancements align with Flame's design principles of efficient distributed computing and ease of use.
