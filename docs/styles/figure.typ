#import "tokens.typ": body-font, body-ink, line-stroke, mono-font, panel-fill

#let figure-caption(kind, number, title) = [
  #text(font: ("SimHei", "Microsoft YaHei", "Times New Roman"), size: 9pt, fill: body-ink)[#kind #number]
  #h(0.75em)
  #text(font: body-font, size: 9pt, fill: body-ink)[#title]
]

#let continued-table(
  number,
  title,
  columns,
  header,
  rows,
  kind: "表",
  continuation-kind: "续表",
  inset: 7pt,
  stroke: line-stroke,
  align: left,
) = context [
  #v(6pt)
  #set par(first-line-indent: (amount: 0pt, all: true), spacing: 0pt)
  #let start-page = counter(page).get().first()
  #table(
    columns: columns,
    inset: inset,
    stroke: stroke,
    align: align,
    table.header(
      repeat: true,
      [#table.cell(colspan: columns.len(), fill: panel-fill, align: center)[
        #context {
          let current-page = counter(page).get().first()
          if current-page == start-page {
            figure-caption(kind, number, title)
          } else {
            figure-caption(continuation-kind, number, title)
          }
        }
      ]],
      ..header,
    ),
    ..rows,
  )
  #v(8pt)
]

#let _manual-code-content(lang, body) = {
  if type(body) == str {
    raw(body, lang: lang, block: true)
  } else {
    body
  }
}

#let _manual-hexdump-source(body) = {
  if type(body) == str {
    body
  } else if body.func() == raw {
    body.text
  } else if body.has("children") {
    let raw-child = body.children
      .filter(child => type(child) == content and child.func() == raw)
      .at(0, default: none)
    if raw-child == none { none } else { raw-child.text }
  } else {
    none
  }
}

#let _hexdump-offset-fill = rgb("#0b63c7")
#let _hexdump-separator-fill = rgb("#6f87a0")
#let _hexdump-white-fill = rgb("#c8d4df")
#let _hexdump-red-fill = rgb("#d1242f")
#let _hexdump-green-fill = rgb("#2b8a3e")
#let _hexdump-default-fill = body-ink
#let _hexdump-blue-fill = rgb("#0969da")
#let _hexdump-font-size = 8.2pt

#let _manual-hexdump-offset(offset) = text(
  font: mono-font,
  size: _hexdump-font-size,
  weight: "bold",
  fill: _hexdump-offset-fill,
)[#offset]

#let _manual-hex-digit-value(ch) = {
  if ch == "0" {
    0
  } else if ch == "1" {
    1
  } else if ch == "2" {
    2
  } else if ch == "3" {
    3
  } else if ch == "4" {
    4
  } else if ch == "5" {
    5
  } else if ch == "6" {
    6
  } else if ch == "7" {
    7
  } else if ch == "8" {
    8
  } else if ch == "9" {
    9
  } else if ch == "a" or ch == "A" {
    10
  } else if ch == "b" or ch == "B" {
    11
  } else if ch == "c" or ch == "C" {
    12
  } else if ch == "d" or ch == "D" {
    13
  } else if ch == "e" or ch == "E" {
    14
  } else if ch == "f" or ch == "F" {
    15
  } else {
    0
  }
}

#let _manual-hexdump-byte-value(byte) = {
  let chars = byte.clusters()
  if chars.len() < 2 {
    0
  } else {
    _manual-hex-digit-value(chars.at(0)) * 16 + _manual-hex-digit-value(chars.at(1))
  }
}

#let _manual-hexdump-byte-fill(byte-value) = {
  if byte-value == 0x00 {
    _hexdump-white-fill
  } else if byte-value <= 0x1f or byte-value == 0x7f {
    _hexdump-red-fill
  } else if byte-value == 0x20 {
    _hexdump-green-fill
  } else if byte-value <= 0x7e {
    _hexdump-default-fill
  } else {
    _hexdump-blue-fill
  }
}

#let _manual-hexdump-byte-char(byte-value) = {
  if byte-value == 0x20 {
    "·"
  } else if byte-value >= 0x21 and byte-value <= 0x7e {
    str.from-unicode(byte-value)
  } else {
    "."
  }
}

#let _manual-hexdump-split-group(group) = {
  let trimmed = group.trim()
  if trimmed == "" {
    ()
  } else if trimmed.len() <= 2 {
    (trimmed,)
  } else {
    (trimmed.slice(0, 2),) + _manual-hexdump-split-group(trimmed.slice(2, trimmed.len()))
  }
}

#let _manual-hexdump-expand-groups(groups) = {
  if groups.len() == 0 {
    ()
  } else {
    _manual-hexdump-split-group(groups.at(0)) + _manual-hexdump-expand-groups(groups.slice(1, groups.len()))
  }
}

#let _manual-hexdump-byte-token(byte) = {
  let value = _manual-hexdump-byte-value(byte)
  text(
    font: mono-font,
    size: _hexdump-font-size,
    weight: "medium",
    fill: _manual-hexdump-byte-fill(value),
  )[#byte]
}

#let _manual-hexdump-ascii-token(byte) = {
  let value = _manual-hexdump-byte-value(byte)
  text(
    font: mono-font,
    size: _hexdump-font-size,
    fill: _manual-hexdump-byte-fill(value),
  )[#(_manual-hexdump-byte-char(value))]
}

#let _manual-hexdump-line(line) = {
  let trimmed = line.trim()
  if trimmed == "" {
    []
  } else {
    let head = trimmed.split(":")
    let offset = head.at(0, default: "")
    let rest = if head.len() > 1 { head.slice(1, head.len()).join(":") } else { "" }
    let sections = rest.split("  ").filter(part => part.trim() != "")
    let hex-source = sections.at(0, default: "")
    let groups = hex-source.split(" ").filter(token => token != "")
    let bytes = _manual-hexdump-expand-groups(groups)
    box[
      #_manual-hexdump-offset(offset)
      #text(font: mono-font, size: _hexdump-font-size, fill: _hexdump-separator-fill)[:]
      #h(0.45em)
      #for index in range(0, bytes.len()) [
        #_manual-hexdump-byte-token(bytes.at(index))
        #if index + 1 < bytes.len() and calc.rem(index + 1, 2) == 0 [
          #h(0.20em)
        ]
      ]
      #h(1.4em)
      #for byte in bytes [#_manual-hexdump-ascii-token(byte)]
    ]
  }
}

#let _manual-hexdump-content(source) = [
  #set par(first-line-indent: (amount: 0pt, all: true), justify: false, spacing: 0pt)
  #block(
    breakable: true,
    width: 100%,
    inset: 8pt,
    radius: 5pt,
    fill: rgb("#f8fbff"),
    stroke: 0.45pt + rgb("#d1dfee"),
  )[
    #for line in source.trim().split("\n") [
      #_manual-hexdump-line(line)
      #v(2.5pt)
    ]
  ]
]

#let code-sample(number, title, body, kind: none, lang: none) = context [
  #v(6pt)
  #set par(first-line-indent: (amount: 0pt, all: true), spacing: 0pt)
  #_manual-code-content(lang, body)
  #v(4pt)
  #align(center)[#figure-caption(kind, number, title)]
  #v(8pt)
]

#let pseudo-sample(number, title, kind: none, body) = code-sample(number, title, body, kind: kind, lang: "c")

#let hexdump-sample(number, title, kind: none, body) = {
  let source = _manual-hexdump-source(body)
  if source == none {
    code-sample(number, title, body, kind: kind, lang: "hexdump")
  } else {
    context [
      #v(6pt)
      #set par(first-line-indent: (amount: 0pt, all: true), spacing: 0pt)
      #_manual-hexdump-content(source)
      #v(4pt)
      #align(center)[#figure-caption(kind, number, title)]
      #v(8pt)
    ]
  }
}

#let asm-sample(number, title, kind: none, body) = code-sample(number, title, body, kind: kind, lang: "asm")
