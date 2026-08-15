#!/usr/bin/env python3
"""重做答辩稿中的 ELM 与网络栈专题，并接入正式全量稿。

专题共 25 页：ELM 19 页、网络栈 6 页。所有专题页可见文字均不小于 14 pt，
并删除正文模板右下角的章节数字。内容以 docs/chapters/chapter-12.typ 与
chapter-13.typ 以及对应实现为事实来源。
"""

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
TOP_Y = 1.80
BOTTOM_Y = 6.78

OLD_TOPIC_TITLES = {
    "可拓展内核单元（ELM）的概念定位",
    "责任单元与管理自举",
    "身份、代际与关系拓扑",
    "能力契约闭环",
    "调用路径分层",
    "镜像投影与装载证明",
    "生命周期与代际替换",
    "策略、预算与资源所有权",
    "原生执行与故障边界",
    "可复核运行证据",
    "工程接入与完成边界",
    "网络子系统责任分层",
    "FlowShard 数据通路",
    "套接字兼容与控制边界",
    "网络生命周期与性能边界",
}

TOPIC_TITLES = OLD_TOPIC_TITLES | {
    "可拓展内核单元（ELM）的概念定位",
    "常驻 Core、elm-mgr 与格式解析器",
    "Cell 身份与实现代际",
    "关系图中的四种责任",
    "能力发布：Contract、Port 与 Provider",
    "能力连接：Binding 与 Lease",
    "四条调用路径与各自成本",
    "从 EKI 文件到候选 EBI",
    "装载事务：提交前外部不可见",
    "生命周期状态与调用门禁",
    "Pause 与 Detach 的执行步骤",
    "Replace：同一 Cell 切换到下一代",
    "Policy、Budget、Lease 与长期资源",
    "原生调用门如何从故障返回",
    "一次 Busy 拒绝如何被复核",
    "开发入口、构建形态与当前接入范围",
    "网络对象分别由谁持有",
    "一次 sendmsg / recvmsg 经过哪些对象",
    "FlowShard 的并行模型与两条执行路径",
    "SocketFacade 如何保持 POSIX 语义稳定",
    "网络模块退出语义与性能开销",
    "装载事务：提交前外部不可见",
    "一次 Busy 拒绝如何被复核",
    "开发入口、构建形态与当前接入范围",
    "一次 sendmsg / recvmsg 经过哪些对象",
    "装载证明：来源与接口兼容",
    "装载提交：镜像准备与唯一公开点",
    "装载提交：镜像准备与一次性提交",
    "装载提交：两级门禁与失败回滚",
    "ELM 运行证据的五种视角",
    "ELM 的五类运行证据",
    "Busy 拒绝的逐项复核",
    "ElmModule 与 y / m / n 构建形态",
    "当前接入范围与未完成边界",
    "完整接收路径：VirtIO DMA 到 recvmsg",
    "完整发送路径：sendmsg 到 VirtIO completion",
    "原生调用门的故障返回机制",
    "网络对象的所有权划分",
    "SocketFacade 的 POSIX 稳定边界",
    "调用边界、核验位置与运行成本",
    "Replace 事务：ElmId 不变，Generation 递增",
    "原生调用门的故障收束边界",
    "Busy 拒绝的证据闭环",
    "网络对象的所有权与退出责任",
    "FlowShard 单写者模型与双执行路径",
    "网络退役协议、用户语义与成本归因",
    "网络能力与责任结构",
    "网络热路径的受管调用边界",
}


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


def slide_text(slide) -> str:
    return "\n".join(
        shape.text.strip()
        for shape in slide.shapes
        if getattr(shape, "has_text_frame", False) and shape.text.strip()
    )


def find_slide(prs: Presentation, exact_text: str):
    for slide in prs.slides:
        if any(
            getattr(shape, "has_text_frame", False)
            and shape.text.strip() == exact_text
            for shape in slide.shapes
        ):
            return slide
    raise RuntimeError(f"没有找到幻灯片：{exact_text}")


def remove_slide(prs: Presentation, slide) -> None:
    for slide_id in list(prs.slides._sldIdLst):
        if prs.part.related_part(slide_id.rId) is slide.part:
            prs.part.drop_rel(slide_id.rId)
            prs.slides._sldIdLst.remove(slide_id)
            return
    raise RuntimeError("没有找到待删除幻灯片关系")


def remove_existing_topic(prs: Presentation) -> None:
    for slide in list(prs.slides):
        texts = {
            shape.text.strip()
            for shape in slide.shapes
            if getattr(shape, "has_text_frame", False) and shape.text.strip()
        }
        if texts & TOPIC_TITLES:
            remove_slide(prs, slide)


def is_page_marker(shape) -> bool:
    if not getattr(shape, "has_text_frame", False):
        return False
    text = shape.text.strip()
    x, y = inches(shape.left), inches(shape.top)
    return x >= 12.0 and y >= 6.9 and text in {"01", "02", "03", "04", "05", "06"}


def remove_page_markers(prs: Presentation) -> None:
    for slide in prs.slides:
        for shape in list(slide.shapes):
            if is_page_marker(shape):
                element = shape._element
                element.getparent().remove(element)


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
        if is_page_marker(shape):
            element = shape._element
            element.getparent().remove(element)
            continue
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
        raise ValueError(f"正文文字不得小于 {MIN_FONT_PT} pt: {size}")
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
    size: float = 20.0,
    color=INK,
    align=PP_ALIGN.LEFT,
):
    return add_text(
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


def lead(slide, text: str) -> None:
    body(
        slide,
        text,
        CONTENT_X,
        1.78,
        CONTENT_W,
        0.62,
        size=16.0,
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
    title_size: float = 19.0,
    detail_size: float = 15.0,
    center: bool = False,
):
    add_rect(slide, x, y, w, h, fill)
    add_rect(slide, x, y, 0.09, h, accent)
    heading(
        slide,
        title,
        x + 0.23,
        y + 0.14,
        w - 0.42,
        0.34,
        size=title_size,
        color=NAVY if fill != NAVY else WHITE,
        align=PP_ALIGN.CENTER if center else PP_ALIGN.LEFT,
    )
    body(
        slide,
        detail,
        x + 0.23,
        y + 0.61,
        w - 0.44,
        h - 0.72,
        size=detail_size,
        color=WHITE if fill == NAVY else BODY,
        bold=True,
        align=PP_ALIGN.CENTER if center else PP_ALIGN.LEFT,
    )


def chip(
    slide,
    text: str,
    x: float,
    y: float,
    w: float,
    *,
    fill=BLUE,
    color=WHITE,
    size: float = 16.0,
):
    add_rect(slide, x, y, w, 0.38, fill)
    add_text(
        slide,
        text,
        x,
        y,
        w,
        0.38,
        size=max(size, MIN_FONT_PT),
        color=color,
        bold=True,
        chinese_font="SimHei",
        latin_font="Times New Roman",
        align=PP_ALIGN.CENTER,
        valign=MSO_ANCHOR.MIDDLE,
        margin=0.01,
    )


def boundary(
    slide,
    text: str,
    *,
    fill=PALE_GRAY,
    accent=MUTED,
    label: str | None = None,
    label_w: float = 1.42,
) -> None:
    add_rect(slide, CONTENT_X, 6.14, CONTENT_W, 0.64, fill)
    add_rect(slide, CONTENT_X, 6.14, 0.09, 0.64, accent)
    text_x = CONTENT_X + 0.23
    text_w = CONTENT_W - 0.42
    align = PP_ALIGN.CENTER
    if label:
        heading(
            slide,
            label,
            CONTENT_X + 0.23,
            6.25,
            label_w,
            0.32,
            size=17.0,
            color=NAVY,
        )
        text_x = CONTENT_X + 0.34 + label_w
        text_w = CONTENT_W - label_w - 0.56
        align = PP_ALIGN.LEFT
    body(
        slide,
        text,
        text_x,
        6.18,
        text_w,
        0.56,
        size=14.0,
        color=INK,
        bold=True,
        align=align,
        valign=MSO_ANCHOR.MIDDLE,
    )


def flow(slide, x1, y1, x2, y2, *, color=BLUE, direction="right", width=1.6):
    add_line(slide, x1, y1, x2, y2, color, width)
    add_arrow_tip(slide, x2, y2, direction, color, 0.11)


def step(
    slide,
    x: float,
    y: float,
    w: float,
    h: float,
    title: str,
    detail: str = "",
    *,
    fill=PALE_BLUE,
    accent=BLUE,
):
    add_rect(slide, x, y, w, h, fill)
    add_rect(slide, x, y, 0.08, h, accent)
    heading(
        slide,
        title,
        x + 0.15,
        y + 0.13,
        w - 0.25,
        0.30,
        size=17.0,
        align=PP_ALIGN.CENTER,
        color=WHITE if fill == NAVY else INK,
    )
    if detail:
        body(
            slide,
            detail,
            x + 0.13,
            y + 0.53,
            w - 0.22,
            h - 0.61,
            size=16.0,
            color=WHITE if fill == NAVY else BODY,
            align=PP_ALIGN.CENTER,
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


def compact_row(
    slide,
    y: float,
    title: str,
    detail: str,
    *,
    fill=PALE_BLUE,
    accent=BLUE,
    h: float = 0.62,
    title_w: float = 2.10,
) -> None:
    add_rect(slide, CONTENT_X, y, CONTENT_W, h, fill)
    add_rect(slide, CONTENT_X, y, 0.09, h, accent)
    text_color = WHITE if fill == NAVY else INK
    heading(
        slide,
        title,
        CONTENT_X + 0.24,
        y + 0.14,
        title_w,
        0.31,
        size=18,
        color=text_color,
    )
    body(
        slide,
        detail,
        CONTENT_X + 0.28 + title_w,
        y + 0.10,
        CONTENT_W - title_w - 0.52,
        h - 0.16,
        size=15,
        color=text_color,
        bold=True,
        valign=MSO_ANCHOR.MIDDLE,
    )


def numbered_row(
    slide,
    y: float,
    number: str,
    title: str,
    detail: str,
    *,
    fill=PALE_BLUE,
    accent=BLUE,
    h: float = 0.78,
    title_w: float = 2.08,
) -> None:
    """全宽流程行；给 14 pt 正文保留两行空间。"""
    add_rect(slide, CONTENT_X, y, CONTENT_W, h, fill)
    add_rect(slide, CONTENT_X, y, 0.09, h, accent)
    add_rect(slide, CONTENT_X + 0.20, y + 0.19, 0.40, 0.40, accent)
    body(
        slide,
        number,
        CONTENT_X + 0.20,
        y + 0.19,
        0.40,
        0.40,
        size=16,
        color=WHITE,
        bold=True,
        align=PP_ALIGN.CENTER,
        valign=MSO_ANCHOR.MIDDLE,
    )
    heading(
        slide,
        title,
        CONTENT_X + 0.76,
        y + 0.13,
        title_w,
        h - 0.24,
        size=17,
        color=NAVY,
    )
    body(
        slide,
        detail,
        CONTENT_X + 0.88 + title_w,
        y + 0.09,
        CONTENT_W - title_w - 1.12,
        h - 0.16,
        size=15,
        color=INK,
        bold=True,
        valign=MSO_ANCHOR.MIDDLE,
    )


def definition_row(
    slide,
    y: float,
    term: str,
    definition: str,
    runtime_rule: str,
    *,
    fill=PALE_BLUE,
    accent=BLUE,
    h: float = 0.82,
    term_w: float = 1.72,
    rule_w: float = 2.56,
) -> None:
    """术语、定义与运行规则三列并列，避免只展示名词。"""
    add_rect(slide, CONTENT_X, y, CONTENT_W, h, fill)
    add_rect(slide, CONTENT_X, y, 0.09, h, accent)
    heading(
        slide,
        term,
        CONTENT_X + 0.24,
        y + 0.13,
        term_w,
        h - 0.24,
        size=17,
        color=NAVY,
    )
    body(
        slide,
        definition,
        CONTENT_X + 0.34 + term_w,
        y + 0.10,
        CONTENT_W - term_w - rule_w - 0.66,
        h - 0.17,
        size=14,
        color=INK,
        bold=True,
        valign=MSO_ANCHOR.MIDDLE,
    )
    add_rect(
        slide,
        CONTENT_RIGHT - rule_w - 0.08,
        y + 0.11,
        0.03,
        h - 0.22,
        accent,
    )
    body(
        slide,
        runtime_rule,
        CONTENT_RIGHT - rule_w + 0.06,
        y + 0.10,
        rule_w - 0.18,
        h - 0.17,
        size=14,
        color=INK,
        bold=True,
        valign=MSO_ANCHOR.MIDDLE,
    )


def process_box(
    slide,
    x: float,
    y: float,
    w: float,
    h: float,
    number: str,
    title: str,
    detail: str,
    *,
    fill=PALE_BLUE,
    accent=BLUE,
) -> None:
    add_rect(slide, x, y, w, h, fill)
    add_rect(slide, x, y, 0.09, h, accent)
    add_rect(slide, x + 0.17, y + 0.14, 0.38, 0.38, accent)
    body(
        slide,
        number,
        x + 0.17,
        y + 0.14,
        0.38,
        0.38,
        size=16,
        color=WHITE,
        bold=True,
        align=PP_ALIGN.CENTER,
        valign=MSO_ANCHOR.MIDDLE,
    )
    heading(slide, title, x + 0.65, y + 0.16, w - 0.82, 0.32, size=17, color=NAVY)
    body(
        slide,
        detail,
        x + 0.20,
        y + 0.61,
        w - 0.38,
        h - 0.72,
        size=15,
        color=BODY,
        bold=True,
        align=PP_ALIGN.LEFT,
    )


def arrow_caption(slide, x1: float, y: float, x2: float, label: str, *, color=BLUE) -> None:
    flow(slide, x1, y, x2, y, color=color)
    if label:
        body(
            slide,
            label,
            (x1 + x2) / 2 - 0.72,
            y - 0.43,
            1.44,
            0.30,
            size=16,
            color=color,
            bold=True,
            align=PP_ALIGN.CENTER,
            valign=MSO_ANCHOR.MIDDLE,
        )


def draw_01(slide) -> None:
    set_title(slide, "可拓展内核单元（ELM）的概念定位")
    lead(slide, "传统模块已经具备装载、符号、依赖和签名；ELM 的改变，是把分散约束收敛为一个可计算的责任边界。")

    panel(
        slide,
        3.00,
        2.66,
        2.55,
        2.78,
        "传统模块已有能力",
        "装入镜像\n解析符号\n维护依赖\n签名与引用保护",
        fill=PALE_GRAY,
        accent=MUTED,
        detail_size=17.0,
        center=True,
    )
    panel(
        slide,
        5.88,
        2.66,
        2.82,
        2.78,
        "约束分散位置",
        "装载器\n子系统注册表\n引用计数与回调\n日志与运维工具",
        fill=PALE_PURPLE,
        accent=PURPLE,
        detail_size=17.0,
        center=True,
    )
    panel(
        slide,
        9.03,
        2.66,
        3.57,
        2.78,
        "ELM 统一责任面",
        "身份与代际\n关系与能力契约\n生命周期与资源\n故障与运行证据",
        fill=NAVY,
        accent=BLUE,
        detail_size=17.0,
        center=True,
    )
    flow(slide, 5.55, 4.05, 5.84, 4.05, color=MUTED)
    flow(slide, 8.70, 4.05, 8.99, 4.05, color=PURPLE)
    boundary(slide, "代码能够装入只是起点；能被发现、约束、复核和安全退役，才构成可治理的扩展。")


def draw_02(slide) -> None:
    set_title(slide, "责任单元与管理自举")
    lead(slide, "常驻 Core 持有硬约束；管理器和格式投影器进入同一拓扑，但不能反向改写 Core 不变量。")

    panel(
        slide,
        3.00,
        2.62,
        3.05,
        2.62,
        "ELM Core",
        "状态机与关系图\n端口、租约与预算\n装载证明与事务提交\n故障恢复与审计事实",
        fill=NAVY,
        accent=BLUE,
        detail_size=16.0,
        center=True,
    )
    panel(
        slide,
        6.48,
        2.62,
        2.70,
        2.62,
        "elm-mgr · ID 1",
        "根管理 Cell\n接收外部管理意图\n编排装载、策略、菜单和生命周期",
        fill=PALE_BLUE,
        accent=BLUE,
        center=True,
    )
    panel(
        slide,
        9.61,
        2.62,
        2.99,
        2.62,
        "eki · ID 2",
        "elm-mgr 的内建子单元\n把 EKI 投影为 EBI\n来源显示为 <builtin>",
        fill=PALE_PURPLE,
        accent=PURPLE,
        center=True,
    )
    flow(slide, 6.05, 3.93, 6.44, 3.93, color=BLUE)
    flow(slide, 9.18, 3.93, 9.57, 3.93, color=PURPLE)
    chip(slide, "启动内建端口", 3.00, 5.57, 1.58, fill=TEAL)
    body(
        slide,
        "core.log@1   core.event@1   mgr.menu.item@1   mgr.action.invoke@1",
        4.78,
        5.53,
        7.82,
        0.48,
        size=16.0,
        color=NAVY,
        bold=True,
        align=PP_ALIGN.CENTER,
        valign=MSO_ANCHOR.MIDDLE,
    )
    boundary(slide, "自举表示管理能力也有身份和状态，不表示最小可信 Core 可以被普通模块替换。")


def draw_03(slide) -> None:
    set_title(slide, "身份、代际与关系拓扑")
    lead(slide, "逻辑 Cell 身份保持稳定，具体实现由 Generation 区分；长期引用必须同时匹配二者。")

    heading(slide, "代际时间轴", 3.00, 2.54, 3.40, size=20)
    for x, label, fill, accent in (
        (3.00, "Gen 1", PALE_GRAY, MUTED),
        (4.68, "Gen 2", PALE_PURPLE, PURPLE),
        (6.36, "Gen 3", PALE_BLUE, BLUE),
    ):
        step(slide, x, 3.08, 1.36, 0.78, label, fill=fill, accent=accent)
    flow(slide, 4.36, 3.47, 4.64, 3.47, color=MUTED)
    flow(slide, 6.04, 3.47, 6.32, 3.47, color=PURPLE)
    body(
        slide,
        "ElmId(42) 始终不变\n旧代句柄在提交点后明确失效",
        3.00,
        4.20,
        4.72,
        1.02,
        size=16.0,
        color=INK,
        bold=True,
        align=PP_ALIGN.CENTER,
    )

    heading(slide, "BindingGraph 的五类关系", 8.15, 2.54, 4.45, size=20)
    panel(slide, 8.15, 3.03, 2.04, 1.28, "父子", "管理归属\n预算委派", fill=PALE_BLUE, accent=BLUE, title_size=18)
    panel(slide, 10.47, 3.03, 2.13, 1.28, "依赖", "激活前置条件", fill=PALE_TEAL, accent=TEAL, title_size=18)
    panel(slide, 8.15, 4.38, 2.04, 1.28, "拓展点", "目标开放位置", fill=PALE_PURPLE, accent=PURPLE, title_size=18)
    panel(slide, 10.47, 4.38, 2.13, 1.28, "拓展项", "契约附着实现", fill=PALE_GRAY, accent=MUTED, title_size=18)
    chip(slide, "能力绑定：消费者 → Port → Provider", 8.15, 5.78, 4.45, fill=NAVY)
    boundary(slide, "关系提交检查端点、契约、重复和有向环；运行 ID 不是地址，也不保证跨启动稳定。")


def draw_04(slide) -> None:
    set_title(slide, "能力契约闭环")
    lead(slide, "一个能力必须从声明走到真实执行，并且在仍有活动引用时不能被提前撤销。")

    stages = [
        ("绑定请求", "消费者 + PortId", PALE_PURPLE, PURPLE, 1.70),
        ("Flow Contract", "name@version", PALE_BLUE, BLUE, 1.72),
        ("PortId", "连接点", PALE_TEAL, TEAL, 1.46),
        ("Binding + Lease", "连接与使用权", PALE_GRAY, MUTED, 1.88),
        ("Provider", "真实执行后端", NAVY, BLUE, 1.62),
    ]
    x = 3.00
    for index, (title, detail, fill, accent, width) in enumerate(stages):
        step(slide, x, 2.88, width, 1.24, title, detail, fill=fill, accent=accent)
        if index < len(stages) - 1:
            flow(slide, x + width, 3.50, x + width + 0.20, 3.50, color=accent)
        x += width + 0.20

    panel(
        slide,
        3.00,
        4.45,
        2.93,
        1.51,
        "Port",
        "方向、模式、访问策略\nowner Generation 与状态",
        fill=PALE_BLUE,
        accent=BLUE,
        title_size=18,
    )
    panel(
        slide,
        6.26,
        4.45,
        2.93,
        1.51,
        "Binding",
        "消费者—端口之间\n已提交、可审计的连接",
        fill=PALE_PURPLE,
        accent=PURPLE,
        title_size=18,
    )
    panel(
        slide,
        9.52,
        4.45,
        3.08,
        1.51,
        "Lease",
        "Active → Revoking → Revoked\n引用归零后完成撤销",
        fill=PALE_TEAL,
        accent=TEAL,
        title_size=18,
    )
    boundary(slide, "Provider / 受管调用使用 256 B 固定帧；管理 ABI 使用有界缓冲，硬上限 256 KiB。")


def draw_05(slide) -> None:
    set_title(slide, "调用路径分层")
    lead(slide, "ELM 不强迫所有调用经过通用 RPC；发现需求、ABI 稳定性和热路径敏感度共同决定路径。")

    panel(
        slide,
        3.00,
        2.60,
        2.90,
        1.58,
        "Provider",
        "动态发现、授权、审计\n同步 / 异步、取消与背压",
        fill=PALE_BLUE,
        accent=BLUE,
        title_size=19,
        center=True,
    )
    panel(
        slide,
        6.15,
        2.60,
        2.90,
        1.58,
        "Managed Import",
        "受管 ELM 接口\n每次调用核验双方代际",
        fill=PALE_PURPLE,
        accent=PURPLE,
        title_size=19,
        center=True,
    )
    panel(
        slide,
        9.30,
        2.60,
        3.30,
        1.58,
        "direct-pinned",
        "固定 export 与 Generation\n装载期精确校验 Rust ABI",
        fill=PALE_TEAL,
        accent=TEAL,
        title_size=19,
        center=True,
    )
    panel(
        slide,
        3.00,
        4.38,
        4.40,
        1.42,
        "kernel-symbol",
        "从登记目录解析常驻内核真实实现；校验名称、契约、版本、权限和完整 Rust ABI。",
        fill=NAVY,
        accent=BLUE,
        title_size=19,
    )
    panel(
        slide,
        7.72,
        4.38,
        4.88,
        1.42,
        "Runtime / Management 根 API",
        "普通 Cell 只取得运行时命名空间；Manager 取得管理命名空间，但每次 dispatch 仍重新鉴权。",
        fill=PALE_GRAY,
        accent=MUTED,
        title_size=19,
    )
    boundary(slide, "治理检查前置到装载、绑定和状态切换；稳定热路径不重复经过 elm-mgr。")


def draw_06(slide) -> None:
    set_title(slide, "镜像投影与装载证明")
    lead(slide, "文件格式解释与 Core 不变量分离：任何来源只能产出候选 EBI，最终执行资格仍由 Core 独立证明。")

    stages = [
        ("Upload", "分段写入", PALE_GRAY, MUTED, 1.28),
        ("Seal", "范围 + SHA-256", PALE_PURPLE, PURPLE, 1.42),
        ("EKI Source", "格式投影", PALE_BLUE, BLUE, 1.55),
        ("EBI", "装载协议对象", PALE_TEAL, TEAL, 1.42),
        ("Loader", "校验 + 重定位", PALE_BLUE, BLUE, 1.52),
        ("Commit", "初始化后公开", NAVY, BLUE, 1.45),
    ]
    x = 3.00
    for index, (title, detail, fill, accent, width) in enumerate(stages):
        step(slide, x, 2.72, width, 1.13, title, detail, fill=fill, accent=accent)
        if index < len(stages) - 1:
            flow(slide, x + width, 3.29, x + width + 0.17, 3.29, color=accent)
        x += width + 0.17

    panel(
        slide,
        3.00,
        4.12,
        4.46,
        1.88,
        "装载证明",
        "来源证明：签名或 BuildBound 绑定\n签名来源的 release epoch\n目标架构、EKI / EBI 与 Rust ABI\nimports / exports、W^X 与 I-cache",
        fill=PALE_BLUE,
        accent=BLUE,
        title_size=19,
    )
    panel(
        slide,
        7.78,
        4.12,
        4.82,
        1.88,
        "发布不变量",
        "create / initialize 成功前不对外可见；关系、Provider 与 imports 先暂存，唯一提交点之后才进入 Active。",
        fill=PALE_TEAL,
        accent=TEAL,
        title_size=19,
    )
    boundary(slide, "EBI 不是文件格式；soyo 仍是未来来源，签名也不能证明模块业务逻辑正确。")


def draw_07(slide) -> None:
    set_title(slide, "生命周期与代际替换")
    lead(slide, "状态机给出合法方向；真正安全性来自预检、锁外钩子、复核、唯一提交点和失败回滚。")

    load_states = [
        ("Discovered", PALE_GRAY, MUTED),
        ("Verified", PALE_PURPLE, PURPLE),
        ("Loaded", PALE_BLUE, BLUE),
        ("Linked", PALE_TEAL, TEAL),
        ("Ready", PALE_BLUE, BLUE),
        ("Active", NAVY, BLUE),
    ]
    x = 3.00
    for index, (name, fill, accent) in enumerate(load_states):
        width = 1.39 if name != "Discovered" else 1.57
        step(slide, x, 2.64, width, 0.65, name, fill=fill, accent=accent)
        if index < len(load_states) - 1:
            flow(slide, x + width, 2.97, x + width + 0.16, 2.97, color=accent)
        x += width + 0.16

    step(slide, 3.00, 3.64, 2.65, 1.13, "Quiescing", "停止新工作并等待排空", fill=PALE_BLUE, accent=BLUE)
    step(slide, 5.98, 3.64, 2.65, 1.13, "Paused", "保留镜像，可恢复", fill=PALE_PURPLE, accent=PURPLE)
    step(slide, 8.96, 3.64, 1.73, 1.13, "Detached", "摘除拓扑", fill=PALE_GRAY, accent=MUTED)
    step(slide, 10.99, 3.64, 1.61, 1.13, "Retired", "完成回收", fill=PALE_TEAL, accent=TEAL)
    flow(slide, 5.65, 4.20, 5.94, 4.20, color=BLUE)
    flow(slide, 8.63, 4.20, 8.92, 4.20, color=PURPLE)
    flow(slide, 10.69, 4.20, 10.95, 4.20, color=MUTED)

    heading(slide, "Replace：Generation N → N+1", 3.00, 5.03, 3.50, size=19)
    for x, title, fill, accent in (
        (3.00, "影子装载", PALE_BLUE, BLUE),
        (5.10, "静默旧代", PALE_PURPLE, PURPLE),
        (7.20, "可选迁移", PALE_TEAL, TEAL),
        (9.30, "唯一提交点", NAVY, BLUE),
        (11.40, "回收旧代", PALE_GRAY, MUTED),
    ):
        step(slide, x, 5.47, 1.20, 0.59, title, fill=fill, accent=accent)
    boundary(slide, "迁移状态上限 64 KiB；默认迁移钩子返回不支持，direct-pinned importer 与长期资源可阻断替换。")


def draw_08(slide) -> None:
    set_title(slide, "策略、预算与资源所有权")
    lead(slide, "权限、容量和长期副作用是三个不同问题，分别由 Policy、Budget 与 Owned Resource 管理。")

    panel(
        slide,
        3.00,
        2.62,
        2.93,
        2.93,
        "Policy",
        "父级给出能力上限\n子级只能缩小权限\n管理能力不自动继承\n策略更新校验代际与 epoch",
        fill=PALE_PURPLE,
        accent=PURPLE,
        title_size=20,
        detail_size=16,
        center=True,
    )
    panel(
        slide,
        6.27,
        2.62,
        2.93,
        2.93,
        "Budget",
        "端口与队列\n订阅与 pending load\n镜像、栈、并发调用\n动态内存与故障记录",
        fill=PALE_BLUE,
        accent=BLUE,
        title_size=20,
        detail_size=16,
        center=True,
    )
    panel(
        slide,
        9.54,
        2.62,
        3.06,
        2.93,
        "Owned Resource",
        "任务、定时器与工作项\n回调、IRQ 与异步请求\n设备和自定义对象\n常驻操作表负责安全收束",
        fill=PALE_TEAL,
        accent=TEAL,
        title_size=20,
        detail_size=16,
        center=True,
    )
    chip(slide, "退役顺序", 3.00, 5.64, 1.32, fill=NAVY)
    body(
        slide,
        "stop admission → quiesce → cancel → drain → release",
        4.57,
        5.60,
        8.03,
        0.46,
        size=17.0,
        color=NAVY,
        bold=True,
        align=PP_ALIGN.CENTER,
        valign=MSO_ANCHOR.MIDDLE,
    )
    boundary(slide, "CPU 时间当前只记账与统计超额，不实施调度节流；部分设备资源可 Detach，但会阻断可回滚 Pause。")


def draw_09(slide) -> None:
    set_title(slide, "原生执行与故障边界")
    lead(slide, "调用门能够把受支持的 fault、panic 与 timeout 收束到固定出口，并隔离责任 Cell；它不是地址空间沙箱。")

    stages = [
        ("执行预检", "代际 / 策略 / 预算", PALE_BLUE, BLUE, 1.58),
        ("ElmGuard", "64 KiB 栈 + 双 Guard 页", PALE_PURPLE, PURPLE, 1.82),
        ("原生 ELM", "Hook · Provider · Entry", PALE_TEAL, TEAL, 1.72),
        ("异常", "fault / panic / timeout", PALE_GRAY, MUTED, 1.66),
        ("恢复出口", "重写 PC / SP / 返回值", NAVY, BLUE, 1.82),
    ]
    x = 3.00
    for index, (title, detail, fill, accent, width) in enumerate(stages):
        step(slide, x, 2.73, width, 1.32, title, detail, fill=fill, accent=accent)
        if index < len(stages) - 1:
            flow(slide, x + width, 3.35, x + width + 0.20, 3.35, color=accent)
        x += width + 0.20

    panel(
        slide,
        3.00,
        4.38,
        4.58,
        1.49,
        "Fault Dump",
        "Cell · 阶段 · PC · fault address · cause · recovery PC / SP",
        fill=PALE_PURPLE,
        accent=PURPLE,
        title_size=19,
        center=True,
    )
    panel(
        slide,
        7.90,
        4.38,
        4.70,
        1.49,
        "隔离标志与状态迁移",
        "普通原生故障先设置 isolated，阻止新 Provider / Binding / import；生命周期收束失败才进入 Quarantined。",
        fill=PALE_TEAL,
        accent=TEAL,
        title_size=19,
    )
    boundary(slide, "共享特权地址空间中的写入无法由调用门撤销；常驻 Kernel Provider 也不属于原生 ELM Guard。")


def draw_10(slide) -> None:
    set_title(slide, "可复核运行证据")
    lead(slide, "可追踪性不是增加日志：记录以 ElmId 与 sequence 关联，Generation、Binding、ticket 按类型补充。")

    add_rect(slide, 4.10, 2.58, 7.40, 0.62, NAVY)
    add_rect(slide, 4.10, 2.58, 0.09, 0.62, BLUE)
    heading(slide, "共同锚点", 4.36, 2.72, 1.34, 0.30, size=18, color=WHITE)
    body(
        slide,
        "ElmId · sequence；其余身份字段按记录类型补充",
        5.75,
        2.69,
        5.48,
        0.33,
        size=16,
        color=WHITE,
        bold=True,
        align=PP_ALIGN.CENTER,
        valign=MSO_ANCHOR.MIDDLE,
    )
    evidence = [
        (3.00, "Snapshot", "当前事实", PALE_BLUE, BLUE),
        (4.98, "Event", "顺序变化", PALE_TEAL, TEAL),
        (6.96, "Audit", "主体与决策", PALE_PURPLE, PURPLE),
        (8.94, "Trace", "执行阶段", PALE_BLUE, BLUE),
        (10.92, "Journal", "哈希链顺序", PALE_GRAY, MUTED),
    ]
    for x, title, detail, fill, accent in evidence:
        step(slide, x, 3.52, 1.68, 1.02, title, detail, fill=fill, accent=accent)

    for x, w, title, detail, fill, accent in (
        (3.00, 4.58, "管理链", "elmctl → sys_elm_ctl → elm-mgr → Core", PALE_BLUE, BLUE),
        (7.88, 4.72, "只读诊断面", "/sys/kernel/elm · 19 个正式节点", PALE_TEAL, TEAL),
    ):
        add_rect(slide, x, 4.90, w, 0.86, fill)
        add_rect(slide, x, 4.90, 0.09, 0.86, accent)
        heading(slide, title, x + 0.25, 5.15, 1.42, 0.30, size=18, color=NAVY)
        body(
            slide,
            detail,
            x + 1.67,
            5.11,
            w - 1.90,
            0.36,
            size=16,
            color=INK,
            bold=True,
            align=PP_ALIGN.CENTER,
            valign=MSO_ANCHOR.MIDDLE,
        )
    boundary(slide, "Journal 默认运行在内存易失模式；当前 replay 主要恢复 trust epoch，不重建 Cell、队列或执行现场。")


def draw_11(slide) -> None:
    set_title(slide, "工程接入与完成边界")
    lead(slide, "ELM 已进入真实驱动和网络执行链；完成度按“运行闭环、受限实现、TODO”区分，而不是用目标态替代现状。")

    panel(
        slide,
        3.00,
        2.62,
        3.02,
        2.30,
        "Rust 开发框架",
        "声明：ElmModule + 模块宏\n启动：create / initialize\n退出：quiesce / finalize\npause、migrate、entry 为可选钩子",
        fill=PALE_BLUE,
        accent=BLUE,
        title_size=19,
        center=True,
    )
    panel(
        slide,
        6.29,
        2.62,
        3.02,
        2.30,
        "y / m / n",
        "m：生成受管 EKI\ny：静态集成，不创建动态 Cell\nn：不构建或打包\n实际模式由 .config 决定",
        fill=PALE_PURPLE,
        accent=PURPLE,
        title_size=19,
        center=True,
    )
    panel(
        slide,
        9.58,
        2.62,
        3.02,
        2.30,
        "真实模块链",
        "net.stack · net.loopback\nvirtio.framework · virtio.block\nnet.virtio\nBuildBound 进入统一治理",
        fill=PALE_TEAL,
        accent=TEAL,
        title_size=19,
        center=True,
    )
    chip(slide, "当前实现", 3.00, 5.12, 1.42, fill=TEAL)
    body(slide, "Cell / Generation、EKI、调用分流、资源归属与故障收束", 4.60, 5.09, 8.00, 0.43, size=16, bold=True, valign=MSO_ANCHOR.MIDDLE)
    chip(slide, "受限 / TODO", 3.00, 5.61, 1.42, fill=MUTED)
    body(slide, "设备 / VFS Provider 需显式注册；IRQ、DMA、MMIO、block、packet 的通用 Provider 契约尚未接通", 4.60, 5.58, 8.00, 0.43, size=16, bold=True, valign=MSO_ANCHOR.MIDDLE)
    boundary(slide, "下一部分用网络栈说明：治理可以进入真实数据面，但不会把每个网络包送进通用管理分发。")


def draw_12(slide) -> None:
    set_title(slide, "网络子系统责任分层")
    lead(slide, "常规 TCP/IP 能力只作完整性证明；核心设计是把用户 ABI、协议状态和设备队列分给三个稳定责任域。")

    layers = [
        ("POSIX syscall", "socket · bind · connect · sendmsg · recvmsg · poll", PALE_BLUE, BLUE),
        ("VFS / Socket facade", "fd、等待队列、readiness 与用户 ABI", PALE_BLUE, BLUE),
        ("常驻 Host / Broker", "worker、registrar、代际与 pinned call slot", PALE_GRAY, MUTED),
        ("net.stack ELM", "FlowShard、路由、邻居、PMTU 与协议定时器", PALE_PURPLE, PURPLE),
        ("net.virtio / loopback", "queue、buffer pool、DMA / IRQ 与设备生命周期", PALE_TEAL, TEAL),
    ]
    y = 2.48
    for index, (title, detail, fill, accent) in enumerate(layers):
        add_rect(slide, 3.00, y, 9.60, 0.57, fill)
        add_rect(slide, 3.00, y, 0.09, 0.57, accent)
        heading(slide, title, 3.25, y + 0.11, 2.68, 0.30, size=18, color=NAVY)
        body(slide, detail, 5.88, y + 0.09, 6.44, 0.36, size=16, color=INK, bold=True, valign=MSO_ANCHOR.MIDDLE)
        if index < len(layers) - 1:
            flow(slide, 7.80, y + 0.57, 7.80, y + 0.72, color=accent, direction="down")
        y += 0.74
    boundary(slide, "IPv4 / IPv6、TCP、UDP、ICMP、Raw、路由、邻居、PMTU、分片重组与 VirtIO-net 已具备。")


def draw_13(slide) -> None:
    set_title(slide, "FlowShard 数据通路")
    lead(slide, "网络性能结构来自分片单写者、有界批次和代际固定调用，而不是增加一个全局轮询线程。")

    stages = [
        ("VirtIO 队列", "IRQ / 设备事件", PALE_TEAL, TEAL, 1.76),
        ("Host worker", "批量收集", PALE_GRAY, MUTED, 1.62),
        ("shard-turn", "direct-pinned", PALE_PURPLE, PURPLE, 1.68),
        ("FlowShard", "单写者协议状态", NAVY, BLUE, 1.70),
        ("TxPlan", "批量提交", PALE_BLUE, BLUE, 1.50),
    ]
    x = 3.00
    for index, (title, detail, fill, accent, width) in enumerate(stages):
        step(slide, x, 2.68, width, 1.16, title, detail, fill=fill, accent=accent)
        if index < len(stages) - 1:
            flow(slide, x + width, 3.26, x + width + 0.24, 3.26, color=accent)
        x += width + 0.24

    panel(
        slide,
        3.00,
        4.18,
        3.00,
        1.64,
        "分片原则",
        "shard 数 ≤ 活动 CPU 与 queue pair；单队列设备不制造虚假并行。",
        fill=PALE_BLUE,
        accent=BLUE,
        title_size=18,
    )
    panel(
        slide,
        6.30,
        4.18,
        3.00,
        1.64,
        "本地快速路径",
        "Busy、预算 / 代际 / 调用失败，或大块连续流量时回落 owner worker。",
        fill=PALE_PURPLE,
        accent=PURPLE,
        title_size=18,
    )
    panel(
        slide,
        9.60,
        4.18,
        3.00,
        1.64,
        "调用约束",
        "每 CPU 固定 slot；校验代际、结构、提交位与宿主允许地址范围。",
        fill=PALE_TEAL,
        accent=TEAL,
        title_size=18,
    )
    boundary(slide, "当前真实热路径不是 Nexus packet.rx / packet.tx；准确表述是池化缓冲、租约和批量边界。")


def draw_14(slide) -> None:
    set_title(slide, "套接字兼容与控制边界")
    lead(slide, "POSIX 兼容保留在常驻 VFS / facade，协议内部使用类型化命令、配置快照和分片状态，两侧可以分别演进。")

    panel(
        slide,
        3.00,
        2.58,
        4.48,
        2.84,
        "数据与等待通路",
        "fd → FileOps → Socket facade → flow command\n\nsocket / bind / listen / accept\nconnect / sendmsg / recvmsg / shutdown\npoll / epoll / close / signal interruption",
        fill=PALE_BLUE,
        accent=BLUE,
        title_size=20,
        detail_size=16,
    )
    panel(
        slide,
        8.12,
        2.58,
        4.48,
        2.84,
        "配置与控制通路",
        "SIOC ioctl → ConfigStore → ConfigSnapshot\nnetlink：查询与 dump\n\n地址、路由、邻居与 PMTU\nMTU、管理启停与链路状态",
        fill=PALE_TEAL,
        accent=TEAL,
        title_size=20,
        detail_size=16,
    )
    flow(slide, 7.48, 4.00, 8.08, 4.00, color=PURPLE)
    chip(slide, "常驻 facade", 5.82, 5.68, 3.96, fill=NAVY)
    boundary(slide, "SIOC 写路径主要覆盖 IPv4，netlink 只查询 / dump；正常就绪精确唤醒，终止事件按 socket 广播。")


def draw_15(slide) -> None:
    set_title(slide, "网络生命周期与性能边界")
    lead(slide, "网络实例证明 ELM 管理的是实际状态、调用和资源，而不只是模块名称；安全退出优先于伪装成无损迁移。")

    rows = [
        ("net.stack", "建 FlowShard → 注册 shard / local endpoint → quiesce → begin_remove → 销毁代际", PALE_PURPLE, PURPLE),
        ("net.virtio", "校验 framework → 注册 MMIO / PCI driver → 停队列 → 分离设备 → 注销 driver", PALE_TEAL, TEAL),
        ("net.loopback", "建回环队列 → 注册设备 → 静默队列 → 销毁状态 → 注销设备", PALE_BLUE, BLUE),
    ]
    y = 2.55
    for title, detail, fill, accent in rows:
        add_rect(slide, 3.00, y, 9.60, 0.78, fill)
        add_rect(slide, 3.00, y, 0.09, 0.78, accent)
        heading(slide, title, 3.25, y + 0.18, 1.65, 0.34, size=19)
        body(slide, detail, 4.95, y + 0.14, 7.34, 0.45, size=16, color=INK, bold=True, valign=MSO_ANCHOR.MIDDLE)
        y += 0.96

    for x, w, title, detail, fill, accent in (
        (3.00, 3.02, "性能来源", "复制 · 推进 · 竞争 · 唤醒", PALE_BLUE, BLUE),
        (6.28, 3.02, "稳定失败", "NetworkDown / hangup", PALE_PURPLE, PURPLE),
        (9.56, 3.04, "后续方向", "等待者选择 · packet Provider", PALE_GRAY, MUTED),
    ):
        add_rect(slide, x, 5.54, w, 0.50, fill)
        add_rect(slide, x, 5.54, 0.09, 0.50, accent)
        heading(slide, title, x + 0.20, 5.65, 1.00, 0.28, size=17, color=NAVY)
        body(
            slide,
            detail,
            x + 1.13,
            5.63,
            w - 1.27,
            0.30,
            size=16,
            color=INK,
            bold=True,
            align=PP_ALIGN.CENTER,
            valign=MSO_ANCHOR.MIDDLE,
        )
    boundary(slide, "已验证 Detach + Reload：旧连接稳定失效，新 Cell / handle 生效；原位 Replace 与状态迁移尚未完成。")


def explain_01(slide) -> None:
    set_title(slide, "可拓展内核单元（ELM）的概念定位")
    lead(slide, "ELM（Extensible Loadable Module，可拓展内核单元）把扩展代码、公开入口、活动引用、长期资源和故障挂到同一条 Cell 记录；装载、调用和退出都以这条记录作准入依据。")

    panel(
        slide,
        3.00,
        2.55,
        4.42,
        2.45,
        "Cell · 一项扩展的运行记录",
        "Core 持有 Cell 记录，保存 ElmId、当前 Generation、状态、管理父级、公开能力、资源账本和故障事实。镜像可更换，Cell 仍表示同一项逻辑服务；它不是文件、地址或 Rust 句柄。",
        fill=PALE_BLUE,
        accent=BLUE,
        title_size=20,
    )
    panel(
        slide,
        7.75,
        2.55,
        4.85,
        2.45,
        "LKM / KLD · 约束分散在不同机制",
        "传统模块同样具备装载、依赖、签名和引用保护；但镜像状态、子系统注册、活动回调、长期资源和诊断信息分别由不同机制保存，安全退出依赖各子系统按自己的规则完成收束。",
        fill=PALE_PURPLE,
        accent=PURPLE,
        title_size=20,
    )
    compact_row(
        slide,
        5.22,
        "Cell 记录的运行事实",
        "ElmId / Generation · 当前状态 · Port / Binding / Lease · 活动执行 · 资源归属 · 具名退役阻断项",
        fill=PALE_TEAL,
        accent=TEAL,
        h=0.70,
        title_w=2.74,
    )
    boundary(
        slide,
        "net.stack Cell 登记镜像和 Provider；FlowShard 由 net.stack 持有，活动 turn 与资源登记携带同一 ElmId / Generation。",
        label="net.stack 例",
    )


def explain_02(slide) -> None:
    set_title(slide, "常驻 Core、elm-mgr 与格式解析器")
    lead(slide, "Core 是常驻的权威状态持有者；elm-mgr（ElmId 1）编排管理请求；eki（ElmId 2）把 EKI 投影为 EBI。三者的身份和越权边界都由 Core 记录。")

    panel(
        slide,
        3.00,
        2.52,
        2.90,
        2.62,
        "Core · 常驻事实与提交",
        "常驻、不可被普通 Cell 替换。保存 Cell、关系、策略、预算和事务；分配身份、核对前置条件并提交状态，不解释网络或设备业务载荷。",
        fill=NAVY,
        accent=BLUE,
        title_size=19,
    )
    panel(
        slide,
        6.24,
        2.52,
        2.85,
        2.62,
        "elm-mgr · ElmId 1",
        "具有 Cell 身份的根管理单元。接收装载、暂停、替换和策略请求，整理为 Core 操作；只能请求提交，不能直接改写权威状态。请求参数非法时在 Core 预检阶段拒绝。",
        fill=PALE_BLUE,
        accent=BLUE,
        title_size=19,
    )
    panel(
        slide,
        9.43,
        2.52,
        3.17,
        2.62,
        "eki · ElmId 2",
        "具有 Cell 身份的内建格式解析器。EKI 是当前 ELM 镜像文件格式；读取已密封文件并生成候选 EBI。EBI 是 Core 接收的内存装载说明，不是文件；格式错误在投影阶段拒绝。",
        fill=PALE_PURPLE,
        accent=PURPLE,
        title_size=19,
    )
    flow(slide, 5.90, 3.83, 6.20, 3.83, color=BLUE)
    flow(slide, 9.09, 3.83, 9.39, 3.83, color=PURPLE)
    compact_row(
        slide,
        5.42,
        "外部管理请求路径",
        "elmctl → sys_elm_ctl → elm-mgr 组织参数 → Core 预检 / 提交 → 返回状态与阻断项（blocker）",
        fill=PALE_GRAY,
        accent=MUTED,
        h=0.70,
        title_w=2.66,
    )
    boundary(
        slide,
        "elm-mgr 与 eki 也有 Cell / Generation；它们不能绕过 Core 提交状态。不可替换部分只有保存全局不变量的常驻 Core。",
        label="不可替换部分",
    )


def explain_03(slide) -> None:
    set_title(slide, "Cell 身份与实现代际")
    lead(slide, "ElmId 标识逻辑服务，Generation 标识当前实现代次。只有新实现提交成功后代次才递增；代次不匹配时调用被拒绝，旧网络 fd 返回 NetworkDown。")

    panel(
        slide,
        3.00,
        2.52,
        4.05,
        2.74,
        "一条 Cell 记录保存的关键事实",
        "ElmId：本次启动中的强类型编号\nGeneration：当前实现代际\nstate：当前是否允许调用\nparent：管理归属\npolicy / budget：权限与额度\nsource：镜像来源和证明结果",
        fill=PALE_BLUE,
        accent=BLUE,
        title_size=19,
    )
    heading(slide, "一般动态 Cell 的代次切换示意", 7.42, 2.57, 5.18, 0.36, size=20)
    for x, label, detail, fill, accent in (
        (7.42, "G1", "初次装载", PALE_GRAY, MUTED),
        (9.19, "G2", "首次成功切换", PALE_PURPLE, PURPLE),
        (10.96, "G3", "再次成功切换", PALE_TEAL, TEAL),
    ):
        step(slide, x, 3.12, 1.46, 1.02, label, detail, fill=fill, accent=accent)
    flow(slide, 8.88, 3.63, 9.15, 3.63, color=MUTED)
    flow(slide, 10.65, 3.63, 10.92, 3.63, color=PURPLE)
    body(
        slide,
        "逻辑 ElmId 始终相同；只有新实现成功提交后 Generation 才递增。句柄 (Cell N, G1) 在 G2 公开后明确失效，即使旧地址尚未复用。此图说明 Core 的事务语义，不代表网络连接已完成原位迁移。",
        7.42,
        4.48,
        5.18,
        0.80,
        size=16,
        color=INK,
        bold=True,
        align=PP_ALIGN.CENTER,
    )
    compact_row(
        slide,
        5.52,
        "强类型 ID",
        "ElmId、PortId、BindingId、LeaseId 底层均为 u64，但不能互换；零值保留，ID 也不等于内核地址。",
        fill=PALE_TEAL,
        accent=TEAL,
        h=0.62,
        title_w=1.62,
    )
    boundary(
        slide,
        "长期引用保存 ElmId + Generation 并在使用时重新匹配；各类 ID 仅在本次启动和对象生命周期内有效。",
        label="失效规则",
    )


def explain_04(slide) -> None:
    set_title(slide, "关系图中的四种责任")
    lead(slide, "BindingGraph 是 Core 持有的关系表：父子边表示管理归属，依赖边表示声明的服务依赖，拓展边表示允许插入的位置，Binding 表示已经提交的调用许可；四类边分别检查，不能相互冒充。")

    panel(
        slide,
        3.00,
        2.50,
        4.48,
        1.48,
        "父子关系",
        "记录管理归属和预算委派。父级给出权限与额度上限；成为子单元不等于取得父级业务接口。",
        fill=PALE_BLUE,
        accent=BLUE,
        title_size=19,
    )
    panel(
        slide,
        7.80,
        2.50,
        4.80,
        1.48,
        "依赖关系",
        "记录提供者与完整契约。装载时检查目标、契约和依赖环；被依赖者退役时成为阻断项。不授予管理权限或调用资格。",
        fill=PALE_TEAL,
        accent=TEAL,
        title_size=19,
    )
    panel(
        slide,
        3.00,
        4.18,
        4.48,
        1.48,
        "拓展点与拓展项",
        "目标 Cell 先声明允许附加行为的位置和契约，拓展项再按该契约挂入；不能对任意函数或局部变量加钩子。",
        fill=PALE_PURPLE,
        accent=PURPLE,
        title_size=19,
    )
    panel(
        slide,
        7.80,
        4.18,
        4.80,
        1.48,
        "能力绑定",
        "Binding 保存消费者、目标 Port、契约、消费者代次和 Lease；提供者由 Port owner 与后端追溯。它是调用许可，不是函数指针。",
        fill=PALE_GRAY,
        accent=MUTED,
        title_size=19,
    )
    boundary(
        slide,
        "父子、依赖、拓展边检查端点与非法环；Binding 检查消费者、Port、契约和重复项。关系记录先被撤销，相关对象才允许回收。",
        label="图一致性",
    )


def explain_05(slide) -> None:
    set_title(slide, "能力发布：Contract、Port 与 Provider")
    lead(slide, "ELM 将一项动态服务拆为契约、端口和执行后端：契约给调用双方解释字节和错误的共同名称，Port 保存发现与访问属性，Provider 保存真正执行请求的后端。")

    process_box(slide, 3.00, 2.58, 2.92, 2.05, "1", "流契约（Contract）", "Core 比对 name@version；业务规范据此解释载荷布局、返回值和错误码。", fill=PALE_PURPLE, accent=PURPLE)
    process_box(slide, 6.26, 2.58, 2.92, 2.05, "2", "连接点（Port）", "Core 持有 PortId、owner、Contract、方向、访问范围和后端状态；它不是函数地址。", fill=PALE_BLUE, accent=BLUE)
    process_box(slide, 9.52, 2.58, 3.08, 2.05, "3", "执行者（Provider）", "Port 对应的执行记录：常驻回调或原生 ELM handler。无后端时返回“未实现”。", fill=PALE_TEAL, accent=TEAL)
    flow(slide, 5.92, 3.49, 6.22, 3.49, color=PURPLE)
    flow(slide, 9.18, 3.49, 9.48, 3.49, color=BLUE)
    compact_row(slide, 4.92, "mgr.action.invoke@1", "管理动作帧；Port 4 是 elm-mgr 控制入口；Provider 按 action_id 执行并返回固定回复。", fill=PALE_GRAY, accent=MUTED, h=0.66, title_w=2.74)
    compact_row(slide, 5.57, "当前运行时契约", "专用 Runtime：core.log、core.event；通用 Provider：mgr.action.invoke；mgr.menu.item 绑定后登记菜单项。", fill=PALE_TEAL, accent=TEAL, h=0.66, title_w=2.74)
    boundary(
        slide,
        "Port 不固定 Generation；通用调用时读取 owner 当前代次，并检查 Binding / Lease、双方状态和后端是否存在。",
        label="调用门槛",
    )


def explain_06(slide) -> None:
    set_title(slide, "能力连接：Binding 与 Lease")
    lead(slide, "Binding 记录消费者获准使用的 Port；Lease 记录消费者身份、代次、权限和 active_refs。通用调用门、原生 Provider 与异步请求持有该引用；内建即时入口不持有。")

    numbered_row(slide, 2.43, "1", "提出绑定", "消费者提交 ElmId、目标 PortId 和预期 Contract；Core 读取当前 Generation。", fill=PALE_BLUE, accent=BLUE, h=0.65)
    numbered_row(slide, 3.16, "2", "Core 预检", "固定消费者代次；检查状态、策略、目标 Port、Contract、访问范围、额度和重复关系。", fill=PALE_PURPLE, accent=PURPLE, h=0.65)
    numbered_row(slide, 3.89, "3", "提交使用权", "生成 BindingId 与 Active、active_refs=0 的 Lease；此后连接才对调用方可见。", fill=PALE_TEAL, accent=TEAL, h=0.65)
    compact_row(slide, 4.64, "需持 Lease 的调用", "调用前核对 Binding、双方状态与代次；引用加一，结束或异步请求终止后释放。", fill=PALE_BLUE, accent=BLUE, h=0.66, title_w=2.30)
    compact_row(slide, 5.39, "当前撤销语义", "引用非零：Busy 且不改对象；归零：删除 Binding，Lease 经 Revoking → Revoked 后移出。", fill=PALE_GRAY, accent=MUTED, h=0.66, title_w=2.30)
    boundary(
        slide,
        "Provider 帧含 BindingId、CallId、opcode 和至多 256 B 内联载荷，不传内核裸指针；管理 ABI 的独立硬上限为 256 KiB。",
        label="边界规格",
    )


def explain_07(slide) -> None:
    set_title(slide, "调用边界、核验位置与运行成本")
    lead(slide, "调用路径按“每次要核验什么、调用者得到什么”分流：Provider 适合发现与撤销，managed import 适合代次敏感接口，direct-pinned 适合热路径，kernel-symbol 只指向常驻白名单。")

    definition_row(slide, 2.42, "Provider 枢纽", "每次查 Binding / Port、取得 Lease、校验固定帧；得到动态发现、异步、取消和审计。", "查表 + 引用 + 帧校验", fill=PALE_BLUE, accent=BLUE, h=0.76)
    definition_row(slide, 3.31, "受管导入", "接口先暂存；每次调用核验双方状态与 Generation，任一变化即失效。", "逐次代次核验", fill=PALE_PURPLE, accent=PURPLE, h=0.76)
    definition_row(slide, 4.20, "固定导入", "direct-pinned 在装载期固定 export、完整 Rust ABI 与代次；网络 PinnedNativeCall 仍逐次查状态、代次和 frame 范围。", "存活 importer 阻断替换", fill=PALE_TEAL, accent=TEAL, h=0.76)
    definition_row(slide, 5.09, "内核符号", "kernel-symbol 只从常驻白名单解析名称、版本、权限与 Rust ABI，不按 ELM Generation 路由。", "登记目录内的常驻实现", fill=PALE_GRAY, accent=MUTED, h=0.76)
    boundary(
        slide,
        "当前网络 Host 仅通过带逐次状态、代次和范围检查的 PinnedNativeCall 调用 shard-turn / local-turn；普通 Cell 不取得管理入口表。",
        label="当前路径",
    )


def explain_08(slide) -> None:
    set_title(slide, "从 EKI 文件到候选 EBI")
    lead(slide, "文件格式与装载规则彼此分离：格式解析器只把字节转换成候选装载说明，是否允许执行仍由 Core 按同一套规则判断。")

    panel(slide, 3.00, 2.52, 2.88, 2.62, "EKI · 当前文件格式", "固定头部和块表；上传会话要求每片 offset 紧接已写内容，并检查总长度。Seal 后核对完整 SHA-256，输入不再可改。", fill=PALE_BLUE, accent=BLUE, title_size=19)
    panel(slide, 6.24, 2.52, 2.92, 2.62, "投影源（Projection Source）", "当前由内建 eki Cell 提供；校验头部、块类型、必需块、块范围重叠和摘要，再输出候选 EBI。它不能激活模块。", fill=PALE_PURPLE, accent=PURPLE, title_size=18)
    panel(slide, 9.52, 2.52, 3.08, 2.62, "EBI · 统一装载对象", "不是磁盘文件。它保存清单、段、符号、重定位、imports / exports、生命周期入口、Provider 声明和 ABI 指纹。", fill=PALE_TEAL, accent=TEAL, title_size=19)
    flow(slide, 5.88, 3.83, 6.20, 3.83, color=BLUE)
    flow(slide, 9.16, 3.83, 9.48, 3.83, color=PURPLE)
    compact_row(slide, 5.40, "新增格式接入", "新增格式只需提供新的投影源，Core 仍消费同一种 EBI；当前生产路径只接通 EKI，其他容器格式尚未接入。", fill=PALE_GRAY, accent=MUTED, h=0.70, title_w=2.28)
    boundary(
        slide,
        "投影源没有激活权限；架构、来源、ABI、Policy、关系和 Budget 均由 Core 在候选 EBI 上复核。",
        label="权限分界",
    )


def explain_09(slide) -> None:
    set_title(slide, "装载证明：来源与接口兼容")
    lead(slide, "所有候选都要证明内容与 ABI；签名来源额外核验信任链、撤销状态和发布代次，构建绑定来源则核验构建清单、镜像摘要与当前接口指纹。")

    numbered_row(slide, 2.46, "1", "镜像摘要", "上传会话 Seal 后核对整镜像 SHA-256；后续投影必须引用同一份不可变输入。", fill=PALE_BLUE, accent=BLUE, h=0.75)
    numbered_row(slide, 3.35, "2", "签名来源", "校验签名者、受信公钥、撤销状态和单调发布代次；同一来源不能装回低于已接受代次的旧镜像。", fill=PALE_PURPLE, accent=PURPLE, h=0.75)
    numbered_row(slide, 4.24, "3", "构建绑定来源", "另一条来源路径不依赖发布代次，而是要求构建清单、镜像摘要和当前内核接口指纹完全一致；其发布代次固定为 0。", fill=PALE_TEAL, accent=TEAL, h=0.75)
    numbered_row(slide, 5.13, "4", "目标与 ABI", "核对架构、panic 策略、代码模型、target feature、接口摘要、入口范围和完整 Rust ABI。", fill=PALE_GRAY, accent=MUTED, h=0.75)
    boundary(
        slide,
        "签名只证明来源与内容完整，不证明业务逻辑正确；任一证明不一致均在 create / initialize 前拒绝。",
        label="证明范围",
    )


def explain_09b(slide) -> None:
    set_title(slide, "装载提交：两级门禁与失败回滚")
    lead(slide, "原生装载分两次提交：initialize 后安装可观察拓扑，但独占执行令牌阻止外部调用；entry 成功后才提交信任、导入导出和镜像所有权。")

    numbered_row(slide, 2.46, "1", "镜像准备", "分配分页、复制段、清零 BSS、完成重定位和 W^X；导入、导出与 Provider 先进入暂存记录。", fill=PALE_BLUE, accent=BLUE, h=0.75)
    numbered_row(slide, 3.35, "2", "initialize", "锁外执行 initialize；失败时放弃信任与暂存导入，释放独占令牌，并把 Cell 隔离。", fill=PALE_PURPLE, accent=PURPLE, h=0.75)
    numbered_row(slide, 4.24, "3", "拓扑门禁", "initialize 成功后，Core 安装关系与 Provider 并推进到 Active；独占执行令牌仍在，外部调用返回 Busy。", fill=PALE_TEAL, accent=TEAL, h=0.75)
    numbered_row(slide, 5.13, "4", "entry 与最终提交", "entry 成功后提交信任、导入、导出与 native image 并释放令牌；失败则撤销已安装关系并进入 Quarantined。", fill=PALE_GRAY, accent=MUTED, h=0.75)
    boundary(
        slide,
        "Active 在此处不等于立即可调用：exclusive_execution 在 entry 结束前构成第二道门禁；失败会留下可审计的隔离事实。",
        label="可见性边界",
    )


def explain_10(slide) -> None:
    set_title(slide, "生命周期状态与调用门禁")
    lead(slide, "状态表示 Core 已经把这项扩展处理到哪一步；实际准入还同时检查 Generation、策略版本、isolated、活动执行、Lease 与长期资源。")

    compact_row(slide, 2.42, "准备阶段", "Discovered：只有来源 → Verified：证明通过 → Loaded：镜像归运行时所有 → Linked：重定位和 import 完成", fill=PALE_BLUE, accent=BLUE, h=0.82, title_w=1.62)
    compact_row(slide, 3.38, "激活阶段", "Ready：initialize 已成功，等待关系与调用入口公开提交 → Active：关系和 Provider 已安装；原生 entry 结束前仍持独占执行令牌，外部调用返回 Busy", fill=PALE_TEAL, accent=TEAL, h=0.82, title_w=1.62)
    compact_row(slide, 4.34, "退出阶段", "Quiescing：拒绝新工作并排空 → Paused：保留镜像、可恢复 → Detached：从公开关系摘除 → Retired：完成回收", fill=PALE_PURPLE, accent=PURPLE, h=0.82, title_w=1.62)
    compact_row(slide, 5.30, "故障处理", "记录 native fault 并置 isolated；生命周期钩子或资源收束失败时，再走 Faulted → Quarantined", fill=PALE_GRAY, accent=MUTED, h=0.82, title_w=1.62)
    boundary(
        slide,
        "Policy、Generation、关系、Lease、活动执行与长期资源同时满足后才提交状态迁移。",
        label="提交条件",
    )


def explain_11(slide) -> None:
    set_title(slide, "Pause 与 Detach 的执行步骤")
    lead(slide, "quiesce 表示停止接纳新调用并排空；Pause 可回滚，Detach 不可逆。两者都在锁内定计划、锁外执行钩子、再回锁核对，模块代码不持有 Core 全局锁。")

    numbered_row(slide, 2.42, "1", "锁内预检", "按请求类型读取 Cell / Generation、策略版本、活动调用、Lease、关系和资源状态，形成本次执行计划。", fill=PALE_BLUE, accent=BLUE, h=0.64)
    numbered_row(slide, 3.18, "2", "锁外执行", "Pause：quiesce → suspend → pause；Detach：quiesce → cancel → drain → release。", fill=PALE_PURPLE, accent=PURPLE, h=0.64)
    numbered_row(slide, 3.94, "3", "锁内复核", "比较代次、policy epoch、独占令牌和资源状态；任一变化，本次提交失败，不沿用旧计划。", fill=PALE_TEAL, accent=TEAL, h=0.64)
    compact_row(slide, 4.76, "Pause · 可回滚", "suspend 或 pause 失败：恢复资源并调用 resume；状态不提交。", fill=PALE_BLUE, accent=BLUE, h=0.60, title_w=2.14)
    compact_row(slide, 5.46, "Detach · 不可逆", "排空资源后才摘除关系与镜像；依赖者、子 Cell 和拓展项可阻断。", fill=PALE_GRAY, accent=MUTED, h=0.60, title_w=2.14)
    boundary(
        slide,
        "Pause 与 Detach 使用不同 blocker 集合；钩子或资源回滚失败时记录故障阶段并隔离 Cell。",
        label="阻断条件",
    )


def explain_12(slide) -> None:
    set_title(slide, "Replace 事务：ElmId 不变，Generation 递增")
    lead(slide, "Replace 保留 ElmId，在影子区准备 Generation N+1，静默旧代并按需迁移；只有复核成功才切换代次。旧代恢复失败时不伪装成功，而是隔离 Cell。")

    stages = [
        ("1", "影子装入 N+1", "完成证明、链接和 initialize"),
        ("2", "静默旧代 N", "拒绝新工作并等待调用退出"),
        ("3", "可选迁移", "默认不支持；export → ≤64 KiB → import"),
        ("4", "一次提交", "切换代次、后端、导入导出与资源归属"),
        ("5", "回收旧代", "旧代不可发现；finalize 后释放镜像"),
    ]
    x = 3.00
    widths = [1.72, 1.72, 1.72, 1.72, 1.72]
    for index, (number, title, detail) in enumerate(stages):
        step(slide, x, 2.52, widths[index], 1.70, f"{number}  {title}", detail, fill=PALE_TEAL if number in {"1", "4"} else PALE_PURPLE, accent=TEAL if number in {"1", "4"} else PURPLE)
        if number != "5":
            flow(slide, x + widths[index], 3.37, x + widths[index] + 0.20, 3.37, color=PURPLE)
        x += widths[index] + 0.22
    panel(slide, 3.00, 4.48, 4.56, 1.58, "失败发生的位置", "提交前销毁 N+1 并恢复旧代；恢复失败进入 Quarantined。提交后旧代 finalize 失败只记错，不撤回代次切换。", fill=PALE_BLUE, accent=BLUE, title_size=18)
    panel(slide, 7.86, 4.48, 4.74, 1.58, "Replace 阻断项", "活动调用 / Lease、排队 Provider、固定 importer、不可迁移资源、动态分配，以及 ABI / 重定位不兼容。", fill=PALE_GRAY, accent=MUTED, title_size=18)
    boundary(
        slide,
        "依赖和拓展表面必须兼容，通常不随提交切换；网络仅验证 Detach + Reload，旧 socket 报 NetworkDown / HANGUP。",
        label="实现范围",
    )


def explain_13(slide) -> None:
    set_title(slide, "Policy、Budget、Lease 与长期资源")
    lead(slide, "同一个 Cell 同时受四种不同约束：操作权限、资源上限、正在使用的短期引用、需要子系统清理的长期对象；四者分别记录，避免只靠引用计数猜测能否退出。")

    panel(slide, 3.00, 2.46, 4.56, 1.56, "策略（Policy）· 能否执行", "按生命周期、绑定、Provider、原生执行、观测和管理动作授权。父级给上限，子级只能收窄；更新时递增策略版本。", fill=PALE_PURPLE, accent=PURPLE, title_size=19)
    panel(slide, 7.86, 2.46, 4.74, 1.56, "预算（Budget）· 最多占用多少", "分别在端口登记、队列提交、镜像与原生栈分配、调用准入和动态分配时检查上限；父级为存活子级保留额度。", fill=PALE_BLUE, accent=BLUE, title_size=19)
    panel(slide, 3.00, 4.24, 4.56, 1.56, "租约（Lease）· 是否仍被使用", "保存 owner / Generation、权限、Binding、状态和 active_refs；活动引用阻止端口、绑定或资源提前撤销。", fill=PALE_TEAL, accent=TEAL, title_size=19)
    panel(slide, 7.86, 4.24, 4.74, 1.56, "长期资源（Owned Resource）· 谁回收", "任务、定时器、工作项、回调、IRQ、异步请求和设备按 Cell / Generation 登记；清理操作表位于常驻子系统。", fill=PALE_GRAY, accent=MUTED, title_size=19)
    boundary(
        slide,
        "停止接纳 → 静默 → 取消 → 排空 → 逆序释放；CPU 时间目前仅记账与统计，不执行调度节流。",
        label="退役协议",
    )


def explain_14(slide) -> None:
    set_title(slide, "原生调用门的故障收束边界")
    lead(slide, "原生 ELM 与内核共享特权级和地址空间，因此不能承诺隔离恶意写入；调用门解决的是可恢复故障：为每次进入准备受控栈、执行记账和固定返回现场。")

    panel(slide, 3.00, 2.48, 2.92, 2.34, "进入前校验", "核对 Cell / Generation、Policy、Budget、入口地址和目标 feature；ElmGuard 保存阶段、期限、代码范围及恢复 PC / SP。", fill=PALE_BLUE, accent=BLUE, title_size=19)
    panel(slide, 6.24, 2.48, 2.92, 2.34, "受控执行现场", "切换到两端带 Guard page 的 64 KiB 独立栈，再执行 hook、entry、Provider handler 或 managed call。", fill=PALE_PURPLE, accent=PURPLE, title_size=19)
    panel(slide, 9.48, 2.48, 3.12, 2.34, "异常记录与固定出口", "trap / panic 保存 fault PC、访问地址、原因和阶段；改写 trap frame 回到固定出口，返回错误并释放执行记账。", fill=PALE_TEAL, accent=TEAL, title_size=19)
    compact_row(slide, 5.02, "故障后的 Cell", "普通原生故障先置 isolated 并拒绝新调用；生命周期钩子或资源收束失败才进入 Faulted → Quarantined。", fill=PALE_BLUE, accent=BLUE, h=0.54, title_w=2.12)
    boundary(
        slide,
        "内存写不能回滚，恶意代码仍共享地址空间；调用门只保存整数 ABI，未覆盖的 Float / Vector / SIMD 镜像在装载前拒绝。",
        label="恢复边界",
        label_w=1.72,
    )


def explain_15(slide) -> None:
    set_title(slide, "ELM 的五类运行证据")
    lead(slide, "运行时提供五类独立查询视图：当前状态、变化顺序、操作主体、控制路径和管理日志。它们各自排序，不能把一张快照误当成完整执行历史。")

    definition_row(slide, 2.38, "快照 Snapshot", "Cell / Port 基础快照给出状态；Binding、拓扑、执行与资源由对应查询视图补齐。", "拼出当时条件", fill=PALE_BLUE, accent=BLUE, h=0.66, rule_w=1.88, term_w=1.92)
    definition_row(slide, 3.14, "事件 Event", "一类状态变化的顺序记录；读取方可以从该事件流的 sequence 继续消费。", "还原该事件流顺序", fill=PALE_TEAL, accent=TEAL, h=0.66, rule_w=1.88, term_w=1.92)
    definition_row(slide, 3.90, "审计 Audit", "记录操作主体、管理动作、结果、拒绝码和具名阻断项。", "核对谁做了什么", fill=PALE_PURPLE, accent=PURPLE, h=0.66, rule_w=1.88, term_w=1.92)
    definition_row(slide, 4.66, "路径 Trace", "生命周期、Provider、拓展、Replace、策略和资源的操作级记录：动作、对象、结果、阻断项。", "定位失败类别", fill=PALE_BLUE, accent=BLUE, h=0.66, rule_w=1.88, term_w=1.92)
    definition_row(slide, 5.42, "日志链 Journal", "240 B 记录以前后 SHA-256 哈希相连；默认仅保留 256 条内存环。", "检查顺序与完整性", fill=PALE_GRAY, accent=MUTED, h=0.66, rule_w=1.88, term_w=1.92)
    boundary(
        slide,
        "各视图独立排序，以对象 ID 或事务票据关联；Journal 默认易失，只恢复 trust epoch，不恢复 Cell、关系、队列或执行现场。",
        label="关联与持久性",
    )


def explain_15b(slide) -> None:
    set_title(slide, "Busy 拒绝记录样例")
    lead(slide, "示意编号，非实测编号：待替换 Cell 自身是 B4 的消费者，L7 是随 B4 创建、归该消费者代次所有的 Lease。预检发现 L7.active_refs > 0，返回 Busy；状态和 Generation 不变。")

    numbered_row(slide, 2.46, "1", "审计记录", "Audit 给出发起主体、目标 ElmId、Replace 动作、Busy 结果和 blocker；audit sequence 只排序审计流。", fill=PALE_BLUE, accent=BLUE, h=0.75)
    numbered_row(slide, 3.35, "2", "绑定查询", "Binding 记录给出 B4 的消费者、Port、Contract、Generation 和 L7；Port 查询再找到 owner 与后端。", fill=PALE_PURPLE, accent=PURPLE, h=0.75)
    numbered_row(slide, 4.24, "3", "执行对照", "executions / diagnostics 给出 port、binding、lease、开始时间和后端代次；与 L7.active_refs 对照。", fill=PALE_TEAL, accent=TEAL, h=0.75)
    numbered_row(slide, 5.13, "4", "状态复核", "Replace 预检未提交，Cell 状态和 Generation 保持原值；调用释放 L7 后，下一次预检才可能通过。", fill=PALE_GRAY, accent=MUTED, h=0.75)
    boundary(
        slide,
        "需要同时存在的证据：Audit 解释谁请求、Binding / Lease 解释哪条连接仍在使用、Execution 解释引用由哪次调用持有、Cell 状态证明没有半提交。",
        label="本例所需记录",
    )


def explain_16(slide) -> None:
    set_title(slide, "ElmModule 与 y / m / n 构建形态")
    lead(slide, "业务实现通过 ElmModule 声明生命周期入口；构建配置再决定它是常驻内核代码、受管动态 Cell，还是完全不进入镜像。")

    panel(slide, 3.00, 2.48, 4.48, 2.32, "开发接口（ElmModule）", "必选 create / initialize / finalize；按需实现 quiesce、pause / resume、migrate 和 entry。属性宏生成描述符、生命周期入口和 ABI 材料，但不能替作者证明业务回滚。", fill=PALE_BLUE, accent=BLUE, title_size=20)
    panel(slide, 7.80, 2.48, 4.80, 2.32, "构建选择（Modules.toml + .config）", "m：位置无关镜像 → EKI → 受管动态 Cell\ny：静态归档 → initcall，不具有动态 Cell / Generation\nn：不构建，也不打包\n最终模式以当前 .config 为准。", fill=PALE_PURPLE, accent=PURPLE, title_size=20)
    compact_row(slide, 5.10, "mode = m", "获得装载证明、Cell / Generation、Policy / Budget、动态调用关系、Pause / Detach / Replace 和运行证据。", fill=PALE_TEAL, accent=TEAL, h=0.76, title_w=1.62)
    compact_row(slide, 5.99, "mode = y", "同一业务实现作为常驻内核代码运行；有 initcall，但没有动态 Cell、Provider、Mixin 或代际替换语义。", fill=PALE_GRAY, accent=MUTED, h=0.64, title_w=1.62)


def explain_16b(slide) -> None:
    set_title(slide, "当前接入范围与未完成边界")
    lead(slide, "把“代码已有”“生产已接通”和“仍需接入”分开列出；模块声明、运行时注册和真实数据路径不是同一件事。")

    panel(slide, 3.00, 2.46, 4.56, 1.72, "通用代码已有", "Cell / Generation、EKI 投影、调用路径、生命周期、Policy / Budget、故障隔离和证据记录。", fill=PALE_TEAL, accent=TEAL, title_size=19)
    panel(slide, 7.86, 2.46, 4.74, 1.72, "生产路径已接通", "net.stack、net.loopback、net.virtio 以私有 direct-pinned / PinnedNativeCall 接入真实网络路径。", fill=PALE_BLUE, accent=BLUE, title_size=19)
    panel(slide, 3.00, 4.46, 4.56, 1.46, "仍需显式注册", "设备与 VFS Provider 不能从模块声明自动出现，必须由子系统在启动路径登记。", fill=PALE_PURPLE, accent=PURPLE, title_size=19)
    panel(slide, 7.86, 4.46, 4.74, 1.46, "尚未接通", "通用 packet / IRQ / DMA / MMIO / block Provider；其他容器格式投影源；公共发布体系。", fill=PALE_GRAY, accent=MUTED, title_size=19)
    boundary(
        slide,
        "网络热路径使用专用 direct-pinned endpoint；通用 packet、IRQ、DMA、MMIO 与 block Provider 未接通。",
        label="生产边界",
    )


def explain_17(slide) -> None:
    set_title(slide, "网络能力与责任结构")
    lead(slide, "网络栈已经覆盖常用接口、协议与设备路径；这些通用能力只列出实现范围，后续集中说明 ELM 接入后形成的对象责任、代际门禁和退出语义。")

    panel(slide, 3.00, 2.45, 4.55, 1.66, "用户接口与协议范围", "POSIX socket、bind、connect、listen、accept、sendmsg、recvmsg 与 poll / epoll 已进入 VFS 文件接口；支持 IPv4 / IPv6、TCP / UDP / ICMP / Raw。", fill=PALE_BLUE, accent=BLUE, title_size=19, detail_size=15)
    panel(slide, 7.86, 2.45, 4.74, 1.66, "寻址、链路与设备范围", "路由、邻居、PMTU、IP 分片与重组已有运行路径；VirtIO-net 负责队列、DMA 缓冲和中断，回环设备由独立网络 ELM 承载。", fill=PALE_TEAL, accent=TEAL, title_size=19, detail_size=15)
    compact_row(slide, 4.38, "常规实现", "协议解析、校验和、重传、拥塞控制、路由查找与设备收发属于操作系统网络栈的通用职责，本稿只证明其存在和接口覆盖。", fill=PALE_GRAY, accent=MUTED, h=0.68, title_w=1.76)
    compact_row(slide, 5.20, "本工程重点", "常驻 fd 对象与可退役协议状态分离；网络热路径固定到受管代际；FlowShard 限定写入者；设备与协议分别退出。", fill=PALE_PURPLE, accent=PURPLE, h=0.68, title_w=1.76)
    boundary(
        slide,
        "配置写入目前主要覆盖 IPv4；通用 packet Provider 尚未接通，网络生产路径使用专用 PinnedNativeCall。",
        label="实现边界",
    )


def explain_18(slide) -> None:
    set_title(slide, "网络对象的所有权与退出责任")
    lead(slide, "网络状态不集中在一个全局对象中：常驻对象维持用户可见语义，可退役 Cell 持有协议或设备状态，代际路由器只把调用送往当前有效实现。")

    compact_row(slide, 2.42, "SocketFacade", "常驻于 VFS；保存 fd 身份、端点、收发缓冲、就绪位、等待队列、错误与关闭状态，不保存 TCP 控制块。", fill=PALE_BLUE, accent=BLUE, h=0.76, title_w=2.12)
    compact_row(slide, 3.31, "Host", "常驻网络协调者；保存设备注册表、配置快照、协议 worker、queue worker 与有界队列，负责推进和排空。", fill=PALE_GRAY, accent=MUTED, h=0.76, title_w=2.12)
    compact_row(slide, 4.20, "Broker", "当前代际路由器；保存 StackHandle、Cell / Generation、shard / local endpoint 与每 CPU 调用槽，只选择 Active 且 ready 的 net.stack。", fill=PALE_PURPLE, accent=PURPLE, h=0.76, title_w=2.12)
    compact_row(slide, 5.09, "网络 ELM", "net.stack 持有 FlowShard 与协议状态；net.virtio 持有 virtqueue、DMA pool 与 IRQ；net.loopback 持有回环队列。", fill=PALE_TEAL, accent=TEAL, h=0.76, title_w=2.12)
    boundary(
        slide,
        "退出时先关闭 Cell 入口并排空在途工作；SocketFacade 继续存在，负责把失效代际转换为稳定的 POSIX 错误与就绪状态。",
        label="退出责任",
    )


def explain_19(slide) -> None:
    set_title(slide, "FlowShard 单写者模型与双执行路径")
    lead(slide, "FlowShard 是协议状态的单写者分区：同一分区同时只有取得 FlowExecution 写入令牌的执行者可修改；它不是线程，也不是只查找的哈希桶。")

    panel(slide, 3.00, 2.46, 4.48, 2.42, "FlowShard 保存的协议状态", "TCP 连接按一致流哈希进入固定分区；IP 分片按分片组键归并。分区内保存传输状态、邻居与待解析队列、PMTU、重组、定时器和发送结果。UDP、Raw 与多数控制报文当前进入协调 shard。", fill=PALE_BLUE, accent=BLUE, title_size=20, detail_size=15)
    panel(slide, 7.80, 2.46, 4.80, 2.42, "执行令牌（FlowExecution）", "Generation、BUSY、PENDING 与执行者 CPU 位于同一原子状态。owner worker 与短系统调用竞争同一写入权；失败方只留下 pending，不并发修改协议状态。它与 ELM 资源 Lease 不是同一对象。", fill=PALE_PURPLE, accent=PURPLE, title_size=20, detail_size=15)
    compact_row(slide, 4.98, "shard-turn", "owner worker 按报文、字节和时间预算批量推进协议状态。", fill=PALE_TEAL, accent=TEAL, h=0.50, title_w=1.72)
    compact_row(slide, 5.56, "local-turn", "短系统调用尝试同一令牌；失败只置 pending，由 owner worker 接管。", fill=PALE_BLUE, accent=BLUE, h=0.46, title_w=1.72)
    boundary(
        slide,
        "活跃 Shard 数取在线 CPU、启动上限和可用 queue pair 的共同约束；单队列设备不制造虚假并行。",
        label="并行上限",
    )


def explain_19b(slide) -> None:
    set_title(slide, "数据通路与受管调用边界")
    lead(slide, "ELM 治理不要求每个报文经过 Core：Core 管理装载、关系和代际，收发数据沿固定入口流动，调用前仍核验目标是否有效。")

    panel(slide, 3.00, 2.42, 4.56, 1.48, "管理面：Core 提交关系", "装载或 Reload 时建立 Cell、Generation、export 和 endpoint；Pause、Detach 与故障隔离改变调用门可见状态。", fill=PALE_PURPLE, accent=PURPLE, title_size=19, detail_size=15)
    panel(slide, 7.86, 2.42, 4.74, 1.48, "数据面：Host 固定入口", "Broker 选取当前 StackHandle；Host 通过 PinnedNativeCall 进入 shard-turn 或 local-turn，不重复执行通用名称查找。", fill=PALE_TEAL, accent=TEAL, title_size=19, detail_size=15)
    compact_row(slide, 4.12, "收发链路", "接收：VirtIO DMA → PacketBatch → FlowShard → SocketFacade → fd；发送沿相反责任链形成 TxPlan，并由设备完成回收。", fill=PALE_TEAL, accent=TEAL, h=0.54, title_w=1.72)
    compact_row(slide, 4.77, "逐次门禁", "调用前核对 Cell 状态、Generation 与 frame 范围；旧代际、退出中或越界参数在进入协议代码前被拒绝。", fill=PALE_BLUE, accent=BLUE, h=0.54, title_w=1.72)
    compact_row(slide, 5.42, "竞争回退", "local-turn 未取得 FlowExecution 时只置 pending；owner worker 随后以 shard-turn 批量推进，不产生第二个写入者。", fill=PALE_GRAY, accent=MUTED, h=0.54, title_w=1.72)
    boundary(
        slide,
        "普通 direct-pinned 不保证逐次经过 Core；网络 PinnedNativeCall 明确保留状态、Generation 和参数范围核验。",
        label="适用边界",
    )


def explain_20(slide) -> None:
    set_title(slide, "SocketFacade 的 POSIX 稳定边界")
    lead(slide, "SocketFacade 是常驻 VFS 代理：协议 Cell 可以退出，但 fd、等待关系和可观察错误仍由常驻对象解释；TCP 控制块仍归 net.stack 的 FlowShard。")

    compact_row(slide, 2.42, "POSIX 与 VFS", "socket / bind / connect / sendmsg / recvmsg / poll 先进入 fd + FileOps；这里检查用户内存、阻塞模式和信号。", fill=PALE_BLUE, accent=BLUE, h=0.76, title_w=2.30)
    compact_row(slide, 3.31, "SocketFacade", "保存栈代次、socket 身份与端点、收发缓冲、可读 / 可写 / 错误 / 挂断位、关闭状态和等待者；不保存 TCP 控制块。", fill=PALE_TEAL, accent=TEAL, h=0.76, title_w=2.30)
    compact_row(slide, 4.20, "Broker / FlowShard", "Broker 保存当前栈实例句柄与代际路由；FlowShard 取得单写者令牌后推进协议状态，再写回 facade。", fill=PALE_PURPLE, accent=PURPLE, h=0.76, title_w=2.30)
    compact_row(slide, 5.08, "等待与配置路径", "普通数据或发送容量只唤醒一名等待者；退出、关闭和错误唤醒全部等待者。SIOC 主要写 IPv4；netlink 只作结构化查询与列表导出。", fill=PALE_BLUE, accent=BLUE, h=0.92, title_w=2.38)
    boundary(
        slide,
        "协议 Cell 退出后旧 fd 仍是有效 VFS 对象；网络调用返回 NetworkDown，poll / epoll 观察 ERROR 或 HANGUP。",
        label="退出可见性",
    )


def explain_21(slide) -> None:
    set_title(slide, "网络退役协议、用户语义与成本归因")
    lead(slide, "net.stack 与 net.virtio 分别拥有协议状态和设备资源，因此有不同的退出顺序；共同原则是先关闭入口，再排空在途工作，最后回收本代对象。")

    compact_row(slide, 2.42, "net.stack", "quiesce 拒绝新 turn → begin_remove 进入 drain → 使本代 proxy / facade 失效并清除 Broker 路由 → finish_remove → 销毁 FlowShard", fill=PALE_PURPLE, accent=PURPLE, h=0.88, title_w=1.58)
    compact_row(slide, 3.44, "net.virtio", "quiesce_active 停止队列 → device begin_remove 摘除设备并排空 Host 引用 → 注销 PCI / MMIO 驱动 → 销毁 active device", fill=PALE_TEAL, accent=TEAL, h=0.88, title_w=1.58)
    compact_row(slide, 4.46, "重新装载后", "旧 fd 仍是有效 VFS 对象，但网络操作返回 NetworkDown，poll / epoll 报告 ERROR / HANGUP；Reload 创建新的 Cell 与 StackHandle，只有新 socket 绑定新实例。", fill=PALE_BLUE, accent=BLUE, h=0.88, title_w=1.58)
    compact_row(slide, 5.50, "可归因性能项", "用户复制 · 协议推进 · FlowExecution / 队列竞争 · 等待唤醒 · 设备 completion", fill=PALE_GRAY, accent=MUTED, h=0.52, title_w=2.42)
    compact_row(slide, 6.12, "验证与实现边界", "已验证 Detach + Reload；原位 Replace、连接迁移和通用 packet Provider 未完成。", fill=PALE_PURPLE, accent=PURPLE, h=0.52, title_w=2.42)


DRAWERS = [
    explain_01,
    explain_02,
    explain_03,
    explain_04,
    explain_05,
    explain_06,
    explain_07,
    explain_08,
    explain_09,
    explain_09b,
    explain_10,
    explain_11,
    explain_12,
    explain_13,
    explain_14,
    explain_15,
    explain_15b,
    explain_16,
    explain_16b,
    explain_17,
    explain_18,
    explain_19,
    explain_19b,
    explain_20,
    explain_21,
]


def create_topic_slides(prs: Presentation, template) -> list:
    slides = []
    for drawer in DRAWERS:
        slide = clone_slide(prs, template)
        clean_body_template(slide)
        drawer(slide)
        enforce_font_floor(slide)
        slides.append(slide)
    return slides


def build_topic(base: Path, output: Path) -> None:
    source = Presentation(base)
    remove_existing_topic(source)
    template = find_slide(source, "设备抽象能力闭环")
    slides = create_topic_slides(source, template)
    keep = {slide.part for slide in slides}
    for slide in list(source.slides):
        if slide.part not in keep:
            remove_slide(source, slide)

    output.parent.mkdir(parents=True, exist_ok=True)
    source.save(output)


def insert_topic(full: Presentation) -> None:
    remove_existing_topic(full)
    anchor = find_slide(full, "设备抽象能力闭环")
    anchor_index = list(full.slides).index(anchor)
    added = create_topic_slides(full, anchor)

    slide_ids = full.slides._sldIdLst
    added_ids = list(slide_ids)[-len(added) :]
    for slide_id in added_ids:
        slide_ids.remove(slide_id)
    for offset, slide_id in enumerate(added_ids, 1):
        slide_ids.insert(anchor_index + offset, slide_id)


def build_full(base: Path, output: Path) -> None:
    full = Presentation(base)
    insert_topic(full)
    remove_page_markers(full)
    output.parent.mkdir(parents=True, exist_ok=True)
    with tempfile.NamedTemporaryFile(
        prefix=f".{output.stem}-",
        suffix=".pptx",
        dir=output.parent,
        delete=False,
    ) as temporary:
        temporary_path = Path(temporary.name)
    try:
        full.save(temporary_path)
        os.replace(temporary_path, output)
        output.chmod(0o644)
    except Exception:
        temporary_path.unlink(missing_ok=True)
        raise


def validate_topic(topic: Path) -> None:
    prs = Presentation(topic)
    if len(prs.slides) != len(DRAWERS):
        raise RuntimeError(f"专题页数错误：{len(prs.slides)}")
    for slide_number, slide in enumerate(prs.slides, 1):
        for shape in slide.shapes:
            if is_page_marker(shape):
                raise RuntimeError(f"第 {slide_number} 页仍包含右下角数字")
            if not getattr(shape, "has_text_frame", False) or not shape.text.strip():
                continue
            for paragraph in shape.text_frame.paragraphs:
                for run in paragraph.runs:
                    if not run.text.strip():
                        continue
                    if run.font.size is None or run.font.size.pt < MIN_FONT_PT:
                        raise RuntimeError(
                            f"第 {slide_number} 页文字小于 {MIN_FONT_PT:g} pt：{run.text!r}"
                        )


def main() -> int:
    parser = argparse.ArgumentParser()
    root = Path(__file__).resolve().parents[2]
    parser.add_argument(
        "--base",
        type=Path,
        default=Path.home() / "Downloads/mygo-defense-full.pptx",
    )
    parser.add_argument(
        "--topic-output",
        type=Path,
        default=root / "output/presentations/mygo-defense-elm-network-25pages.pptx",
    )
    parser.add_argument(
        "--full-output",
        type=Path,
        default=root / "output/presentations/mygo-defense-full.pptx",
    )
    args = parser.parse_args()
    base = args.base.resolve()
    topic = args.topic_output.resolve()
    full = args.full_output.resolve()
    if base == full:
        raise RuntimeError("--base 必须指向独立模板，不能与 --full-output 为同一文件")
    build_topic(base, topic)
    validate_topic(topic)
    build_full(base, full)
    print(topic)
    print(full)
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
