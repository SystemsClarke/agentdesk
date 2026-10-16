//! Dictation (Ctrl+D): local speech-to-text for the window's text boxes, with two interchangeable backends behind one trait.
//!
//! * `PythonDictation`, the original: the embedded Python plugin (`python/agentdesk/dictate.py`: sounddevice and sherpa-onnx).
//! * `NativeDictation`, behind the cargo feature `native-dictation` (off by default): the microphone through `cpal` and the same
//!   speech model through the `sherpa-onnx` crate, with the same 2-second pre-roll, and no Python.
//!
//! Which one runs is decided at each Ctrl+D by [`select`]: the setting `dictation_native` or the environment variable
//! `AGENTDESK_DICTATION` (`native` / `python`) wins; otherwise Python when there is an interpreter, else native when it is
//! compiled in, else a plain error that says what to install. The pure parts (the pre-roll ring buffer, the resampler to 16 kHz
//! mono, the selection, the dispatch) have no hardware in them and are unit-tested below.
// the native backend and its helpers are only called from a build with the feature; the pure parts are tested either way
#![allow(dead_code)]

use anyhow::{anyhow, Result};
use serde_json::{json, Value};
use std::collections::VecDeque;
use std::sync::Mutex;

/// What the speech model wants: 16 kHz, mono, f32.
pub const SAMPLE_RATE: u32 = 16_000;
/// How much audio the armed microphone keeps, so Ctrl+D catches what was said just before it.
pub const PRE_ROLL_SECONDS: f32 = 2.0;

/// Where a dictation is: the shape the window already reads (`state` is idle, downloading, loading, listening or done).
#[derive(Clone, Debug, PartialEq)]
pub struct Status {
    pub state: String,
    pub text: String,
    pub error: Option<String>,
    pub progress: f64,
    /// The microphone in use, when known (start reports it).
    pub device: Option<String>,
}

impl Status {
    pub fn idle() -> Self {
        Status { state: "idle".into(), text: String::new(), error: None, progress: 0.0, device: None }
    }

    pub fn failed(error: impl Into<String>) -> Self {
        Status { state: "done".into(), text: String::new(), error: Some(error.into()), progress: 0.0, device: None }
    }

    pub fn from_value(v: &Value) -> Self {
        Status {
            state: v["state"].as_str().unwrap_or("idle").to_string(),
            text: v["text"].as_str().unwrap_or("").to_string(),
            error: v["error"].as_str().map(String::from),
            progress: v["progress"].as_f64().unwrap_or(0.0),
            device: v["device"].as_str().map(String::from),
        }
    }

    pub fn to_value(&self) -> Value {
        let mut v = json!({"state": self.state, "text": self.text, "error": self.error, "progress": self.progress});
        if let Some(d) = &self.device {
            v["device"] = json!(d);
        }
        v
    }
}

/// One way of dictating. `start` begins listening, `poll` returns the words so far, `stop` ends it (the final words arrive in a
/// later `poll`, whose state is then `done`). `arm` / `disarm` switch the pre-roll on and off while a text box has focus.
pub trait Dictation: Sync {
    fn start(&self) -> Result<Status>;
    fn poll(&self) -> Result<Status>;
    fn stop(&self) -> Result<Status>;
    fn arm(&self) -> Result<Status> {
        self.poll()
    }
    fn disarm(&self) -> Result<Status> {
        self.poll()
    }
    /// The name of the microphone that would be used, if it can be found.
    fn device(&self) -> Option<String>;
}

/// The embedded Python plugin.
pub struct PythonDictation;

impl PythonDictation {
    fn call(action: &str) -> Result<Status> {
        Ok(Status::from_value(&crate::pyhost::call(&format!("dictate.{action}"), json!({}))?))
    }
}

impl Dictation for PythonDictation {
    fn start(&self) -> Result<Status> {
        Self::call("start")
    }
    fn poll(&self) -> Result<Status> {
        Self::call("poll")
    }
    fn stop(&self) -> Result<Status> {
        Self::call("stop")
    }
    fn arm(&self) -> Result<Status> {
        Self::call("arm")
    }
    fn disarm(&self) -> Result<Status> {
        Self::call("disarm")
    }
    fn device(&self) -> Option<String> {
        None // the plugin reports it in start's reply
    }
}

// ---- selection

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Backend {
    Python,
    Native,
}

/// What the user asked for.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Preference {
    Auto,
    Python,
    Native,
}

/// The environment variable wins over the setting `dictation_native` (true = native). Anything else is automatic.
pub fn preference(env: Option<&str>, setting: &Value) -> Preference {
    match env.map(|e| e.trim().to_ascii_lowercase()).as_deref() {
        Some("native") => Preference::Native,
        Some("python") => Preference::Python,
        _ if setting.as_bool() == Some(true) => Preference::Native,
        _ => Preference::Auto,
    }
}

/// Which backend runs, or why none can. Pure: the facts (is there a Python, is the native code compiled in) are arguments.
pub fn select(pref: Preference, python_available: bool, native_compiled: bool) -> Result<Backend, String> {
    match pref {
        Preference::Native if native_compiled => Ok(Backend::Native),
        Preference::Native => {
            Err("Native dictation was asked for (dictation_native / AGENTDESK_DICTATION=native), but this build does not include it: build with --features native-dictation."
                .into())
        }
        Preference::Python if python_available => Ok(Backend::Python),
        Preference::Python => Err("Dictation was set to Python, but no Python 3 interpreter was found (set AGENTDESK_PYTHON_BIN).".into()),
        Preference::Auto if python_available => Ok(Backend::Python),
        Preference::Auto if native_compiled => Ok(Backend::Native),
        Preference::Auto => Err("Dictation needs Python 3 with the packages in python/requirements.txt, or a build with --features native-dictation.".into()),
    }
}

fn choose() -> Result<Backend, String> {
    let pref = preference(std::env::var("AGENTDESK_DICTATION").ok().as_deref(), &crate::slack::setting("dictation_native"));
    // asking for the interpreter runs `python --version`: only when its answer could matter
    let python = pref != Preference::Native && crate::pyhost::interpreter().is_some();
    select(pref, python, cfg!(feature = "native-dictation"))
}

fn backend(b: Backend) -> &'static dyn Dictation {
    match b {
        Backend::Python => &PythonDictation,
        #[cfg(feature = "native-dictation")]
        Backend::Native => &native::NativeDictation,
        #[cfg(not(feature = "native-dictation"))]
        Backend::Native => &PythonDictation, // unreachable: select() never offers native when it is not compiled in
    }
}

/// The backend a dictation was started on: poll and stop go to the one that holds the microphone.
static ACTIVE: Mutex<Option<Backend>> = Mutex::new(None);

/// Runs one action on a backend. Separate from the choice so a test can pass a fake.
pub fn dispatch(d: &dyn Dictation, action: &str) -> Result<Status> {
    match action {
        "start" => d.start(),
        "poll" => d.poll(),
        "stop" => d.stop(),
        "arm" => d.arm(),
        "disarm" => d.disarm(),
        _ => Err(anyhow!("action is start, poll, stop, arm or disarm")),
    }
}

/// The entry point for the window and the UI API: `start`, `poll`, `stop`, `arm` or `disarm`, answered in the plugin's JSON shape
/// (state, text, error, progress, and the device on start). An `Err` is a failure to even begin (no backend, no plugin).
pub fn call(action: &str) -> Result<Value> {
    let mut active = ACTIVE.lock().unwrap_or_else(|e| e.into_inner());
    let b = match (action, *active) {
        ("start" | "arm", _) => {
            let b = choose().map_err(|e| anyhow!(e))?;
            *active = Some(b);
            b
        }
        (_, Some(b)) => b,
        (_, None) => return Ok(Status::idle().to_value()), // nothing was started: nothing to poll or stop
    };
    drop(active);
    Ok(dispatch(backend(b), action)?.to_value())
}

// ---- the pure audio parts

/// The last N samples, in order. Older audio falls off the front, so the microphone can stay armed without growing.
pub struct PreRoll {
    buf: VecDeque<f32>,
    cap: usize,
}

impl PreRoll {
    pub fn new(seconds: f32, sample_rate: u32) -> Self {
        let cap = ((seconds * sample_rate as f32) as usize).max(1);
        PreRoll { buf: VecDeque::with_capacity(cap), cap }
    }

    pub fn push(&mut self, samples: &[f32]) {
        // only the last `cap` samples of a very long chunk can survive
        let keep = &samples[samples.len().saturating_sub(self.cap)..];
        let overflow = (self.buf.len() + keep.len()).saturating_sub(self.cap);
        self.buf.drain(..overflow);
        self.buf.extend(keep);
    }

    pub fn len(&self) -> usize {
        self.buf.len()
    }

    pub fn clear(&mut self) {
        self.buf.clear();
    }

    /// Everything held, oldest first, leaving the buffer empty.
    pub fn take(&mut self) -> Vec<f32> {
        self.buf.drain(..).collect()
    }
}

/// Interleaved device samples to 16 kHz mono f32, one call per callback buffer, with state carried between calls so the
/// buffer boundaries make no difference. Channels are averaged; the rate is converted by averaging the input over each output
/// sample's span (a box filter: cheap, and it keeps 44.1 / 48 kHz audio from aliasing into the model's band).
pub struct Resampler {
    channels: usize,
    ratio: f64,
    acc: f64,
    acc_w: f64,
}

impl Resampler {
    pub fn new(in_rate: u32, channels: u16) -> Self {
        Resampler { channels: channels.max(1) as usize, ratio: in_rate.max(1) as f64 / SAMPLE_RATE as f64, acc: 0.0, acc_w: 0.0 }
    }

    pub fn process(&mut self, interleaved: &[f32]) -> Vec<f32> {
        let mut out = Vec::with_capacity((interleaved.len() as f64 / self.channels as f64 / self.ratio) as usize + 1);
        for frame in interleaved.chunks_exact(self.channels) {
            let x = frame.iter().sum::<f32>() as f64 / self.channels as f64;
            let mut w = 1.0;
            while w > 1e-9 {
                let take = (self.ratio - self.acc_w).min(w);
                self.acc += x * take;
                self.acc_w += take;
                w -= take;
                if self.acc_w >= self.ratio - 1e-9 {
                    out.push((self.acc / self.ratio) as f32);
                    self.acc = 0.0;
                    self.acc_w = 0.0;
                }
            }
        }
        out
    }
}

/// The armed microphone's hand-off: audio goes to the pre-roll until a dictation attaches, then (pre-roll first) to the
/// dictation until it detaches. Nothing is written anywhere.
pub struct Tap {
    pre: PreRoll,
    sink: Option<std::sync::mpsc::Sender<Vec<f32>>>,
}

impl Tap {
    pub fn new(seconds: f32) -> Self {
        Tap { pre: PreRoll::new(seconds, SAMPLE_RATE), sink: None }
    }

    pub fn feed(&mut self, samples: Vec<f32>) {
        if samples.is_empty() {
            return;
        }
        match &self.sink {
            Some(tx) => {
                if tx.send(samples).is_err() {
                    self.sink = None; // the dictation went away without detaching
                }
            }
            None => self.pre.push(&samples),
        }
    }

    /// The buffered seconds arrive first, then everything fed from now on.
    pub fn attach(&mut self, tx: std::sync::mpsc::Sender<Vec<f32>>) {
        let held = self.pre.take();
        if !held.is_empty() {
            let _ = tx.send(held);
        }
        self.sink = Some(tx);
    }

    /// What was just dictated must not be dictated again by the next press, so the pre-roll starts empty.
    pub fn detach(&mut self) {
        self.sink = None;
        self.pre.clear();
    }

    pub fn attached(&self) -> bool {
        self.sink.is_some()
    }

    pub fn clear(&mut self) {
        self.pre.clear();
    }
}

// ---- the native backend

/// The input devices cpal can see (names only, the default first); empty when this build has no native dictation.
pub fn input_devices() -> Vec<String> {
    #[cfg(feature = "native-dictation")]
    return native::input_devices();
    #[cfg(not(feature = "native-dictation"))]
    Vec::new()
}

#[cfg(feature = "native-dictation")]
mod native {
    use super::*;
    use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
    use cpal::{FromSample, Sample, SampleFormat};
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::mpsc;
    use std::sync::Arc;
    use std::time::Duration;

    /// The same model folder the Python side fetches (and fills): encoder / decoder / joiner `.int8.onnx` and `tokens.txt`.
    pub fn model_dir() -> PathBuf {
        crate::paths::data_dir().join("models").join("nemotron-speech-streaming-en-0.6b-int8")
    }

    /// Names of the input devices, the default one first.
    pub fn input_devices() -> Vec<String> {
        let host = cpal::default_host();
        let default = host.default_input_device().and_then(|d| d.description().ok()).map(|d| d.to_string());
        let mut names: Vec<String> = host.input_devices().map(|it| it.filter_map(|d| d.description().ok()).map(|d| d.to_string()).collect()).unwrap_or_default();
        if let Some(d) = default {
            names.retain(|n| *n != d);
            names.insert(0, d);
        }
        names
    }

    static TAP: Mutex<Option<Tap>> = Mutex::new(None);
    static MIC: Mutex<Option<Mic>> = Mutex::new(None);
    static WANT: AtomicBool = AtomicBool::new(false);
    static RECOGNIZER: Mutex<Option<Arc<sherpa_onnx::OnlineRecognizer>>> = Mutex::new(None);
    static JOB: Mutex<Option<Job>> = Mutex::new(None);

    fn lock<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
        m.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn with_tap<R>(f: impl FnOnce(&mut Tap) -> R) -> R {
        f(lock(&TAP).get_or_insert_with(|| Tap::new(PRE_ROLL_SECONDS)))
    }

    /// An open microphone. The cpal stream is not `Send`, so a thread owns it and this handle just tells it to stop.
    struct Mic {
        stop: mpsc::Sender<()>,
        name: String,
    }

    fn convert<T: Sample>(data: &[T], rs: &mut Resampler) -> Vec<f32>
    where
        f32: FromSample<T>,
    {
        let f: Vec<f32> = data.iter().map(|&s| f32::from_sample(s)).collect();
        rs.process(&f)
    }

    fn open_mic() -> Result<Mic> {
        let (ready_tx, ready_rx) = mpsc::channel::<Result<String, String>>();
        let (stop_tx, stop_rx) = mpsc::channel::<()>();
        std::thread::Builder::new().name("agentdesk-dictate-mic".into()).spawn(move || {
            let opened = (|| -> Result<(cpal::Stream, String), String> {
                let device = cpal::default_host().default_input_device().ok_or("no microphone found: plug one in or check the system's privacy settings for the microphone")?;
                let name = device.description().map(|d| d.to_string()).unwrap_or_else(|_| "the default microphone".into());
                let cfg = device.default_input_config().map_err(|e| format!("the microphone {name} has no usable input format: {e}"))?;
                let mut rs = Resampler::new(cfg.sample_rate(), cfg.channels());
                let err_fn = |e: cpal::Error| eprintln!("agentdesk dictation: microphone stream: {e}");
                let stream = match cfg.sample_format() {
                    SampleFormat::F32 => device.build_input_stream(cfg.into(), move |d: &[f32], _: &_| feed(convert(d, &mut rs)), err_fn, None),
                    SampleFormat::I16 => device.build_input_stream(cfg.into(), move |d: &[i16], _: &_| feed(convert(d, &mut rs)), err_fn, None),
                    SampleFormat::I32 => device.build_input_stream(cfg.into(), move |d: &[i32], _: &_| feed(convert(d, &mut rs)), err_fn, None),
                    SampleFormat::U16 => device.build_input_stream(cfg.into(), move |d: &[u16], _: &_| feed(convert(d, &mut rs)), err_fn, None),
                    other => return Err(format!("the microphone {name} uses the sample format {other}, which dictation does not read")),
                }
                .map_err(|e| format!("could not open the microphone {name}: {e} (input devices seen: {})", input_devices().join(", ")))?;
                stream.play().map_err(|e| format!("could not start the microphone {name}: {e}"))?;
                Ok((stream, name))
            })();
            match opened {
                Ok((stream, name)) => {
                    let _ = ready_tx.send(Ok(name));
                    let _ = stop_rx.recv(); // returns when the handle is dropped or told to stop
                    drop(stream);
                }
                Err(e) => {
                    let _ = ready_tx.send(Err(e));
                }
            }
        })?;
        match ready_rx.recv_timeout(Duration::from_secs(10)) {
            Ok(Ok(name)) => Ok(Mic { stop: stop_tx, name }),
            Ok(Err(e)) => Err(anyhow!(e)),
            Err(_) => Err(anyhow!("the microphone did not open within 10 seconds")),
        }
    }

    fn feed(samples: Vec<f32>) {
        // try_lock: never make the audio thread wait; a dropped 10 ms buffer is better than a glitch in the device
        if let Ok(mut g) = TAP.try_lock() {
            g.get_or_insert_with(|| Tap::new(PRE_ROLL_SECONDS)).feed(samples);
        }
    }

    fn ensure_mic() -> Result<String> {
        let mut mic = lock(&MIC);
        if mic.is_none() {
            *mic = Some(open_mic()?);
        }
        Ok(mic.as_ref().map(|m| m.name.clone()).unwrap_or_default())
    }

    fn close_mic() {
        if let Some(m) = lock(&MIC).take() {
            let _ = m.stop.send(());
        }
        with_tap(|t| t.clear());
    }

    /// Armed, or not, and no dictation using it: let the device go.
    fn release_if_idle() {
        if !WANT.load(Ordering::SeqCst) && !with_tap(|t| t.attached()) {
            close_mic();
        }
    }

    fn recognizer() -> Result<Arc<sherpa_onnx::OnlineRecognizer>> {
        let mut slot = lock(&RECOGNIZER);
        if let Some(r) = slot.as_ref() {
            return Ok(r.clone());
        }
        let dir = model_dir();
        let path = |n: &str| dir.join(n);
        for n in ["tokens.txt", "encoder.int8.onnx", "decoder.int8.onnx", "joiner.int8.onnx"] {
            if !path(n).is_file() {
                return Err(anyhow!(
                    "the speech model is not in {} (missing {n}); native dictation does not download it: run a Python dictation once, or put the Nemotron int8 files there",
                    dir.display()
                ));
            }
        }
        let s = |n: &str| Some(path(n).to_string_lossy().into_owned());
        let mut cfg = sherpa_onnx::OnlineRecognizerConfig::default();
        cfg.feat_config.sample_rate = SAMPLE_RATE as i32;
        cfg.feat_config.feature_dim = 128;
        cfg.model_config.transducer.encoder = s("encoder.int8.onnx");
        cfg.model_config.transducer.decoder = s("decoder.int8.onnx");
        cfg.model_config.transducer.joiner = s("joiner.int8.onnx");
        cfg.model_config.tokens = s("tokens.txt");
        cfg.model_config.num_threads = 2;
        cfg.model_config.model_type = Some("nemotron".into());
        cfg.decoding_method = Some("greedy_search".into());
        let r = Arc::new(sherpa_onnx::OnlineRecognizer::create(&cfg).ok_or_else(|| anyhow!("sherpa-onnx could not load the speech model from {}", dir.display()))?);
        *slot = Some(r.clone());
        Ok(r)
    }

    /// One dictation: a thread that feeds the mic's audio to the recognizer and keeps the words so far where `poll` can see them.
    struct Job {
        status: Arc<Mutex<Status>>,
        stop: Arc<AtomicBool>,
    }

    fn run_job(status: Arc<Mutex<Status>>, stop: Arc<AtomicBool>, audio: mpsc::Receiver<Vec<f32>>) {
        let set = |f: &dyn Fn(&mut Status)| f(&mut lock(&status));
        let result = (|| -> Result<()> {
            // the mic is already attached, so what is said while the model loads waits in the channel
            let rec = recognizer()?;
            let stream = rec.create_stream();
            let text = |rec: &sherpa_onnx::OnlineRecognizer, st: &sherpa_onnx::OnlineStream| rec.get_result(st).map(|r| r.text.trim().to_string());
            let feed = |chunk: &[f32]| {
                stream.accept_waveform(SAMPLE_RATE as i32, chunk);
                while rec.is_ready(&stream) {
                    rec.decode(&stream);
                }
                if let Some(t) = text(&rec, &stream) {
                    set(&|s| s.text = t.clone());
                }
            };
            set(&|s| s.state = "listening".into());
            while !stop.load(Ordering::SeqCst) {
                if let Ok(chunk) = audio.recv_timeout(Duration::from_millis(200)) {
                    feed(&chunk);
                }
            }
            with_tap(|t| t.detach()); // audio after this goes back to the ring buffer, not to us
            while let Ok(chunk) = audio.try_recv() {
                feed(&chunk); // the tail the device delivered after the stop
            }
            stream.input_finished();
            while rec.is_ready(&stream) {
                rec.decode(&stream);
            }
            if let Some(t) = text(&rec, &stream).filter(|t| !t.is_empty()) {
                set(&|s| s.text = t.clone());
            }
            Ok(())
        })();
        with_tap(|t| t.detach());
        release_if_idle();
        set(&|s| {
            if let Err(e) = &result {
                s.error = Some(e.to_string());
            }
            s.state = "done".into();
        });
    }

    pub struct NativeDictation;

    impl Dictation for NativeDictation {
        fn start(&self) -> Result<Status> {
            let mut job = lock(&JOB);
            if let Some(j) = job.as_ref() {
                let st = lock(&j.status).clone();
                if st.state != "done" {
                    return Ok(Status { device: self.device(), ..st }); // already listening
                }
            }
            let device = ensure_mic().inspect_err(|_| release_if_idle())?;
            let (tx, rx) = mpsc::channel();
            with_tap(|t| t.attach(tx)); // the last 2 seconds come first
            let status = Arc::new(Mutex::new(Status { state: "loading".into(), device: Some(device), ..Status::idle() }));
            let stop = Arc::new(AtomicBool::new(false));
            let (s2, p2) = (status.clone(), stop.clone());
            std::thread::Builder::new().name("agentdesk-dictate".into()).spawn(move || run_job(s2, p2, rx))?;
            let first = lock(&status).clone();
            *job = Some(Job { status, stop });
            Ok(first)
        }

        fn poll(&self) -> Result<Status> {
            Ok(lock(&JOB).as_ref().map(|j| lock(&j.status).clone()).unwrap_or_else(Status::idle))
        }

        fn stop(&self) -> Result<Status> {
            if let Some(j) = lock(&JOB).as_ref() {
                j.stop.store(true, Ordering::SeqCst);
            }
            self.poll()
        }

        fn arm(&self) -> Result<Status> {
            WANT.store(true, Ordering::SeqCst);
            ensure_mic()?;
            // warm the model off the caller's thread, so the first press does not wait for the load
            if lock(&RECOGNIZER).is_none() {
                std::thread::spawn(|| {
                    let _ = recognizer();
                });
            }
            self.poll()
        }

        fn disarm(&self) -> Result<Status> {
            WANT.store(false, Ordering::SeqCst);
            release_if_idle();
            self.poll()
        }

        fn device(&self) -> Option<String> {
            cpal::default_host().default_input_device().and_then(|d| d.description().ok()).map(|d| d.to_string())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pre_roll_keeps_only_the_last_samples_in_order() {
        let mut p = PreRoll::new(1.0, 10); // 10 samples
        p.push(&[1.0, 2.0, 3.0, 4.0, 5.0, 6.0]);
        p.push(&[7.0, 8.0, 9.0, 10.0, 11.0, 12.0]);
        assert_eq!(p.len(), 10);
        assert_eq!(p.take(), vec![3.0, 4.0, 5.0, 6.0, 7.0, 8.0, 9.0, 10.0, 11.0, 12.0]);
        assert_eq!(p.len(), 0);
        p.push(&(0..25).map(|i| i as f32).collect::<Vec<_>>()); // one chunk longer than the buffer
        assert_eq!(p.take(), (15..25).map(|i| i as f32).collect::<Vec<_>>());
        p.push(&[1.0]);
        p.clear();
        assert!(p.take().is_empty());
    }

    #[test]
    fn resampler_48k_to_16k_averages_and_counts() {
        let mut r = Resampler::new(48_000, 1);
        let out = r.process(&[1.0; 480]);
        assert_eq!(out.len(), 160);
        assert!(out.iter().all(|&x| (x - 1.0).abs() < 1e-6));
        let out = Resampler::new(48_000, 1).process(&[0.0, 0.3, 0.6, 0.3, 0.6, 0.9]);
        assert_eq!(out.len(), 2);
        assert!((out[0] - 0.3).abs() < 1e-6 && (out[1] - 0.6).abs() < 1e-6);
    }

    #[test]
    fn resampler_does_not_care_where_the_buffers_split() {
        let input: Vec<f32> = (0..4410).map(|i| ((i as f32) * 0.01).sin()).collect();
        let whole = Resampler::new(44_100, 1).process(&input);
        let mut r = Resampler::new(44_100, 1);
        let mut parts = Vec::new();
        for c in input.chunks(37) {
            parts.extend(r.process(c));
        }
        assert_eq!(whole.len(), parts.len());
        assert!(whole.len().abs_diff(1600) <= 1);
        assert!(whole.iter().zip(&parts).all(|(a, b)| (a - b).abs() < 1e-5));
    }

    #[test]
    fn resampler_averages_channels_and_passes_16k_through() {
        let mut r = Resampler::new(16_000, 2);
        assert_eq!(r.process(&[1.0, 0.0, 0.5, 0.5, -1.0, 1.0]), vec![0.5, 0.5, 0.0]);
        let mut up = Resampler::new(8_000, 1); // slower than the model wants: each sample is held for two
        assert_eq!(up.process(&[1.0, 2.0]), vec![1.0, 1.0, 2.0, 2.0]);
    }

    #[test]
    fn tap_sends_the_pre_roll_first_then_live_audio_then_forgets() {
        let mut t = Tap::new(1.0);
        t.feed(vec![1.0, 2.0]);
        t.feed(vec![3.0]);
        let (tx, rx) = std::sync::mpsc::channel();
        t.attach(tx);
        assert!(t.attached());
        t.feed(vec![4.0, 5.0]);
        assert_eq!(rx.try_recv().unwrap(), vec![1.0, 2.0, 3.0]);
        assert_eq!(rx.try_recv().unwrap(), vec![4.0, 5.0]);
        t.detach();
        assert!(!t.attached());
        t.feed(vec![6.0]); // goes to the ring buffer, not to the finished dictation
        assert!(rx.try_recv().is_err());
        let (tx2, rx2) = std::sync::mpsc::channel();
        t.detach(); // the next press must not hear what was just dictated
        t.attach(tx2);
        assert!(rx2.try_recv().is_err());
    }

    #[test]
    fn tap_survives_a_dictation_that_vanished() {
        let mut t = Tap::new(1.0);
        let (tx, rx) = std::sync::mpsc::channel();
        t.attach(tx);
        drop(rx);
        t.feed(vec![1.0]);
        assert!(!t.attached());
        t.feed(vec![2.0]);
        let (tx, rx) = std::sync::mpsc::channel();
        t.attach(tx);
        assert_eq!(rx.try_recv().unwrap(), vec![2.0]);
    }

    #[test]
    fn preference_reads_the_environment_before_the_setting() {
        assert_eq!(preference(None, &Value::Null), Preference::Auto);
        assert_eq!(preference(None, &json!(true)), Preference::Native);
        assert_eq!(preference(None, &json!(false)), Preference::Auto);
        assert_eq!(preference(Some("native"), &json!(false)), Preference::Native);
        assert_eq!(preference(Some(" Python "), &json!(true)), Preference::Python);
        assert_eq!(preference(Some("whatever"), &json!(true)), Preference::Native);
        assert_eq!(preference(Some(""), &Value::Null), Preference::Auto);
    }

    #[test]
    fn selection_prefers_python_then_native_then_explains() {
        assert_eq!(select(Preference::Auto, true, true), Ok(Backend::Python));
        assert_eq!(select(Preference::Auto, true, false), Ok(Backend::Python));
        assert_eq!(select(Preference::Auto, false, true), Ok(Backend::Native));
        assert!(select(Preference::Auto, false, false).unwrap_err().contains("native-dictation"));
        assert_eq!(select(Preference::Native, true, true), Ok(Backend::Native));
        assert!(select(Preference::Native, true, false).unwrap_err().contains("does not include it"));
        assert_eq!(select(Preference::Python, true, true), Ok(Backend::Python));
        assert!(select(Preference::Python, false, true).unwrap_err().contains("no Python 3"));
    }

    #[test]
    fn status_round_trips_the_plugins_json() {
        let v = json!({"state": "listening", "text": "hello", "error": null, "progress": 0.5, "device": "USB Mic"});
        let s = Status::from_value(&v);
        assert_eq!((s.state.as_str(), s.text.as_str(), s.device.as_deref()), ("listening", "hello", Some("USB Mic")));
        assert_eq!(s.to_value(), v);
        let e = Status::failed("no microphone").to_value();
        assert_eq!((e["state"].as_str(), e["error"].as_str()), (Some("done"), Some("no microphone")));
        assert!(e.get("device").is_none());
        assert_eq!(Status::from_value(&json!({})), Status::idle());
    }

    struct Fake(Mutex<Vec<&'static str>>);

    impl Fake {
        fn note(&self, what: &'static str) -> Result<Status> {
            self.0.lock().unwrap().push(what);
            Ok(Status { text: what.into(), ..Status::idle() })
        }
    }

    impl Dictation for Fake {
        fn start(&self) -> Result<Status> {
            self.note("start")
        }
        fn poll(&self) -> Result<Status> {
            self.note("poll")
        }
        fn stop(&self) -> Result<Status> {
            self.note("stop")
        }
        fn device(&self) -> Option<String> {
            Some("Fake mic".into())
        }
    }

    #[test]
    fn dispatch_routes_each_action_and_rejects_unknown_ones() {
        let f = Fake(Mutex::new(Vec::new()));
        for a in ["start", "poll", "stop", "arm", "disarm"] {
            dispatch(&f, a).unwrap();
        }
        // arm and disarm default to a poll
        assert_eq!(*f.0.lock().unwrap(), vec!["start", "poll", "stop", "poll", "poll"]);
        assert!(dispatch(&f, "dance").is_err());
        assert_eq!(f.device().as_deref(), Some("Fake mic"));
    }

    #[test]
    fn without_the_feature_there_are_no_native_devices() {
        if !cfg!(feature = "native-dictation") {
            assert!(input_devices().is_empty());
        }
    }
}
