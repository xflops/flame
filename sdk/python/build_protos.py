#!/usr/bin/env python3
"""
Build script to generate protobuf Python files.
"""

import os
import subprocess
import sys
from pathlib import Path


def main():
    """Generate protobuf Python files."""
    # Get the directory containing this script
    script_dir = Path(__file__).parent
    protos_dir = script_dir / "protos"

    # Create the protos directory if it doesn't exist
    protos_dir.mkdir(parents=True, exist_ok=True)

    # Generate Python files from protobuf definitions
    proto_files = ["frontend.proto", "shim.proto", "types.proto", "cache.proto"]

    for proto_file in proto_files:
        proto_path = protos_dir / proto_file
        if proto_path.exists():
            print(f"Generating Python files from {proto_file}...")

            # Generate Python files
            cmd = [
                sys.executable,
                "-m",
                "grpc_tools.protoc",
                f"--python_out={script_dir}/src/flamepy/",
                f"--grpc_python_out={script_dir}/src/flamepy/",
                f"--proto_path={protos_dir}",
                str(proto_path),
            ]

            try:
                subprocess.run(cmd, check=True, cwd=script_dir)
                print(f"Successfully generated Python files from {proto_file}")
            except subprocess.CalledProcessError as e:
                print(f"Error generating Python files from {proto_file}: {e}")
                return 1

    print("Protobuf generation completed successfully!")
    return 0


if __name__ == "__main__":
    sys.exit(main())
