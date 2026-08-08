#!/usr/bin/env python3

from __future__ import annotations

import hashlib
import importlib.util
import sys
import tempfile
import unittest
from pathlib import Path


SCRIPT = Path(__file__).parents[1] / "kcsan-symbolize.py"
SPEC = importlib.util.spec_from_file_location("kcsan_symbolize", SCRIPT)
assert SPEC is not None and SPEC.loader is not None
KCSAN_SYMBOLIZE = importlib.util.module_from_spec(SPEC)
sys.modules[SPEC.name] = KCSAN_SYMBOLIZE
SPEC.loader.exec_module(KCSAN_SYMBOLIZE)


class KcsanSymbolizeTests(unittest.TestCase):
    def test_manifest_and_return_address_resolution(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            kernel = root / "kernel"
            symbol_map = root / "kernel.map"
            manifest = root / "kernel.map.manifest"
            kernel.write_bytes(b"kernel-image")
            symbol_map.write_text(
                "VMA LMA Size Align Out In Symbol\n"
                "9000000090001000 90001000 20 4 demo::race::h1234\n",
                encoding="utf-8",
            )
            manifest.write_text(
                "schema=mygo.kernel-map-manifest.v1\n"
                "target=loongarch64-unknown-none\n"
                f"kernel_sha256={hashlib.sha256(kernel.read_bytes()).hexdigest()}\n"
                f"symbol_map_sha256={hashlib.sha256(symbol_map.read_bytes()).hexdigest()}\n",
                encoding="utf-8",
            )

            KCSAN_SYMBOLIZE.verify_build_pair(symbol_map)
            starts, symbols = KCSAN_SYMBOLIZE.load_symbols(symbol_map)
            symbol = KCSAN_SYMBOLIZE.resolve(starts, symbols, 0x9000_0000_9000_1004)
            self.assertIsNotNone(symbol)
            self.assertEqual(symbol.name, "demo::race::h1234")

            kernel.write_bytes(b"stale-image")
            with self.assertRaisesRegex(ValueError, "SHA-256"):
                KCSAN_SYMBOLIZE.verify_build_pair(symbol_map)


if __name__ == "__main__":
    unittest.main()
