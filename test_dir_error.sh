#!/bin/bash
# Test whether SQLite can create a file in a non-existent parent directory

# Create a test using Rust
cat > /tmp/test_sqlite_parent.rs << 'RUST'
use std::path::Path;

fn main() {
    // Test 1: Try to open a DB file in a non-existent parent
    let path = "/tmp/test_rust_sqlite_xyz_nonexistent/state.db";
    
    // Clean up first
    let _ = std::fs::remove_dir_all("/tmp/test_rust_sqlite_xyz_nonexistent");
    
    match rusqlite::Connection::open(path) {
        Ok(conn) => println!("OK: Created database at {}", path),
        Err(e) => println!("ERROR: {}", e),
    }
    
    // Clean up
    let _ = std::fs::remove_dir_all("/tmp/test_rust_sqlite_xyz_nonexistent");
}
RUST

# Try to compile and run it with the project's dependencies
cd /Users/aleph/Desktop/Projects/marlboro-red/odin && \
cargo build --manifest-path /tmp/Cargo.toml 2>/dev/null || true

# Actually, let's just check SQLite behavior using Python
python3 << 'PYTHON'
import sqlite3
import os

# Clean up
os.system("rm -rf /tmp/test_sqlite_py_xyz")

# Try to create a database in a non-existent parent
try:
    conn = sqlite3.connect("/tmp/test_sqlite_py_xyz/state.db")
    print("SUCCESS: SQLite created database")
except Exception as e:
    print(f"ERROR: {e}")
    print(f"Type: {type(e).__name__}")

# Clean up
os.system("rm -rf /tmp/test_sqlite_py_xyz")
PYTHON
