// bella-presenter showcase: every feature on a slide, so the deck is a manual and a test script.
// typst compile --root . examples/showcase.typ examples/showcase.pdf
#import "/bella-media.typ": media, media-url

#let crimson = rgb("#aa0159")
#let plum = rgb("#4a0128")
#let ink = rgb("#1f1d2b")
#let soft = rgb("#f7f0f3")
#let muted = rgb("#6b6775")
#let slide-w = 25.4cm
#let slide-h = 14.29cm
#let site = "https://bellatola.zahno.dev/en/bella-presenter/"
#let repo = "https://github.com/tschinz/bella-presenter"

#set document(title: "bella-presenter showcase")
#set page(
  width: slide-w,
  height: slide-h,
  margin: (x: 1.6cm, top: 1.3cm, bottom: 1.4cm),
  footer: context text(size: 10pt, fill: muted, grid(
    columns: (1fr, auto),
    [bella-presenter · showcase], counter(page).display(),
  )),
)
// Avenir Next or Helvetica Neue where installed, else the fonts built into Typst.
#set text(font: ("Avenir Next", "Helvetica Neue", "Arial", "Libertinus Serif"), size: 18pt, fill: ink)
#set par(leading: 0.55em)
#set list(marker: text(fill: crimson)[•], spacing: 0.7em)
#show link: set text(fill: crimson)
#show heading.where(level: 1): it => block(below: 0.55cm, {
  text(size: 28pt, weight: "bold", fill: crimson, it.body)
  v(-0.2cm)
  box(width: 2.4cm, height: 4pt, radius: 2pt, fill: crimson)
})
// Inline code shows as a key cap.
#show raw.where(block: false): it => box(
  inset: (x: 0.32em, y: 0.12em),
  outset: (y: 0.12em),
  radius: 0.25em,
  fill: white,
  stroke: 0.6pt + luma(165),
  text(font: ("Menlo", "DejaVu Sans Mono"), size: 0.78em, fill: ink, it.text),
)

// --- Building blocks -------------------------------------------------------------------------

// How to try the feature: a tinted bar under the title.
#let try(body) = block(
  width: 100%,
  inset: (x: 14pt, y: 9pt),
  radius: (right: 6pt),
  fill: soft,
  stroke: (left: 4pt + crimson),
  text(size: 15pt)[*Try:* #body],
)

// A feature card: its keys, a name and one line of text.
#let card(keys, name, body) = block(width: 100%, height: 100%, inset: (x: 14pt, y: 11pt), radius: 8pt, fill: soft, breakable: false, stack(
  spacing: 0.22cm,
  text(size: 15pt, keys),
  text(size: 18pt, weight: "bold", name),
  text(size: 14pt, fill: muted, body),
))
// Up to three cards in a row; four make a 2 x 2 grid, so the text stays wide enough.
#let cards(height: auto, columns: auto, ..items) = {
  let n = items.pos().len()
  let cols = if columns != auto { columns } else if n == 4 { 2 } else { n }
  let rows = if height != auto { height } else if n > cols { 3cm } else { 4.6cm }
  grid(columns: (1fr,) * cols, rows: rows, gutter: 0.45cm, ..items.pos())
}

// A screenshot `width` wide with aspect `ratio` (w / h). With `video` (an H.264 MP4), a recording
// plays over the screenshot in bella-presenter; other viewers keep showing the screenshot.
#let shot(path, width: 12cm, ratio: 16 / 9, video: none) = {
  let height = width / ratio
  let frame(body) = block(radius: 6pt, clip: true, stroke: 0.8pt + luma(200), body)
  if video == none {
    frame(image(path, width: width, height: height, fit: "contain"))
  } else {
    frame(media(video, width: width, height: height, poster: path))
  }
}

// A content slide.
#let slide(title, body) = page({
  heading(title)
  body
})

// Text on the left, a picture on the right.
#let feature(title, how, points, picture, split: (1fr, 1.35fr)) = slide(title, {
  try(how)
  v(0.35cm)
  grid(columns: split, gutter: 0.8cm, align: (left + top, center + top), text(size: 16pt, points), picture)
})

// A part divider.
#let part(number, title, note) = page(
  footer: none,
  fill: gradient.linear(crimson, plum, angle: 30deg),
  {
    place(right + bottom, dx: 1cm, dy: 2.2cm, text(size: 220pt, weight: "bold", fill: white.transparentize(88%), number))
    align(left + horizon, text(fill: white)[
      #text(size: 16pt, tracking: 2pt, upper("Part " + number)) \
      #text(size: 44pt, weight: "bold", title) \
      #v(0.1cm)
      #text(size: 20pt, fill: white.transparentize(15%), note)
    ])
  },
)

// --- Opening ---------------------------------------------------------------------------------

#page(footer: none, margin: 0pt, grid(
  columns: (1.6fr, 1fr),
  rows: slide-h,
  block(width: 100%, height: 100%, inset: (x: 1.8cm, y: 1.6cm), align(left + horizon, {
    text(size: 15pt, tracking: 2pt, fill: crimson, upper[Showcase])
    v(0.1cm)
    // A box keeps the name on one line.
    box(text(size: 40pt, weight: "bold")[bella-presenter])
    v(0.1cm)
    text(size: 20pt, fill: muted)[A fast, native dual-screen presenter for Typst and LaTeX slides.]
    v(0.8cm)
    text(size: 16pt)[
      Every feature on its own slide, with how to try it. \
      Press `F5` to present and `→` to step through.
    ]
    v(0.6cm)
    text(size: 15pt, link(site)[bellatola.zahno.dev/en/bella-presenter])
  })),
  block(width: 100%, height: 100%, fill: gradient.linear(soft, rgb("#efdde6"), angle: 90deg), align(
    center + horizon,
    image("/img/logo.png", width: 80%),
  )),
))

// Every key and mouse action on one page, grouped like the parts of this deck.
#slide("Shortcuts at a glance", {
  set text(size: 12.5pt)
  let group(title, rows) = block(breakable: false, below: 0.35cm, {
    text(size: 13pt, weight: "bold", fill: crimson, upper(title))
    v(-0.1cm)
    table(
      columns: (4.7cm, 1fr),
      align: left + horizon,
      stroke: none,
      inset: (x: 0pt, y: 2.6pt),
      ..rows.flatten(),
    )
  })
  grid(columns: (1fr, 1fr), gutter: 1cm, {
    group("Navigation", (
      ([`→` `Space` `PgDn` scroll], [next slide]),
      ([`←` `PgUp` scroll], [previous slide]),
      ([`Home` `End`], [first / last slide]),
      ([number `Enter`], [go to that slide]),
      ([`T`], [thumbnail strip]),
      ([click a link], [follow it]),
    ))
    group("Presenting", (
      ([`F5` `Esc`], [start / quit presenting]),
      ([`S`], [swap the screens]),
      ([`L`], [flip the layout]),
      ([`K` `R`], [pause / reset the timer]),
      ([`+` `-`], [footer font size]),
    ))
  }, {
    group("On the slide", (
      ([hold the mouse], [laser pointer]),
      ([`P`], [pointer / drawing mode]),
      ([left / right drag], [draw / erase]),
      ([`D`], [delete this slide's drawings]),
      ([`Ctrl` `Alt` `Cmd` + scroll], [zoom (+ drag: pan)]),
      ([`B` `,`], [black screen / whiteboard]),
      ([`C`], [webcam overlay]),
    ))
    group("Media and files", (
      ([`Space` `R` (media slide)], [play, pause / restart clips]),
      ([`O` `W`], [open a PDF / close it]),
    ))
  })
})

// --- Navigation ------------------------------------------------------------------------------

#part("1", "Navigation", "Moving through the deck")

#slide("Moving around", {
  try[scroll turns slides too. Type `1` `Enter` and come back with `End`.]
  v(0.4cm)
  cards(
    card([`→` `Space` `PgDn`], [Next slide], [or scroll down]),
    card([`←` `PgUp`], [Previous slide], [or scroll up]),
    card([`Home` `End`], [First and last], [jump to either end of the deck]),
    card([`7` `Enter`], [Go to a slide], [type its number; the footer shows it, `Esc` cancels]),
  )
}) <nav>

#slide("Links", {
  try[click a link in either window, in pointer or drawing mode. The cursor turns into a hand over a link.]
  v(0.4cm)
  cards(
    height: 5.2cm,
    card([click], [Inside the deck], [back to #link(<nav>)[Moving around]]),
    card([click], [On the web], [#link(site)[bellatola.zahno.dev] opens in the browser]),
    card([click], [Source code], [#link(repo)[github.com/tschinz/bella-presenter]]),
  )
})

// --- Presenting ------------------------------------------------------------------------------

#part("2", "Presenting", "Two screens, notes, timer")

#slide("Present on two screens", {
  try[press `F5` now, then `Esc` to come back.]
  v(0.4cm)
  cards(
    card([`F5`], [Start], [the audience window goes fullscreen on the second screen]),
    card([`Esc`], [Quit], [closes the audience window; the presenter stays]),
    card([`S`], [Swap screens], [when the projector shows the wrong window]),
    card([double-click], [Fullscreen], [toggles the audience window]),
  )
})

#feature(
  "The presenter window",
  [press `L` to flip the layout and `T` for the thumbnails.],
  [
    - Current slide, next slide and notes; drag the dividers, the sizes are kept
    - `T` thumbnail strip: click to jump
    - Footer: clock and talk timer, `K` pause, `R` reset
    - `+` `-` footer font size
  ],
  shot("/img/screenshot-presenter-view.png", width: 10.8cm, ratio: 3824 / 2484),
)

// A double-width page: the left half is the slide, the right half the speaker notes.
#page(width: 2 * slide-w, margin: 0pt, footer: none, grid(
  columns: (slide-w, slide-w),
  rows: slide-h,
  block(width: 100%, height: 100%, inset: (x: 1.6cm, top: 1.3cm, bottom: 1cm), {
    heading[Speaker notes]
    try[the presenter window shows the notes of this slide; the audience sees this half only.]
    v(0.4cm)
    grid(
      columns: (auto, 1fr),
      gutter: 0.7cm,
      media("examples/clips/big-lebowski-1.gif", width: 3.5cm, height: 4.8cm),
      text(size: 15pt)[
        Typst and Beamer write notes on a double-width page: the slide on the left, the notes on
        the right. Decks can mix such pages with plain ones, as this deck does.

        #text(fill: muted)[The GIF plays exactly over its placeholder on the slide half.]
      ],
    )
  }),
  block(width: 100%, height: 100%, inset: 1.6cm, fill: rgb("#fff8e6"), {
    text(size: 24pt, weight: "bold")[Notes for the presenter]
    v(0.3cm)
    list(
      [Typst/touying: notes on second screen = right],
      [Beamer: `show notes on second screen=right`],
      [Only the presenter sees this half.],
    )
  }),
))

// --- On the slide ----------------------------------------------------------------------------

#part("3", "On the slide", "Point, draw, zoom, cover")

#feature(
  "Pointer, drawing, erasing",
  [hold the mouse on this slide, then press `P` and draw.],
  [
    - Hold the mouse: laser pointer dot
    - `P` switches to drawing mode and back
    - Left-drag draws, right-drag erases (light red circle)
    - `D` deletes the drawings of this slide only
    - The colour swatch in the header sets pointer and ink
  ],
  shot("/img/screenshot-drawing.png", width: 10.6cm, ratio: 3549 / 2024),
  split: (1fr, 1.1fr),
)

#slide("Zoom and pan", {
  try[hold `Ctrl` and scroll over the slide below, then drag.]
  v(0.4cm)
  cards(
    card([`Ctrl` `Alt` `Cmd` + scroll], [Zoom], [at the cursor, sharp at any level]),
    card([same key + drag], [Pan], [move around the zoomed slide]),
    card([either window], [Mirrored], [the audience sees the same zoom]),
  )
})

// What the audience sees after `B`: a black page.
#page(fill: black, footer: none, align(center + horizon, text(fill: luma(150))[
  #text(size: 40pt, weight: "bold", fill: white)[Black screen]
  #v(0.2cm)
  #text(size: 20pt)[Press `B` to blank the audience screen.] \
  #text(size: 16pt)[`B` again brings the slide back; the presenter window keeps showing it.]
]))

// What both screens show after `,`: a white page to draw on.
#page({
  heading[Whiteboard]
  text(size: 18pt, fill: muted)[Press `,` for a white page on both screens; drawing mode starts. Draw here: left-drag draws, right-drag erases, `D` clears, `,` again to leave.]
})

#feature(
  "Webcam overlay",
  [press `C`; the gear in the header opens the camera settings.],
  [
    - In a corner of the audience window only
    - Rectangle, rounded or circle; size and corner
    - Background: keep, blur or remove (runs locally)
  ],
  grid(
    columns: (auto, auto),
    gutter: 0.4cm,
    align: horizon,
    shot("/img/screenshot-audience-view.png", width: 9.6cm, ratio: 3600 / 2024),
    shot("/img/screenshot-camera-settings.png", width: 2.9cm, ratio: 694 / 1255),
  ),
  split: (1fr, 1.6fr),
)

// --- Media -----------------------------------------------------------------------------------

#part("4", "Media", "GIFs and videos inside the PDF")

// The four media kinds in four sections, with the playback keys in the footer.
#let four-media(title, subtitle) = {
  let (mw, mh) = (5.4cm, 3.04cm)
  let section(label, clip) = align(center + horizon, stack(
    spacing: 0.25cm,
    text(size: 15pt, weight: "bold", label),
    clip,
  ))
  let line = luma(215)
  let hint = text(size: 14pt, fill: muted)[`Space` pauses and plays the clips #h(0.4em) · #h(0.4em) `R` restarts them]
  page(footer: align(center, hint), {
    heading(title)
    v(-0.3cm)
    text(size: 16pt, fill: muted, subtitle)
    grid(
      columns: (1fr, 1fr),
      // Fixed rows: `1fr` rows would take the whole page height, ignoring the heading.
      rows: (3.9cm, 3.9cm),
      stroke: (x, y) => (left: if x > 0 { line }, top: if y > 0 { line }),
      section("GIF local", media("examples/clips/big-lebowski-1.gif", width: mw, height: mh)),
      // The GIF file of https://giphy.com/gifs/big-lebowski-yk8tRCZHCV0qY (the page itself is HTML).
      section("GIF url", media-url("https://media.giphy.com/media/yk8tRCZHCV0qY/giphy.gif", width: mw, height: mh)),
      section("Video local", media("examples/clips/big-buck-bunny-sound.mp4", width: mw, height: mh, poster: "examples/clips/buck-bunny.jpg", muted: true)),
      section("Video url", media-url(
        "https://test-videos.co.uk/vids/bigbuckbunny/mp4/h264/360/Big_Buck_Bunny_360_10s_1MB.mp4",
        width: mw,
        height: mh,
        poster: "examples/clips/buck-bunny.jpg",
      )),
    )
  })
}

#four-media("Animated GIFs and videos", [Embedded in the PDF, or loaded from a URL])

// A build in two steps, as touying's `#pause` makes them: the clip is laid out but hidden on step 1
// (like content after a pause) and shows on step 2. bella-presenter must play it on step 2 only.
#let build-step(step) = slide("Clips in builds (step " + str(step) + " of 2)", {
  try[step 1 hides the clip (like content after `#pause`), so nothing plays; `→` shows it.]
  v(0.4cm)
  let clip = media("examples/clips/big-buck-bunny-sound.mp4", width: 12cm, height: 6.75cm, poster: "examples/clips/buck-bunny.jpg", muted: true)
  align(center, if step == 1 { hide(clip) } else { clip })
})
#build-step(1)
#build-step(2)

#slide("Video with sound", {
  try[videos play their sound (`muted: true` silences them); `Space` pauses both, `R` starts over.]
  v(0.4cm)
  align(center, media(
    "examples/clips/big-buck-bunny-sound.mp4",
    width: 10.4cm,
    height: 5.85cm,
    poster: "examples/clips/buck-bunny.jpg",
  ))
  align(center, text(size: 11pt, fill: muted)[Big Buck Bunny, © Blender Foundation, #link("https://peach.blender.org")[peach.blender.org], CC BY 3.0])
})

#slide("How clips play", {
  try[go back one slide and forward again: the clip starts over.]
  v(0.4cm)
  cards(
    card([`Space`], [Play / pause], [the clips of this slide; elsewhere it turns the slide]),
    card([`R`], [Restart], [from the first frame; elsewhere it resets the timer]),
    card([`←` `→`], [Leave and return], [clips stop when you leave and start over]),
    card([preview], [Placeholders], [next slide and thumbnails stay still]),
  )
})

// --- Files and app ---------------------------------------------------------------------------

#part("5", "Files and app", "Opening, reloading, settings")

#feature(
  "Open, recent files, hot reload",
  [press `W` to see the recent files, then open this deck again.],
  [
    - `O` opens a PDF; drag and drop works too
    - `W` closes it; recent decks reopen at the last page
    - Recompile the PDF while it is open: it reloads in place
  ],
  shot("/img/screenshot-startpage.png", width: 9.2cm, ratio: 2452 / 1802),
)

#slide("Theme, shortcuts, about", {
  try[use the buttons on the right of the header.]
  v(0.4cm)
  cards(
    card([theme], [Light and dark], [system, dark or light]),
    card([keyboard], [Shortcuts], [every key in one list]),
    card([info], [About and updates], [version and the update check]),
  )
})

#page(footer: none, fill: gradient.linear(crimson, plum, angle: 30deg), align(center + horizon, text(fill: white)[
  #text(size: 44pt, weight: "bold")[Get bella-presenter]
  #v(0.2cm)
  #text(size: 20pt)[macOS · Windows · Linux]
  #v(0.6cm)
  #text(size: 18pt)[#link(site, text(fill: white)[bellatola.zahno.dev/en/bella-presenter]) \
    #link(repo, text(fill: white)[github.com/tschinz/bella-presenter])]
]))
