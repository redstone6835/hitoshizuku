"""Tests for stable, bit-derived RISC-V64 instruction keys."""

from __future__ import annotations

import unittest

from scripts.riscv_instruction_encoding import (
    decode_riscv64_instruction,
    instruction_encoding_key,
)


def le(word: int, length: int = 4) -> bytes:
    return word.to_bytes(length, "little")


def r_type(opcode: int, funct3: int, funct7: int = 0) -> bytes:
    return le(
        (funct7 << 25)
        | (11 << 20)
        | (10 << 15)
        | (funct3 << 12)
        | (9 << 7)
        | opcode
    )


def i_type(opcode: int, funct3: int, immediate: int = 1) -> bytes:
    return le(
        ((immediate & 0xFFF) << 20)
        | (10 << 15)
        | (funct3 << 12)
        | (9 << 7)
        | opcode
    )


def fp_type(funct7: int, funct3: int = 0, rs2: int = 11) -> bytes:
    return le(
        (funct7 << 25)
        | (rs2 << 20)
        | (10 << 15)
        | (funct3 << 12)
        | (9 << 7)
        | 0x53
    )


def compressed(quadrant: int, funct3: int, body: int = 0) -> bytes:
    return le((funct3 << 13) | body | quadrant, 2)


class BaseIntegerDecodeTests(unittest.TestCase):
    def assert_mnemonic(self, raw: bytes, mnemonic: str, extension: str = "i") -> None:
        decoded = decode_riscv64_instruction(raw)
        self.assertTrue(decoded.recognized, decoded)
        self.assertEqual(decoded.mnemonic, mnemonic)
        self.assertEqual(decoded.extension, extension)
        self.assertTrue(decoded.key.startswith(f"rv64:{len(raw) * 8}:{extension}:"))

    def test_u_jump_and_jalr(self) -> None:
        for raw, mnemonic in (
            (le(0x123454B7), "lui"),
            (le(0x12345497), "auipc"),
            (le(0x008000EF), "jal"),
            (le(0x000500E7), "jalr"),
        ):
            with self.subTest(mnemonic=mnemonic):
                self.assert_mnemonic(raw, mnemonic)

    def test_all_base_branches_loads_and_stores(self) -> None:
        for funct3, mnemonic in {
            0: "beq",
            1: "bne",
            4: "blt",
            5: "bge",
            6: "bltu",
            7: "bgeu",
        }.items():
            self.assert_mnemonic(r_type(0x63, funct3), mnemonic)
        for funct3, mnemonic in {
            0: "lb",
            1: "lh",
            2: "lw",
            3: "ld",
            4: "lbu",
            5: "lhu",
            6: "lwu",
        }.items():
            self.assert_mnemonic(i_type(0x03, funct3), mnemonic)
        for funct3, mnemonic in {0: "sb", 1: "sh", 2: "sw", 3: "sd"}.items():
            self.assert_mnemonic(r_type(0x23, funct3), mnemonic)

    def test_all_immediate_alu_operations(self) -> None:
        for funct3, mnemonic in {
            0: "addi",
            2: "slti",
            3: "sltiu",
            4: "xori",
            6: "ori",
            7: "andi",
        }.items():
            self.assert_mnemonic(i_type(0x13, funct3), mnemonic)
        self.assert_mnemonic(i_type(0x13, 1, 1), "slli")
        self.assert_mnemonic(i_type(0x13, 5, 1), "srli")
        self.assert_mnemonic(i_type(0x13, 5, 0x401), "srai")
        self.assert_mnemonic(i_type(0x1B, 0, 1), "addiw")
        self.assert_mnemonic(i_type(0x1B, 1, 1), "slliw")
        self.assert_mnemonic(i_type(0x1B, 5, 1), "srliw")
        self.assert_mnemonic(i_type(0x1B, 5, 0x401), "sraiw")

    def test_all_register_alu_and_m_operations(self) -> None:
        base = {
            (0, 0): "add",
            (0x20, 0): "sub",
            (0, 1): "sll",
            (0, 2): "slt",
            (0, 3): "sltu",
            (0, 4): "xor",
            (0, 5): "srl",
            (0x20, 5): "sra",
            (0, 6): "or",
            (0, 7): "and",
        }
        for (funct7, funct3), mnemonic in base.items():
            self.assert_mnemonic(r_type(0x33, funct3, funct7), mnemonic)
        wide = {
            (0, 0): "addw",
            (0x20, 0): "subw",
            (0, 1): "sllw",
            (0, 5): "srlw",
            (0x20, 5): "sraw",
        }
        for (funct7, funct3), mnemonic in wide.items():
            self.assert_mnemonic(r_type(0x3B, funct3, funct7), mnemonic)
        multiply = {
            0: "mul",
            1: "mulh",
            2: "mulhsu",
            3: "mulhu",
            4: "div",
            5: "divu",
            6: "rem",
            7: "remu",
        }
        for funct3, mnemonic in multiply.items():
            self.assert_mnemonic(r_type(0x33, funct3, 1), mnemonic, "m")
        for funct3, mnemonic in {
            0: "mulw",
            4: "divw",
            5: "divuw",
            6: "remw",
            7: "remuw",
        }.items():
            self.assert_mnemonic(r_type(0x3B, funct3, 1), mnemonic, "m")

    def test_pseudoinstructions_are_normalized_from_bits(self) -> None:
        aliases = (
            (le(0x00000013), "nop", "addi"),
            (le(0x00058513), "mv", "addi"),
            (le(0xFFF5C513), "not", "xori"),
            (le(0x40B00533), "neg", "sub"),
            (le(0x0005851B), "sext.w", "addiw"),
            (le(0x00008067), "ret", "jalr"),
            (le(0x00028067), "jr", "jalr"),
            (le(0x0080006F), "j", "jal"),
        )
        for raw, qemu_name, canonical in aliases:
            with self.subTest(qemu_name=qemu_name):
                decoded = decode_riscv64_instruction(raw, qemu_name)
                self.assertEqual(decoded.mnemonic, canonical)
                self.assertEqual(decoded.qemu_mnemonic, qemu_name)
                self.assertEqual(
                    decoded.key, decode_riscv64_instruction(raw, canonical).key
                )

    def test_fence_system_and_cbo(self) -> None:
        self.assert_mnemonic(le(0x0100000F), "pause", "zihintpause")
        self.assert_mnemonic(le(0x8330000F), "fence.tso")
        fence = decode_riscv64_instruction(le(0x0FF0000F))
        self.assertEqual(fence.mnemonic, "fence")
        self.assertEqual(fence.modifiers, ("fm=0x0", "pred=0xf", "succ=0xf"))
        self.assert_mnemonic(le(0x0000100F), "fence.i", "zifencei")
        for immediate, mnemonic, extension in (
            (0, "cbo.inval", "zicbom"),
            (1, "cbo.clean", "zicbom"),
            (2, "cbo.flush", "zicbom"),
            (4, "cbo.zero", "zicboz"),
        ):
            self.assert_mnemonic(
                le((immediate << 20) | (19 << 15) | (2 << 12) | 0x0F),
                mnemonic,
                extension,
            )
        # This exact catalog encoding is incorrectly displayed as lq by QEMU.
        cbo_zero = decode_riscv64_instruction(bytes.fromhex("0fa04900"), "lq")
        self.assertEqual(cbo_zero.mnemonic, "cbo.zero")

        for immediate, mnemonic, extension in (
            (0x000, "ecall", "i"),
            (0x001, "ebreak", "i"),
            (0x002, "uret", "priv"),
            (0x102, "sret", "priv"),
            (0x105, "wfi", "priv"),
            (0x302, "mret", "priv"),
        ):
            self.assert_mnemonic(le((immediate << 20) | 0x73), mnemonic, extension)


class CsrAndAtomicDecodeTests(unittest.TestCase):
    def test_csr_number_and_access_semantics_are_retained(self) -> None:
        # csrrs a0, mhartid, x0; exact bytes observed in the catalog.
        read = decode_riscv64_instruction(bytes.fromhex("732540f1"), "csrr")
        self.assertEqual(read.mnemonic, "csrrs")
        self.assertEqual(read.modifiers, ("csr=0xf14", "write=0"))
        self.assertIn("csr=0xf14", read.key)

        csrrw = le((0x305 << 20) | (7 << 15) | (1 << 12) | 0x73)
        write_only = decode_riscv64_instruction(csrrw, "csrw")
        self.assertEqual(write_only.mnemonic, "csrrw")
        self.assertEqual(write_only.modifiers, ("csr=0x305", "read=0"))

        immediate = le(
            (0x003 << 20) | (0x1F << 15) | (7 << 12) | (8 << 7) | 0x73
        )
        decoded = decode_riscv64_instruction(immediate, "csrci")
        self.assertEqual(decoded.mnemonic, "csrrci")
        self.assertEqual(
            decoded.modifiers,
            ("csr=0x003", "write=1", "zimm=0x1f"),
        )

    def test_csr_operations_and_numbers_do_not_merge(self) -> None:
        keys: set[str] = set()
        for funct3 in (1, 2, 3, 5, 6, 7):
            for csr in (0x001, 0xC00, 0xF14):
                raw = le(
                    (csr << 20)
                    | (3 << 15)
                    | (funct3 << 12)
                    | (4 << 7)
                    | 0x73
                )
                keys.add(instruction_encoding_key(raw))
        self.assertEqual(len(keys), 18)

    def test_all_base_amo_operations_widths_and_orderings(self) -> None:
        operations = {
            0x00: "amoadd",
            0x01: "amoswap",
            0x02: "lr",
            0x03: "sc",
            0x04: "amoxor",
            0x08: "amoor",
            0x0C: "amoand",
            0x10: "amomin",
            0x14: "amomax",
            0x18: "amominu",
            0x1C: "amomaxu",
        }
        keys: set[str] = set()
        for funct5, operation in operations.items():
            for funct3, width in ((2, "w"), (3, "d")):
                for aq in (0, 1):
                    for rl in (0, 1):
                        rs2 = 0 if operation == "lr" else 11
                        raw = le(
                            (funct5 << 27)
                            | (aq << 26)
                            | (rl << 25)
                            | (rs2 << 20)
                            | (10 << 15)
                            | (funct3 << 12)
                            | (9 << 7)
                            | 0x2F
                        )
                        decoded = decode_riscv64_instruction(raw)
                        self.assertEqual(decoded.mnemonic, f"{operation}.{width}")
                        self.assertEqual(decoded.extension, "a")
                        self.assertEqual(
                            decoded.modifiers, (f"aq={aq}", f"rl={rl}")
                        )
                        keys.add(decoded.key)
        self.assertEqual(len(keys), len(operations) * 2 * 4)

    def test_invalid_lr_encoding_is_not_folded_into_lr(self) -> None:
        raw = le((0x02 << 27) | (1 << 20) | (3 << 12) | 0x2F)
        decoded = decode_riscv64_instruction(raw, "lr.d")
        self.assertFalse(decoded.recognized)
        self.assertIn("reserved-lr-rs2-nonzero", decoded.key)


class FloatingPointDecodeTests(unittest.TestCase):
    def assert_fp(self, raw: bytes, mnemonic: str, extension: str) -> None:
        decoded = decode_riscv64_instruction(raw)
        self.assertTrue(decoded.recognized, decoded)
        self.assertEqual((decoded.mnemonic, decoded.extension), (mnemonic, extension))

    def test_load_store_and_fused_operations(self) -> None:
        for opcode, funct3, mnemonic, extension in (
            (0x07, 2, "flw", "f"),
            (0x07, 3, "fld", "d"),
            (0x27, 2, "fsw", "f"),
            (0x27, 3, "fsd", "d"),
        ):
            self.assert_fp(r_type(opcode, funct3), mnemonic, extension)
        for opcode, operation in (
            (0x43, "fmadd"),
            (0x47, "fmsub"),
            (0x4B, "fnmsub"),
            (0x4F, "fnmadd"),
        ):
            for fmt, suffix, extension in ((0, "s", "f"), (1, "d", "d")):
                raw = le((fmt << 25) | (3 << 12) | opcode)
                decoded = decode_riscv64_instruction(raw)
                self.assertEqual(decoded.mnemonic, f"{operation}.{suffix}")
                self.assertEqual(decoded.extension, extension)
                self.assertEqual(decoded.modifiers, ("rm=rup",))

    def test_arithmetic_sign_min_compare_and_sqrt(self) -> None:
        for funct7, mnemonic, extension in (
            (0x00, "fadd.s", "f"),
            (0x01, "fadd.d", "d"),
            (0x04, "fsub.s", "f"),
            (0x05, "fsub.d", "d"),
            (0x08, "fmul.s", "f"),
            (0x09, "fmul.d", "d"),
            (0x0C, "fdiv.s", "f"),
            (0x0D, "fdiv.d", "d"),
        ):
            self.assert_fp(fp_type(funct7, 7), mnemonic, extension)
        self.assert_fp(fp_type(0x2C, 0, 0), "fsqrt.s", "f")
        self.assert_fp(fp_type(0x2D, 0, 0), "fsqrt.d", "d")

        for funct7, suffix, extension in ((0x10, "s", "f"), (0x11, "d", "d")):
            for funct3, operation in ((0, "fsgnj"), (1, "fsgnjn"), (2, "fsgnjx")):
                self.assert_fp(
                    fp_type(funct7, funct3), f"{operation}.{suffix}", extension
                )
        for funct7, suffix, extension in ((0x14, "s", "f"), (0x15, "d", "d")):
            for funct3, operation in ((0, "fmin"), (1, "fmax")):
                self.assert_fp(
                    fp_type(funct7, funct3), f"{operation}.{suffix}", extension
                )
        for funct7, suffix, extension in ((0x50, "s", "f"), (0x51, "d", "d")):
            for funct3, operation in ((0, "fle"), (1, "flt"), (2, "feq")):
                self.assert_fp(
                    fp_type(funct7, funct3), f"{operation}.{suffix}", extension
                )

    def test_all_integer_float_conversions_and_moves(self) -> None:
        for funct7, source, extension in ((0x60, "s", "f"), (0x61, "d", "d")):
            for rs2, destination in ((0, "w"), (1, "wu"), (2, "l"), (3, "lu")):
                self.assert_fp(
                    fp_type(funct7, 0, rs2),
                    f"fcvt.{destination}.{source}",
                    extension,
                )
        for funct7, destination, extension in ((0x68, "s", "f"), (0x69, "d", "d")):
            for rs2, source in ((0, "w"), (1, "wu"), (2, "l"), (3, "lu")):
                self.assert_fp(
                    fp_type(funct7, 0, rs2),
                    f"fcvt.{destination}.{source}",
                    extension,
                )
        self.assert_fp(fp_type(0x20, 0, 1), "fcvt.s.d", "d")
        self.assert_fp(fp_type(0x21, 0, 0), "fcvt.d.s", "d")
        for funct7, funct3, mnemonic, extension in (
            (0x70, 0, "fmv.x.w", "f"),
            (0x70, 1, "fclass.s", "f"),
            (0x71, 0, "fmv.x.d", "d"),
            (0x71, 1, "fclass.d", "d"),
            (0x78, 0, "fmv.w.x", "f"),
            (0x79, 0, "fmv.d.x", "d"),
        ):
            self.assert_fp(fp_type(funct7, funct3, 0), mnemonic, extension)

    def test_rounding_mode_is_part_of_the_key(self) -> None:
        keys = {
            instruction_encoding_key(fp_type(0x00, rounding_mode))
            for rounding_mode in range(8)
        }
        self.assertEqual(len(keys), 8)
        self.assertTrue(any("rm=dyn" in key for key in keys))


class CompressedDecodeTests(unittest.TestCase):
    def assert_c(self, raw: bytes, mnemonic: str) -> None:
        decoded = decode_riscv64_instruction(raw, mnemonic.removeprefix("c."))
        self.assertTrue(decoded.recognized, decoded)
        self.assertEqual(decoded.mnemonic, mnemonic)
        self.assertEqual(decoded.extension, "c")
        self.assertTrue(decoded.key.startswith("rv64:16:c:"))

    def test_quadrant_zero_memory_and_addi4spn(self) -> None:
        self.assert_c(compressed(0, 0, (1 << 5) | (1 << 2)), "c.addi4spn")
        for funct3, mnemonic in (
            (1, "c.fld"),
            (2, "c.lw"),
            (3, "c.ld"),
            (5, "c.fsd"),
            (6, "c.sw"),
            (7, "c.sd"),
        ):
            self.assert_c(compressed(0, funct3, (1 << 7) | (1 << 2)), mnemonic)

    def test_quadrant_one_immediates_alu_and_control_flow(self) -> None:
        self.assert_c(compressed(1, 0, (10 << 7) | (1 << 2)), "c.addi")
        self.assert_c(compressed(1, 1, (10 << 7) | (1 << 2)), "c.addiw")
        self.assert_c(compressed(1, 2, (10 << 7) | (1 << 2)), "c.li")
        self.assert_c(compressed(1, 3, (2 << 7) | (1 << 6)), "c.addi16sp")
        self.assert_c(compressed(1, 3, (3 << 7) | (1 << 2)), "c.lui")
        for subop, mnemonic in ((0, "c.srli"), (1, "c.srai"), (2, "c.andi")):
            self.assert_c(
                compressed(1, 4, (subop << 10) | (1 << 7) | (1 << 2)),
                mnemonic,
            )
        for arithmetic, mnemonic in (
            (0, "c.sub"),
            (1, "c.xor"),
            (2, "c.or"),
            (3, "c.and"),
        ):
            self.assert_c(
                compressed(1, 4, (3 << 10) | (1 << 7) | (arithmetic << 5)),
                mnemonic,
            )
        self.assert_c(
            compressed(1, 4, (1 << 12) | (3 << 10) | (1 << 7)), "c.subw"
        )
        self.assert_c(
            compressed(
                1, 4, (1 << 12) | (3 << 10) | (1 << 7) | (1 << 5)
            ),
            "c.addw",
        )
        for funct3, mnemonic in ((5, "c.j"), (6, "c.beqz"), (7, "c.bnez")):
            self.assert_c(compressed(1, funct3, 1 << 2), mnemonic)

    def test_quadrant_two_stack_memory_register_and_control_forms(self) -> None:
        self.assert_c(compressed(2, 0, (10 << 7) | (1 << 2)), "c.slli")
        for funct3, mnemonic in (
            (1, "c.fldsp"),
            (2, "c.lwsp"),
            (3, "c.ldsp"),
        ):
            self.assert_c(compressed(2, funct3, (10 << 7) | (1 << 2)), mnemonic)
        for funct3, mnemonic in (
            (5, "c.fsdsp"),
            (6, "c.swsp"),
            (7, "c.sdsp"),
        ):
            self.assert_c(compressed(2, funct3, 1 << 2), mnemonic)

        self.assert_c(compressed(2, 4, 10 << 7), "c.jr")
        self.assert_c(compressed(2, 4, (10 << 7) | (11 << 2)), "c.mv")
        self.assert_c(
            compressed(2, 4, (1 << 12) | (10 << 7)), "c.jalr"
        )
        self.assert_c(
            compressed(2, 4, (1 << 12) | (10 << 7) | (11 << 2)),
            "c.add",
        )
        self.assert_c(compressed(2, 4, 1 << 12), "c.ebreak")

    def test_sp_and_non_sp_encodings_never_merge(self) -> None:
        pairs = (
            (compressed(0, 2, (1 << 7) | (1 << 2)), compressed(2, 2, 10 << 7)),
            (compressed(0, 3, (1 << 7) | (1 << 2)), compressed(2, 3, 10 << 7)),
            (compressed(0, 6, (1 << 7) | (1 << 2)), compressed(2, 6, 1 << 2)),
            (compressed(0, 7, (1 << 7) | (1 << 2)), compressed(2, 7, 1 << 2)),
            (compressed(0, 1, (1 << 7) | (1 << 2)), compressed(2, 1, 10 << 7)),
            (compressed(0, 5, (1 << 7) | (1 << 2)), compressed(2, 5, 1 << 2)),
        )
        for regular, stack in pairs:
            self.assertNotEqual(
                instruction_encoding_key(regular), instruction_encoding_key(stack)
            )

    def test_expanded_qemu_names_cannot_merge_compressed_forms(self) -> None:
        forms = (
            compressed(0, 0, (1 << 5) | (1 << 2)),
            compressed(1, 0, (10 << 7) | (1 << 2)),
            compressed(1, 2, (10 << 7) | (1 << 2)),
            compressed(1, 3, (2 << 7) | (1 << 6)),
        )
        keys = {instruction_encoding_key(raw, "addi") for raw in forms}
        self.assertEqual(len(keys), 4)


class UnknownEncodingTests(unittest.TestCase):
    def test_unknown_standard_lengths_are_deterministic_and_exact(self) -> None:
        raw48_a = bytes.fromhex("1f0001020304")
        raw48_b = bytes.fromhex("1f0001020305")
        first = instruction_encoding_key(raw48_a, "future.op")
        self.assertEqual(first, instruction_encoding_key(raw48_a, "different-name"))
        self.assertNotEqual(first, instruction_encoding_key(raw48_b, "future.op"))
        self.assertIn(f"raw={raw48_a.hex()}", first)
        self.assertIn("unsupported-standard-length", first)

        raw64 = bytes.fromhex("3f00010203040506")
        self.assertIn("unsupported-standard-length", instruction_encoding_key(raw64))

    def test_reserved_and_unrecognized_encodings_keep_full_raw_bits(self) -> None:
        reserved_c = compressed(0, 4, 0)
        unknown_a = le(0x0000007B)
        unknown_b = le(0x0000017B)
        for raw in (reserved_c, unknown_a, unknown_b):
            decoded = decode_riscv64_instruction(raw, "mystery")
            self.assertFalse(decoded.recognized)
            self.assertIn(f"raw={raw.hex()}", decoded.key)
        self.assertNotEqual(
            instruction_encoding_key(unknown_a), instruction_encoding_key(unknown_b)
        )

    def test_truncation_and_length_mismatch_are_explicit(self) -> None:
        self.assertIn("truncated-prefix", instruction_encoding_key(b""))
        self.assertIn("truncated-prefix", instruction_encoding_key(b"\x13"))
        mismatch = decode_riscv64_instruction(bytes.fromhex("1300"))
        self.assertFalse(mismatch.recognized)
        self.assertIn("length-mismatch-expected-4", mismatch.key)

    def test_input_validation_and_normalized_diagnostic_name(self) -> None:
        with self.assertRaises(TypeError):
            decode_riscv64_instruction("13000000")  # type: ignore[arg-type]
        with self.assertRaises(TypeError):
            decode_riscv64_instruction(le(0x13), 12)  # type: ignore[arg-type]
        decoded = decode_riscv64_instruction(bytearray(le(0x13)), "  NOP  ")
        self.assertEqual(decoded.qemu_mnemonic, "nop")
        self.assertEqual(decoded.mnemonic, "addi")


if __name__ == "__main__":
    unittest.main()
