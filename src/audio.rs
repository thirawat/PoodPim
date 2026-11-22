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
    devices.filter_map(|d| Some((d.id().ok()?.to_string(), d.to_string()))).collect()
}

/// The name of the system's default microphone.
pub fn default_microphone() -> Option<String> {
    cpal::default_host().default_input_device().map(|d| d.to_string())
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
