#!/usr/bin/env python3
"""在不重建版式的前提下更新设备抽象四页的答辩文案。"""

from __future__ import annotations

import argparse
from copy import deepcopy
import os
from pathlib import Path
import tempfile
from zipfile import ZIP_DEFLATED, ZipFile

from lxml import etree


P_NS = "http://schemas.openxmlformats.org/presentationml/2006/main"
A_NS = "http://schemas.openxmlformats.org/drawingml/2006/main"
NS = {"p": P_NS, "a": A_NS}
EMU_PER_INCH = 914_400


TEXT_UPDATES = {
    1: {
        "TextBox 43": "DeviceFunction 的能力契约结构",
        "TextBox 58": "第 N + 1 种设备能力的接入路径",
        "TextBox 60": "新能力类别注册",
        "TextBox 63": "设备能力契约实现",
        "TextBox 66": "设备能力投影策略",
        "TextBox 69": "PnP core 的类型无关拓展",
        "TextBox 72": "设备抽象内核能够显著增加可拓展性的原因是什么？",
    },
    2: {
        "TextBox 27": "块设备能力实例",
        "TextBox 64": "同步与异步的统一 I/O 路径",
        "TextBox 71": "设备退役的完整生命周期",
    },
    3: {
        "TextBox 52": "probe 失败的逆序回滚路径",
        "TextBox 55": "probe 事务模型",
        "TextBox 57": "总线身份识别",
        "TextBox 61": "探测副作用集合",
        "TextBox 65": "设备能力原子发布",
        "TextBox 69": "探测失败逆序回滚",
        "TextBox 73": "remove 分阶段退役模型",
        "TextBox 75": "设备隔离阶段",
        "TextBox 79": "设备拓扑清理",
        "TextBox 83": "数据流静默阶段",
        "TextBox 87": "硬件停机阶段",
        "TextBox 91": "用户态投影撤销",
    },
    4: {
        "TextBox 31": "设备发现",
        "TextBox 36": "驱动探测",
        "TextBox 41": "设备能力发布",
        "TextBox 46": "设备节点投影",
        "TextBox 51": "块 I/O 请求提交",
        "TextBox 56": "块 I/O 请求完成",
        "TextBox 70": "设备反向退役路径",
        "TextBox 74": "设备能力实现来源",
    },
}


def shape_by_name(root: etree._Element, name: str) -> etree._Element:
    for shape in root.xpath(".//p:sp", namespaces=NS):
        properties = shape.find("p:nvSpPr/p:cNvPr", namespaces=NS)
        if properties is not None and properties.get("name") == name:
            return shape
    raise ValueError(f"没有找到图形：{name}")


def set_shape_text(root: etree._Element, name: str, value: str) -> None:
    shape = shape_by_name(root, name)
    text_nodes = shape.xpath(".//a:t", namespaces=NS)
    if not text_nodes:
        raise ValueError(f"图形没有文本节点：{name}")
    text_nodes[0].text = value
    for node in text_nodes[1:]:
        node.text = ""


def set_shape_geometry(shape: etree._Element, *, x: float, y: float, w: float, h: float) -> None:
    transform = shape.find("p:spPr/a:xfrm", namespaces=NS)
    if transform is None:
        raise ValueError("图形缺少变换信息")
    offset = transform.find("a:off", namespaces=NS)
    extent = transform.find("a:ext", namespaces=NS)
    if offset is None or extent is None:
        raise ValueError("图形缺少位置或尺寸信息")
    offset.set("x", str(round(x * EMU_PER_INCH)))
    offset.set("y", str(round(y * EMU_PER_INCH)))
    extent.set("cx", str(round(w * EMU_PER_INCH)))
    extent.set("cy", str(round(h * EMU_PER_INCH)))


def add_virtio_example_label(root: etree._Element) -> None:
    tree = root.find("p:cSld/p:spTree", namespaces=NS)
    if tree is None:
        raise ValueError("幻灯片缺少 spTree")

    rectangle_name = "Rectangle VirtIO example"
    textbox_name = "TextBox VirtIO example"
    try:
        rectangle = shape_by_name(root, rectangle_name)
        textbox = shape_by_name(root, textbox_name)
    except ValueError:
        rectangle = deepcopy(shape_by_name(root, "Rectangle 26"))
        textbox = deepcopy(shape_by_name(root, "TextBox 27"))

        ids = [
            int(node.get("id"))
            for node in root.xpath(".//p:cNvPr[@id]", namespaces=NS)
        ]
        next_id = max(ids) + 1
        rectangle_properties = rectangle.find("p:nvSpPr/p:cNvPr", namespaces=NS)
        textbox_properties = textbox.find("p:nvSpPr/p:cNvPr", namespaces=NS)
        if rectangle_properties is None or textbox_properties is None:
            raise ValueError("模板标签缺少非可视属性")
        rectangle_properties.set("id", str(next_id))
        rectangle_properties.set("name", rectangle_name)
        textbox_properties.set("id", str(next_id + 1))
        textbox_properties.set("name", textbox_name)
        tree.append(rectangle)
        tree.append(textbox)

    set_shape_geometry(rectangle, x=4.30, y=1.88, w=2.25, h=0.30)
    set_shape_geometry(textbox, x=4.30, y=1.88, w=2.25, h=0.30)
    set_shape_text(root, textbox_name, "以VirtIO块设备为例")


def update_slide(data: bytes, slide_number: int) -> bytes:
    parser = etree.XMLParser(remove_blank_text=False)
    root = etree.fromstring(data, parser)
    for shape_name, text in TEXT_UPDATES[slide_number].items():
        set_shape_text(root, shape_name, text)
    if slide_number == 4:
        add_virtio_example_label(root)
    return etree.tostring(
        root,
        xml_declaration=True,
        encoding="UTF-8",
        standalone=True,
    )


def update_presentation(path: Path) -> None:
    if not path.is_file():
        raise FileNotFoundError(path)

    with tempfile.NamedTemporaryFile(
        prefix=f".{path.stem}-",
        suffix=".pptx",
        dir=path.parent,
        delete=False,
    ) as temporary:
        temporary_path = Path(temporary.name)

    try:
        with ZipFile(path, "r") as source, ZipFile(
            temporary_path, "w", compression=ZIP_DEFLATED
        ) as destination:
            for info in source.infolist():
                data = source.read(info.filename)
                if info.filename.startswith("ppt/slides/slide") and info.filename.endswith(".xml"):
                    stem = Path(info.filename).stem
                    slide_number = int(stem.removeprefix("slide"))
                    if slide_number in TEXT_UPDATES:
                        data = update_slide(data, slide_number)
                destination.writestr(info, data)
        os.replace(temporary_path, path)
    except Exception:
        temporary_path.unlink(missing_ok=True)
        raise


def main() -> None:
    parser = argparse.ArgumentParser()
    parser.add_argument("presentation", type=Path)
    args = parser.parse_args()
    update_presentation(args.presentation.resolve())


if __name__ == "__main__":
    main()
