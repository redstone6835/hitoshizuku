#import "config.typ": *
#import "styles/cover.typ": manual-cover
#import "styles/document.typ": manual
#import "styles/navigation.typ": manual-toc
#import "styles/tokens.typ": manual-page-kind, manual-running-title

#show: manual.with(
  title: manual-title,
  author: project-name,
  badge: "🧭",
)

#manual-cover(
  title: manual-title,
  competition: cover-competition,
  work-label: cover-work-label,
  work: cover-work,
  school-label: cover-school-label,
  school: cover-school,
  team-label: cover-team-label,
  team: cover-team,
  members-label: cover-members-label,
  members: cover-members,
  advisor-label: cover-advisor-label,
  advisor: cover-advisor,
  author-label: cover-author-label,
  author: cover-author,
  date: cover-date,
)

#pagebreak()

#set page(numbering: "I")
#counter(page).update(1)

#manual-page-kind.update("front")
#manual-running-title.update(preface-title)
#include "chapters/preface.typ"

#manual-running-title.update(legend-title)
#pagebreak()

#include "chapters/legend.typ"

#manual-running-title.update(toc-title)
#pagebreak()

#manual-toc(title: toc-title)

#manual-running-title.update(chapter-01-title)
#pagebreak()

#set page(numbering: "1")
#counter(page).update(1)

#manual-page-kind.update("body")
#include "chapters/chapter-01.typ"

#manual-running-title.update(chapter-02-title)
#pagebreak()

#include "chapters/chapter-02.typ"

#manual-running-title.update(chapter-03-title)
#pagebreak()

#include "chapters/chapter-03.typ"

#manual-running-title.update(chapter-04-title)
#pagebreak()

#include "chapters/chapter-04.typ"

#manual-running-title.update(chapter-05-title)
#pagebreak()

#include "chapters/chapter-05.typ"

#manual-running-title.update(chapter-06-title)
#pagebreak()

#include "chapters/chapter-06.typ"

#manual-running-title.update(chapter-07-title)
#pagebreak()

#include "chapters/chapter-07.typ"

#manual-running-title.update(chapter-08-title)
#pagebreak()

#include "chapters/chapter-08.typ"

#manual-running-title.update(chapter-09-title)
#pagebreak()

#include "chapters/chapter-09.typ"

#manual-running-title.update(chapter-10-title)
#pagebreak()

#include "chapters/chapter-10.typ"

#manual-running-title.update(chapter-11-title)
#pagebreak()

#include "chapters/chapter-11.typ"

#manual-running-title.update(chapter-12-title)
#pagebreak()

#include "chapters/chapter-12.typ"

#manual-running-title.update(chapter-13-title)
#pagebreak()

#include "chapters/chapter-13.typ"

#manual-running-title.update(appendix-title)
#pagebreak()

#include "chapters/appendix.typ"

#manual-running-title.update(term-index-title)
#pagebreak()

#include "chapters/term-index.typ"

#manual-running-title.update(references-ack-title)
#pagebreak()

#include "chapters/references-and-ack.typ"
