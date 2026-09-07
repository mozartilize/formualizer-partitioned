"""Compatibility launcher for the standalone Rust corpus benchmark.

Direct invocation: cargo run --release --locked --manifest-path bench/Cargo.toml -- DIR
"""

import os
import sys
from pathlib import Path

if __name__ == "__main__":
    manifest = Path(__file__).resolve().with_name("Cargo.toml")
    os.execvp("cargo", ["cargo", "run", "--release", "--locked", "--manifest-path",
                        str(manifest), "--", *sys.argv[1:]])
