//! Animated media on slides (see `bella-media.typ`): the `bella-media.json` manifest attached to
//! the PDF, decoding GIF and H.264 MP4 on background threads, and playing the media of the current
//! slide over its placeholder.
//!
//! A clip decodes on its own thread into a channel of two frames, so it only runs ahead of the
//! screen by that much; dropping its [`Player`] ends the thread. There is no audio output, so every
//! clip plays muted.

use std::collections::HashMap;
use std::collections::hash_map::Entry;
use std::io::Cursor;
use std::sync::Arc;
use std::sync::mpsc::{self, Receiver, TryRecvError};
use std::time::{Duration, Instant};

use anyhow::{Context as _, Result, anyhow};
use gpui_kit::RenderImage;
use image::codecs::gif::GifDecoder;
use image::{AnimationDecoder as _, RgbaImage};
use openh264::OpenH264API;
use openh264::decoder::{DecodedYUV, Decoder, DecoderConfig, Flush};
use openh264::formats::YUVSource as _;
use serde::Deserialize;

use crate::Waker;
use crate::render::{PageSize, Region};

/// The name of the manifest attachment.
pub const MANIFEST: &str = "bella-media.json";

/// A media format bella-presenter can play.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Format {
  Gif,
  Mp4,
}

/// Where a media item's bytes come from.
#[derive(Clone, Debug, PartialEq)]
pub enum Source {
  /// Attached to the PDF.
  Embedded(Arc<[u8]>),
  /// Loaded from the web when its slide comes up.
  Url(String),
}

/// One media item of the deck.
#[derive(Clone, Debug, PartialEq)]
pub struct Item {
  /// Zero-based page.
  pub page: usize,
  /// Where on the page, normalized (0..1, origin top left).
  pub area: Region,
  pub format: Format,
  pub source: Source,
  pub autoplay: bool,
  pub looping: bool,
}

/// A manifest entry as `bella-media.typ` writes it.
#[derive(Deserialize)]
struct ManifestEntry {
  kind: String,
  page: usize,
  x_pt: f32,
  y_pt: f32,
  w_pt: f32,
  h_pt: f32,
  mime: String,
  #[serde(default)]
  file: Option<String>,
  #[serde(default)]
  url: Option<String>,
  #[serde(default = "yes")]
  autoplay: bool,
  #[serde(default = "yes", rename = "loop")]
  looping: bool,
}

fn yes() -> bool {
  true
}

/// The media items of a PDF from its attachments (`(name, bytes)`, in PDF order) and page sizes,
/// plus a warning for every item skipped. A PDF without a manifest has no media.
pub fn from_attachments(attachments: Vec<(String, Vec<u8>)>, page_sizes: &[PageSize]) -> (Vec<Item>, Vec<String>) {
  let mut warnings = Vec::new();
  let mut files: HashMap<String, Arc<[u8]>> = HashMap::new();
  for (name, bytes) in attachments {
    match files.entry(name) {
      Entry::Occupied(e) => warnings.push(format!("two PDF attachments are named {}; using the first", e.key())),
      Entry::Vacant(e) => {
        e.insert(bytes.into());
      }
    }
  }
  let Some(json) = files.get(MANIFEST) else {
    return (Vec::new(), warnings);
  };
  let items = parse_manifest(&String::from_utf8_lossy(json), page_sizes, &files, &mut warnings);
  (items, warnings)
}

/// The playable items of manifest `json`. Entries that are malformed, point outside the deck,
/// miss their file or have an unsupported type are skipped with a warning.
fn parse_manifest(json: &str, page_sizes: &[PageSize], files: &HashMap<String, Arc<[u8]>>, warnings: &mut Vec<String>) -> Vec<Item> {
  let entries: Vec<serde_json::Value> = match serde_json::from_str(json) {
    Ok(entries) => entries,
    Err(e) => {
      warnings.push(format!("{MANIFEST} is not a list of media: {e}"));
      return Vec::new();
    }
  };
  let mut items = Vec::new();
  for (n, value) in entries.into_iter().enumerate() {
    match ManifestEntry::deserialize(value)
      .map_err(|e| e.to_string())
      .and_then(|e| item(e, page_sizes, files))
    {
      Ok(item) => items.push(item),
      Err(e) => warnings.push(format!("media {}: {e}", n + 1)),
    }
  }
  items
}

fn item(e: ManifestEntry, page_sizes: &[PageSize], files: &HashMap<String, Arc<[u8]>>) -> Result<Item, String> {
  let format = match e.mime.as_str() {
    "image/gif" => Format::Gif,
    "video/mp4" => Format::Mp4,
    "video/webm" => return Err("WebM is not supported (use GIF or H.264 MP4); showing its placeholder".into()),
    other => return Err(format!("unknown media type {other}")),
  };
  let page = e.page.checked_sub(1).filter(|p| *p < page_sizes.len()).ok_or(format!("no page {}", e.page))?;
  let size = page_sizes[page];
  let numbers = [e.x_pt, e.y_pt, e.w_pt, e.h_pt];
  if numbers.iter().any(|v| !v.is_finite()) || e.w_pt <= 0.0 || e.h_pt <= 0.0 || size.width <= 0.0 || size.height <= 0.0 {
    return Err("bad media rectangle".into());
  }
  let source = match (e.kind.as_str(), e.file, e.url) {
    ("file", Some(file), _) => Source::Embedded(files.get(&file).cloned().ok_or(format!("attachment {file} is missing"))?),
    // Web media loads over HTTPS only, like the update check.
    ("url", _, Some(url)) if url.starts_with("https://") => Source::Url(url),
    (kind, _, _) => return Err(format!("{kind:?} media without a usable file or url")),
  };
  Ok(Item {
    page,
    area: Region {
      x: e.x_pt / size.width,
      y: e.y_pt / size.height,
      w: e.w_pt / size.width,
      h: e.h_pt / size.height,
    },
    format,
    source,
    autoplay: e.autoplay,
    looping: e.looping,
  })
}

/// A decoded picture (BGRA, GPUI's order) and how long it shows.
pub struct Frame {
  pub bgra: RgbaImage,
  pub duration: Duration,
}

/// Decode `bytes` from the start, handing each frame to `emit` until it returns false. Returns the
/// number of frames handed over, or `None` when `emit` stopped early.
pub fn decode(format: Format, bytes: &[u8], mut emit: impl FnMut(Frame) -> bool) -> Result<Option<usize>> {
  let mut count = 0;
  let mut send = |frame| {
    count += 1;
    emit(frame)
  };
  let finished = match format {
    Format::Gif => decode_gif(bytes, &mut send)?,
    Format::Mp4 => decode_mp4(bytes, &mut send)?,
  };
  Ok(finished.then_some(count))
}

fn decode_gif(bytes: &[u8], emit: &mut impl FnMut(Frame) -> bool) -> Result<bool> {
  for frame in GifDecoder::new(Cursor::new(bytes))?.into_frames() {
    let frame = frame?;
    let (num, den) = frame.delay().numer_denom_ms();
    // Browsers show frames with (almost) no delay for 100 ms; GIFs are made to look right there.
    let ms = num / den.max(1);
    let duration = Duration::from_millis(u64::from(if ms < 20 { 100 } else { ms }));
    let mut bgra = frame.into_buffer();
    swap_red_blue(&mut bgra);
    if !emit(Frame { bgra, duration }) {
      return Ok(false);
    }
  }
  Ok(true)
}

fn decode_mp4(bytes: &[u8], emit: &mut impl FnMut(Frame) -> bool) -> Result<bool> {
  let mut mp4 = mp4::Mp4Reader::read_header(Cursor::new(bytes), bytes.len() as u64)?;
  let track = mp4
    .tracks()
    .values()
    .find(|t| matches!(t.media_type(), Ok(mp4::MediaType::H264)))
    .ok_or_else(|| anyhow!("no H.264 video track"))?;
  let avcc = &track.trak.mdia.minf.stbl.stsd.avc1.as_ref().context("no H.264 configuration")?.avcc;
  let length_size = usize::from(avcc.length_size_minus_one & 3) + 1;
  // MP4 keeps the parameter sets aside; the decoder wants them in the stream before a key frame.
  let mut params = Vec::new();
  for set in avcc.sequence_parameter_sets.iter().chain(&avcc.picture_parameter_sets) {
    params.extend([0, 0, 0, 1]);
    params.extend(&set.bytes);
  }
  let (track_id, samples) = (track.track_id(), track.sample_count());
  // Frames come out in display order; spreading the track time evenly over them keeps the pace.
  let duration = track.duration().checked_div(samples.max(1)).unwrap_or_default().max(Duration::from_millis(10));

  // Flushing after every packet breaks streams with B-frames; the rest comes out at the end.
  let config = DecoderConfig::new().flush_after_decode(Flush::NoFlush);
  let mut decoder = Decoder::with_api_config(OpenH264API::from_source(), config)?;
  let mut packet = Vec::new();
  for id in 1..=samples {
    let Some(sample) = mp4.read_sample(track_id, id)? else { continue };
    packet.clear();
    if sample.is_sync || id == 1 {
      packet.extend(&params);
    }
    annex_b(&sample.bytes, length_size, &mut packet);
    match decoder.decode(&packet) {
      Ok(Some(yuv)) => {
        if !emit(Frame {
          bgra: yuv_to_bgra(&yuv)?,
          duration,
        }) {
          return Ok(false);
        }
      }
      Ok(None) => {}
      // A damaged frame is skipped; the next key frame repairs the picture.
      Err(e) => tracing::debug!("H.264 frame {id}: {e}"),
    }
  }
  for yuv in decoder.flush_remaining()? {
    if !emit(Frame {
      bgra: yuv_to_bgra(&yuv)?,
      duration,
    }) {
      return Ok(false);
    }
  }
  Ok(true)
}

/// Append the NAL units of an MP4 sample (each prefixed with its `length_size`-byte length) to
/// `out` with start codes instead, as an H.264 decoder reads them.
fn annex_b(sample: &[u8], length_size: usize, out: &mut Vec<u8>) {
  let mut rest = sample;
  while rest.len() > length_size {
    let len = rest[..length_size].iter().fold(0usize, |n, b| n << 8 | usize::from(*b));
    let Some(unit) = rest.get(length_size..length_size + len) else { break };
    out.extend([0, 0, 0, 1]);
    out.extend(unit);
    rest = &rest[length_size + len..];
  }
}

fn yuv_to_bgra(yuv: &DecodedYUV<'_>) -> Result<RgbaImage> {
  let (w, h) = yuv.dimensions();
  let mut pixels = vec![0; w * h * 4];
  yuv.write_rgba8(&mut pixels);
  let mut image = RgbaImage::from_raw(u32::try_from(w)?, u32::try_from(h)?, pixels).context("frame size")?;
  swap_red_blue(&mut image);
  Ok(image)
}

fn swap_red_blue(image: &mut RgbaImage) {
  for px in image.as_chunks_mut::<4>().0 {
    px.swap(0, 2);
  }
}

/// One clip: decoding in the background and the clock that says which frame shows.
pub struct Player {
  format: Format,
  bytes: Arc<[u8]>,
  looping: bool,
  frames: Receiver<Result<Frame, String>>,
  /// A decoded frame waiting for its turn.
  next: Option<Frame>,
  /// When the frame on screen ends; `None` before the first one.
  until: Option<Instant>,
  playing: bool,
  /// Time left of the frame on screen, while paused.
  paused_left: Duration,
  /// Whether the clip ever played (a played clip restarts from the beginning next time).
  started: bool,
  /// Whether a clip that does not loop reached its end.
  ended: bool,
}

impl Player {
  /// Start decoding `bytes`; the clip plays at once when `playing`.
  pub fn new(format: Format, bytes: Arc<[u8]>, looping: bool, playing: bool) -> Self {
    let frames = spawn_decoder(format, bytes.clone(), looping);
    Self {
      format,
      bytes,
      looping,
      frames,
      next: None,
      until: None,
      playing,
      paused_left: Duration::ZERO,
      started: false,
      ended: false,
    }
  }

  pub fn started(&self) -> bool {
    self.started
  }

  pub fn playing(&self) -> bool {
    self.playing
  }

  /// Start over from the first frame, playing.
  pub fn restart(&mut self) {
    *self = Self::new(self.format, self.bytes.clone(), self.looping, true);
  }

  /// Pause or resume; an ended clip starts over.
  pub fn toggle(&mut self, now: Instant) {
    if self.ended {
      self.restart();
    } else if self.playing {
      self.paused_left = self.until.map_or(Duration::ZERO, |t| t.saturating_duration_since(now));
      self.playing = false;
    } else {
      self.until = self.until.map(|_| now + self.paused_left);
      self.playing = true;
    }
  }

  /// The picture due at `now`, if it changed. Errors are decoding failures (the clip stops).
  pub fn poll(&mut self, now: Instant) -> Result<Option<RgbaImage>, String> {
    if !self.playing {
      return Ok(None);
    }
    self.started = true;
    if self.until.is_some_and(|t| now < t) {
      return Ok(None);
    }
    let frame = match self.next.take() {
      Some(frame) => frame,
      None => match self.frames.try_recv() {
        Ok(Ok(frame)) => frame,
        Ok(Err(e)) => {
          self.playing = false;
          self.ended = true;
          return Err(e);
        }
        Err(TryRecvError::Empty) => return Ok(None),
        Err(TryRecvError::Disconnected) => {
          self.playing = false;
          self.ended = true;
          return Ok(None);
        }
      },
    };
    // Keep the pace, but after a stall (pause, slow decode) go on from now instead of rushing.
    let start = match self.until {
      Some(t) if now.saturating_duration_since(t) < frame.duration => t,
      _ => now,
    };
    self.until = Some(start + frame.duration);
    Ok(Some(frame.bgra))
  }

  /// When to call [`Self::poll`] again: at the next frame, or shortly while the decoder catches up.
  pub fn wake_in(&self, now: Instant) -> Option<Duration> {
    let wait = self.until.map_or(Duration::ZERO, |t| t.saturating_duration_since(now));
    self.playing.then_some(wait.max(Duration::from_millis(8)))
  }
}

/// Decode on a new thread, again and again when `looping`, until the receiver is dropped.
fn spawn_decoder(format: Format, bytes: Arc<[u8]>, looping: bool) -> Receiver<Result<Frame, String>> {
  let (tx, rx) = mpsc::sync_channel(2);
  std::thread::spawn(move || {
    loop {
      match decode(format, &bytes, |frame| tx.send(Ok(frame)).is_ok()) {
        Ok(Some(0)) => {
          let _ = tx.send(Err("the clip has no frames".into()));
          return;
        }
        Ok(Some(_)) if looping => {}
        Ok(_) => return,
        Err(e) => {
          let _ = tx.send(Err(format!("{e:#}")));
          return;
        }
      }
    }
  });
  rx
}

/// A web media file: loading, loaded, or failed (not retried until the document is reopened).
enum Web {
  Loading(Receiver<Result<Vec<u8>, String>>),
  Ready(Arc<[u8]>),
  Failed,
}

/// A running clip and its picture on the GPU.
struct Running {
  page: usize,
  player: Player,
  picture: Option<Arc<RenderImage>>,
}

/// The media playing on the current slide and pre-loaded for the next one, by item index.
pub struct Playback {
  running: HashMap<usize, Running>,
  web: HashMap<String, Web>,
  waker: Waker,
  /// The latest problem, shown small in the presenter footer.
  pub note: Option<String>,
  /// Pictures replaced or dropped since the last paint, to take off the GPU.
  pub evicted: Vec<Arc<RenderImage>>,
}

impl Playback {
  pub fn new(waker: Waker) -> Self {
    Self {
      running: HashMap::new(),
      web: HashMap::new(),
      waker,
      note: None,
      evicted: Vec::new(),
    }
  }

  /// Stop everything (another document, or the same one reloaded). Loaded web media stays cached.
  pub fn clear(&mut self) {
    self.evicted.extend(self.running.drain().filter_map(|(_, r)| r.picture));
    self.web.retain(|_, w| matches!(w, Web::Ready(_)));
    self.note = None;
  }

  /// Run the media of page `current` (advancing it while `visible`) and get those of the next page
  /// ready. A clip whose slide is left stops; it starts over when the slide comes back. Returns
  /// when to call again while something plays.
  pub fn update(&mut self, items: &[Item], current: usize, visible: bool, now: Instant) -> Option<Duration> {
    let next = current + 1;
    let evicted = &mut self.evicted;
    self.running.retain(|_, r| {
      let keep = r.page == current || (r.page == next && !r.player.started());
      if !keep {
        evicted.extend(r.picture.take());
      }
      keep
    });
    self.poll_web();
    for (i, item) in items.iter().enumerate() {
      if (item.page == current || item.page == next)
        && !self.running.contains_key(&i)
        && let Some(bytes) = self.bytes(&item.source)
      {
        let player = Player::new(item.format, bytes, item.looping, item.autoplay);
        let picture = None;
        self.running.insert(
          i,
          Running {
            page: item.page,
            player,
            picture,
          },
        );
      }
    }
    if !visible {
      return None;
    }
    let mut wake: Option<Duration> = None;
    for r in self.running.values_mut().filter(|r| r.page == current) {
      match r.player.poll(now) {
        Ok(Some(bgra)) => self.evicted.extend(r.picture.replace(crate::app::render_image(bgra))),
        Ok(None) => {}
        Err(e) => {
          tracing::warn!("media on slide {}: {e}", current + 1);
          self.note = Some(format!("Media on slide {} cannot play", current + 1));
        }
      }
      if let Some(w) = r.player.wake_in(now) {
        wake = Some(wake.map_or(w, |v| v.min(w)));
      }
    }
    wake
  }

  /// The picture of item `index` to paint, while it plays (or was paused).
  pub fn picture(&self, index: usize) -> Option<Arc<RenderImage>> {
    self.running.get(&index).and_then(|r| r.picture.clone())
  }

  /// Pause the clips on `page` if any plays, else play them all. Returns whether there were any.
  pub fn toggle(&mut self, page: usize) -> bool {
    let now = Instant::now();
    let mut clips: Vec<_> = self.running.values_mut().filter(|r| r.page == page).collect();
    let any_playing = clips.iter().any(|r| r.player.playing());
    for r in &mut clips {
      if r.player.playing() == any_playing {
        r.player.toggle(now);
      }
    }
    !clips.is_empty()
  }

  /// Start the clips on `page` over, playing (the picture on screen stays until the first frame).
  /// Returns whether there were any.
  pub fn restart(&mut self, page: usize) -> bool {
    let clips = self.running.values_mut().filter(|r| r.page == page);
    clips.fold(false, |_, r| {
      r.player.restart();
      true
    })
  }

  /// The bytes of `source`, or `None` while a web file loads (starting the load).
  fn bytes(&mut self, source: &Source) -> Option<Arc<[u8]>> {
    let url = match source {
      Source::Embedded(bytes) => return Some(bytes.clone()),
      Source::Url(url) => url,
    };
    match self.web.get(url) {
      Some(Web::Ready(bytes)) => Some(bytes.clone()),
      Some(Web::Loading(_) | Web::Failed) => None,
      None => {
        let (tx, rx) = mpsc::channel();
        let (target, waker) = (url.clone(), self.waker.clone());
        std::thread::spawn(move || {
          let _ = tx.send(crate::update::fetch(&target));
          waker.wake();
        });
        self.web.insert(url.clone(), Web::Loading(rx));
        None
      }
    }
  }

  /// Take finished web loads; a failure keeps the placeholder and shows a note.
  fn poll_web(&mut self) {
    for (url, web) in &mut self.web {
      let Web::Loading(rx) = web else { continue };
      match rx.try_recv() {
        Ok(Ok(bytes)) => *web = Web::Ready(bytes.into()),
        Ok(Err(e)) => {
          tracing::warn!("loading media {url}: {e}");
          self.note = Some(format!("Could not load {url}"));
          *web = Web::Failed;
        }
        Err(TryRecvError::Empty) => {}
        Err(TryRecvError::Disconnected) => *web = Web::Failed,
      }
    }
  }
}

#[cfg(test)]
mod tests {
  use super::*;

  const PAGE: PageSize = PageSize { width: 720.0, height: 405.0 };

  fn parse(json: &str, files: &[(&str, &[u8])]) -> (Vec<Item>, Vec<String>) {
    let mut attachments: Vec<_> = files.iter().map(|(n, b)| (n.to_string(), b.to_vec())).collect();
    attachments.push((MANIFEST.to_string(), json.as_bytes().to_vec()));
    from_attachments(attachments, &[PAGE, PAGE])
  }

  #[test]
  fn reads_the_manifest_into_page_relative_items() {
    let json = r#"[
      {"kind":"file","page":2,"x_pt":72,"y_pt":40.5,"w_pt":360,"h_pt":202.5,"mime":"video/mp4",
       "file":"media/a.mp4","url":null,"autoplay":false,"loop":true,"muted":true},
      {"kind":"url","page":1,"x_pt":0,"y_pt":0,"w_pt":720,"h_pt":405,"mime":"image/gif",
       "file":null,"url":"https://example.org/a.gif","autoplay":true,"loop":false,"muted":true}]"#;
    let (items, warnings) = parse(json, &[("media/a.mp4", b"mp4")]);
    assert!(warnings.is_empty(), "{warnings:?}");
    assert_eq!(items.len(), 2);
    let a = &items[0];
    assert_eq!((a.page, a.format, a.autoplay, a.looping), (1, Format::Mp4, false, true));
    assert_eq!(
      a.area,
      Region {
        x: 0.1,
        y: 0.1,
        w: 0.5,
        h: 0.5
      }
    );
    assert_eq!(a.source, Source::Embedded(Arc::from(&b"mp4"[..])));
    assert_eq!(items[1].source, Source::Url("https://example.org/a.gif".into()));
    assert_eq!(items[1].area, Region::FULL);
  }

  #[test]
  fn skips_bad_entries_with_a_warning() {
    let json = r#"[
      {"kind":"file","page":1,"x_pt":0,"y_pt":0,"w_pt":10,"h_pt":10,"mime":"video/webm","file":"media/a.webm"},
      {"kind":"file","page":1,"x_pt":0,"y_pt":0,"w_pt":10,"h_pt":10,"mime":"text/plain","file":"media/a.gif"},
      {"kind":"file","page":9,"x_pt":0,"y_pt":0,"w_pt":10,"h_pt":10,"mime":"image/gif","file":"media/a.gif"},
      {"kind":"file","page":1,"x_pt":0,"y_pt":0,"w_pt":10,"h_pt":10,"mime":"image/gif","file":"media/missing.gif"},
      {"kind":"file","page":1,"x_pt":0,"y_pt":0,"w_pt":-1,"h_pt":10,"mime":"image/gif","file":"media/a.gif"},
      {"kind":"url","page":1,"x_pt":0,"y_pt":0,"w_pt":10,"h_pt":10,"mime":"image/gif","url":"file:///etc/passwd"},
      {"page":"one"},
      {"kind":"file","page":1,"x_pt":0,"y_pt":0,"w_pt":10,"h_pt":10,"mime":"image/gif","file":"media/a.gif"}]"#;
    let (items, warnings) = parse(json, &[("media/a.gif", b"gif"), ("media/a.gif", b"other")]);
    assert_eq!(items.len(), 1);
    assert_eq!(items[0].source, Source::Embedded(Arc::from(&b"gif"[..])), "the first of two same-named files");
    assert_eq!(warnings.len(), 8, "{warnings:#?}");
  }

  #[test]
  fn a_broken_or_missing_manifest_means_no_media() {
    assert!(from_attachments(Vec::new(), &[PAGE]).0.is_empty());
    let (items, warnings) = parse("{not json", &[]);
    assert!(items.is_empty());
    assert_eq!(warnings.len(), 1);
  }

  #[test]
  fn converts_length_prefixed_units_to_start_codes() {
    let mut out = Vec::new();
    annex_b(&[0, 0, 0, 2, 0x67, 1, 0, 0, 0, 1, 0x68, 0, 0, 0, 9, 1], 4, &mut out);
    assert_eq!(out, [0, 0, 0, 1, 0x67, 1, 0, 0, 0, 1, 0x68]);
  }

  #[test]
  fn decodes_the_example_clips() {
    let Ok(gif) = std::fs::read("examples/clips/big-lebowski-1.gif") else {
      return;
    };
    let mut frames = Vec::new();
    let count = decode(Format::Gif, &gif, |f| {
      frames.push((f.bgra.dimensions(), f.duration));
      true
    });
    assert_eq!(count.expect("decodes"), Some(frames.len()));
    assert!(frames.len() > 5 && frames.iter().all(|f| f.0 == (350, 480) && f.1 >= Duration::from_millis(20)));

    // H.264 High with B-frames: the first two seconds (stopping early).
    let Ok(mp4) = std::fs::read("examples/clips/spl.mp4") else { return };
    let mut frames = Vec::new();
    let count = decode(Format::Mp4, &mp4, |f| {
      frames.push((f.bgra.dimensions(), f.duration));
      frames.len() < 120
    });
    assert_eq!(count.expect("decodes"), None, "stopped by the caller");
    assert_eq!(frames.len(), 120);
    assert!(frames.iter().all(|f| f.0 == (1920, 1080)));
    let fps = 1.0 / frames[0].1.as_secs_f64();
    assert!((fps - 60.0).abs() < 1.0, "{fps} fps");
  }

  #[test]
  fn player_keeps_frame_times_and_pauses() {
    let Ok(bytes) = std::fs::read("examples/clips/big-lebowski-1.gif") else {
      return;
    };
    let mut first_delay = Duration::ZERO;
    decode(Format::Gif, &bytes, |f| {
      first_delay = f.duration;
      false
    })
    .expect("decodes");
    let mut player = Player::new(Format::Gif, bytes.into(), true, true);
    let t0 = Instant::now();
    let first = loop {
      if let Some(frame) = player.poll(t0).expect("plays") {
        break frame;
      }
      std::thread::sleep(Duration::from_millis(2));
    };
    assert_eq!(first.dimensions(), (350, 480));
    assert_eq!(player.wake_in(t0), Some(first_delay));
    let half = first_delay / 2;
    assert!(player.poll(t0 + half).expect("plays").is_none(), "frame not due yet");
    player.toggle(t0 + half);
    assert_eq!(player.wake_in(t0), None, "paused");
    let later = t0 + Duration::from_secs(5);
    player.toggle(later);
    assert_eq!(player.wake_in(later), Some(first_delay - half), "the rest of the frame");
    player.restart();
    assert!(player.playing() && !player.started());
    let again = loop {
      if let Some(frame) = player.poll(later).expect("plays") {
        break frame;
      }
      std::thread::sleep(Duration::from_millis(2));
    };
    assert_eq!(again, first, "the first frame again");
    assert_eq!(player.wake_in(later), Some(first_delay));
  }
}
