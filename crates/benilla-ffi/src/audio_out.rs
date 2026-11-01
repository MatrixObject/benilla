//! kira's mix into the machine: the [`Backend`] over cpal 0.17 the audio thread runs its
//! `AudioManager` on. kira's own cpal backend wants cpal 0.18, which cannot share this
//! workspace's lock with the 0.17 everything else here uses, so the callback lives here: one
//! render per cpal cycle and nothing else.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use cpal::{FromSample, SizedSample};
use kira::backend::{Backend, Renderer};

/// The most frames one callback asks kira for; a bigger host buffer is served in several renders
/// rather than allocating on the audio thread.
const MAX_FRAMES: usize = 4096;

/// Everything the cpal callback owns: the renderer and its interleaved stereo scratch.
struct Sink {
    renderer: Renderer,
    channels: usize,
    scratch: Vec<f32>,
}

impl Sink {
    /// One cpal cycle. Realtime thread: no allocation, no lock, no log.
    fn run<S>(&mut self, data: &mut [S])
    where
        S: SizedSample + FromSample<f32>,
    {
        let cap = self.scratch.len() / self.channels;
        let frames = data.len() / self.channels;
        self.renderer.on_start_processing();
        let mut done = 0;
        while done < frames {
            let take = (frames - done).min(cap);
            let n = take * self.channels;
            self.renderer.process(&mut self.scratch[..n], self.channels as u16);
            let start = done * self.channels;
            for i in 0..n {
                data[start + i] = S::from_sample(self.scratch[i]);
            }
            done += take;
        }
    }
}

/// kira's [`Backend`] over the default output device at its native format and rate.
pub(crate) struct CpalBackend {
    device: cpal::Device,
    config: cpal::StreamConfig,
    sample_format: cpal::SampleFormat,
    sample_rate: u32,
    /// The running stream, from [`Backend::start`]; dropping `CpalBackend` stops it.
    stream: Option<cpal::Stream>,
}

impl Backend for CpalBackend {
    type Settings = ();
    type Error = String;

    fn setup(_settings: (), _internal_buffer_size: usize) -> Result<(Self, u32), String> {
        let host = cpal::default_host();
        let device = host
            .default_output_device()
            .ok_or_else(|| "no default output device".to_owned())?;
        let supported = device
            .default_output_config()
            .map_err(|e| format!("default output config: {e}"))?;
        let sample_rate = supported.sample_rate();
        let sample_format = supported.sample_format();
        let config = supported.config();
        Ok((
            Self {
                device,
                config,
                sample_format,
                sample_rate,
                stream: None,
            },
            sample_rate,
        ))
    }

    fn start(&mut self, renderer: Renderer) -> Result<(), String> {
        if self.stream.is_some() {
            return Err("the backend is already started".to_owned());
        }
        let channels = self.config.channels.max(1) as usize;
        let sink = Sink {
            renderer,
            channels,
            scratch: vec![0.0; MAX_FRAMES * channels],
        };
        let stream = match self.sample_format {
            cpal::SampleFormat::F32 => open::<f32>(&self.device, &self.config, sink)?,
            cpal::SampleFormat::I16 => open::<i16>(&self.device, &self.config, sink)?,
            cpal::SampleFormat::U8 => open::<u8>(&self.device, &self.config, sink)?,
            other => return Err(format!("unsupported output sample format {other}")),
        };
        self.stream = Some(stream);
        Ok(())
    }
}

/// Build and start the output stream for the device's sample type. The error callback reports a
/// lost device once: the mix keeps running, and an xrun is already audible.
fn open<S>(
    device: &cpal::Device,
    config: &cpal::StreamConfig,
    mut sink: Sink,
) -> Result<cpal::Stream, String>
where
    S: SizedSample + FromSample<f32>,
{
    let reported = Arc::new(AtomicBool::new(false));
    let errors = Arc::clone(&reported);
    let stream = device
        .build_output_stream::<S, _, _>(
            config,
            move |data, _info| sink.run(data),
            move |e| {
                if !errors.swap(true, Ordering::Relaxed) {
                    eprintln!("audio: output stream error: {e}");
                }
            },
            None,
        )
        .map_err(|e| format!("building the output stream: {e}"))?;
    stream.play().map_err(|e| format!("starting the output stream: {e}"))?;
    Ok(stream)
}
