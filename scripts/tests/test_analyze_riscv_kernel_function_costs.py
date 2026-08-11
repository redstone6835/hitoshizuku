"""RISC-V 内核函数成本归因器的符号与归一化回归测试。"""

from __future__ import annotations

import importlib.util
import math
import sys
import unittest
from pathlib import Path


REPOSITORY = Path(__file__).resolve().parents[2]
SCRIPTS = REPOSITORY / "scripts"
sys.path.insert(0, str(SCRIPTS))
SPEC = importlib.util.spec_from_file_location(
    "analyze_riscv_kernel_function_costs",
    SCRIPTS / "analyze-riscv-kernel-function-costs.py",
)
assert SPEC is not None and SPEC.loader is not None
ANALYZER = importlib.util.module_from_spec(SPEC)
sys.modules[SPEC.name] = ANALYZER
SPEC.loader.exec_module(ANALYZER)


def descriptor(
    descriptor_id: int,
    count: int,
    low: float,
    high: float,
    center: float,
) -> object:
    return ANALYZER.DescriptorCost(
        descriptor_id=descriptor_id,
        mnemonic=f"d{descriptor_id}",
        size_bytes=4,
        exact_kernel_count=count,
        assignment="single-context",
        quality="exploratory",
        bounded=True,
        strict=False,
        point_ns=center,
        low_ns=low,
        high_ns=high,
        center_ns=center,
        allocation_weight_ns=center,
        allocation_weight_imputed=False,
    )


class SymbolResolverTests(unittest.TestCase):
    def test_smallest_containing_symbol_wins(self) -> None:
        outer = ANALYZER.FunctionSymbol(0x1000, 0x100, "outer", "T")
        inner = ANALYZER.FunctionSymbol(0x1040, 0x20, "inner", "t")
        resolver = ANALYZER.SymbolResolver([outer, inner], 0x1000, 0x1100)

        self.assertEqual(resolver.resolve(0x1048), inner)
        self.assertEqual(resolver.resolve(0x1020), outer)
        self.assertEqual(resolver.resolve(0x1080), outer)
        self.assertEqual(resolver.resolve(0x2000), ANALYZER.DYNAMIC_CODE_SYMBOL)

    def test_readelf_parser_keeps_only_defined_nonempty_functions(self) -> None:
        content = "\n".join(
            [
                "  1: 0000000000001000    32 FUNC    GLOBAL DEFAULT    1 useful",
                "  2: 0000000000001020     0 FUNC    GLOBAL DEFAULT    1 zero",
                "  3: 0000000000001040    16 FUNC    GLOBAL DEFAULT  UND missing",
                "  4: 0000000000001050     8 OBJECT  GLOBAL DEFAULT    1 data",
            ]
        )
        symbols = ANALYZER.parse_readelf_symbols(content)

        self.assertEqual(symbols, [(0x1000, 32, "GLOBAL", "useful")])
        self.assertEqual(
            ANALYZER.parse_readelf_defined_symbol_names(content),
            {"useful", "zero", "data"},
        )

    def test_elm_manifest_parser_restores_stable_api_name(self) -> None:
        content = "\n".join(
            [
                "ELM-KERNEL-INTERFACE-V1",
                "target=riscv64gc-unknown-none-elf",
                "symbol_count=1",
                "symbol\t3\t3\t1\t2\t0\tallocator.GlobalAlloc.dealloc\tallocator::KernelMemorySubsystem as GlobalAlloc::dealloc\t__elm_kernel_api_e4623bda28defbee\tkernel.allocator.global-alloc@1\t00\texact-rust\t",
            ]
        )

        symbols, header = ANALYZER.parse_elm_manifest(content)

        self.assertEqual(header["symbol_count"], "1")
        self.assertEqual(
            symbols["__elm_kernel_api_e4623bda28defbee"].name,
            "allocator.GlobalAlloc.dealloc",
        )
        self.assertEqual(
            symbols["__elm_kernel_api_e4623bda28defbee"].contract,
            "kernel.allocator.global-alloc@1",
        )

    def test_custom_dynamic_code_bucket_is_preserved(self) -> None:
        dynamic = ANALYZER.FunctionSymbol(
            -3, 0, "elm-module::virtio.block::<symbols-unavailable>", "?"
        )
        resolver = ANALYZER.SymbolResolver([], 0x1000, 0x1100, dynamic)

        self.assertIs(resolver.resolve(0x2000), dynamic)


class AllocationTests(unittest.TestCase):
    def setUp(self) -> None:
        self.left = ANALYZER.FunctionSymbol(0x1000, 0x20, "left", "t")
        self.right = ANALYZER.FunctionSymbol(0x1020, 0x20, "right", "t")

    def test_descriptor_normalization_closes_exact_count(self) -> None:
        descriptors = {1: descriptor(1, 100, 1.0, 2.0, 1.5)}
        allocated = ANALYZER.allocate_exact_counts(
            {1: {self.left: 1.0, self.right: 3.0}}, descriptors
        )

        self.assertEqual(allocated[(self.left, 1)], 25.0)
        self.assertEqual(allocated[(self.right, 1)], 75.0)
        self.assertEqual(math.fsum(allocated.values()), 100.0)

    def test_missing_exposure_is_kept_in_explicit_bucket(self) -> None:
        descriptors = {1: descriptor(1, 100, 1.0, 2.0, 1.5)}

        allocated = ANALYZER.allocate_exact_counts({}, descriptors)

        self.assertEqual(
            allocated[(ANALYZER.UNSAMPLED_SYMBOL, 1)],
            100.0,
        )

    def test_share_bounds_optimize_common_weight_box(self) -> None:
        descriptors = {
            1: descriptor(1, 100, 1.0, 2.0, 1.5),
            2: descriptor(2, 100, 1.0, 4.0, 2.5),
        }

        low, high = ANALYZER.share_bounds({1: 100.0}, descriptors)

        self.assertAlmostEqual(low, 0.2)
        self.assertAlmostEqual(high, 2.0 / 3.0)

    def test_share_bounds_allow_inclusive_ratio_above_one(self) -> None:
        descriptors = {1: descriptor(1, 100, 1.0, 2.0, 1.5)}

        low, high = ANALYZER.share_bounds({1: 200.0}, descriptors)

        self.assertAlmostEqual(low, 2.0)
        self.assertAlmostEqual(high, 2.0)


class InclusiveCallGraphTests(unittest.TestCase):
    def setUp(self) -> None:
        self.a = ANALYZER.FunctionSymbol(0x1000, 0x100, "a", "T")
        self.b = ANALYZER.FunctionSymbol(0x2000, 0x100, "b", "T")
        self.c = ANALYZER.FunctionSymbol(0x3000, 0x100, "c", "T")
        self.d = ANALYZER.FunctionSymbol(0x4000, 0x100, "d", "T")

    def closure(
        self, edges: dict[object, set[object]]
    ) -> object:
        return ANALYZER.compute_inclusive_closure(edges)

    def test_chain_includes_every_descendant_once(self) -> None:
        closure = self.closure(
            {self.a: {self.b}, self.b: {self.c}, self.c: set()}
        )

        values = ANALYZER.inclusive_metric(
            {self.a: 1.0, self.b: 2.0, self.c: 3.0}, closure
        )

        self.assertEqual(values[self.a], 6.0)
        self.assertEqual(values[self.b], 5.0)
        self.assertEqual(values[self.c], 3.0)

    def test_diamond_does_not_duplicate_shared_descendant(self) -> None:
        closure = self.closure(
            {
                self.a: {self.b, self.c},
                self.b: {self.d},
                self.c: {self.d},
                self.d: set(),
            }
        )

        values = ANALYZER.inclusive_metric(
            {self.a: 1.0, self.b: 2.0, self.c: 3.0, self.d: 4.0},
            closure,
        )

        self.assertEqual(values[self.a], 10.0)
        self.assertEqual(values[self.b], 6.0)
        self.assertEqual(values[self.c], 7.0)
        self.assertEqual(values[self.d], 4.0)

    def test_recursive_scc_is_finite_and_shared_by_members(self) -> None:
        closure = self.closure(
            {self.a: {self.b}, self.b: {self.a, self.c}, self.c: set()}
        )

        values = ANALYZER.inclusive_metric(
            {self.a: 1.0, self.b: 2.0, self.c: 3.0}, closure
        )

        self.assertEqual(values[self.a], 6.0)
        self.assertEqual(values[self.b], 6.0)
        self.assertEqual(values[self.c], 3.0)
        self.assertEqual(closure.recursive_component_count, 1)

    def test_descriptor_counts_share_the_same_deduplicated_closure(self) -> None:
        closure = self.closure(
            {
                self.a: {self.b, self.c},
                self.b: {self.d},
                self.c: {self.d},
                self.d: set(),
            }
        )

        counts = ANALYZER.inclusive_descriptor_counts(
            {
                self.a: {1: 1.0},
                self.b: {1: 2.0},
                self.c: {2: 3.0},
                self.d: {1: 4.0},
            },
            closure,
        )

        self.assertEqual(counts[self.a], {1: 7.0, 2: 3.0})

    def test_objdump_parses_direct_forms_and_reports_indirect_call(self) -> None:
        disassembly = "\n".join(
            [
                "0000000000001000 <a>:",
                " 1000: jal ra,2000 <b>",
                " 1004: jal zero,3000 <c>",
                " 1008: auipc t0,0x2",
                " 100c: jalr ra,-8(t0) # 3000 <c>",
                " 1010: jalr t1",
                "0000000000002000 <b>:",
                " 2000: ret",
                "0000000000003000 <c>:",
                " 3000: ret",
            ]
        )
        resolver = ANALYZER.SymbolResolver(
            [self.a, self.b, self.c], 0x1000, 0x3100
        )

        graph = ANALYZER.build_direct_call_graph(
            ANALYZER.parse_riscv_objdump(disassembly),
            [self.a, self.b, self.c],
            resolver,
        )

        self.assertEqual(graph.call_site_count, 3)
        self.assertEqual(graph.resolved_call_site_count, 2)
        self.assertEqual(graph.unresolved_call_site_count, 1)
        self.assertEqual(graph.edges[self.a], {self.b, self.c})

    def test_auipc_jalr_target_is_decoded_without_objdump_comment(self) -> None:
        disassembly = "\n".join(
            [
                "0000000000001000 <a>:",
                " 1000: auipc t0,0x1",
                " 1004: jalr ra,0(t0)",
                "0000000000002000 <b>:",
                " 2000: ret",
            ]
        )
        resolver = ANALYZER.SymbolResolver([self.a, self.b], 0x1000, 0x2100)

        graph = ANALYZER.build_direct_call_graph(
            ANALYZER.parse_riscv_objdump(disassembly),
            [self.a, self.b],
            resolver,
        )

        self.assertEqual(graph.edges[self.a], {self.b})
        self.assertEqual(graph.unresolved_call_site_count, 0)

    def test_cross_symbol_direct_tail_transfer_is_an_inclusive_edge(self) -> None:
        disassembly = "\n".join(
            [
                "0000000000001000 <a>:",
                " 1000: j 1008 <a+0x8>",
                " 1004: j 2000 <b>",
                " 1008: ret",
                "0000000000002000 <b>:",
                " 2000: ret",
            ]
        )
        resolver = ANALYZER.SymbolResolver([self.a, self.b], 0x1000, 0x2100)

        graph = ANALYZER.build_direct_call_graph(
            ANALYZER.parse_riscv_objdump(disassembly),
            [self.a, self.b],
            resolver,
        )

        self.assertEqual(graph.edges[self.a], {self.b})
        self.assertEqual(graph.resolved_tail_transfer_site_count, 1)
        self.assertEqual(graph.tail_edges, {(self.a, self.b)})


if __name__ == "__main__":
    unittest.main()
