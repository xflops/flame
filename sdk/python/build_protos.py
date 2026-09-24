#!/usr/bin/env python3
"""
Build script to generate protobuf Python files.
"""

import re
import subprocess
import sys
from pathlib import Path


def fix_imports(proto_dir: Path) -> None:
    """Fix imports in generated protobuf files to use flamepy.proto prefix."""
    # Patterns to fix: 'import X_pb2' -> 'import flamepy.proto.X_pb2'
    import_pattern = re.compile(r"^import (cache_pb2|frontend_pb2|shim_pb2|types_pb2)", re.MULTILINE)
    replacement = r"import flamepy.proto.\1"

    for pb2_file in proto_dir.glob("*_pb2*.py"):
        content = pb2_file.read_text()
        new_content = import_pattern.sub(replacement, content)
        if new_content != content:
            pb2_file.write_text(new_content)
            print(f"Fixed imports in {pb2_file.name}")


def main():
    """Generate protobuf Python files."""
    # Get the directory containing this script
    script_dir = Path(__file__).parent
    protos_dir = script_dir / "protos"
    proto_out_dir = script_dir / "src" / "flamepy" / "proto"

    # Create the protos directory if it doesn't exist
    protos_dir.mkdir(parents=True, exist_ok=True)

    # Generate Python files from protobuf definitions
    proto_files = ["cache.proto", "frontend.proto", "shim.proto", "types.proto"]

    for proto_file in proto_files:
        proto_path = protos_dir / proto_file
        if proto_path.exists():
            print(f"Generating Python files from {proto_file}...")

            # Generate Python files into proto module
            cmd = [
                sys.executable,
                "-m",
                "grpc_tools.protoc",
                f"--python_out={proto_out_dir}",
                f"--grpc_python_out={proto_out_dir}",
                f"--proto_path={protos_dir}",
                str(proto_path),
            ]

            try:
                subprocess.run(cmd, check=True, cwd=script_dir)
                print(f"Successfully generated Python files from {proto_file}")
            except subprocess.CalledProcessError as e:
                print(f"Error generating Python files from {proto_file}: {e}")
                return 1

    # Fix imports in generated files
    print("Fixing imports in generated files...")
    fix_imports(proto_out_dir)

    print("Protobuf generation completed successfully!")
    return 0


if __name__ == "__main__":
    sys.exit(main())
