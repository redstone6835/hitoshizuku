#!/usr/bin/env python3
"""组装答辩全量稿，并保留尚未撰写章节的模板页。"""

from __future__ import annotations

import argparse
from hashlib import sha256
import os
from pathlib import Path, PurePosixPath
import re
import tempfile
from zipfile import ZIP_DEFLATED, ZipFile

from lxml import etree


P_NS = "http://schemas.openxmlformats.org/presentationml/2006/main"
R_NS = "http://schemas.openxmlformats.org/officeDocument/2006/relationships"
PKG_R_NS = "http://schemas.openxmlformats.org/package/2006/relationships"
CT_NS = "http://schemas.openxmlformats.org/package/2006/content-types"
EP_NS = "http://schemas.openxmlformats.org/officeDocument/2006/extended-properties"
VT_NS = "http://schemas.openxmlformats.org/officeDocument/2006/docPropsVTypes"

NS = {"p": P_NS, "r": R_NS, "pr": PKG_R_NS, "ct": CT_NS, "ep": EP_NS, "vt": VT_NS}
SLIDE_REL_TYPE = f"{R_NS}/slide"
IMAGE_REL_TYPE = f"{R_NS}/image"
LAYOUT_REL_TYPE = f"{R_NS}/slideLayout"


def read_archive(path: Path) -> dict[str, bytes]:
    with ZipFile(path, "r") as archive:
        return {name: archive.read(name) for name in archive.namelist() if not name.endswith("/")}


def xml(data: bytes) -> etree._Element:
    return etree.fromstring(data, etree.XMLParser(remove_blank_text=False))


def serialize(root: etree._Element) -> bytes:
    return etree.tostring(root, xml_declaration=True, encoding="UTF-8", standalone=True)


def presentation_slide_parts(entries: dict[str, bytes]) -> list[str]:
    presentation = xml(entries["ppt/presentation.xml"])
    relationships = xml(entries["ppt/_rels/presentation.xml.rels"])
    targets = {
        relation.get("Id"): relation.get("Target")
        for relation in relationships.findall(f"{{{PKG_R_NS}}}Relationship")
    }
    parts = []
    for slide_id in presentation.xpath("./p:sldIdLst/p:sldId", namespaces=NS):
        relation_id = slide_id.get(f"{{{R_NS}}}id")
        target = targets[relation_id]
        parts.append(str(PurePosixPath("ppt") / target))
    return parts


def slide_relationship_part(slide_part: str) -> str:
    path = PurePosixPath(slide_part)
    return str(path.parent / "_rels" / f"{path.name}.rels")


def relationship_target_part(source_part: str, target: str) -> str:
    return str(PurePosixPath(source_part).parent.joinpath(target))


def normalize_part(path: str) -> str:
    parts: list[str] = []
    for component in PurePosixPath(path).parts:
        if component == "..":
            parts.pop()
        elif component != ".":
            parts.append(component)
    return "/".join(parts)


def relative_target(source_part: str, target_part: str) -> str:
    source_dirs = list(PurePosixPath(source_part).parent.parts)
    target_parts = list(PurePosixPath(target_part).parts)
    common = 0
    while (
        common < len(source_dirs)
        and common < len(target_parts)
        and source_dirs[common] == target_parts[common]
    ):
        common += 1
    return "/".join([".."] * (len(source_dirs) - common) + target_parts[common:])


def destination_media_by_hash(entries: dict[str, bytes]) -> dict[bytes, str]:
    return {
        sha256(data).digest(): name
        for name, data in entries.items()
        if name.startswith("ppt/media/")
    }


def rewrite_slide_relationships(
    source_entries: dict[str, bytes],
    source_slide_part: str,
    destination_entries: dict[str, bytes],
    destination_slide_part: str,
) -> bytes:
    relationship_part = slide_relationship_part(source_slide_part)
    root = xml(source_entries[relationship_part])
    media_by_hash = destination_media_by_hash(destination_entries)

    for relation in root.findall(f"{{{PKG_R_NS}}}Relationship"):
        relation_type = relation.get("Type")
        target = relation.get("Target")
        if relation.get("TargetMode") == "External":
            continue
        if relation_type == LAYOUT_REL_TYPE:
            target_part = normalize_part(relationship_target_part(source_slide_part, target))
            if target_part not in destination_entries:
                raise ValueError(f"目标演示文稿缺少版式：{target_part}")
            relation.set("Target", relative_target(destination_slide_part, target_part))
            continue
        if relation_type == IMAGE_REL_TYPE:
            source_media = normalize_part(relationship_target_part(source_slide_part, target))
            digest = sha256(source_entries[source_media]).digest()
            destination_media = media_by_hash.get(digest)
            if destination_media is None:
                raise ValueError(f"目标演示文稿缺少同源媒体：{source_media}")
            relation.set("Target", relative_target(destination_slide_part, destination_media))
            continue
        raise ValueError(f"暂不支持导入关系：{relation_type} -> {target}")
    return serialize(root)


def next_relationship_id(relationships: etree._Element) -> int:
    numbers = []
    for relation in relationships.findall(f"{{{PKG_R_NS}}}Relationship"):
        match = re.fullmatch(r"rId(\d+)", relation.get("Id", ""))
        if match:
            numbers.append(int(match.group(1)))
    return max(numbers, default=0) + 1


def update_presentation_order(
    entries: dict[str, bytes],
    additional_slide_parts: list[str],
) -> None:
    presentation = xml(entries["ppt/presentation.xml"])
    relationships = xml(entries["ppt/_rels/presentation.xml.rels"])
    target_by_id = {
        relation.get("Id"): relation.get("Target")
        for relation in relationships.findall(f"{{{PKG_R_NS}}}Relationship")
    }

    slide_list = presentation.find("p:sldIdLst", namespaces=NS)
    if slide_list is None:
        raise ValueError("基础演示文稿缺少幻灯片列表")
    anchor_index = None
    for index, slide_id in enumerate(slide_list):
        relation_id = slide_id.get(f"{{{R_NS}}}id")
        if target_by_id.get(relation_id) == "slides/slide12.xml":
            anchor_index = index
            break
    if anchor_index is None:
        raise ValueError("没有找到第三章空白正文页")

    relationship_number = next_relationship_id(relationships)
    slide_numeric_id = max(int(node.get("id")) for node in slide_list) + 1
    for offset, slide_part in enumerate(additional_slide_parts, 1):
        relation_id = f"rId{relationship_number}"
        relationship_number += 1
        relation = etree.SubElement(relationships, f"{{{PKG_R_NS}}}Relationship")
        relation.set("Id", relation_id)
        relation.set("Type", SLIDE_REL_TYPE)
        relation.set("Target", str(PurePosixPath(slide_part).relative_to("ppt")))

        slide_id = etree.Element(f"{{{P_NS}}}sldId")
        slide_id.set("id", str(slide_numeric_id))
        slide_numeric_id += 1
        slide_id.set(f"{{{R_NS}}}id", relation_id)
        slide_list.insert(anchor_index + offset, slide_id)

    entries["ppt/presentation.xml"] = serialize(presentation)
    entries["ppt/_rels/presentation.xml.rels"] = serialize(relationships)


def update_content_types(entries: dict[str, bytes], new_slide_parts: list[str]) -> None:
    root = xml(entries["[Content_Types].xml"])
    slide_content_type = None
    known_parts = set()
    for override in root.findall(f"{{{CT_NS}}}Override"):
        part_name = override.get("PartName")
        known_parts.add(part_name)
        if part_name == "/ppt/slides/slide12.xml":
            slide_content_type = override.get("ContentType")
    if slide_content_type is None:
        raise ValueError("没有找到幻灯片内容类型")
    for part in new_slide_parts:
        part_name = f"/{part}"
        if part_name in known_parts:
            continue
        override = etree.SubElement(root, f"{{{CT_NS}}}Override")
        override.set("PartName", part_name)
        override.set("ContentType", slide_content_type)
    entries["[Content_Types].xml"] = serialize(root)


def update_extended_properties(entries: dict[str, bytes], slide_count: int) -> None:
    root = xml(entries["docProps/app.xml"])
    slides = root.find(f"{{{EP_NS}}}Slides")
    if slides is not None:
        old_count = int(slides.text or "0")
        slides.text = str(slide_count)
    else:
        old_count = 0

    heading_vector = root.find(f"{{{EP_NS}}}HeadingPairs/{{{VT_NS}}}vector")
    if heading_vector is not None:
        variants = list(heading_vector)
        for index in range(0, len(variants) - 1, 2):
            label = variants[index].find(f"{{{VT_NS}}}lpstr")
            value = variants[index + 1].find(f"{{{VT_NS}}}i4")
            if label is not None and value is not None and label.text in {"幻灯片标题", "Slide Titles"}:
                old_count = int(value.text or old_count)
                value.text = str(slide_count)
                break

    titles = root.find(f"{{{EP_NS}}}TitlesOfParts/{{{VT_NS}}}vector")
    if titles is not None and slide_count > old_count:
        for _ in range(slide_count - old_count):
            title = etree.SubElement(titles, f"{{{VT_NS}}}lpstr")
            title.text = "PowerPoint 演示文稿"
        titles.set("size", str(len(titles)))
    entries["docProps/app.xml"] = serialize(root)


def assemble(base: Path, chapter3: Path, device: Path, output: Path) -> None:
    entries = read_archive(base)
    chapter3_entries = read_archive(chapter3)
    device_entries = read_archive(device)

    chapter3_parts = presentation_slide_parts(chapter3_entries)
    device_parts = presentation_slide_parts(device_entries)
    if len(chapter3_parts) != 8 or len(device_parts) != 4:
        raise ValueError("第三章输入页数必须为 8 + 4")
    sources = [(chapter3_entries, part) for part in chapter3_parts]
    sources.extend((device_entries, part) for part in device_parts)

    destination_parts = ["ppt/slides/slide12.xml"]
    destination_parts.extend(f"ppt/slides/slide{number}.xml" for number in range(20, 31))
    for (source_entries, source_part), destination_part in zip(sources, destination_parts):
        entries[destination_part] = source_entries[source_part]
        destination_relationship_part = slide_relationship_part(destination_part)
        entries[destination_relationship_part] = rewrite_slide_relationships(
            source_entries,
            source_part,
            entries,
            destination_part,
        )

    additional_parts = destination_parts[1:]
    update_presentation_order(entries, additional_parts)
    update_content_types(entries, additional_parts)
    update_extended_properties(entries, slide_count=30)

    output.parent.mkdir(parents=True, exist_ok=True)
    with tempfile.NamedTemporaryFile(
        prefix=f".{output.stem}-", suffix=".pptx", dir=output.parent, delete=False
    ) as temporary:
        temporary_path = Path(temporary.name)
    try:
        with ZipFile(temporary_path, "w", compression=ZIP_DEFLATED) as archive:
            for name, data in entries.items():
                archive.writestr(name, data)
        os.replace(temporary_path, output)
        output.chmod(0o644)
    except Exception:
        temporary_path.unlink(missing_ok=True)
        raise


def main() -> None:
    parser = argparse.ArgumentParser()
    parser.add_argument("--base", type=Path, required=True)
    parser.add_argument("--chapter3", type=Path, required=True)
    parser.add_argument("--device", type=Path, required=True)
    parser.add_argument("--output", type=Path, required=True)
    args = parser.parse_args()
    assemble(
        args.base.resolve(),
        args.chapter3.resolve(),
        args.device.resolve(),
        args.output.resolve(),
    )


if __name__ == "__main__":
    main()
