#import "tokens.typ": accent-dark, body-font, body-ink, heading-font

#let _manual-toc-row(body, page, depth: 1) = {
  let indent = (depth - 1) * 1.7em
  let weight = if depth == 1 { "bold" } else { "regular" }
  let entry-fill = if depth == 1 { accent-dark } else { body-ink }
  let entry-size = if depth == 1 { 12.2pt } else if depth == 2 { 11.3pt } else { 10.8pt }
  let entry-font = if depth == 1 { heading-font } else { body-font }
  let gap = if depth == 1 { 16pt } else if depth == 2 { 10.5pt } else { 8.5pt }

  block(above: gap, below: 3pt)[
    #grid(
      columns: (indent, 1fr, 13mm),
      gutter: 0pt,
      align: (left, left, right),
    )[][
      #grid(columns: (auto, 1fr), gutter: 8pt, align: (left + horizon, left + horizon))[
        #text(font: entry-font, size: entry-size, weight: weight, fill: entry-fill)[#body]
      ][
        #line(length: 100%, stroke: 0.45pt + rgb("#cbdbea"))
      ]
    ][
      #text(font: "Times New Roman", size: entry-size, fill: entry-fill)[#page]
    ]
  ]
}

#let manual-toc(title: none) = [
  #heading(level: 1)[#title]
  #set par(first-line-indent: (amount: 0pt, all: true), spacing: 0pt)
  #v(11pt)
  #show outline.entry: it => {
    let depth = it.element.depth
    block[
      #link(it.element.location())[
        #_manual-toc-row(it.body(), it.page(), depth: depth)
      ]
    ]
  }
  #outline(title: none, depth: 3)
]

#let manual-front-section(title, body) = [
  #heading(level: 1)[#title]
  #set par(justify: true, first-line-indent: (amount: 2em, all: true), spacing: 0.9em)
  #body
]
