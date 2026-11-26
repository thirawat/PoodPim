//! Microphone capture (any device rate, mixed to mono), resampling to 16 kHz,
//! silence trimming, and WAV loading for testing.

use std::io::{Cursor, Read, Seek};
use std::path::Path;

use anyhow::{Context, Result};

use crate::features::SAMPLE_RATE;

#[cfg(feature = "desktop")]
pub use recorder::{Recorder, default_microphone, microphones};

#[cfg(feature = "desktop")]
mod recorder {
use std::sync::{Arc, Mutex};

use anyhow::{Context, Result, anyhow};
use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use cpal::{FromSample, Sample, SizedSample};

use super::resample;
use crate::features::SAMPLE_RATE;

pub struct Recorder {
    stream: Option<cpal::Stream>,
    buf: Arc<Mutex<Vec<f32>>>,
    rate: u32,
    /// The stream reported an error (the device went away…) since `start`.
    error: Arc<Mutex<Option<String>>>,
}

impl Recorder {
    pub fn new() -> Self {
        Self { stream: None, buf: Arc::new(Mutex::new(Vec::new())), rate: SAMPLE_RATE, error: Default::default() }
    }

    /// `mic`: the id of the microphone to use (`microphones`); empty or gone: the system default.
    pub fn start(&mut self, mic: &str) -> Result<()> {
        let device = input_device(mic)?;
        let supported = device.default_input_config().context("microphone config")?;
        let config = supported.config();
        self.rate = config.sample_rate;
        self.buf.lock().unwrap().clear();
        *self.error.lock().unwrap() = None;
        let ch = config.channels as usize;
        let (buf, err) = (self.buf.clone(), self.error.clone());
        let stream = match supported.sample_format() {
            cpal::SampleFormat::F32 => build::<f32>(&device, config, ch, buf, err)?,
            cpal::SampleFormat::I16 => build::<i16>(&device, config, ch, buf, err)?,
            cpal::SampleFormat::I32 => build::<i32>(&device, config, ch, buf, err)?,
            cpal::SampleFormat::U16 => build::<u16>(&device, config, ch, buf, err)?,
            f => return Err(anyhow!("unsupported microphone sample format {f:?}")),
        };
        stream.play()?;
        self.stream = Some(stream);
        Ok(())
    }

    /// Opens the default microphone briefly on a background thread and closes it again. The
    /// first time a process opens it can take seconds (2.7 s measured on a Mac), long enough
    /// to lose the first words or a whole short phrase; afterwards it takes tens of ms.
    pub fn warm_up(mic: String) {
        std::thread::spawn(move || {
            let t0 = std::time::Instant::now();
            let r = (|| -> Result<()> {
                let device = input_device(&mic)?;
                let supported = device.default_input_config().context("microphone config")?;
                let stream = match supported.sample_format() {
                    cpal::SampleFormat::F32 => build::<f32>(&device, supported.config(), 1, Arc::new(Mutex::new(Vec::new())), Default::default())?,
                    cpal::SampleFormat::I16 => build::<i16>(&device, supported.config(), 1, Arc::new(Mutex::new(Vec::new())), Default::default())?,
                    cpal::SampleFormat::I32 => build::<i32>(&device, supported.config(), 1, Arc::new(Mutex::new(Vec::new())), Default::default())?,
                    cpal::SampleFormat::U16 => build::<u16>(&device, supported.config(), 1, Arc::new(Mutex::new(Vec::new())), Default::default())?,
                    f => return Err(anyhow!("unsupported microphone sample format {f:?}")),
                };
                stream.play()?;
                std::thread::sleep(std::time::Duration::from_millis(200));
                Ok(())
            })();
            match r {
                Ok(()) => eprintln!("microphone warmed up in {} ms", t0.elapsed().as_millis()),
                Err(e) => eprintln!("microphone warm-up: {e:#}"),
            }
        });
    }

    /// What the stream reported going wrong since `start`, if anything.
    pub fn error(&self) -> Option<String> {
        self.error.lock().unwrap().clone()
    }

    /// The microphone's sample rate (the buffer is mono at this rate).
    pub fn rate(&self) -> u32 {
        self.rate
    }

    /// Runs `f` on the recording so far (mono, at `rate()`), without stopping it.
    pub fn with_buffer<R>(&self, f: impl FnOnce(&[f32]) -> R) -> R {
        f(&self.buf.lock().unwrap())
    }

    /// Stops recording and returns the whole recording (mono, at `rate()`).
    pub fn stop_raw(&mut self) -> Vec<f32> {
        self.stream = None; // dropping the stream stops it
        std::mem::take(&mut *self.buf.lock().unwrap())
    }

    /// Stops recording and returns 16 kHz mono audio.
    pub fn stop(&mut self) -> Vec<f32> {
        let raw = self.stop_raw();
        resample(&raw, self.rate, SAMPLE_RATE)
    }
}

/// The microphones there are: (id, name), in the system's order.
pub fn microphones() -> Vec<(String, String)> {
    let Ok(devices) = cpal::default_host().input_devices() else { return Vec::new() };
    // (Not `Device`'s Display: it fails, and to_string() panics, for a device that went away
    // while listed. Such a one is left out.)
    devices.filter_map(|d| Some((d.id().ok()?.to_string(), d.description().ok()?.name().to_string()))).collect()
}

/// The name of the system's default microphone.
pub fn default_microphone() -> Option<String> {
    Some(cpal::default_host().default_input_device()?.description().ok()?.name().to_string())
}

/// The microphone `id` (from `microphones`), or the system default when `id` is empty or that
/// microphone isn't there now (unplugged headset).
fn input_device(id: &str) -> Result<cpal::Device> {
    let host = cpal::default_host();
    if !id.is_empty() {
        match id.parse::<cpal::DeviceId>().ok().and_then(|id| host.device_by_id(&id)) {
            Some(d) => return Ok(d),
            None => eprintln!("the chosen microphone isn't there: using the system's"),
        }
    }
    host.default_input_device().ok_or_else(|| anyhow!("no microphone found"))
}

fn build<T>(
    device: &cpal::Device,
    config: cpal::StreamConfig,
    ch: usize,
    buf: Arc<Mutex<Vec<f32>>>,
    error: Arc<Mutex<Option<String>>>,
) -> Result<cpal::Stream>
where
    T: SizedSample,
    f32: FromSample<T>,
{
    let stream = device.build_input_stream::<T, _, _>(
        config,
        move |data: &[T], _| {
            let mut b = buf.lock().unwrap();
            for frame in data.chunks(ch) {
                let s: f32 = frame.iter().map(|&v| f32::from_sample(v)).sum::<f32>() / ch as f32;
                b.push(s);
            }
        },
        move |e: cpal::Error| {
            eprintln!("audio stream error: {e}");
            // Only a stream that is gone ends the recording: an Xrun (the CPU was late once,
            // e.g. while a part is transcribed) or a DeviceChanged (rerouted, still running) don't.
            if matches!(e.kind(), cpal::ErrorKind::DeviceNotAvailable | cpal::ErrorKind::StreamInvalidated) {
                error.lock().unwrap().get_or_insert_with(|| e.to_string());
            }
        },
        None,
    )?;
    Ok(stream)
}
}

/// Windowed-sinc resampler (Hann window, 16 zero-crossings). Offline; fast enough for dictation clips.
pub fn resample(input: &[f32], from: u32, to: u32) -> Vec<f32> {
    if from == to || input.is_empty() {
        return input.to_vec();
    }
    let ratio = to as f64 / from as f64;
    let cutoff = ratio.min(1.0) * 0.95;
    let half = (16.0 / cutoff).ceil() as isize;
    let out_len = (input.len() as f64 * ratio).floor() as usize;
    let mut out = Vec::with_capacity(out_len);
    for n in 0..out_len {
        let t = n as f64 / ratio;
        let center = t.floor() as isize;
        let mut acc = 0.0f64;
        for k in (center - half + 1)..=(center + half) {
            if k < 0 || k as usize >= input.len() {
                continue;
            }
            let d = t - k as f64;
            let x = cutoff * d;
            let sinc = if x.abs() < 1e-9 { 1.0 } else { (std::f64::consts::PI * x).sin() / (std::f64::consts::PI * x) };
            let w = 0.5 + 0.5 * (std::f64::consts::PI * d / half as f64).cos();
            acc += input[k as usize] as f64 * cutoff * sinc * w;
        }
        out.push(acc as f32);
    }
    out
}

/// `resample` done with `Resampler`, whose weights are worked out once: ~8x faster (48 kHz
/// to 16 kHz on an M3: 6 s in 9 ms instead of 70, a 25 s part in 34 ms instead of 283). The
/// app's recordings go through this on the event loop, when the talk key is let go.
pub fn resample_fast(input: &[f32], from: u32, to: u32) -> Vec<f32> {
    if from == to || from == 0 || input.is_empty() {
        return input.to_vec();
    }
    let mut out = Vec::with_capacity(input.len() * to as usize / from as usize + 64);
    let mut r = Resampler::new(from, to);
    r.push(input, &mut out);
    r.finish(&mut out);
    out
}

/// The same filter as `resample`, fed a piece at a time: for files too long to hold at their
/// own rate, and too long for `resample` (it works out every weight again for each sample).
/// Here the weights are worked out once for each of the (to / gcd) positions between input
/// samples, and only the input still needed is kept.
pub struct Resampler {
    /// Output sample n sits at input n · step / phases.
    step: usize,
    phases: usize,
    half: isize,
    /// Weights for taps k = centre - half + 1 ..= centre + half, `2 · half` per phase.
    /// Empty when there would be too many: then worked out per sample, as `resample` does.
    table: Vec<f32>,
    cutoff: f64,
    /// Input kept, from absolute sample `base`; `seen` samples so far.
    buf: Vec<f32>,
    base: usize,
    seen: usize,
    /// The next output sample.
    next: usize,
}

impl Resampler {
    pub fn new(from: u32, to: u32) -> Self {
        let (mut a, mut b) = (from as usize, to as usize);
        while b != 0 {
            (a, b) = (b, a % b);
        }
        let (step, phases) = (from as usize / a, to as usize / a);
        let cutoff = (to as f64 / from as f64).min(1.0) * 0.95;
        let half = (16.0 / cutoff).ceil() as isize;
        let taps = 2 * half as usize;
        let mut r = Self { step, phases, half, table: Vec::new(), cutoff, buf: Vec::new(), base: 0, seen: 0, next: 0 };
        // Up to 16 MB of weights (44.1 kHz: 160 phases × 102 taps).
        if from != to && phases * taps <= 4 << 20 {
            r.table = (0..phases).flat_map(|p| (0..taps).map(move |i| (p, i))).map(|(p, i)| r.weight(p, i) as f32).collect();
        }
        r
    }

    /// Weight of tap `i` (k = centre - half + 1 + i) at phase `p`; `resample`'s formula.
    fn weight(&self, p: usize, i: usize) -> f64 {
        let d = p as f64 / self.phases as f64 - (i as isize - self.half + 1) as f64;
        let x = self.cutoff * d;
        let sinc = if x.abs() < 1e-9 { 1.0 } else { (std::f64::consts::PI * x).sin() / (std::f64::consts::PI * x) };
        let w = 0.5 + 0.5 * (std::f64::consts::PI * d / self.half as f64).cos();
        self.cutoff * sinc * w
    }

    /// Adds input; appends the output samples it completes to `out`.
    pub fn push(&mut self, input: &[f32], out: &mut Vec<f32>) {
        if self.phases == self.step {
            out.extend_from_slice(input);
            return;
        }
        self.buf.extend_from_slice(input);
        self.seen += input.len();
        // Ready once its last tap has arrived.
        while (self.next * self.step / self.phases) as isize + self.half < self.seen as isize {
            out.push(self.sample(self.next));
            self.next += 1;
        }
        let keep_from = ((self.next * self.step / self.phases) as isize - self.half + 1).max(0) as usize;
        if keep_from > self.base {
            self.buf.drain(..(keep_from - self.base).min(self.buf.len()));
            self.base = keep_from;
        }
    }

    /// No more input: the rest of the output, the input taken as silent past its end.
    pub fn finish(mut self, out: &mut Vec<f32>) {
        if self.phases == self.step {
            return;
        }
        let len = self.seen * self.phases / self.step;
        while self.next < len {
            out.push(self.sample(self.next));
            self.next += 1;
        }
    }

    fn sample(&self, n: usize) -> f32 {
        let (centre, p) = ((n * self.step / self.phases) as isize, n * self.step % self.phases);
        let first = centre - self.half + 1;
        let taps = 2 * self.half as usize;
        let mut acc = 0.0f32;
        for i in 0..taps {
            let k = first + i as isize;
            if k < 0 || k as usize >= self.seen {
                continue;
            }
            let x = self.buf[k as usize - self.base];
            let w = if self.table.is_empty() { self.weight(p, i) as f32 } else { self.table[p * taps + i] };
            acc += x * w;
        }
        acc
    }
}

/// A file read by `load_audio`.
#[derive(Debug)]
pub struct Loaded {
    /// 16 kHz mono.
    pub audio: Vec<f32>,
    /// Why some of the file's audio is missing from `audio`, if any is: it ended part way,
    /// some of it couldn't be decoded, or it changed part way.
    pub incomplete: Option<String>,
}

/// Reads a whole audio or video file as 16 kHz mono f32 (see `MediaReader`).
#[cfg(feature = "desktop")]
pub fn load_audio(path: &Path) -> Result<Loaded> {
    let mut reader = MediaReader::open(path)?;
    let mut audio = Vec::new();
    if let Some(s) = reader.duration_s() {
        // Up to 3 h: no doubling as it grows (a wrong length only costs a reallocation).
        let _ = audio.try_reserve_exact(((s.min(3.0 * 3600.0) + 1.0) * SAMPLE_RATE as f32) as usize);
    }
    while reader.read(&mut audio)? {}
    let incomplete = reader.finish()?;
    Ok(Loaded { audio, incomplete })
}

/// Whether the file starts like a WAV file (whatever its name).
#[cfg(feature = "desktop")]
fn is_wav(path: &Path) -> bool {
    let mut head = [0u8; 12];
    std::fs::File::open(path).and_then(|mut f| f.read_exact(&mut head)).is_ok() && &head[..4] == b"RIFF" && &head[8..] == b"WAVE"
}

/// The sample rates a file may have: anything else is a damaged header (0 Hz would never
/// finish resampling).
fn usable_rate(rate: u32) -> Result<u32> {
    if (4_000..=768_000).contains(&rate) { Ok(rate) } else { Err(anyhow::anyhow!("its sample rate ({rate} Hz) can't be right")) }
}

/// Reads an audio or video file a piece at a time, as 16 kHz mono f32: WAV, AAC / ALAC / MP3
/// audio in .m4a, .mp4, .mov or .mp3, or Opus in .webm, .mkv or .opus (a video's first audio
/// track). Not ALAC in .mov: symphonia 0.6 finds no audio track in what ffmpeg writes
/// (QuickTime's own screen recordings are AAC). Not Opus with more than 2 channels.
/// A piece at a time, so an hour-long file needn't be held whole (230 MB at 16 kHz).
#[cfg(feature = "desktop")]
pub struct MediaReader {
    path: std::path::PathBuf,
    source: Source,
    resampler: Option<(u32, Resampler)>,
    /// 16 kHz samples given out so far.
    produced: usize,
    /// How long the file says it is.
    declared_s: Option<f64>,
    /// MP3 lengths are often guessed from the first frames: only a large shortfall counts.
    rough_length: bool,
    /// Why reading stopped before the end, if it did.
    stopped: Option<String>,
    good_packets: usize,
    bad_packets: usize,
    done: bool,
    mono: Vec<f32>,
}

#[cfg(feature = "desktop")]
enum Source {
    Wav { reader: hound::WavReader<std::io::BufReader<std::fs::File>>, channels: usize, float: bool, scale: f32 },
    Media {
        format: Box<dyn symphonia::core::formats::FormatReader>,
        decoder: Box<dyn symphonia::core::codecs::audio::AudioDecoder>,
        track_id: u32,
        interleaved: Vec<f32>,
    },
}

#[cfg(feature = "desktop")]
impl MediaReader {
    pub fn open(path: &Path) -> Result<Self> {
        Self::open_source(path).with_context(|| format!("reading {}", path.display()))
    }

    fn open_source(path: &Path) -> Result<Self> {
        use anyhow::anyhow;
        use symphonia::core::formats::TrackType;
        use symphonia::core::formats::probe::Hint;
        use symphonia::core::io::MediaSourceStream;

        let new = |source, resampler, declared_s, rough_length| MediaReader {
            path: path.to_path_buf(),
            source,
            resampler,
            produced: 0,
            declared_s,
            rough_length,
            stopped: None,
            good_packets: 0,
            bad_packets: 0,
            done: false,
            mono: Vec::new(),
        };
        let ext = path.extension().and_then(|e| e.to_str()).unwrap_or("").to_ascii_lowercase();
        if ext == "wav" || is_wav(path) {
            let file = std::fs::File::open(path).context("opening it")?;
            let reader = hound::WavReader::new(std::io::BufReader::new(file)).context("not a WAV file")?;
            let spec = reader.spec();
            let rate = usable_rate(spec.sample_rate)?;
            if spec.channels == 0 || !(1..=32).contains(&spec.bits_per_sample) {
                return Err(anyhow!("its format can't be right ({} channels, {} bits)", spec.channels, spec.bits_per_sample));
            }
            let declared_s = Some(reader.duration() as f64 / rate as f64);
            let float = spec.sample_format == hound::SampleFormat::Float;
            let scale = if float { 1.0 } else { (1i64 << (spec.bits_per_sample - 1)) as f32 };
            let source = Source::Wav { reader, channels: spec.channels as usize, float, scale };
            return Ok(new(source, Some((rate, Resampler::new(rate, SAMPLE_RATE))), declared_s, false));
        }
        let file = std::fs::File::open(path).context("opening it")?;
        let mss = MediaSourceStream::new(Box::new(file), Default::default());
        let mut hint = Hint::new();
        hint.with_extension(&ext);
        let format = symphonia::default::get_probe()
            .probe(&hint, mss, Default::default(), Default::default())
            .map_err(|e| anyhow!("not an audio or video file it can read ({e})"))?;
        const NO_TRACK: &str = "no audio track it can read (AAC, ALAC, MP3 or Opus)";
        let track = format.default_track(TrackType::Audio).ok_or_else(|| anyhow!(NO_TRACK))?;
        let track_id = track.id;
        let declared_s = match (track.time_base, track.duration) {
            (Some(tb), Some(d)) => tb.calc_duration(d).map(|t| t.as_secs_f64()),
            _ => None,
        };
        let params = track.codec_params.as_ref().and_then(|p| p.audio()).ok_or_else(|| anyhow!(NO_TRACK))?;
        let rough_length = params.codec == symphonia::core::codecs::audio::well_known::CODEC_ID_MP3;
        let mut codecs = symphonia::core::codecs::registry::CodecRegistry::new();
        symphonia::default::register_enabled_codecs(&mut codecs);
        codecs.register_audio_decoder::<symphonia_adapter_libopus::OpusDecoder>();
        let decoder =
            codecs.make_audio_decoder(params, &Default::default()).map_err(|e| anyhow!("its audio can't be decoded ({e})"))?;
        let source = Source::Media { format, decoder, track_id, interleaved: Vec::new() };
        Ok(new(source, None, declared_s, rough_length))
    }

    /// How long the file says it is, if it says.
    pub fn duration_s(&self) -> Option<f32> {
        self.declared_s.map(|s| s as f32)
    }

    /// Appends the next piece (a packet, or ~0.1 s of a WAV) to `out`. False at the end.
    pub fn read(&mut self, out: &mut Vec<f32>) -> Result<bool> {
        let r = self.read_piece(out);
        r.with_context(|| format!("reading {}", self.path.display()))
    }

    fn read_piece(&mut self, out: &mut Vec<f32>) -> Result<bool> {
        use anyhow::anyhow;
        use symphonia::core::errors::Error;
        if self.done {
            return Ok(false);
        }
        let before = out.len();
        match &mut self.source {
            Source::Wav { reader, channels, float, scale } => {
                const FRAMES: usize = 1600;
                let want = FRAMES * *channels;
                let mut interleaved = Vec::with_capacity(want);
                let r: std::result::Result<(), hound::Error> = if *float {
                    reader.samples::<f32>().take(want).try_for_each(|v| v.map(|v| interleaved.push(v)))
                } else {
                    let scale = *scale;
                    reader.samples::<i32>().take(want).try_for_each(|v| v.map(|v| interleaved.push(v as f32 / scale)))
                };
                if let Err(e) = r {
                    if self.produced == 0 && interleaved.is_empty() {
                        return Err(anyhow!("{e}"));
                    }
                    self.stopped = Some(format!("the rest of the file can't be read ({e})"));
                    self.done = true;
                }
                if interleaved.len() < want {
                    self.done = true;
                }
                self.mono.clear();
                self.mono.extend(interleaved.chunks(*channels).map(|f| f.iter().sum::<f32>() / *channels as f32));
                if !self.mono.is_empty() {
                    self.good_packets += 1;
                }
                if let Some((_, r)) = &mut self.resampler {
                    r.push(&self.mono, out);
                }
            }
            Source::Media { format, decoder, track_id, interleaved } => loop {
                let packet = match format.next_packet() {
                    Ok(Some(p)) => p,
                    Ok(None) => {
                        self.done = true;
                        break;
                    }
                    Err(Error::IoError(e)) if e.kind() == std::io::ErrorKind::UnexpectedEof => {
                        self.stopped = Some("the file ends part way (cut off?)".into());
                        self.done = true;
                        break;
                    }
                    // E.g. recordings joined one after another in one .opus file.
                    Err(Error::ResetRequired) => {
                        self.stopped = Some("its audio changes part way (joined recordings?); only the first part was read".into());
                        self.done = true;
                        break;
                    }
                    Err(e) if self.good_packets > 0 => {
                        self.stopped = Some(format!("the rest of the file can't be read ({e})"));
                        self.done = true;
                        break;
                    }
                    Err(e) => return Err(anyhow!("{e}")),
                };
                if packet.track_id != *track_id {
                    continue;
                }
                let buf = match decoder.decode(&packet) {
                    Ok(b) => b,
                    Err(Error::DecodeError(_)) => {
                        self.bad_packets += 1;
                        continue;
                    }
                    Err(e) => return Err(anyhow!("{e}")),
                };
                self.good_packets += 1;
                let (rate, ch) = (buf.spec().rate(), buf.num_planes().max(1));
                buf.copy_to_vec_interleaved::<f32>(interleaved);
                self.mono.clear();
                self.mono.extend(interleaved.chunks(ch).map(|f| f.iter().sum::<f32>() / ch as f32));
                let (r0, r) = match &mut self.resampler {
                    Some(r) => r,
                    None => self.resampler.insert((rate, Resampler::new(usable_rate(rate)?, SAMPLE_RATE))),
                };
                if *r0 != rate {
                    return Err(anyhow!("its sample rate changes part way ({r0} → {rate} Hz)"));
                }
                r.push(&self.mono, out);
                break;
            },
        }
        if self.done
            && let Some((_, r)) = self.resampler.take()
        {
            r.finish(out);
        }
        self.produced += out.len() - before;
        Ok(!self.done || out.len() > before)
    }

    /// After the end: whether the file was read whole (None) or why not; an error if it gave
    /// no audio at all.
    pub fn finish(self) -> Result<Option<String>> {
        use anyhow::anyhow;
        let MediaReader { path, produced, declared_s, rough_length, mut stopped, good_packets, bad_packets, .. } = self;
        let read_s = produced as f64 / SAMPLE_RATE as f64;
        let slack = |d: f64| (d * if rough_length { 0.1 } else { 0.02 }).max(0.25);
        if let Some(declared) = declared_s.filter(|&d| stopped.is_none() && read_s < d - slack(d)) {
            stopped = Some(format!("the file ends part way (cut off?): {read_s:.1} of its {declared:.1} s"));
        }
        if good_packets == 0 || produced == 0 {
            let why = match (bad_packets, &stopped) {
                (0, None) => "its audio track is empty".to_string(),
                (0, Some(why)) => why.clone(),
                (n, _) => format!("none of its audio could be decoded ({n} damaged packet(s))"),
            };
            return Err(anyhow!("reading {}: {why}", path.display()));
        }
        let damaged = (bad_packets > 0).then(|| format!("{bad_packets} damaged packet(s) skipped"));
        Ok(match (damaged, stopped) {
            (Some(a), Some(b)) => Some(format!("{a}; {b}")),
            (a, b) => a.or(b),
        })
    }
}

/// Whether any 20 ms frame of `audio` (at `rate`) is louder than -45 dBFS, the level below
/// which `trim_silence` treats it as silence.
pub fn has_sound(audio: &[f32], rate: u32) -> bool {
    let frame = (rate as usize / 50).max(1);
    audio.chunks_exact(frame).any(|s| {
        let rms = (s.iter().map(|v| v * v).sum::<f32>() / frame as f32 + 1e-12).sqrt();
        20.0 * rms.log10() > -45.0
    })
}

/// Cuts leading/trailing silence (20 ms frames below -45 dBFS), keeping 250 ms padding.
pub fn trim_silence(audio: &[f32]) -> &[f32] {
    let frame = (SAMPLE_RATE / 50) as usize;
    let n = audio.len() / frame;
    if n == 0 {
        return audio;
    }
    let loud: Vec<usize> = (0..n)
        .filter(|&i| {
            let s = &audio[i * frame..(i + 1) * frame];
            let rms = (s.iter().map(|v| v * v).sum::<f32>() / frame as f32 + 1e-12).sqrt();
            20.0 * rms.log10() > -45.0
        })
        .collect();
    let (Some(&first), Some(&last)) = (loud.first(), loud.last()) else {
        return &audio[..0];
    };
    let pad = (SAMPLE_RATE as usize) / 4;
    let start = (first * frame).saturating_sub(pad);
    let end = ((last + 1) * frame + pad).min(audio.len());
    &audio[start..end]
}

/// Loads a WAV file as 16 kHz mono f32.
pub fn load_wav(path: &Path) -> Result<Vec<f32>> {
    let f = std::io::BufReader::new(std::fs::File::open(path).with_context(|| format!("opening {}", path.display()))?);
    read_wav(f)
}

/// Decodes WAV bytes (any rate / channels / PCM or float) to 16 kHz mono f32.
pub fn decode_wav(bytes: &[u8]) -> Result<Vec<f32>> {
    read_wav(Cursor::new(bytes))
}

fn read_wav<R: Read + Seek>(reader: R) -> Result<Vec<f32>> {
    let mut r = hound::WavReader::new(reader).context("not a WAV file")?;
    let spec = r.spec();
    usable_rate(spec.sample_rate)?;
    let ch = spec.channels.max(1) as usize;
    let samples: Vec<f32> = match spec.sample_format {
        hound::SampleFormat::Float => r.samples::<f32>().collect::<Result<_, _>>()?,
        hound::SampleFormat::Int => {
            let scale = (1i64 << (spec.bits_per_sample - 1)) as f32;
            r.samples::<i32>().map(|s| s.map(|v| v as f32 / scale)).collect::<Result<_, _>>()?
        }
    };
    let mono: Vec<f32> = samples.chunks(ch).map(|f| f.iter().sum::<f32>() / ch as f32).collect();
    Ok(resample(&mono, spec.sample_rate, SAMPLE_RATE))
}

/// Encodes 16 kHz mono audio as 16-bit PCM WAV (32 kB per second) for upload.
pub fn encode_wav(audio: &[f32]) -> Vec<u8> {
    let spec = hound::WavSpec { channels: 1, sample_rate: SAMPLE_RATE, bits_per_sample: 16, sample_format: hound::SampleFormat::Int };
    let mut buf = Cursor::new(Vec::with_capacity(44 + audio.len() * 2));
    {
        let mut w = hound::WavWriter::new(&mut buf, spec).expect("wav writer");
        for &s in audio {
            w.write_sample((s.clamp(-1.0, 1.0) * i16::MAX as f32) as i16).expect("wav write");
        }
        w.finalize().expect("wav finalize");
    }
    buf.into_inner()
}

#[cfg(test)]
mod tests {
    use super::{Resampler, resample, resample_fast};

    #[test]
    fn resample_fast_matches_resample() {
        let input: Vec<f32> = (0..30_000).map(|i| ((i * 7919 % 1000) as f32 / 500.0 - 1.0) * 0.5).collect();
        for from in [8_000, 22_050, 44_100, 48_000] {
            let (want, got) = (resample(&input, from, 16_000), resample_fast(&input, from, 16_000));
            assert_eq!(got.len(), want.len(), "{from} Hz");
            let worst = got.iter().zip(&want).map(|(a, b)| (a - b).abs()).fold(0.0, f32::max);
            assert!(worst < 1e-4, "{from} Hz: off by {worst}");
        }
        assert_eq!(resample_fast(&input, 16_000, 16_000), input);
        assert!(resample_fast(&[], 48_000, 16_000).is_empty());
        assert_eq!(resample_fast(&input[..10], 0, 16_000).len(), 10); // no rate: not resampled, no hang
    }

    fn noise(n: usize) -> Vec<f32> {
        let mut x = 12345u32;
        (0..n)
            .map(|_| {
                x = x.wrapping_mul(1664525).wrapping_add(1013904223);
                (x >> 8) as f32 / (1 << 23) as f32 - 1.0
            })
            .collect()
    }

    #[test]
    fn resampler_matches_resample_fed_in_pieces() {
        for from in [48_000, 44_100, 22_050, 11_025, 8_000, 16_000] {
            let input = noise(from as usize / 3);
            let want = resample(&input, from, 16_000);
            let (mut r, mut got) = (Resampler::new(from, 16_000), Vec::new());
            // Uneven pieces, an empty one included.
            let mut at = 0;
            for (i, n) in [1usize, 0, 777, 5, 4096, 31].iter().cycle().enumerate() {
                if at >= input.len() || i > 10_000 {
                    break;
                }
                let end = (at + n).min(input.len());
                r.push(&input[at..end], &mut got);
                at = end;
            }
            r.finish(&mut got);
            assert_eq!(got.len(), want.len(), "{from} Hz");
            let worst = got.iter().zip(&want).map(|(a, b)| (a - b).abs()).fold(0.0, f32::max);
            assert!(worst < 1e-4, "{from} Hz: off by {worst}");
        }
    }

    #[cfg(feature = "desktop")]
    #[test]
    fn reads_a_440_hz_tone_from_each_kind_of_file() {
        // 1 s of 440 Hz: AAC 44.1 kHz stereo, ALAC and MP3, video files (H.264 first, then
        // AAC at 48 kHz), and Opus in WebM (as YouTube serves it), in a VP9 WebM video and
        // in Ogg (`yt-dlp -x`). Made with ffmpeg, see tests/media/README.md.
        for name in
            ["tone.m4a", "tone-alac.m4a", "tone.mp3", "tone.mp4", "tone.mov", "tone.webm", "tone-video.webm", "tone.opus"]
        {
            let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/media").join(name);
            let loaded = super::load_audio(&path).unwrap();
            assert!(loaded.incomplete.is_none(), "{name}: {:?}", loaded.incomplete);
            let audio = loaded.audio;
            let secs = audio.len() as f32 / 16_000.0;
            // Encoders add a little silence at the start and end (no gapless trimming here).
            assert!((0.95..1.15).contains(&secs), "{name}: {secs} s");
            // Count rising zero crossings in the middle half second.
            let mid = &audio[4_000..12_000];
            let rising = mid.windows(2).filter(|w| w[0] < 0.0 && w[1] >= 0.0).count();
            assert!((215..=225).contains(&rising), "{name}: {rising} rising crossings in 0.5 s");
            let peak = mid.iter().fold(0.0f32, |m, v| m.max(v.abs()));
            assert!(peak > 0.05, "{name}: peak {peak}");
        }
    }

    #[cfg(feature = "desktop")]
    fn media(name: &str) -> std::path::PathBuf {
        std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/media").join(name)
    }

    /// A copy of `bytes` in the temp folder, named `name`.
    #[cfg(feature = "desktop")]
    fn temp_file(name: &str, bytes: &[u8]) -> std::path::PathBuf {
        let path = std::env::temp_dir().join(format!("poodpim-test-{}-{name}", std::process::id()));
        std::fs::write(&path, bytes).unwrap();
        path
    }

    #[cfg(feature = "desktop")]
    #[test]
    fn a_file_cut_off_part_way_is_never_read_as_whole() {
        // Some readers won't open a cut-off file at all (symphonia 0.6: WebM, Ogg, and MP4 /
        // M4A with the index at the end, as ffmpeg writes it): an error. The rest must say so.
        for name in ["tone.m4a", "tone-alac.m4a", "tone.mp3", "tone.mp4", "tone.mov", "tone.webm", "tone-video.webm", "tone.opus"] {
            let bytes = std::fs::read(media(name)).unwrap();
            let path = temp_file(name, &bytes[..bytes.len() * 6 / 10]);
            let result = super::load_audio(&path);
            let _ = std::fs::remove_file(&path);
            if let Ok(loaded) = result {
                assert!(loaded.incomplete.is_some(), "{name}: read as whole");
            }
        }
    }

    #[cfg(feature = "desktop")]
    #[test]
    fn a_cut_off_mp3_is_read_up_to_the_cut() {
        let bytes = std::fs::read(media("tone.mp3")).unwrap();
        let path = temp_file("cut.mp3", &bytes[..bytes.len() * 6 / 10]);
        let loaded = super::load_audio(&path).unwrap();
        let _ = std::fs::remove_file(&path);
        let secs = loaded.audio.len() as f32 / 16_000.0;
        assert!((0.3..0.9).contains(&secs), "{secs} s");
        assert!(loaded.incomplete.unwrap().contains("ends part way"));
    }

    /// A WAV file as `hound` writes it.
    #[cfg(feature = "desktop")]
    fn wav_bytes(rate: u32, channels: u16, samples: &[i16]) -> Vec<u8> {
        let spec = hound::WavSpec { channels, sample_rate: rate, bits_per_sample: 16, sample_format: hound::SampleFormat::Int };
        let mut buf = std::io::Cursor::new(Vec::new());
        let mut w = hound::WavWriter::new(&mut buf, spec).unwrap();
        for &v in samples {
            w.write_sample(v).unwrap();
        }
        w.finalize().unwrap();
        buf.into_inner()
    }

    #[cfg(feature = "desktop")]
    #[test]
    fn a_wav_with_an_impossible_rate_or_no_samples_is_an_error() {
        let mut zero_hz = wav_bytes(16_000, 1, &[1000; 16_000]);
        zero_hz[24..28].copy_from_slice(&0u32.to_le_bytes()); // the header's sample rate
        for (name, bytes) in [("zero-hz.wav", zero_hz), ("empty.wav", wav_bytes(16_000, 1, &[]))] {
            let path = temp_file(name, &bytes);
            let r = super::load_audio(&path);
            let _ = std::fs::remove_file(&path);
            assert!(r.is_err(), "{name}: {:?}", r.map(|l| l.audio.len()));
        }
    }

    #[cfg(feature = "desktop")]
    #[test]
    fn a_stereo_44_khz_wav_is_read_as_16_khz_mono() {
        // 2 s of 440 Hz in both channels.
        let frames: Vec<i16> = (0..88_200).flat_map(|i| {
            let v = (8000.0 * (i as f32 * 440.0 * std::f32::consts::TAU / 44_100.0).sin()) as i16;
            [v, v]
        }).collect();
        let path = temp_file("stereo.wav", &wav_bytes(44_100, 2, &frames));
        let loaded = super::load_audio(&path).unwrap();
        let _ = std::fs::remove_file(&path);
        assert!(loaded.incomplete.is_none());
        assert!((31_990..=32_010).contains(&loaded.audio.len()), "{}", loaded.audio.len());
        let mid = &loaded.audio[8_000..24_000];
        let rising = mid.windows(2).filter(|w| w[0] < 0.0 && w[1] >= 0.0).count();
        assert!((435..=445).contains(&rising), "{rising}");
    }

    #[cfg(feature = "desktop")]
    #[test]
    fn a_video_without_audio_is_an_error() {
        let e = super::load_audio(&media("video-only.mp4")).unwrap_err();
        assert!(format!("{e:#}").contains("no audio track"), "{e:#}");
    }

    #[cfg(feature = "desktop")]
    #[test]
    fn a_wav_file_is_read_whatever_its_name() {
        let tone: Vec<f32> = (0..16_000).map(|i| 0.3 * (i as f32 * 0.17).sin()).collect();
        let path = temp_file("recording", &super::encode_wav(&tone));
        let loaded = super::load_audio(&path).unwrap();
        let _ = std::fs::remove_file(&path);
        assert_eq!(loaded.audio.len(), 16_000);
    }

    #[cfg(feature = "desktop")]
    #[test]
    fn a_sample_rate_that_cant_be_right_is_an_error() {
        for rate in [0, 1, 3_999, 1_000_000] {
            assert!(super::usable_rate(rate).is_err(), "{rate} Hz");
        }
        for rate in [8_000, 44_100, 48_000, 192_000] {
            assert_eq!(super::usable_rate(rate).unwrap(), rate);
        }
    }

    #[cfg(feature = "desktop")]
    #[test]
    fn a_file_that_is_not_media_is_an_error() {
        let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("Cargo.toml");
        let e = super::load_audio(&path).unwrap_err();
        assert!(format!("{e:#}").contains("Cargo.toml"), "{e:#}");
    }
}
