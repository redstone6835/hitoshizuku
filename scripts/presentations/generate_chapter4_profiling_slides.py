#!/usr/bin/env python3
"""生成答辩第四章“调试方法与指令成本建模”并接入完整答辩稿。"""

from __future__ import annotations

import argparse
from copy import deepcopy
from io import BytesIO
import os
from pathlib import Path
import tempfile

from pptx import Presentation
from pptx.enum.shapes import MSO_SHAPE_TYPE
from pptx.enum.text import MSO_ANCHOR, PP_ALIGN
from pptx.util import Pt

from generate_engineering_structure_slide import (
    BG,
    BODY,
    BLUE,
    INK,
    LINE,
    MUTED,
    NAVY,
    PALE_BLUE,
    PALE_GRAY,
    PALE_PURPLE,
    PALE_TEAL,
    PURPLE,
    TEAL,
    WHITE,
    add_arrow_tip,
    add_line,
    add_rect,
    add_text,
    rgb,
    set_run_typefaces,
    style_run,
)


EMU_PER_INCH = 914400
MIN_FONT_PT = 14.0
CONTENT_X = 3.00
CONTENT_RIGHT = 12.60
CONTENT_W = CONTENT_RIGHT - CONTENT_X

CHAPTER4_TRANSITION = "第四章 · 调试方法"
CHAPTER5_TRANSITION = "第五章 · 结果结论"
CHAPTER4_TITLES = (
    "内核性能证据体系",
    "受控指令截窗模型",
    "单次系统调用的指令对照",
    "动态指令数量与执行成本",
    "探针与基线的成对差分",
    "指令语义与执行上下文",
    "混杂因素与稳健回归",
    "Super-run 与分层 Bootstrap",
    "统计区间与质量分级",
    "机器学习辅助的结构发现",
    "独立留出与时间稳定性",
    "统计建模与机器学习协同闭环",
)


def inches(value: int) -> float:
    return value / EMU_PER_INCH


def clone_slide(prs: Presentation, source):
    destination = prs.slides.add_slide(source.slide_layout)
    for shape in list(destination.shapes):
        element = shape._element
        element.getparent().remove(element)
    destination.background.fill.solid()
    destination.background.fill.fore_color.rgb = rgb(BG)
    for shape in source.shapes:
        if shape.shape_type == MSO_SHAPE_TYPE.PICTURE:
            picture = destination.shapes.add_picture(
                BytesIO(shape.image.blob),
                shape.left,
                shape.top,
                shape.width,
                shape.height,
            )
            picture.crop_left = shape.crop_left
            picture.crop_right = shape.crop_right
            picture.crop_top = shape.crop_top
            picture.crop_bottom = shape.crop_bottom
            picture.rotation = shape.rotation
            continue
        destination.shapes._spTree.insert_element_before(
            deepcopy(shape._element), "p:extLst"
        )
    return destination


def slide_texts(slide) -> set[str]:
    return {
        shape.text.strip()
        for shape in slide.shapes
        if getattr(shape, "has_text_frame", False) and shape.text.strip()
    }


def find_slide(prs: Presentation, exact_text: str):
    for slide in prs.slides:
        if exact_text in slide_texts(slide):
            return slide
    raise RuntimeError(f"没有找到幻灯片：{exact_text}")


def remove_slide(prs: Presentation, slide) -> None:
    for slide_id in list(prs.slides._sldIdLst):
        if prs.part.related_part(slide_id.rId) is slide.part:
            prs.part.drop_rel(slide_id.rId)
            prs.slides._sldIdLst.remove(slide_id)
            return
    raise RuntimeError("没有找到待删除幻灯片关系")


def is_template_chrome(shape) -> bool:
    x, y = inches(shape.left), inches(shape.top)
    w, h = inches(shape.width), inches(shape.height)
    if x + w <= 2.20:
        return True
    if 2.55 <= x <= 2.75 and 0.52 <= y <= 0.75 and h <= 0.90:
        return True
    if 2.80 <= x <= 3.15 and 0.45 <= y <= 0.75 and w >= 8.0:
        return True
    if shape.shape_type == MSO_SHAPE_TYPE.LINE and 1.50 <= y <= 1.66:
        return True
    return False


def clean_body_template(slide) -> None:
    for shape in list(slide.shapes):
        if not is_template_chrome(shape):
            element = shape._element
            element.getparent().remove(element)


def set_title(slide, title: str) -> None:
    candidates = []
    for shape in slide.shapes:
        if not getattr(shape, "has_text_frame", False):
            continue
        x, y = inches(shape.left), inches(shape.top)
        if 2.80 <= x <= 3.15 and 0.45 <= y <= 0.75:
            candidates.append(shape)
    if not candidates:
        raise RuntimeError("没有找到正文标题文本框")
    box = candidates[0]
    frame = box.text_frame
    frame.clear()
    frame.margin_left = 0
    frame.margin_right = 0
    frame.margin_top = 0
    frame.margin_bottom = 0
    frame.word_wrap = True
    frame.vertical_anchor = MSO_ANCHOR.MIDDLE
    paragraph = frame.paragraphs[0]
    paragraph.alignment = PP_ALIGN.LEFT
    run = paragraph.add_run()
    run.text = title
    style_run(
        run,
        size=30,
        color=INK,
        bold=True,
        chinese_font="SimHei",
        latin_font="Times New Roman",
    )


def body(
    slide,
    text: str,
    x: float,
    y: float,
    w: float,
    h: float,
    *,
    size: float = 15.0,
    color=BODY,
    bold: bool = False,
    align=PP_ALIGN.LEFT,
    valign=MSO_ANCHOR.TOP,
):
    if size < MIN_FONT_PT:
        raise ValueError(f"正文文字不得小于 {MIN_FONT_PT:g} pt：{text}")
    box = add_text(
        slide,
        text,
        x,
        y,
        w,
        h,
        size=size,
        color=color,
        bold=bold,
        chinese_font="SimSun",
        latin_font="Times New Roman",
        align=align,
        valign=valign,
        margin=0.05,
    )
    box.text_frame.word_wrap = True
    return box


def heading(
    slide,
    text: str,
    x: float,
    y: float,
    w: float,
    h: float = 0.34,
    *,
    size: float = 19.0,
    color=INK,
    align=PP_ALIGN.LEFT,
):
    box = add_text(
        slide,
        text,
        x,
        y,
        w,
        h,
        size=max(size, MIN_FONT_PT),
        color=color,
        bold=True,
        chinese_font="SimHei",
        latin_font="Times New Roman",
        align=align,
        valign=MSO_ANCHOR.MIDDLE,
        margin=0.03,
    )
    box.text_frame.word_wrap = True
    return box


def lead(slide, text: str, *, size: float = 16.0) -> None:
    body(
        slide,
        text,
        CONTENT_X,
        1.78,
        CONTENT_W,
        0.62,
        size=size,
        color=INK,
        bold=True,
        valign=MSO_ANCHOR.MIDDLE,
    )


def panel(
    slide,
    x: float,
    y: float,
    w: float,
    h: float,
    title: str,
    detail: str,
    *,
    fill=PALE_BLUE,
    accent=BLUE,
    title_size: float = 18.0,
    detail_size: float = 15.0,
    center: bool = False,
) -> None:
    add_rect(slide, x, y, w, h, fill)
    add_rect(slide, x, y, 0.08, h, accent)
    heading(
        slide,
        title,
        x + 0.22,
        y + 0.13,
        w - 0.40,
        0.34,
        size=title_size,
        color=NAVY,
        align=PP_ALIGN.CENTER if center else PP_ALIGN.LEFT,
    )
    body(
        slide,
        detail,
        x + 0.22,
        y + 0.57,
        w - 0.40,
        h - 0.67,
        size=detail_size,
        color=BODY,
        bold=True,
        align=PP_ALIGN.CENTER if center else PP_ALIGN.LEFT,
    )


def compact_row(
    slide,
    y: float,
    title: str,
    detail: str,
    *,
    fill=PALE_BLUE,
    accent=BLUE,
    h: float = 0.62,
    title_w: float = 2.08,
    detail_size: float = 15.0,
) -> None:
    add_rect(slide, CONTENT_X, y, CONTENT_W, h, fill)
    add_rect(slide, CONTENT_X, y, 0.08, h, accent)
    heading(
        slide,
        title,
        CONTENT_X + 0.23,
        y + 0.12,
        title_w,
        h - 0.22,
        size=17,
        color=NAVY,
    )
    body(
        slide,
        detail,
        CONTENT_X + 0.33 + title_w,
        y + 0.08,
        CONTENT_W - title_w - 0.55,
        h - 0.14,
        size=detail_size,
        color=INK,
        bold=True,
        valign=MSO_ANCHOR.MIDDLE,
    )


def boundary(
    slide,
    text: str,
    *,
    label: str,
    fill=PALE_GRAY,
    accent=MUTED,
    label_w: float = 1.68,
) -> None:
    add_rect(slide, CONTENT_X, 6.14, CONTENT_W, 0.64, fill)
    add_rect(slide, CONTENT_X, 6.14, 0.08, 0.64, accent)
    heading(
        slide,
        label,
        CONTENT_X + 0.23,
        6.25,
        label_w,
        0.31,
        size=17,
        color=NAVY,
    )
    body(
        slide,
        text,
        CONTENT_X + 0.34 + label_w,
        6.18,
        CONTENT_W - label_w - 0.55,
        0.56,
        size=14,
        color=INK,
        bold=True,
        valign=MSO_ANCHOR.MIDDLE,
    )


def arrow(slide, x1: float, y: float, x2: float, *, color=BLUE) -> None:
    add_line(slide, x1, y, x2, y, color, 1.5)
    add_arrow_tip(slide, x2, y, "right", color, 0.11)


def stage_box(
    slide,
    x: float,
    y: float,
    w: float,
    h: float,
    title: str,
    detail: str,
    *,
    fill=PALE_BLUE,
    accent=BLUE,
    title_size: float = 17,
    detail_size: float = 14,
) -> None:
    add_rect(slide, x, y, w, h, fill)
    add_rect(slide, x, y, 0.07, h, accent)
    heading(
        slide,
        title,
        x + 0.16,
        y + 0.12,
        w - 0.28,
        0.34,
        size=title_size,
        color=NAVY,
        align=PP_ALIGN.CENTER,
    )
    if detail:
        body(
            slide,
            detail,
            x + 0.14,
            y + 0.52,
            w - 0.24,
            h - 0.60,
            size=detail_size,
            color=BODY,
            bold=True,
            align=PP_ALIGN.CENTER,
        )


def metric(
    slide,
    x: float,
    y: float,
    w: float,
    value: str,
    label: str,
    *,
    fill=PALE_BLUE,
    accent=BLUE,
) -> None:
    add_rect(slide, x, y, w, 0.92, fill)
    add_rect(slide, x, y, 0.07, 0.92, accent)
    heading(
        slide,
        value,
        x + 0.16,
        y + 0.10,
        w - 0.26,
        0.38,
        size=20,
        color=accent,
        align=PP_ALIGN.CENTER,
    )
    body(
        slide,
        label,
        x + 0.12,
        y + 0.52,
        w - 0.20,
        0.28,
        size=14,
        color=INK,
        bold=True,
        align=PP_ALIGN.CENTER,
        valign=MSO_ANCHOR.MIDDLE,
    )


def enforce_font_floor(slide) -> None:
    for shape in slide.shapes:
        if not getattr(shape, "has_text_frame", False) or not shape.text.strip():
            continue
        for paragraph in shape.text_frame.paragraphs:
            for run in paragraph.runs:
                if not run.text.strip():
                    continue
                if run.font.size is None or run.font.size.pt < MIN_FONT_PT:
                    run.font.size = Pt(MIN_FONT_PT)
                if run.font.name is None:
                    set_run_typefaces(run, "SimSun", "Times New Roman")


def draw_01(slide) -> None:
    set_title(slide, CHAPTER4_TITLES[0])
    lead(slide, "性能结论沿五层证据逐步收敛：先确认工作负载可比，再还原动态路径，最后把指令工作量换算为带不确定性的成本。")

    labels = (
        ("固定负载", "同一镜像、CPU、内存与窗口", PALE_BLUE, BLUE),
        ("动态路径", "marker 内完整指令顺序", PALE_TEAL, TEAL),
        ("执行计数", "用户态 / 内核态及语义分类", PALE_PURPLE, PURPLE),
        ("成本估计", "成对差分、区间与质量标签", PALE_BLUE, BLUE),
        ("热点归因", "函数、阶段与源码责任", PALE_GRAY, MUTED),
    )
    x = 3.00
    for index, (title, detail, fill, accent) in enumerate(labels):
        stage_box(slide, x, 2.55, 1.64, 1.50, title, detail, fill=fill, accent=accent)
        if index != len(labels) - 1:
            arrow(slide, x + 1.67, 3.30, x + 1.88, color=accent)
        x += 1.96

    compact_row(slide, 4.36, "宏观比较", "BuildStorm 的时间与进度回答“整体是否变快”；QEMU CPU、指令数和函数热点回答“工作量发生了什么变化”。", fill=PALE_BLUE, accent=BLUE, h=0.68, title_w=1.76)
    compact_row(slide, 5.18, "微观比较", "单次 syscall 或缺页探针固定输入与边界，逐条对照 MyGO / Linux 动态指令，定位首个结构分歧。", fill=PALE_TEAL, accent=TEAL, h=0.68, title_w=1.76)
    boundary(slide, "统计模型负责量化成本及不确定性；机器学习从残差和时间留出中寻找新的结构，并反馈下一轮实验设计。", label="建模分工")


def draw_02(slide) -> None:
    set_title(slide, CHAPTER4_TITLES[1])
    lead(slide, "固定逻辑探针给出唯一开始和结束边界；QEMU 插件只记录边界内真实执行的 RISC-V 指令，再由同次镜像的 ELF 与 map 还原函数身份。")

    steps = (
        ("固定探针", "单一算法或 syscall", PALE_BLUE, BLUE),
        ("START marker", "窗口恰好开启一次", PALE_PURPLE, PURPLE),
        ("TCG 指令流", "sequence · PC · bytes · disassembly", PALE_TEAL, TEAL),
        ("STOP marker", "窗口恰好闭合一次", PALE_PURPLE, PURPLE),
        ("符号化", "ELF / map · function + offset", PALE_GRAY, MUTED),
    )
    x = 3.00
    for index, (title, detail, fill, accent) in enumerate(steps):
        stage_box(slide, x, 2.40, 1.66, 1.38, title, detail, fill=fill, accent=accent)
        if index != len(steps) - 1:
            arrow(slide, x + 1.68, 3.09, x + 1.90, color=accent)
        x += 1.96

    panel(slide, 3.00, 4.08, 4.55, 1.62, "完整性门禁", "单 vCPU；轨迹为 user → kernel → user；sequence 连续；dropped=0；translation_failures=0；窗口退出时 inactive。", fill=PALE_BLUE, accent=BLUE, title_size=18, detail_size=15)
    panel(slide, 7.86, 4.08, 4.74, 1.62, "镜像一致性", "动态字节必须与静态反汇编一致；Linux alternative 仅在元数据与替换字节精确闭合时接受；kernel、map 与 manifest 绑定哈希。", fill=PALE_TEAL, accent=TEAL, title_size=18, detail_size=15)
    boundary(slide, "最终 TSV 保存 sequence、权限态、PC、编码、函数、函数内偏移和动态汇编，形成可逐条复核的执行证据。", label="跟踪产物")


def draw_03(slide) -> None:
    set_title(slide, CHAPTER4_TITLES[2])
    lead(slide, "以 getpid 为例：同一用户探针在 MyGO 与 Linux 中各执行一次，比较对象不是同名函数，而是从 ecall 到 sret 的同一组责任阶段。")

    stages = (
        ("ecall", "用户边界"),
        ("trap 入口", "寄存器与 CSR"),
        ("syscall 分发", "编号与目标选择"),
        ("getpid", "当前任务读取"),
        ("返回检查", "信号与调度标志"),
        ("sret", "用户现场恢复"),
    )
    x = 3.00
    for index, (title, detail) in enumerate(stages):
        fill, accent = (PALE_TEAL, TEAL) if index in (0, 5) else (PALE_BLUE, BLUE)
        if index == 3:
            fill, accent = PALE_PURPLE, PURPLE
        stage_box(slide, x, 2.35, 1.35, 1.18, title, detail, fill=fill, accent=accent, title_size=16, detail_size=14)
        if index != len(stages) - 1:
            arrow(slide, x + 1.37, 2.94, x + 1.55, color=accent)
        x += 1.60

    compact_row(slide, 3.84, "阶段指令数", "分别统计入口、分发、业务实现和返回路径，避免总数差异掩盖具体责任。", fill=PALE_BLUE, accent=BLUE, h=0.58, title_w=1.82)
    compact_row(slide, 4.54, "首个动态分歧", "从两条顺序流的第一个不同位置开始回看函数与汇编，识别额外搬运、状态检查、原子操作或慢路径进入。", fill=PALE_PURPLE, accent=PURPLE, h=0.68, title_w=1.82)
    compact_row(slide, 5.34, "源码责任对齐", "再将差异映射到 Linux entry、syscall dispatch 与 ret-to-user 的对应责任，而非按函数名机械匹配。", fill=PALE_TEAL, accent=TEAL, h=0.58, title_w=1.82)
    boundary(slide, "单次轨迹解释路径结构；同一结构在长负载中出现多少次，由 BuildStorm 动态计数补足。", label="两类尺度")


def draw_04(slide) -> None:
    set_title(slide, CHAPTER4_TITLES[3])
    lead(slide, "内核动态指令总数描述工作量，却不能直接代表时间；成本归因需要同时保留每类指令的执行次数、语义上下文和成本区间。")

    add_rect(slide, 3.00, 2.48, 9.60, 1.06, NAVY)
    body(slide, "T_kernel  ≈  Σₖ  Nₖ · θₖ", 3.20, 2.64, 9.20, 0.68, size=28, color=WHITE, bold=True, align=PP_ALIGN.CENTER, valign=MSO_ANCHOR.MIDDLE)

    panel(slide, 3.00, 3.82, 2.92, 1.74, "Nₖ · 动态次数", "QEMU TCG 在固定窗口内统计真实执行次数，并区分用户态、内核态、指令编码和执行阶段。", fill=PALE_BLUE, accent=BLUE, title_size=19, detail_size=15)
    panel(slide, 6.24, 3.82, 2.92, 1.74, "θₖ · 上下文成本", "同一助记符按依赖关系、分支方向、访存模式和控制流形态分别估计中心值与区间。", fill=PALE_PURPLE, accent=PURPLE, title_size=19, detail_size=15)
    panel(slide, 9.48, 3.82, 3.12, 1.74, "函数与阶段归因", "将动态 PC 映射到 ELF 函数，再按指令成本分配到 trap、MM、VFS、调度等责任阶段。", fill=PALE_TEAL, accent=TEAL, title_size=19, detail_size=15)
    boundary(slide, "该模型用于比较同一 QEMU TCG 环境中的相对成本构成；中心估计与上下界同时进入函数归因。", label="归因输出")


def draw_05(slide) -> None:
    set_title(slide, CHAPTER4_TITLES[4])
    lead(slide, "每个目标指令上下文都配有同形态 baseline；二者共享序言、循环、调用、返回与 marker，差分后保留目标指令带来的增量。")

    panel(slide, 3.00, 2.36, 4.16, 1.78, "Probe 窗口", "START → 公共准备 → 目标指令 × N → 公共收束 → STOP", fill=PALE_BLUE, accent=BLUE, title_size=20, detail_size=16, center=True)
    panel(slide, 8.44, 2.36, 4.16, 1.78, "Baseline 窗口", "START → 相同准备 → 同形态基线 × N → 相同收束 → STOP", fill=PALE_TEAL, accent=TEAL, title_size=20, detail_size=16, center=True)
    body(slide, "−", 7.36, 2.86, 0.88, 0.48, size=28, color=PURPLE, bold=True, align=PP_ALIGN.CENTER, valign=MSO_ANCHOR.MIDDLE)

    add_rect(slide, 3.00, 4.39, 9.60, 0.76, PALE_PURPLE)
    add_rect(slide, 3.00, 4.39, 0.08, 0.76, PURPLE)
    body(slide, "dᵢ = (T_probe,i − T_baseline,i) / (N_probe,i − N_baseline,i)", 3.24, 4.49, 9.12, 0.52, size=21, color=NAVY, bold=True, align=PP_ALIGN.CENTER, valign=MSO_ANCHOR.MIDDLE)

    metric(slide, 3.00, 5.34, 2.90, "4K / 16K / 64K", "三档 batch", fill=PALE_BLUE, accent=BLUE)
    metric(slide, 6.23, 5.34, 2.92, "AB / BA", "执行顺序交错", fill=PALE_TEAL, accent=TEAL)
    metric(slide, 9.48, 5.34, 3.12, "相同路径", "预热与固定操作数", fill=PALE_GRAY, accent=MUTED)


def draw_06(slide) -> None:
    set_title(slide, CHAPTER4_TITLES[5])
    lead(slide, "成本键由“指令身份 + 原始编码 + 执行模式”共同确定；同一 mnemonic 的不同数据流和控制流不被静默平均。")

    rows = (
        ("整数算术", "dependency-chain · independent/reset", PALE_BLUE, BLUE),
        ("条件分支", "taken · not-taken · 不同分支历史", PALE_TEAL, TEAL),
        ("访存与栈", "hot-load · hot-store · stack-load/store", PALE_PURPLE, PURPLE),
        ("跳转与调用", "direct · indirect · call · return", PALE_BLUE, BLUE),
        ("原子操作", "reservation-pair · SC 成功/失败 · aq/rl", PALE_TEAL, TEAL),
        ("浮点与系统指令", "dependency · convert · compare · CSR 编号 · fence immediate", PALE_GRAY, MUTED),
    )
    y = 2.42
    for title, detail, fill, accent in rows:
        compact_row(slide, y, title, detail, fill=fill, accent=accent, h=0.52, title_w=1.76, detail_size=15)
        y += 0.61

    boundary(slide, "60-run 差分实验中 8 个 div/rem 数据流效应均成立，幅度为 2.780–5.075 ns/instruction；两个 mixed-TB 效应仅为 0.038–0.041 ns。", label="数据流证据")


def draw_07(slide) -> None:
    set_title(slide, CHAPTER4_TITLES[6])
    lead(slide, "成对差分仍会受到 QEMU 进程差异、执行顺序、时间漂移和 batch 的影响；稳健回归把这些量显式纳入模型。")

    add_rect(slide, 3.00, 2.39, 9.60, 0.76, NAVY)
    body(slide, "dᵢ = θ + α_run(i) + βₒOᵢ + β_dDᵢ + β_bBᵢ + εᵢ", 3.16, 2.50, 9.28, 0.52, size=22, color=WHITE, bold=True, align=PP_ALIGN.CENTER, valign=MSO_ANCHOR.MIDDLE)

    panels = (
        (3.00, "目标量 θ", "目标指令相对 baseline 的中心成本。", PALE_BLUE, BLUE),
        (5.45, "run 效应 α", "吸收不同 QEMU 进程的整体快慢差异。", PALE_TEAL, TEAL),
        (7.90, "顺序与漂移", "Oᵢ 表示 AB/BA；Dᵢ 表示 run 内时间位置。", PALE_PURPLE, PURPLE),
        (10.35, "batch 档位", "Bᵢ 检查计数规模与成本是否保持稳定。", PALE_GRAY, MUTED),
    )
    for x, title, detail, fill, accent in panels:
        panel(slide, x, 3.43, 2.25, 1.56, title, detail, fill=fill, accent=accent, title_size=17, detail_size=14)

    compact_row(slide, 5.24, "Huber IRLS", "δ=1.345；主体残差采用二次损失，长尾残差逐步降权，不同 batch 的 MAD 方差形成异方差权重。", fill=PALE_BLUE, accent=BLUE, h=0.58, title_w=1.74)
    boundary(slide, "60-run 实验每个上下文含 1,800 pair，ESS 为 653.44–1039.95；target → nop → empty-call 控制图继续传播 reference 不确定性。", label="样本与控制")


def draw_08(slide) -> None:
    set_title(slide, CHAPTER4_TITLES[7])
    lead(slide, "独立样本以完整启动组合定义，而不是以窗口数量定义；时间相关性由 super-run 顶层和 run 内连续块共同保留。")

    body(slide, "一个独立 super-run", 3.00, 2.36, 2.16, 0.34, size=16, color=NAVY, bold=True, valign=MSO_ANCHOR.MIDDLE)
    sequence = (("A", "timing", BLUE), ("B", "plugin-off", TEAL), ("B", "plugin-off", TEAL), ("A", "timing", BLUE))
    x = 5.05
    for index, (letter, detail, accent) in enumerate(sequence):
        add_rect(slide, x, 2.28, 1.46, 0.78, PALE_BLUE if letter == "A" else PALE_TEAL)
        heading(slide, letter, x + 0.10, 2.36, 0.38, 0.32, size=20, color=accent, align=PP_ALIGN.CENTER)
        body(slide, detail, x + 0.46, 2.34, 0.88, 0.34, size=14, color=INK, bold=True, align=PP_ALIGN.CENTER, valign=MSO_ANCHOR.MIDDLE)
        if index != len(sequence) - 1:
            arrow(slide, x + 1.47, 2.67, x + 1.67, color=accent)
        x += 1.82

    compact_row(slide, 3.40, "顶层重采样", "对完整 ABBA / BAAB super-run 有放回抽样；四次启动保持在同一独立簇内。", fill=PALE_PURPLE, accent=PURPLE, h=0.68, title_w=1.86)
    compact_row(slide, 4.22, "时间块重采样", "在每个 run 内按连续 probe-round block 重采样，保留相邻窗口的自相关和慢漂移。", fill=PALE_TEAL, accent=TEAL, h=0.68, title_w=1.86)
    compact_row(slide, 5.04, "全量重新拟合", "每个 replicate 重新执行 Huber 拟合、控制图解析、锚点尺度和全部上下文联合估计。", fill=PALE_BLUE, accent=BLUE, h=0.68, title_w=1.86)
    boundary(slide, "60-run 结构实验完成 4,999/4,999 个有效 replicate；复制同一次 QEMU 启动内的 pair 只增加组内重复。", label="重采样闭合")


def draw_09(slide) -> None:
    set_title(slide, CHAPTER4_TITLES[8])
    lead(slide, "模型同时评价点估计精度、跨运行稳定性和采集有效性；质量标签来自整套门禁，而不是单个窄区间。")

    add_rect(slide, 3.00, 2.40, 4.44, 1.18, NAVY)
    body(slide, "M_b = maxₖ |(θ̂ₖ⁽ᵇ⁾ − θ̂ₖ) / sₖ|", 3.16, 2.55, 4.12, 0.40, size=20, color=WHITE, bold=True, align=PP_ALIGN.CENTER, valign=MSO_ANCHOR.MIDDLE)
    body(slide, "全族同时区间控制多上下文比较中的整体误报", 3.18, 2.99, 4.08, 0.30, size=14, color=WHITE, bold=True, align=PP_ALIGN.CENTER, valign=MSO_ANCHOR.MIDDLE)

    panel(slide, 7.76, 2.40, 4.84, 1.18, "正成本锚点", "dependency-chain div 在 head / body / tail 与不同 batch 重复；每个 bootstrap replicate 内重新估计 plugin-off 到 primary 的尺度。", fill=PALE_TEAL, accent=TEAL, title_size=18, detail_size=14)

    left = (
        ("样本闭合", "编码纯度 · pair 数 · ESS · bootstrap 有效率"),
        ("模型稳定", "Huber 收敛 · batch · 顺序 · run 内漂移"),
    )
    right = (
        ("跨运行一致性", "plugin-off · cross-clock · future-run interval"),
        ("宿主审计", "CPU 绑定 · SMT sibling · 频率 · PSI · 温度"),
    )
    for index, (title, detail) in enumerate(left):
        panel(slide, 3.00, 3.86 + index * 1.02, 4.44, 0.88, title, detail, fill=PALE_BLUE, accent=BLUE, title_size=17, detail_size=14)
    for index, (title, detail) in enumerate(right):
        panel(slide, 7.76, 3.86 + index * 1.02, 4.84, 0.88, title, detail, fill=PALE_PURPLE if index == 0 else PALE_GRAY, accent=PURPLE if index == 0 else MUTED, title_size=17, detail_size=14)

    boundary(slide, "输出保留中心值、同时区间、质量标签和具名失败项；下游函数归因可以区分严格成本、有限区间与探索估计。", label="分级产物")


def draw_10(slide) -> None:
    set_title(slide, CHAPTER4_TITLES[9])
    lead(slide, "机器学习读取同一批成对差分样本，从指令身份、执行模式和实验位置中寻找稳健回归尚未显式表达的结构。")

    stages = (
        ("差分样本", "ns / target-instruction", PALE_BLUE, BLUE),
        ("特征展开", "语义 · 编码 · pattern · batch · order · drift", PALE_TEAL, TEAL),
        ("HGB", "absolute-error loss", PALE_PURPLE, PURPLE),
        ("残差结构", "非线性关联与上下文交互", PALE_BLUE, BLUE),
        ("实验反馈", "新探针 · 新分层 · 采样优先级", PALE_GRAY, MUTED),
    )
    x = 3.00
    for index, (title, detail, fill, accent) in enumerate(stages):
        stage_box(slide, x, 2.38, 1.66, 1.44, title, detail, fill=fill, accent=accent)
        if index != len(stages) - 1:
            arrow(slide, x + 1.68, 3.10, x + 1.90, color=accent)
        x += 1.96

    panel(slide, 3.00, 4.12, 4.55, 1.60, "结构发现", "识别 dependency/reset、mixed-TB、batch、AB/BA 与 run 内位置之间的复杂关联，筛选需要建立专用差分窗口的上下文。", fill=PALE_BLUE, accent=BLUE, title_size=19, detail_size=15)
    panel(slide, 7.86, 4.12, 4.74, 1.60, "采样配置", "依据残差幅度、区间宽度和时间稳定性分配后续 run；把模型发现转化为可预注册、可独立复核的实验条件。", fill=PALE_TEAL, accent=TEAL, title_size=19, detail_size=15)
    boundary(slide, "60-run 数据：HGB OOF MAE 0.119238 ns，context+batch 基线 0.120531 ns；增量改善 0.001292 ns，约为 0.15 ns 实用尺度的 0.86%。", label="结构增量")


def draw_11(slide) -> None:
    set_title(slide, CHAPTER4_TITLES[10])
    lead(slide, "完整 super-run 分组避免同源数据泄漏；随机留出衡量独立重复的一致性，时间前向留出检验早期模型对后期运行的解释能力。")

    panel(slide, 3.00, 2.36, 4.55, 1.62, "随机完整组留出", "20 train → 20 calibration → 80 test\nGroupKFold 与 split conformal 都以完整 super-run 为最小分组。", fill=PALE_BLUE, accent=BLUE, title_size=19, detail_size=15, center=True)
    panel(slide, 7.86, 2.36, 4.74, 1.62, "时间前向留出", "早期 train → 中期 calibration → 后期 test\n采集顺序固定保留，用于识别共同慢漂移与时间外推风险。", fill=PALE_TEAL, accent=TEAL, title_size=19, detail_size=15, center=True)

    metric(slide, 3.00, 4.26, 2.90, "0.12258 ns", "HGB OOF MAE", fill=PALE_PURPLE, accent=PURPLE)
    metric(slide, 6.23, 4.26, 2.92, "0.12392 ns", "context+batch 基线", fill=PALE_BLUE, accent=BLUE)
    metric(slide, 9.48, 4.26, 3.12, "0.00134 ns", "增量改善", fill=PALE_TEAL, accent=TEAL)

    compact_row(slide, 5.40, "时间结构发现", "120-run 复核测得 lag-1 相关 0.36–0.61、早晚成本上升 2.3%–3.9%；前向 test 的 800/800 个差分分类保持一致。", fill=PALE_GRAY, accent=MUTED, h=0.58, title_w=1.92, detail_size=14)
    boundary(slide, "前向验证推动 CPU 亲和性、governor、温度遥测、QEMU 预热和 run-block Bootstrap 进入正式采集协议。", label="协议反馈")


def draw_12(slide) -> None:
    set_title(slide, CHAPTER4_TITLES[11])
    lead(slide, "统计估计与机器学习形成闭环：前者给出可解释的成本及区间，后者发现剩余结构并把它转化为下一轮受控实验。")

    stages = (
        ("受控微基准", "固定上下文", PALE_BLUE, BLUE),
        ("成对差分", "消除公共成本", PALE_TEAL, TEAL),
        ("稳健统计", "中心与同时区间", PALE_PURPLE, PURPLE),
        ("ML 结构检查", "残差与前向稳定性", PALE_BLUE, BLUE),
        ("实验协议更新", "探针、分层与门禁", PALE_GRAY, MUTED),
    )
    x = 3.00
    for index, (title, detail, fill, accent) in enumerate(stages):
        stage_box(slide, x, 2.36, 1.66, 1.42, title, detail, fill=fill, accent=accent)
        if index != len(stages) - 1:
            arrow(slide, x + 1.68, 3.07, x + 1.90, color=accent)
        x += 1.96

    compact_row(slide, 4.10, "指令层产物", "每个语义上下文的动态次数、中心成本、同时区间、稳定性标签与实验身份。", fill=PALE_BLUE, accent=BLUE, h=0.64, title_w=1.82)
    compact_row(slide, 4.88, "函数层产物", "结合动态 PC 与 ELF/map，将成本区间汇总到函数、trap/MM/VFS 等责任阶段。", fill=PALE_TEAL, accent=TEAL, h=0.64, title_w=1.82)
    compact_row(slide, 5.66, "现行实验设计", "205 个独立 super-run = 20 train + 39 calibration + 146 honest test；随机与时间前向两组共同复核。", fill=PALE_PURPLE, accent=PURPLE, h=0.40, title_w=1.82, detail_size=14)
    boundary(slide, "动态指令流从“调试日志”转化为可计数、可定价、可归因的证据；模型发现继续反馈新探针与独立 BuildStorm 验证。", label="方法收束")


DRAWERS = (
    draw_01,
    draw_02,
    draw_03,
    draw_04,
    draw_05,
    draw_06,
    draw_07,
    draw_08,
    draw_09,
    draw_10,
    draw_11,
    draw_12,
)


def update_transition(slide) -> None:
    replacement = "从动态指令出发，建立可归因、可复核的成本模型。"
    candidates = [
        shape
        for shape in slide.shapes
        if getattr(shape, "has_text_frame", False)
        and 3.10 <= inches(shape.top) <= 3.45
        and shape.text.strip()
    ]
    if not candidates:
        return
    box = candidates[0]
    frame = box.text_frame
    frame.clear()
    frame.margin_left = 0
    frame.margin_right = 0
    paragraph = frame.paragraphs[0]
    paragraph.alignment = PP_ALIGN.CENTER
    run = paragraph.add_run()
    run.text = replacement
    style_run(
        run,
        size=18,
        color=BODY,
        bold=False,
        chinese_font="SimSun",
        latin_font="Times New Roman",
    )


def insert_chapter(prs: Presentation) -> list:
    transition = find_slide(prs, CHAPTER4_TRANSITION)
    next_transition = find_slide(prs, CHAPTER5_TRANSITION)
    transition_index = list(prs.slides).index(transition)
    next_index = list(prs.slides).index(next_transition)
    if next_index <= transition_index + 1:
        raise RuntimeError("第四章缺少可复用正文模板")

    existing = list(prs.slides)[transition_index + 1 : next_index]
    template = existing[0]
    update_transition(transition)

    added = []
    for drawer in DRAWERS:
        slide = clone_slide(prs, template)
        clean_body_template(slide)
        drawer(slide)
        enforce_font_floor(slide)
        added.append(slide)

    for slide in existing:
        remove_slide(prs, slide)

    transition_index = list(prs.slides).index(transition)
    slide_ids = prs.slides._sldIdLst
    added_ids = list(slide_ids)[-len(added) :]
    for slide_id in added_ids:
        slide_ids.remove(slide_id)
    for offset, slide_id in enumerate(added_ids, 1):
        slide_ids.insert(transition_index + offset, slide_id)
    return added


def atomic_save(prs: Presentation, output: Path) -> None:
    output.parent.mkdir(parents=True, exist_ok=True)
    with tempfile.NamedTemporaryFile(
        prefix=f".{output.stem}-",
        suffix=".pptx",
        dir=output.parent,
        delete=False,
    ) as temporary:
        temporary_path = Path(temporary.name)
    try:
        prs.save(temporary_path)
        os.replace(temporary_path, output)
        output.chmod(0o644)
    except Exception:
        temporary_path.unlink(missing_ok=True)
        raise


def build_topic(full_output: Path, topic_output: Path) -> None:
    prs = Presentation(full_output)
    keep_titles = set(CHAPTER4_TITLES)
    for slide in list(prs.slides):
        if not (slide_texts(slide) & keep_titles):
            remove_slide(prs, slide)
    if len(prs.slides) != len(CHAPTER4_TITLES):
        raise RuntimeError(f"第四章专题页数错误：{len(prs.slides)}")
    atomic_save(prs, topic_output)


def validate(path: Path, *, expected_slides: int | None = None) -> None:
    prs = Presentation(path)
    if expected_slides is not None and len(prs.slides) != expected_slides:
        raise RuntimeError(f"{path} 页数错误：{len(prs.slides)}")
    titles_found = []
    for slide_number, slide in enumerate(prs.slides, 1):
        texts = slide_texts(slide)
        titles_found.extend(title for title in CHAPTER4_TITLES if title in texts)
        for shape in slide.shapes:
            if not getattr(shape, "has_text_frame", False) or not shape.text.strip():
                continue
            for paragraph in shape.text_frame.paragraphs:
                for run in paragraph.runs:
                    if not run.text.strip() or run.font.size is None:
                        continue
                    if any(title in texts for title in CHAPTER4_TITLES) and run.font.size.pt < MIN_FONT_PT:
                        raise RuntimeError(
                            f"第 {slide_number} 页文字小于 {MIN_FONT_PT:g} pt：{run.text!r}"
                        )
    if sorted(titles_found) != sorted(CHAPTER4_TITLES):
        raise RuntimeError("第四章标题集合不完整或重复")


def main() -> int:
    parser = argparse.ArgumentParser()
    root = Path(__file__).resolve().parents[2]
    parser.add_argument(
        "--base",
        type=Path,
        default=root / "output/presentations/mygo-defense-full.pptx",
    )
    parser.add_argument(
        "--full-output",
        type=Path,
        default=root / "output/presentations/mygo-defense-full.pptx",
    )
    parser.add_argument(
        "--topic-output",
        type=Path,
        default=root / "output/presentations/mygo-defense-chapter4-profiling-12pages.pptx",
    )
    args = parser.parse_args()

    base = args.base.resolve()
    full_output = args.full_output.resolve()
    topic_output = args.topic_output.resolve()
    prs = Presentation(base)
    insert_chapter(prs)
    atomic_save(prs, full_output)
    validate(full_output)
    build_topic(full_output, topic_output)
    validate(topic_output, expected_slides=len(CHAPTER4_TITLES))
    print(full_output)
    print(topic_output)
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
