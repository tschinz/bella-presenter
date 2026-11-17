// bella-media.typ: animated media (GIF, MP4, WebM) for slides shown with bella-presenter.
//
//   #import "/bella-media.typ": media, media-url
//   #media("clips/demo.gif", width: 8cm, height: 4.5cm)
//   #media("clips/demo.mp4", width: 8cm, height: 4.5cm, poster: "clips/demo-poster.png")
//   #media("clips/demo.mp4", width: 8cm, height: 4.5cm, muted: true)   // without its sound
//   #media("clips/demo.mp4", width: 8cm, height: 4.5cm, autoplay: false)   // waits for Space
//   #media("clips/wide.mp4", width: 100%, height: 100%, fit: "cover")   // fills the box, crops the rest
//   #media-url("https://example.org/clip.gif", width: 8cm, height: 4.5cm)
//
// Clips start by themselves; `autoplay: false` shows the placeholder and waits for Space. Videos
// play their sound; `muted: true` silences a clip. A clip and its placeholder are letterboxed in
// their box; `fit: "cover"` fills the box instead and cuts off what overflows (CSS `object-fit`).
//
// The page shows a static placeholder in the media rectangle (a GIF's first frame, the poster, or
// a black block), so any PDF viewer shows something sensible. The media files and a manifest,
// `bella-media.json`, are attached to the PDF; bella-presenter plays them over the placeholder.
//
// A small PDF without clips: `typst compile --input media=still` draws only the placeholders and
// attaches nothing, so the PDF shows the stills in bella-presenter too.
//
// Paths are read by this file, so they are relative to the folder holding bella-media.typ (the
// project root), or start with "/" (also the project root with `typst compile --root .`). A file
// can also be passed as bytes, then `name` is required.
//
// Manifest entry (one per media item, in document order):
//   kind      "file" (bytes attached as media/<file>) or "url" (loaded from `url`)
//   page      1-based page number
//   x_pt y_pt top-left corner in points, origin top-left of the page, y down
//   w_pt h_pt size in points
//   mime      image/gif, video/mp4 or video/webm
//   file url  attachment name ("media/...") or web address; the other one is null
//   autoplay loop muted   playback hints (a clip starts by itself unless autoplay is false; a
//                         video plays its sound unless muted)
//   fit       "contain" (letterboxed) or "cover" (fills the box, cropped)

#let _items = counter("bella-media")

// `--input media=embed|still`: still leaves out the clips and the manifest, only the placeholders stay.
#let _mode = sys.inputs.at("media", default: "embed")
#assert(_mode in ("embed", "still"), message: "bella-media: --input media is embed or still, not " + repr(_mode))

#let _mime-of(name) = {
  let ext = lower(name.split(".").last())
  if ext == "gif" { "image/gif" } else if ext in ("mp4", "m4v") { "video/mp4" } else if ext == "webm" {
    "video/webm"
  } else { panic("bella-media: unknown media type of " + name + ", pass mime:") }
}

#let _base-name(path) = path.split("/").last()

// The placeholder filling the media rectangle. A poster or a GIF's first frame is drawn as it is,
// so a transparent image (an SVG, a PNG with alpha, a GIF of another shape than its box) keeps the
// slide behind it; only a video without a poster gets a black block. The invisible backing box
// marks the rectangle as drawn on this page (hidden content draws nothing), which bella-presenter
// checks before playing.
#let _placeholder(poster, gif-data, fit) = {
  let backing(fill, body) = block(width: 100%, height: 100%, fill: fill, clip: true, align(center + horizon, body))
  let clear = black.transparentize(100%)
  if type(poster) == str or type(poster) == bytes {
    backing(clear, image(poster, width: 100%, height: 100%, fit: fit))
  } else if type(poster) == content {
    backing(clear, poster)
  } else if gif-data != none {
    backing(clear, image(gif-data, format: "gif", width: 100%, height: 100%, fit: fit))
  } else {
    backing(black, none)
  }
}

// Attaches the manifest and every local media file once per document (called by the first item).
#let _attach-all() = {
  let names = (:) // source key -> attachment name
  let used = ()
  let entries = ()
  for (i, m) in query(<bella-media>).map(m => m.value).enumerate() {
    let file = none
    if m.kind == "file" {
      if m.key in names {
        file = names.at(m.key)
      } else {
        // Different files with the same base name get a number in front.
        file = "media/" + m.name
        if file in used { file = "media/" + str(i + 1) + "-" + m.name }
        names.insert(m.key, file)
        used.push(file)
        pdf.attach(file, m.data, mime-type: m.mime, description: "bella-presenter media")
      }
    }
    entries.push((
      kind: m.kind,
      page: m.page,
      x_pt: m.x,
      y_pt: m.y,
      w_pt: m.w,
      h_pt: m.h,
      mime: m.mime,
      file: file,
      url: m.url,
      autoplay: m.autoplay,
      loop: m.loop,
      muted: m.muted,
      fit: m.fit,
    ))
  }
  pdf.attach(
    "bella-media.json",
    bytes(json.encode(entries)),
    mime-type: "application/json",
    description: "bella-presenter media manifest",
  )
}

#let _media(kind, key, name, data, url, mime, width, height, poster, autoplay, loop, muted, fit) = {
  assert(fit in ("contain", "cover"), message: "bella-media: fit is \"contain\" or \"cover\", not " + repr(fit))
  let gif-data = if mime == "image/gif" { data } else { none }
  if _mode == "still" { return box(width: width, height: height, _placeholder(poster, gif-data, fit)) }
  _items.step()
  // The placeholder fills the box; `layout` only records where the box landed (a `place` inside
  // `layout` shifts it off the box in some containers, such as grid cells).
  box(width: width, height: height, {
    place(top + left, _placeholder(poster, gif-data, fit))
    layout(size => {
    let p = here().position()
    [#metadata((
        kind: kind,
        key: key,
        name: name,
        data: data,
        url: url,
        mime: mime,
        page: p.page,
        x: p.x.pt(),
        y: p.y.pt(),
        w: size.width.pt(),
        h: size.height.pt(),
        autoplay: autoplay,
        loop: loop,
        muted: muted,
        fit: fit,
      ))<bella-media>]
    if _items.get().first() == 1 { _attach-all() }
  })
  })
}

/// A local GIF, MP4 or WebM file, embedded in the PDF.
/// - src: path (see above) or bytes
/// - name: attachment file name, required when `src` is bytes
/// - poster: none (GIF: first frame, video: black), an image path or bytes, or any content
/// - autoplay: false shows the placeholder and waits for Space
/// - muted: true silences a video's sound
/// - fit: "contain" letterboxes the clip in its box, "cover" fills the box and crops the rest
#let media(
  src,
  width: 100%,
  height: 100%,
  name: auto,
  mime: auto,
  poster: none,
  autoplay: true,
  loop: true,
  muted: false,
  fit: "contain",
) = {
  let name = if name != auto { name } else if type(src) == str { _base-name(src) } else {
    panic("bella-media: pass name: for media given as bytes")
  }
  let mime = if mime == auto { _mime-of(name) } else { mime }
  // a still PDF needs the bytes of a GIF only (its first frame), never those of a video
  let data = if type(src) == bytes { src } else if _mode == "still" and mime != "image/gif" { none } else {
    read(src, encoding: none)
  }
  let key = if type(src) == str { src } else { "bytes:" + name }
  _media("file", key, name, data, none, mime, width, height, poster, autoplay, loop, muted, fit)
}

/// Media on the web, loaded by bella-presenter when its slide comes up (nothing is embedded).
#let media-url(
  url,
  width: 100%,
  height: 100%,
  mime: auto,
  poster: none,
  autoplay: true,
  loop: true,
  muted: false,
  fit: "contain",
) = {
  if "youtube.com/" in url or "youtu.be/" in url {
    panic("bella-media: YouTube serves no video file to play; link to the page instead")
  }
  // A page such as giphy.com/gifs/... has no file extension: use the file's own address.
  let mime = if mime == auto { _mime-of(url.split("?").first()) } else { mime }
  _media("url", url, none, none, url, mime, width, height, poster, autoplay, loop, muted, fit)
}
