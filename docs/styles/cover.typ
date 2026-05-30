#import "tokens.typ": accent, accent-dark, accent-soft, body-font, body-ink, cover-page-core, cover-page-edge, cover-wave-deep, cover-wave-fill, heading-font, ink, line-stroke, panel-stroke

#let manual-cover(
  title: none,
  competition: none,
  work-label: none,
  work: none,
  school-label: none,
  school: none,
  team-label: none,
  team: none,
  members-label: none,
  members: none,
  advisor-label: none,
  advisor: none,
  author-label: none,
  author: none,
  date: none,
) = page(header: none, footer: none, fill: rgb("#ffffff"), margin: (top: 22mm, bottom: 22mm, left: 26mm, right: 26mm))[
  #let cover-info-size = 13.5pt
  #let cover-value(value) = block(width: 100%)[
    #text(size: cover-info-size, fill: body-ink)[#value]
    #v(2.5pt)
    #line(length: 100%, stroke: line-stroke)
  ]
  #let cover-row(label, value) = (
    [#text(font: heading-font, size: cover-info-size, weight: "bold", fill: accent-dark)[#label]],
    [#cover-value(value)],
  )
  #set text(font: body-font, fill: ink)
  #set par(first-line-indent: (amount: 0pt, all: true), justify: false, spacing: 0pt)

  #place(
    top + left,
    dx: -26mm,
    dy: -22mm,
    rect(
      width: 210mm,
      height: 297mm,
      fill: gradient.radial(cover-page-core, cover-page-edge),
      stroke: none,
    ),
  )
  #place(
    top + left,
    dx: -26mm,
    dy: -22mm,
    curve(
      curve.move((0mm, 0mm)),
      curve.line((210mm, 0mm)),
      curve.line((210mm, 24mm)),
      curve.cubic((154mm, 40mm), (60mm, 18mm), (0mm, 30mm)),
      curve.close(),
      fill: gradient.linear(cover-wave-deep, cover-wave-fill, angle: 90deg),
      stroke: none,
    ),
  )
  #place(
    top + left,
    dx: -26mm,
    dy: -22mm,
    curve(
      curve.move((0mm, 30mm)),
      curve.cubic((60mm, 18mm), (154mm, 40mm), (210mm, 24mm)),
      stroke: 0.75pt + accent,
      fill: none,
    ),
  )
  #place(
    bottom + left,
    dx: -26mm,
    dy: 22mm,
    curve(
      curve.move((0mm, 0mm)),
      curve.line((210mm, 0mm)),
      curve.line((210mm, -12mm)),
      curve.cubic((150mm, -28mm), (68mm, -4mm), (0mm, -18mm)),
      curve.close(),
      fill: gradient.linear(cover-wave-fill, cover-wave-deep, angle: 90deg),
      stroke: none,
    ),
  )
  #place(
    bottom + left,
    dx: -26mm,
    dy: 22mm,
    curve(
      curve.move((0mm, -18mm)),
      curve.cubic((68mm, -4mm), (150mm, -28mm), (210mm, -12mm)),
      stroke: 0.75pt + accent,
      fill: none,
    ),
  )

  #rect(
    width: 100%,
    inset: (x: 14mm, y: 14mm),
    radius: 10pt,
    fill: none,
    stroke: none,
  )[
    #v(12mm)
    #align(center)[
      #rect(
        inset: (x: 10pt, y: 3pt),
        radius: 4pt,
        fill: accent-soft,
        stroke: none,
      )[
        #text(font: "Times New Roman", size: 10pt, weight: "bold", fill: accent-dark)[2026]
      ]
    ]
    #v(10mm)
    #align(center)[
      #text(font: heading-font, size: 14.5pt, weight: "bold", fill: accent-dark)[#competition]
    ]
    #v(24mm)
    #align(center)[
      #text(font: heading-font, size: 30pt, weight: "bold", fill: ink)[#title]
    ]
    #v(30mm)
    #rect(
      width: 100%,
      inset: (x: 11mm, y: 10mm),
      radius: 9pt,
      fill: gradient.linear(rgb("#ffffff"), rgb("#f8fbff"), angle: 90deg),
      stroke: panel-stroke,
    )[
      #grid(
        columns: (32mm, 1fr),
        rows: auto,
        gutter: 8pt,
        align: (right, left),
        ..cover-row(work-label, work),
        ..cover-row(school-label, school),
        ..cover-row(team-label, team),
        ..cover-row(members-label, members),
        ..cover-row(advisor-label, advisor),
        ..cover-row(author-label, author),
      )
    ]
    #v(32mm)
    #align(center)[
      #text(font: heading-font, size: 13.5pt, fill: accent-dark)[#date]
    ]
    #v(10mm)
  ]
]
