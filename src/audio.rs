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

/// Reads an audio or video file as 16 kHz mono f32: WAV, or AAC / ALAC / MP3 audio in .m4a,
/// .mp4, .mov or .mp3 (a video's first audio track). Not ALAC in .mov: symphonia 0.6 finds
/// no audio track in what ffmpeg writes (QuickTime's own screen recordings are AAC).
#[cfg(feature = "desktop")]
pub fn load_audio(path: &Path) -> Result<Vec<f32>> {
    let ext = path.extension().and_then(|e| e.to_str()).unwrap_or("").to_ascii_lowercase();
    if ext == "wav" {
        return load_wav(path);
    }
    decode_media(path, &ext).with_context(|| format!("reading {}", path.display()))
}

#[cfg(feature = "desktop")]
fn decode_media(path: &Path, ext: &str) -> Result<Vec<f32>> {
    use anyhow::anyhow;
    use symphonia::core::errors::Error;
    use symphonia::core::formats::TrackType;
    use symphonia::core::formats::probe::Hint;
    use symphonia::core::io::MediaSourceStream;

    let file = std::fs::File::open(path).context("opening it")?;
    let mss = MediaSourceStream::new(Box::new(file), Default::default());
    let mut hint = Hint::new();
    hint.with_extension(ext);
    let mut format = symphonia::default::get_probe()
        .probe(&hint, mss, Default::default(), Default::default())
        .map_err(|e| anyhow!("not an audio or video file it can read ({e})"))?;
    let track = format.default_track(TrackType::Audio).ok_or_else(|| anyhow!("no audio track it can read (AAC, ALAC or MP3)"))?;
    let track_id = track.id;
    let params = track.codec_params.as_ref().and_then(|p| p.audio()).ok_or_else(|| anyhow!("no audio track it can read (AAC, ALAC or MP3)"))?;
    let mut decoder = symphonia::default::get_codecs()
        .make_audio_decoder(params, &Default::default())
        .map_err(|e| anyhow!("its audio can't be decoded ({e})"))?;

    let (mut out, mut interleaved, mut mono) = (Vec::new(), Vec::new(), Vec::new());
    let mut resampler: Option<(u32, Resampler)> = None;
    let mut bad_packets = 0usize;
    loop {
        let packet = match format.next_packet() {
            Ok(Some(p)) => p,
            // The end (a file cut off counts as ending there).
            Ok(None) | Err(Error::ResetRequired) => break,
            Err(Error::IoError(e)) if e.kind() == std::io::ErrorKind::UnexpectedEof => break,
            Err(e) => return Err(anyhow!("{e}")),
        };
        if packet.track_id != track_id {
            continue;
        }
        let buf = match decoder.decode(&packet) {
            Ok(b) => b,
            Err(Error::DecodeError(_)) => {
                bad_packets += 1;
                continue;
            }
            Err(e) => return Err(anyhow!("{e}")),
        };
        let (rate, ch) = (buf.spec().rate(), buf.num_planes().max(1));
        buf.copy_to_vec_interleaved::<f32>(&mut interleaved);
        mono.clear();
        mono.extend(interleaved.chunks(ch).map(|f| f.iter().sum::<f32>() / ch as f32));
        let (r0, r) = resampler.get_or_insert_with(|| (rate, Resampler::new(rate, SAMPLE_RATE)));
        if *r0 != rate {
            return Err(anyhow!("its sample rate changes part way ({r0} → {rate} Hz)"));
        }
        r.push(&mono, &mut out);
    }
    if let Some((_, r)) = resampler {
        r.finish(&mut out);
    }
    if bad_packets > 0 {
        eprintln!("{}: {bad_packets} damaged packet(s) skipped", path.display());
    }
    Ok(out)
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
    use super::{Resampler, resample};

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
        // 1 s of 440 Hz: AAC 44.1 kHz stereo, ALAC and MP3, and video files (H.264 first,
        // then AAC at 48 kHz). Made with ffmpeg, see tests/media/README.md.
        for name in ["tone.m4a", "tone-alac.m4a", "tone.mp3", "tone.mp4", "tone.mov"] {
            let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/media").join(name);
            let audio = super::load_audio(&path).unwrap();
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
    #[test]
    fn a_file_that_is_not_media_is_an_error() {
        let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("Cargo.toml");
        let e = super::load_audio(&path).unwrap_err();
        assert!(format!("{e:#}").contains("Cargo.toml"), "{e:#}");
    }
}
