#import "tokens.typ": accent, accent-dark, body-font, body-ink, body-size, chapter-fill, heading-font, manual-page-kind, manual-page-targets, manual-running-title, mono-font, muted, panel-fill, panel-stroke, paper-fill, rule-stroke, song-font

#let manual(title: none, author: none, badge: none, body) = {
  let document-author = if author == none { title } else { author }
  set document(title: title, author: document-author)
  manual-running-title.update(title)
  set page(
    paper: "a4",
    margin: (top: 25.4mm, bottom: 25.4mm, left: 31.8mm, right: 31.8mm),
    fill: paper-fill,
    header: context [
      #set text(font: song-font, size: 10pt, fill: muted)
      #let page-number = counter(page).get().first()
      #let page-kind = manual-page-kind.get()
      #let page-loc = here()
      #manual-page-targets.update(entries => entries + ((page-kind, page-number, page-loc),))
      #let running-title = manual-running-title.get()
      #let header-text = if calc.rem(page-number, 2) == 1 {
        title
      } else if running-title != none {
        running-title
      } else {
        title
      }
      #align(center)[#header-text]
      #v(3pt)
      #line(length: 100%, stroke: rule-stroke)
    ],
    footer: context [
      #set text(font: "Times New Roman", size: 8.5pt, fill: muted)
      #line(length: 100%, stroke: rule-stroke)
      #v(3pt)
      #align(center)[#text(fill: accent-dark)[#counter(page).display()]]
    ],
  )
  set text(font: body-font, size: body-size, fill: body-ink, lang: "zh", region: "CN")
  set par(justify: true, first-line-indent: (amount: 2em, all: true), spacing: 0.9em)
  set heading(numbering: none)
  set figure(numbering: none, supplement: none)
  set raw(tab-size: 2)

  show raw.where(block: false): it => text(font: mono-font)[#it.text]

  show raw.where(block: true): it => block(
    breakable: true,
    width: 100%,
    fill: rgb("#f6f9fc"),
    stroke: panel-stroke,
    radius: 5pt,
    inset: 8pt,
  )[
    #set par(first-line-indent: (amount: 0pt, all: true), justify: false, spacing: 0pt)
    #set text(font: mono-font, size: 9.5pt)
    #it
  ]

  show figure.caption: set text(size: 9pt, fill: muted)
  show figure.caption: set par(first-line-indent: (amount: 0pt, all: true), spacing: 0pt)

  show heading.where(level: 1): it => [
    #v(10pt)
    #set par(first-line-indent: (amount: 0pt, all: true), spacing: 0pt)
    #rect(width: 100%, inset: (x: 14pt, y: 12pt), radius: 8pt, fill: chapter-fill, stroke: panel-stroke)[
      #text(font: heading-font, size: 15.5pt, weight: "bold", fill: accent-dark)[#it.body]
      #v(5pt)
      #line(length: 38mm, stroke: 1.1pt + accent)
    ]
    #v(11pt)
  ]

  show heading.where(level: 2): it => [
    #v(11pt)
    #set par(first-line-indent: (amount: 0pt, all: true), spacing: 0pt)
    #grid(columns: (5pt, 1fr), gutter: 7pt, align: (left + horizon, left + horizon))[
      #rect(width: 5pt, height: 14pt, radius: 2.5pt, fill: accent, stroke: none)
    ][
      #text(font: heading-font, size: 13.5pt, weight: "bold", fill: accent-dark)[#it.body]
    ]
    #v(5pt)
  ]

  show heading.where(level: 3): it => [
    #v(8pt)
    #set par(first-line-indent: (amount: 0pt, all: true), spacing: 0pt)
    #text(font: heading-font, size: 11.5pt, weight: "bold", fill: accent)[#it.body]
    #v(4pt)
  ]

  show heading.where(level: 4): it => [
    #v(6pt)
    #set par(first-line-indent: (amount: 0pt, all: true), spacing: 0pt)
    #text(font: heading-font, size: 10.5pt, weight: "semibold", fill: body-ink)[#it.body]
    #v(3pt)
  ]

  show figure: it => [
    #v(6pt)
    #set par(first-line-indent: (amount: 0pt, all: true), spacing: 0pt)
    #if it.body.func() == raw [
      #align(center)[#it]
    ] else [
      #rect(width: 100%, inset: 8pt, radius: 8pt, fill: panel-fill, stroke: panel-stroke)[
        #align(center)[#it]
      ]
    ]
    #v(8pt)
  ]

  body
}
