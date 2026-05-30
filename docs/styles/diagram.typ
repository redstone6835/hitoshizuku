#import "tokens.typ": accent, accent-dark, body-ink, handoff-fill, heading-font, line-stroke, muted, soft-fill, song-font, stable-fill, warm-fill

#let legend-color-cell(title, body, fill: soft-fill) = table.cell(fill: fill)[
  #text(font: heading-font, weight: "bold", fill: accent-dark)[#title]
  #linebreak()
  #body
]

#let legend-color-table(cells) = table(
  columns: (1fr, 1fr),
  inset: 7pt,
  stroke: line-stroke,
  align: (left, left),
  ..cells,
)

#let legend-flow-demo(first, second, third) = [
  #grid(columns: (1fr, 18pt, 1fr, 18pt, 1fr), gutter: 4pt, align: center + horizon)[
    #rect(width: 100%, inset: 6pt, radius: 5pt, fill: warm-fill, stroke: line-stroke)[#align(center)[#first]]
  ][
    #text(font: heading-font, size: 12pt, fill: accent)[→]
  ][
    #rect(width: 100%, inset: 6pt, radius: 5pt, fill: handoff-fill, stroke: line-stroke)[#align(center)[#second]]
  ][
    #text(font: heading-font, size: 12pt, fill: accent)[→]
  ][
    #rect(width: 100%, inset: 6pt, radius: 5pt, fill: stable-fill, stroke: line-stroke)[#align(center)[#third]]
  ]
]

#let layer-card(title, body, fill: soft-fill) = [
  #v(2.5pt)
  #rect(
    width: 100%,
    inset: 8pt,
    radius: 5pt,
    fill: fill,
    stroke: line-stroke,
  )[
    #set par(first-line-indent: (amount: 0pt, all: true), justify: false)
    #text(font: heading-font, weight: "bold", fill: accent-dark)[#title]
    #linebreak()
    #text(size: 9pt, fill: body-ink)[#body]
  ]
  #v(2.5pt)
]

#let flow-node(body, fill: soft-fill) = [
  #v(2.5pt)
  #rect(
    width: 100%,
    inset: 7pt,
    radius: 5pt,
    fill: fill,
    stroke: line-stroke,
  )[
    #set par(first-line-indent: (amount: 0pt, all: true), justify: false)
    #align(center)[#text(size: 9pt, fill: body-ink)[#body]]
  ]
  #v(2.5pt)
]

#let flow-arrow(label: none) = [
  #v(1.5pt)
  #set par(first-line-indent: (amount: 0pt, all: true))
  #align(center)[
    #text(font: heading-font, size: 12.5pt, fill: accent)[↓]
    #if label != none { [ #text(font: song-font, size: 8.3pt, fill: muted)[#label] ] }
  ]
  #v(1.5pt)
]
