//! The sound of video clips (unless `muted: true` in `bella-media.typ`): the AAC track of an MP4 is
//! decoded on a background thread, converted to the output device's rate and channels, and played
//! through the default output device. The frames played so far are the clip's clock, so the
//! picture follows the sound.

use std::io::Cursor;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::{self, Receiver, SyncSender};
use std::time::Duration;

use anyhow::{Context as _, Result, anyhow};
use cpal::traits::{DeviceTrait as _, HostTrait as _, StreamTrait as _};
use cpal::{FromSample, SampleFormat, SizedSample};
use symphonia::core::errors::Error as DecodeError;
use symphonia::core::formats::TrackType;
use symphonia::core::formats::probe::Hint;
use symphonia::core::io::MediaSourceStream;

/// Decoded chunks waiting for the output device: about a quarter of a second at 48 kHz.
const QUEUE: usize = 12;

/// Whether `bytes` (an MP4) has a sound track that can be played (AAC).
pub fn has_sound(bytes: &[u8]) -> bool {
  mp4::Mp4Reader::read_header(Cursor::new(bytes), bytes.len() as u64)
    .is_ok_and(|mp4| mp4.tracks().values().any(|t| matches!(t.media_type(), Ok(mp4::MediaType::AAC))))
}

/// A clip's sound on the default output device. It starts paused; dropping it stops the sound and
/// its decoder.
pub struct Sound {
  stream: cpal::Stream,
  /// Frames the device has played.
  played: Arc<AtomicU64>,
  rate: u32,
}

impl Sound {
  /// Decode the sound of `bytes` (an MP4 with an AAC track), from the start and again at the end
  /// when `looping`.
  pub fn new(bytes: Arc<[u8]>, looping: bool) -> Result<Self> {
    let device = cpal::default_host().default_output_device().context("no sound output device")?;
    let supported = device.default_output_config()?;
    let format = supported.sample_format();
    let config: cpal::StreamConfig = supported.into();
    let (rate, channels) = (config.sample_rate, usize::from(config.channels));
    let (tx, rx) = mpsc::sync_channel(QUEUE);
    std::thread::spawn(move || decode_loop(&bytes, looping, rate, channels, &tx));
    let played = Arc::new(AtomicU64::new(0));
    let stream = match format {
      SampleFormat::F32 => output::<f32>(&device, config, rx, played.clone()),
      SampleFormat::I16 => output::<i16>(&device, config, rx, played.clone()),
      SampleFormat::U16 => output::<u16>(&device, config, rx, played.clone()),
      SampleFormat::I32 => output::<i32>(&device, config, rx, played.clone()),
      other => Err(anyhow!("unsupported sample format {other}")),
    }?;
    // Some systems start a new stream at once.
    stream.pause()?;
    Ok(Self { stream, played, rate })
  }

  pub fn play(&self) {
    if let Err(e) = self.stream.play() {
      tracing::warn!("starting the sound: {e}");
    }
  }

  pub fn pause(&self) {
    if let Err(e) = self.stream.pause() {
      tracing::warn!("pausing the sound: {e}");
    }
  }

  /// How much of the clip has been heard.
  pub fn position(&self) -> Duration {
    Duration::from_secs_f64(self.played.load(Ordering::Relaxed) as f64 / f64::from(self.rate.max(1)))
  }
}

/// An output stream that plays the chunks from `rx` and counts the frames it played. While no chunk
/// is ready it plays silence, which does not count, so the clock waits for the sound.
fn output<T>(device: &cpal::Device, config: cpal::StreamConfig, rx: Receiver<Vec<f32>>, played: Arc<AtomicU64>) -> Result<cpal::Stream>
where
  T: SizedSample + FromSample<f32>,
{
  let channels = usize::from(config.channels).max(1);
  let (mut chunk, mut at) = (Vec::new(), 0);
  let stream = device.build_output_stream(
    config,
    move |out: &mut [T], _: &cpal::OutputCallbackInfo| {
      let mut frames = 0;
      for frame in out.chunks_mut(channels) {
        if at >= chunk.len() {
          match rx.try_recv() {
            Ok(next) => (chunk, at) = (next, 0),
            Err(_) => {
              frame.fill(T::EQUILIBRIUM);
              continue;
            }
          }
        }
        for (sample, value) in frame.iter_mut().zip(&chunk[at..]) {
          *sample = T::from_sample(*value);
        }
        at += channels;
        frames += 1;
      }
      played.fetch_add(frames, Ordering::Relaxed);
    },
    |e| tracing::warn!("sound output: {e}"),
    None,
  )?;
  Ok(stream)
}

/// Decode again and again while `looping`, until the output is dropped.
fn decode_loop(bytes: &Arc<[u8]>, looping: bool, rate: u32, channels: usize, tx: &SyncSender<Vec<f32>>) {
  loop {
    match decode_sound(bytes, rate, channels, |chunk| tx.send(chunk).is_ok()) {
      Ok(true) if looping => {}
      Ok(_) => return,
      Err(e) => {
        tracing::warn!("decoding the sound: {e:#}");
        return;
      }
    }
  }
}

/// Decode the sound of `bytes` to interleaved chunks at `rate` with `channels`, handing each to
/// `emit` until it returns false. Returns whether the whole track was decoded.
pub fn decode_sound(bytes: &Arc<[u8]>, rate: u32, channels: usize, mut emit: impl FnMut(Vec<f32>) -> bool) -> Result<bool> {
  let source = MediaSourceStream::new(Box::new(Cursor::new(bytes.clone())), Default::default());
  let mut hint = Hint::new();
  hint.with_extension("mp4");
  let mut format = symphonia::default::get_probe().probe(&hint, source, Default::default(), Default::default())?;
  let (track, mut decoder) = {
    let track = format.default_track(TrackType::Audio).context("no sound track")?;
    let params = track.codec_params.as_ref().and_then(|p| p.audio()).context("no sound parameters")?;
    let decoder = symphonia::default::get_codecs().make_audio_decoder(params, &Default::default())?;
    (track.id, decoder)
  };
  let mut resampler: Option<Resampler> = None;
  let mut samples: Vec<f32> = Vec::new();
  while let Some(packet) = format.next_packet()? {
    if packet.track_id != track {
      continue;
    }
    let buffer = match decoder.decode(&packet) {
      Ok(buffer) => buffer,
      // A damaged packet is skipped, like a damaged video frame.
      Err(DecodeError::DecodeError(_)) => continue,
      Err(e) => return Err(e.into()),
    };
    let (from_rate, from_channels) = (buffer.spec().rate(), buffer.spec().channels().count());
    samples.resize(buffer.samples_interleaved(), 0.0);
    buffer.copy_to_slice_interleaved(&mut samples);
    let resampler = resampler.get_or_insert_with(|| Resampler::new(from_rate, from_channels, rate, channels));
    if !emit(resampler.process(&samples)) {
      return Ok(false);
    }
  }
  Ok(true)
}

/// Converts interleaved sound to another rate (linear interpolation) and channel count.
struct Resampler {
  /// Input frames per output frame.
  step: f64,
  /// Position of the next output frame, in input frames after `last`.
  at: f64,
  /// The last input frame of the previous chunk.
  last: Vec<f32>,
  to_channels: usize,
}

impl Resampler {
  fn new(from_rate: u32, from_channels: usize, to_rate: u32, to_channels: usize) -> Self {
    Self {
      step: f64::from(from_rate.max(1)) / f64::from(to_rate.max(1)),
      at: 0.0,
      last: vec![0.0; from_channels.max(1)],
      to_channels: to_channels.max(1),
    }
  }

  fn process(&mut self, input: &[f32]) -> Vec<f32> {
    let from = self.last.len();
    let frames = input.len() / from;
    let frame = |i: usize| if i == 0 { &self.last[..] } else { &input[(i - 1) * from..i * from] };
    let mut out = Vec::with_capacity((frames as f64 / self.step) as usize * self.to_channels + self.to_channels);
    let mut mixed = vec![0.0; from];
    while self.at < frames as f64 {
      let i = self.at as usize;
      let t = (self.at - i as f64) as f32;
      for ((m, a), b) in mixed.iter_mut().zip(frame(i)).zip(frame(i + 1)) {
        *m = a + (b - a) * t;
      }
      out.extend(map_channels(&mixed, self.to_channels));
      self.at += self.step;
    }
    self.at -= frames as f64;
    if frames > 0 {
      self.last = input[(frames - 1) * from..frames * from].to_vec();
    }
    out
  }
}

/// One frame with `to` channels: mono goes to every channel, more channels down to mono are
/// averaged, otherwise the first ones are kept and missing ones stay silent.
fn map_channels(frame: &[f32], to: usize) -> Vec<f32> {
  match (frame.len(), to) {
    (1, _) => vec![frame[0]; to],
    (n, 1) => vec![frame.iter().sum::<f32>() / n as f32],
    _ => (0..to).map(|c| frame.get(c).copied().unwrap_or(0.0)).collect(),
  }
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn resampling_keeps_the_duration_and_maps_channels() {
    // One second of mono at 24 kHz becomes one second of stereo at 48 kHz.
    let mut r = Resampler::new(24_000, 1, 48_000, 2);
    let out: usize = (0..10).map(|_| r.process(&[0.5; 2_400]).len()).sum();
    assert_eq!(out / 2, 48_000);
    // Same rate: the samples come through, one frame late.
    let mut r = Resampler::new(48_000, 2, 48_000, 2);
    assert_eq!(r.process(&[1.0, 2.0, 3.0, 4.0]), [0.0, 0.0, 1.0, 2.0]);
    assert!((map_channels(&[0.2, 0.4], 1)[0] - 0.3).abs() < 1e-6, "stereo to mono averages");
    assert_eq!(map_channels(&[0.2, 0.4], 6), [0.2, 0.4, 0.0, 0.0, 0.0, 0.0]);
  }

  #[test]
  fn finds_and_decodes_the_sound_of_the_example_clip() {
    let Ok(bytes) = std::fs::read("examples/clips/big-buck-bunny-sound.mp4") else {
      return;
    };
    assert!(has_sound(&bytes));
    let bytes: Arc<[u8]> = bytes.into();
    let mut samples = 0;
    let whole = decode_sound(&bytes, 48_000, 2, |chunk| {
      samples += chunk.len();
      true
    })
    .expect("decodes");
    assert!(whole);
    let seconds = samples as f64 / 2.0 / 48_000.0;
    assert!((seconds - 20.0).abs() < 0.2, "{seconds} s of sound");
  }
}
