#import "tokens.typ": accent, accent-dark, body-font, body-ink, chapter-fill, heading-font, manual-page-targets, muted, panel-stroke

#let term-page(kind, number, label) = context [
  #let matches = (manual-page-targets.final().filter(entry => entry.at(0) == kind and entry.at(1) == number))
  #if matches == () [
    #text(font: "Times New Roman", size: 8.9pt, fill: accent)[#label]
  ] else [
    #link(matches.first().at(2))[
      #text(font: "Times New Roman", size: 8.9pt, fill: accent)[#label]
    ]
  ]
]

#let term-pages(..items) = {
  if items.pos().len() == 0 {
    none
  } else {
    items.pos().join([
      #text(font: "Times New Roman", size: 8.9pt, fill: muted)[,]
      #h(0.22em)
    ])
  }
}

#let term-index-entry(term, gloss, pages: none, divider: true) = block(
  width: 100%,
  above: 3pt,
  below: 3pt,
  breakable: false,
)[
  #grid(
    columns: (24mm, 1fr),
    gutter: 7pt,
    align: (left + top, left + top),
  )[
    #text(font: body-font, size: 10.2pt, weight: "bold", fill: accent-dark)[#term]
  ][
    #set par(first-line-indent: (amount: 0pt, all: true), spacing: 0.35em, justify: true)
    #text(font: body-font, size: 9.8pt, fill: body-ink)[#gloss]
    #if pages != none [
      #v(4pt)
      #set par(first-line-indent: (amount: 0pt, all: true), spacing: 0pt, justify: false)
      #text(font: body-font, size: 8.9pt, fill: muted)[引用页：]
      #pages
    ]
  ]
  #if divider [
    #v(4pt)
    #line(length: 100%, stroke: 0.35pt + rgb("#d7e3ee"))
  ]
]

#let term-index-group(letter, body) = [
  #v(9pt)
  #grid(
    columns: (auto, 1fr),
    gutter: 8pt,
    align: (left + horizon, left + horizon),
  )[
    #text(font: "Times New Roman", size: 13.2pt, weight: "bold", fill: accent)[#letter]
  ][
    #line(length: 100%, stroke: 0.5pt + rgb("#c9d9e7"))
  ]
  #v(4pt)
  #body
]

#let term-index-anchor(title) = [
  #show heading.where(level: 1): it => []
  = #title
]

#let term-index-title-box(title) = [
  #set par(first-line-indent: (amount: 0pt, all: true), spacing: 0pt)
  #rect(width: 100%, inset: (x: 14pt, y: 12pt), radius: 8pt, fill: chapter-fill, stroke: panel-stroke)[
    #text(font: heading-font, size: 15.5pt, weight: "bold", fill: accent-dark)[#title]
    #v(5pt)
    #line(length: 38mm, stroke: 1.1pt + accent)
  ]
  #v(11pt)
]

#let term-index-columns(body) = [
  #block(width: 100%)[
    #columns(2, gutter: 18pt)[
      #body
    ]
  ]
]
