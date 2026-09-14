#!/usr/bin/env python3
"""Verify built-in extensions and timezone behavior without extension downloads."""
import argparse
import ctypes
from pathlib import Path
import tempfile
import os


def check(library, fixture):
    library, fixture = library.resolve(strict=True), fixture.resolve(strict=True)
    with tempfile.TemporaryDirectory(prefix="trawl-runtime-check-") as temporary:
        os.environ["HOME"] = temporary
        dll = ctypes.CDLL(str(library))
        pointer = ctypes.c_void_p
        dll.duckdb_library_version.restype = ctypes.c_char_p
        dll.duckdb_open.argtypes = [ctypes.c_char_p, ctypes.POINTER(pointer)]
        dll.duckdb_connect.argtypes = [pointer, ctypes.POINTER(pointer)]
        dll.duckdb_query.argtypes = [pointer, ctypes.c_char_p, pointer]
        dll.duckdb_disconnect.argtypes = [ctypes.POINTER(pointer)]
        dll.duckdb_close.argtypes = [ctypes.POINTER(pointer)]
        assert dll.duckdb_library_version() == b"v1.5.5"
        database, connection = pointer(), pointer()
        assert dll.duckdb_open(None, ctypes.byref(database)) == 0
        try:
            assert dll.duckdb_connect(database, ctypes.byref(connection)) == 0
            try:
                queries = [
                    "SET autoinstall_known_extensions=false; SET autoload_known_extensions=false",
                    "SELECT CASE WHEN count(*)=3 THEN true ELSE error('missing built-in extension') END FROM duckdb_extensions() WHERE extension_name IN ('icu','json','parquet') AND loaded AND install_mode='STATICALLY_LINKED'",
                    "SET TimeZone='UTC'",
                    "SELECT CASE WHEN epoch(TIMESTAMPTZ '2024-01-01 00:00:00 Asia/Kolkata')=epoch(TIMESTAMPTZ '2023-12-31 18:30:00 UTC') THEN true ELSE error('timezone mismatch') END",
                    "SELECT CASE WHEN json_extract_string('{\"status\":\"ready\"}', '$.status')='ready' THEN true ELSE error('JSON mismatch') END",
                    "SELECT CASE WHEN count(*)=3 THEN true ELSE error('Parquet mismatch') END FROM read_parquet('" + str(fixture).replace("'", "''") + "')",
                ]
                for i, query in enumerate(queries):
                    assert dll.duckdb_query(connection, query.encode(), None) == 0, f"runtime query {i} failed"
            finally:
                dll.duckdb_disconnect(ctypes.byref(connection))
        finally:
            dll.duckdb_close(ctypes.byref(database))
        assert not list(Path(temporary).iterdir()), "runtime created files in fresh HOME"
    print("DuckDB 1.5.5: built-in ICU/JSON/Parquet, UTC/IANA timezone and offline Parquet checks passed")


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("library", type=Path)
    parser.add_argument("fixture", type=Path)
    args = parser.parse_args()
    check(args.library, args.fixture)
