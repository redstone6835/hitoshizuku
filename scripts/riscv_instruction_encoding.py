"""RISC-V64 instruction encodings used by instruction-weight profiling.

The QEMU disassembler intentionally prints aliases (``ret``, ``mv`` and so
on), and for compressed instructions it often prints the expanded mnemonic.
Consequently profiling keys must be derived from the instruction bits rather
than from the display string.  This module decodes the RV64 I/M/A/F/D/C
families used by the catalog and keeps semantic encoding modifiers which can
change the translated path (CSR number, rounding mode, fence masks and AMO
ordering bits).

Unknown or reserved encodings retain every input byte in their key.  They can
therefore be reported and calibrated independently without accidentally being
folded into an unrelated mnemonic.
"""

from __future__ import annotations

from dataclasses import dataclass


_ROUNDING_MODES = {
    0: "rne",
    1: "rtz",
    2: "rdn",
    3: "rup",
    4: "rmm",
    5: "reserved5",
    6: "reserved6",
    7: "dyn",
}


@dataclass(frozen=True, slots=True)
class RiscvInstructionEncoding:
    """Canonical classification of one little-endian RISC-V instruction."""

    key: str
    mnemonic: str
    extension: str
    length: int
    recognized: bool
    modifiers: tuple[str, ...]
    raw_hex: str
    qemu_mnemonic: str | None


def _instruction_length(first_halfword: int) -> int | None:
    """Return the standard instruction length encoded by the low bits.

    A ``None`` result denotes the reserved >=192-bit length escape.  The
    caller still emits a deterministic raw key for it.
    """

    if first_halfword & 0b11 != 0b11:
        return 2
    if first_halfword & 0b1_1111 != 0b1_1111:
        return 4
    if first_halfword & 0b11_1111 == 0b01_1111:
        return 6
    if first_halfword & 0b111_1111 == 0b011_1111:
        return 8
    if first_halfword & 0b111_1111 == 0b111_1111:
        extra = (first_halfword >> 12) & 0b111
        if extra != 0b111:
            return 10 + 2 * extra
    return None


def _known(
    raw: bytes,
    qemu_mnemonic: str | None,
    extension: str,
    mnemonic: str,
    *modifiers: str,
) -> RiscvInstructionEncoding:
    parts = ("rv64", str(len(raw) * 8), extension, mnemonic, *modifiers)
    return RiscvInstructionEncoding(
        key=":".join(parts),
        mnemonic=mnemonic,
        extension=extension,
        length=len(raw),
        recognized=True,
        modifiers=tuple(modifiers),
        raw_hex=raw.hex(),
        qemu_mnemonic=qemu_mnemonic,
    )


def _unknown(
    raw: bytes, qemu_mnemonic: str | None, reason: str
) -> RiscvInstructionEncoding:
    length = f"{len(raw) * 8}" if raw else "0"
    return RiscvInstructionEncoding(
        key=f"rv64:{length}:unknown:{reason}:raw={raw.hex() or '-'}",
        mnemonic="unknown",
        extension="unknown",
        length=len(raw),
        recognized=False,
        modifiers=(f"reason={reason}",),
        raw_hex=raw.hex(),
        qemu_mnemonic=qemu_mnemonic,
    )


def _rm(raw: bytes, qemu: str | None, extension: str, mnemonic: str, word: int):
    return _known(
        raw,
        qemu,
        extension,
        mnemonic,
        f"rm={_ROUNDING_MODES[(word >> 12) & 0x7]}",
    )


def _decode_compressed(
    raw: bytes, word: int, qemu: str | None
) -> RiscvInstructionEncoding:
    quadrant = word & 0x3
    funct3 = (word >> 13) & 0x7
    rd = (word >> 7) & 0x1F
    rs2 = (word >> 2) & 0x1F
    immediate_nonzero = bool(((word >> 2) & 0x1F) | ((word >> 12) & 0x1))

    if quadrant == 0:
        if funct3 == 0:
            # nzuimm[5:4|9:6|2|3] occupies bits 12:5.
            if word & 0x1FE0:
                return _known(raw, qemu, "c", "c.addi4spn")
            return _unknown(raw, qemu, "reserved-c.addi4spn-zero")
        names = {
            1: "c.fld",
            2: "c.lw",
            3: "c.ld",
            5: "c.fsd",
            6: "c.sw",
            7: "c.sd",
        }
        if funct3 in names:
            return _known(raw, qemu, "c", names[funct3])
        return _unknown(raw, qemu, "reserved-c-quadrant0")

    if quadrant == 1:
        if funct3 == 0:
            if rd == 0 and not immediate_nonzero:
                return _known(raw, qemu, "c", "c.nop")
            if rd == 0 or not immediate_nonzero:
                return _known(raw, qemu, "c", "c.hint.addi")
            return _known(raw, qemu, "c", "c.addi")
        if funct3 == 1:
            if rd == 0:
                return _unknown(raw, qemu, "reserved-c.addiw-rd-zero")
            modifiers = ("form=sext.w",) if not immediate_nonzero else ()
            return _known(raw, qemu, "c", "c.addiw", *modifiers)
        if funct3 == 2:
            if rd == 0:
                return _known(raw, qemu, "c", "c.hint.li")
            return _known(raw, qemu, "c", "c.li")
        if funct3 == 3:
            if rd == 2:
                if immediate_nonzero:
                    return _known(raw, qemu, "c", "c.addi16sp")
                return _unknown(raw, qemu, "reserved-c.addi16sp-zero")
            if rd == 0:
                return _known(raw, qemu, "c", "c.hint.lui")
            if immediate_nonzero:
                return _known(raw, qemu, "c", "c.lui")
            return _unknown(raw, qemu, "reserved-c.lui-zero")
        if funct3 == 4:
            subop = (word >> 10) & 0x3
            if subop in (0, 1):
                name = "c.srli" if subop == 0 else "c.srai"
                if immediate_nonzero:
                    return _known(raw, qemu, "c", name)
                return _known(raw, qemu, "c", f"c.hint.{name[2:]}")
            if subop == 2:
                return _known(raw, qemu, "c", "c.andi")
            arithmetic = (word >> 5) & 0x3
            wide = bool(word & (1 << 12))
            if not wide:
                return _known(
                    raw,
                    qemu,
                    "c",
                    ("c.sub", "c.xor", "c.or", "c.and")[arithmetic],
                )
            if arithmetic == 0:
                return _known(raw, qemu, "c", "c.subw")
            if arithmetic == 1:
                return _known(raw, qemu, "c", "c.addw")
            return _unknown(raw, qemu, "reserved-c-arithmetic-wide")
        if funct3 == 5:
            return _known(raw, qemu, "c", "c.j")
        if funct3 == 6:
            return _known(raw, qemu, "c", "c.beqz")
        if funct3 == 7:
            return _known(raw, qemu, "c", "c.bnez")

    if quadrant == 2:
        if funct3 == 0:
            if rd == 0 or not immediate_nonzero:
                return _known(raw, qemu, "c", "c.hint.slli")
            return _known(raw, qemu, "c", "c.slli")
        if funct3 == 1:
            return _known(raw, qemu, "c", "c.fldsp")
        if funct3 == 2:
            if rd == 0:
                return _unknown(raw, qemu, "reserved-c.lwsp-rd-zero")
            return _known(raw, qemu, "c", "c.lwsp")
        if funct3 == 3:
            if rd == 0:
                return _unknown(raw, qemu, "reserved-c.ldsp-rd-zero")
            return _known(raw, qemu, "c", "c.ldsp")
        if funct3 == 4:
            high = bool(word & (1 << 12))
            if not high:
                if rs2 == 0:
                    if rd == 0:
                        return _unknown(raw, qemu, "reserved-c.jr-rs1-zero")
                    form = "ret" if rd == 1 else "jr"
                    return _known(raw, qemu, "c", "c.jr", f"form={form}")
                if rd == 0:
                    return _known(raw, qemu, "c", "c.hint.mv")
                return _known(raw, qemu, "c", "c.mv")
            if rs2 == 0:
                if rd == 0:
                    return _known(raw, qemu, "c", "c.ebreak")
                return _known(raw, qemu, "c", "c.jalr")
            if rd == 0:
                return _known(raw, qemu, "c", "c.hint.add")
            return _known(raw, qemu, "c", "c.add")
        if funct3 == 5:
            return _known(raw, qemu, "c", "c.fsdsp")
        if funct3 == 6:
            return _known(raw, qemu, "c", "c.swsp")
        if funct3 == 7:
            return _known(raw, qemu, "c", "c.sdsp")

    return _unknown(raw, qemu, "reserved-compressed")


def _decode_op_fp(
    raw: bytes, word: int, qemu: str | None
) -> RiscvInstructionEncoding:
    funct7 = (word >> 25) & 0x7F
    funct3 = (word >> 12) & 0x7
    rs2 = (word >> 20) & 0x1F

    arithmetic = {
        0x00: ("f", "fadd.s"),
        0x01: ("d", "fadd.d"),
        0x04: ("f", "fsub.s"),
        0x05: ("d", "fsub.d"),
        0x08: ("f", "fmul.s"),
        0x09: ("d", "fmul.d"),
        0x0C: ("f", "fdiv.s"),
        0x0D: ("d", "fdiv.d"),
    }
    if funct7 in arithmetic:
        extension, mnemonic = arithmetic[funct7]
        return _rm(raw, qemu, extension, mnemonic, word)

    if funct7 in (0x2C, 0x2D) and rs2 == 0:
        suffix = "s" if funct7 == 0x2C else "d"
        return _rm(raw, qemu, "f" if suffix == "s" else "d", f"fsqrt.{suffix}", word)

    if funct7 in (0x10, 0x11):
        suffix = "s" if funct7 == 0x10 else "d"
        names = {0: "fsgnj", 1: "fsgnjn", 2: "fsgnjx"}
        if funct3 in names:
            return _known(
                raw,
                qemu,
                "f" if suffix == "s" else "d",
                f"{names[funct3]}.{suffix}",
            )

    if funct7 in (0x14, 0x15):
        suffix = "s" if funct7 == 0x14 else "d"
        names = {0: "fmin", 1: "fmax"}
        if funct3 in names:
            return _known(
                raw,
                qemu,
                "f" if suffix == "s" else "d",
                f"{names[funct3]}.{suffix}",
            )

    if funct7 in (0x50, 0x51):
        suffix = "s" if funct7 == 0x50 else "d"
        names = {0: "fle", 1: "flt", 2: "feq"}
        if funct3 in names:
            return _known(
                raw,
                qemu,
                "f" if suffix == "s" else "d",
                f"{names[funct3]}.{suffix}",
            )

    if funct7 in (0x60, 0x61):
        source = "s" if funct7 == 0x60 else "d"
        destinations = {0: "w", 1: "wu", 2: "l", 3: "lu"}
        if rs2 in destinations:
            return _rm(
                raw,
                qemu,
                "f" if source == "s" else "d",
                f"fcvt.{destinations[rs2]}.{source}",
                word,
            )

    if funct7 in (0x68, 0x69):
        destination = "s" if funct7 == 0x68 else "d"
        sources = {0: "w", 1: "wu", 2: "l", 3: "lu"}
        if rs2 in sources:
            return _rm(
                raw,
                qemu,
                "f" if destination == "s" else "d",
                f"fcvt.{destination}.{sources[rs2]}",
                word,
            )

    if funct7 == 0x20 and rs2 == 1:
        return _rm(raw, qemu, "d", "fcvt.s.d", word)
    if funct7 == 0x21 and rs2 == 0:
        return _rm(raw, qemu, "d", "fcvt.d.s", word)

    if funct7 in (0x70, 0x71) and rs2 == 0:
        suffix = "s" if funct7 == 0x70 else "d"
        if funct3 == 0:
            mnemonic = "fmv.x.w" if suffix == "s" else "fmv.x.d"
        elif funct3 == 1:
            mnemonic = f"fclass.{suffix}"
        else:
            return _unknown(raw, qemu, "reserved-fmv-fclass")
        return _known(raw, qemu, "f" if suffix == "s" else "d", mnemonic)

    if funct7 in (0x78, 0x79) and rs2 == 0 and funct3 == 0:
        suffix = "w" if funct7 == 0x78 else "d"
        return _known(
            raw,
            qemu,
            "f" if suffix == "w" else "d",
            f"fmv.{suffix}.x",
        )

    return _unknown(raw, qemu, "reserved-op-fp")


def _decode_standard(
    raw: bytes, word: int, qemu: str | None
) -> RiscvInstructionEncoding:
    opcode = word & 0x7F
    rd = (word >> 7) & 0x1F
    funct3 = (word >> 12) & 0x7
    rs1 = (word >> 15) & 0x1F
    rs2 = (word >> 20) & 0x1F
    funct7 = (word >> 25) & 0x7F

    if opcode == 0x37:
        return _known(raw, qemu, "i", "lui")
    if opcode == 0x17:
        return _known(raw, qemu, "i", "auipc")
    if opcode == 0x6F:
        form = "j" if rd == 0 else "call" if rd == 1 else "link"
        return _known(raw, qemu, "i", "jal", f"form={form}")
    if opcode == 0x67 and funct3 == 0:
        immediate = (word >> 20) & 0xFFF
        if rd == 0 and rs1 == 1 and immediate == 0:
            form = "ret"
        elif rd == 0:
            form = "jr"
        elif rd == 1:
            form = "call"
        else:
            form = "link"
        return _known(raw, qemu, "i", "jalr", f"form={form}")

    if opcode == 0x63:
        names = {0: "beq", 1: "bne", 4: "blt", 5: "bge", 6: "bltu", 7: "bgeu"}
        if funct3 in names:
            return _known(raw, qemu, "i", names[funct3])

    if opcode == 0x03:
        names = {0: "lb", 1: "lh", 2: "lw", 3: "ld", 4: "lbu", 5: "lhu", 6: "lwu"}
        if funct3 in names:
            return _known(raw, qemu, "i", names[funct3])

    if opcode == 0x23:
        names = {0: "sb", 1: "sh", 2: "sw", 3: "sd"}
        if funct3 in names:
            return _known(raw, qemu, "i", names[funct3])

    if opcode == 0x13:
        names = {0: "addi", 2: "slti", 3: "sltiu", 4: "xori", 6: "ori", 7: "andi"}
        if funct3 in names:
            immediate = (word >> 20) & 0xFFF
            modifiers: list[str] = []
            if funct3 == 0:
                if rd == 0 and rs1 == 0 and immediate == 0:
                    modifiers.append("form=nop")
                elif rs1 == 0:
                    modifiers.append("form=li")
                elif immediate == 0:
                    modifiers.append("form=mv")
            elif funct3 == 3 and immediate == 1:
                modifiers.append("form=seqz")
            elif funct3 == 4 and immediate == 0xFFF:
                modifiers.append("form=not")
            return _known(raw, qemu, "i", names[funct3], *modifiers)
        funct6 = (word >> 26) & 0x3F
        if funct3 == 1 and funct6 == 0:
            return _known(raw, qemu, "i", "slli")
        if funct3 == 5 and funct6 in (0, 0x10):
            return _known(raw, qemu, "i", "srli" if funct6 == 0 else "srai")

    if opcode == 0x1B:
        if funct3 == 0:
            immediate = (word >> 20) & 0xFFF
            modifiers = ("form=sext.w",) if immediate == 0 else ()
            return _known(raw, qemu, "i", "addiw", *modifiers)
        if funct3 == 1 and funct7 == 0:
            return _known(raw, qemu, "i", "slliw")
        if funct3 == 5 and funct7 in (0, 0x20):
            return _known(raw, qemu, "i", "srliw" if funct7 == 0 else "sraiw")

    if opcode in (0x33, 0x3B):
        wide = opcode == 0x3B
        base = {
            (0x00, 0): "addw" if wide else "add",
            (0x20, 0): "subw" if wide else "sub",
            (0x00, 1): "sllw" if wide else "sll",
            (0x00, 2): None if wide else "slt",
            (0x00, 3): None if wide else "sltu",
            (0x00, 4): None if wide else "xor",
            (0x00, 5): "srlw" if wide else "srl",
            (0x20, 5): "sraw" if wide else "sra",
            (0x00, 6): None if wide else "or",
            (0x00, 7): None if wide else "and",
        }.get((funct7, funct3))
        if base is not None:
            modifiers: tuple[str, ...] = ()
            if base in {"sub", "subw"} and rs1 == 0:
                modifiers = (f"form={'negw' if wide else 'neg'}",)
            elif base == "sltu" and rs1 == 0:
                modifiers = ("form=snez",)
            elif base == "slt" and rs1 == 0:
                modifiers = ("form=sgtz",)
            return _known(raw, qemu, "i", base, *modifiers)
        multiply = {
            0: "mulw" if wide else "mul",
            1: None if wide else "mulh",
            2: None if wide else "mulhsu",
            3: None if wide else "mulhu",
            4: "divw" if wide else "div",
            5: "divuw" if wide else "divu",
            6: "remw" if wide else "rem",
            7: "remuw" if wide else "remu",
        }.get(funct3)
        if funct7 == 1 and multiply is not None:
            return _known(raw, qemu, "m", multiply)

    if opcode == 0x0F:
        if funct3 == 0:
            immediate = (word >> 20) & 0xFFF
            if rd == 0 and rs1 == 0 and immediate == 0x010:
                return _known(raw, qemu, "zihintpause", "pause")
            fm = (immediate >> 8) & 0xF
            predecessor = (immediate >> 4) & 0xF
            successor = immediate & 0xF
            if rd == 0 and rs1 == 0 and immediate == 0x833:
                return _known(raw, qemu, "i", "fence.tso")
            return _known(
                raw,
                qemu,
                "i",
                "fence",
                f"fm=0x{fm:x}",
                f"pred=0x{predecessor:x}",
                f"succ=0x{successor:x}",
            )
        if funct3 == 1 and rd == 0 and rs1 == 0 and (word >> 20) == 0:
            return _known(raw, qemu, "zifencei", "fence.i")
        if funct3 == 2 and rd == 0:
            cbo = {0: "cbo.inval", 1: "cbo.clean", 2: "cbo.flush", 4: "cbo.zero"}
            immediate = (word >> 20) & 0xFFF
            if immediate in cbo:
                extension = "zicboz" if immediate == 4 else "zicbom"
                return _known(raw, qemu, extension, cbo[immediate])

    if opcode == 0x73:
        if funct3 == 0:
            immediate = (word >> 20) & 0xFFF
            fixed = {
                0x000: ("i", "ecall"),
                0x001: ("i", "ebreak"),
                0x002: ("priv", "uret"),
                0x102: ("priv", "sret"),
                0x105: ("priv", "wfi"),
                0x302: ("priv", "mret"),
            }
            if rd == 0 and rs1 == 0 and immediate in fixed:
                extension, mnemonic = fixed[immediate]
                return _known(raw, qemu, extension, mnemonic)
            if rd == 0 and funct7 in (0x09, 0x11, 0x31):
                names = {0x09: "sfence.vma", 0x11: "hfence.vvma", 0x31: "hfence.gvma"}
                return _known(raw, qemu, "priv", names[funct7])
        csr_names = {
            1: "csrrw",
            2: "csrrs",
            3: "csrrc",
            5: "csrrwi",
            6: "csrrsi",
            7: "csrrci",
        }
        if funct3 in csr_names:
            csr = (word >> 20) & 0xFFF
            name = csr_names[funct3]
            modifiers = [f"csr=0x{csr:03x}"]
            if funct3 in (1, 5):
                modifiers.append(f"read={int(rd != 0)}")
            else:
                modifiers.append(f"write={int(rs1 != 0)}")
            if funct3 >= 5:
                modifiers.append(f"zimm=0x{rs1:02x}")
            return _known(raw, qemu, "zicsr", name, *modifiers)

    if opcode == 0x2F:
        width = {2: "w", 3: "d"}.get(funct3)
        operation = {
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
        }.get((word >> 27) & 0x1F)
        if width is not None and operation is not None:
            if operation == "lr" and rs2 != 0:
                return _unknown(raw, qemu, "reserved-lr-rs2-nonzero")
            aq = (word >> 26) & 1
            rl = (word >> 25) & 1
            return _known(
                raw,
                qemu,
                "a",
                f"{operation}.{width}",
                f"aq={aq}",
                f"rl={rl}",
            )

    if opcode == 0x07:
        names = {2: ("f", "flw"), 3: ("d", "fld")}
        if funct3 in names:
            extension, mnemonic = names[funct3]
            return _known(raw, qemu, extension, mnemonic)

    if opcode == 0x27:
        names = {2: ("f", "fsw"), 3: ("d", "fsd")}
        if funct3 in names:
            extension, mnemonic = names[funct3]
            return _known(raw, qemu, extension, mnemonic)

    if opcode in (0x43, 0x47, 0x4B, 0x4F):
        operation = {0x43: "fmadd", 0x47: "fmsub", 0x4B: "fnmsub", 0x4F: "fnmadd"}[opcode]
        fmt = (word >> 25) & 0x3
        if fmt in (0, 1):
            suffix = "s" if fmt == 0 else "d"
            return _rm(
                raw,
                qemu,
                "f" if suffix == "s" else "d",
                f"{operation}.{suffix}",
                word,
            )

    if opcode == 0x53:
        return _decode_op_fp(raw, word, qemu)

    return _unknown(raw, qemu, "unrecognized-standard")


def decode_riscv64_instruction(
    raw_bytes: bytes | bytearray | memoryview,
    qemu_mnemonic: str | None = None,
) -> RiscvInstructionEncoding:
    """Decode one complete little-endian instruction into a stable key.

    ``qemu_mnemonic`` is retained as diagnostic metadata only.  It never
    changes a key, so aliases and QEMU disassembler-version changes cannot
    split or merge measurements.
    """

    if not isinstance(raw_bytes, (bytes, bytearray, memoryview)):
        raise TypeError("raw_bytes must be a bytes-like object")
    raw = bytes(raw_bytes)
    if qemu_mnemonic is not None:
        if not isinstance(qemu_mnemonic, str):
            raise TypeError("qemu_mnemonic must be str or None")
        qemu_mnemonic = qemu_mnemonic.strip().lower() or None
    if len(raw) < 2:
        return _unknown(raw, qemu_mnemonic, "truncated-prefix")

    expected_length = _instruction_length(int.from_bytes(raw[:2], "little"))
    if expected_length is None:
        return _unknown(raw, qemu_mnemonic, "reserved-length")
    if len(raw) != expected_length:
        return _unknown(
            raw,
            qemu_mnemonic,
            f"length-mismatch-expected-{expected_length}",
        )
    if len(raw) == 2:
        return _decode_compressed(
            raw, int.from_bytes(raw, "little"), qemu_mnemonic
        )
    if len(raw) == 4:
        return _decode_standard(raw, int.from_bytes(raw, "little"), qemu_mnemonic)
    return _unknown(raw, qemu_mnemonic, "unsupported-standard-length")


def instruction_encoding_key(
    raw_bytes: bytes | bytearray | memoryview,
    qemu_mnemonic: str | None = None,
) -> str:
    """Return only the stable key for one RISC-V64 instruction."""

    return decode_riscv64_instruction(raw_bytes, qemu_mnemonic).key


# Short aliases are convenient in streaming catalog code.
decode_instruction = decode_riscv64_instruction
encoding_key = instruction_encoding_key


__all__ = [
    "RiscvInstructionEncoding",
    "decode_instruction",
    "decode_riscv64_instruction",
    "encoding_key",
    "instruction_encoding_key",
]
