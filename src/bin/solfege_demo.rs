//! `solfege-demo` — an egui front end for the stretching engine.
//!
//! It exists to make the parts of the design that are hard to describe audible
//! and visible: the time map as an editable object, the difference between the
//! modes on the same material, what a hard anchor does to the waveform, and
//! what the compiler rejects and why.
//!
//! Analysis and rendering run on worker threads. The audio callback owns its
//! buffer outright and touches nothing but an atomic cursor, so it never locks
//! and never allocates (system-design.md sec.8).

use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use eframe::egui;
use egui::{
    pos2, vec2, Align2, Color32, FontId, Pos2, CornerRadius, Rect, Sense, Stroke, Vec2,
};
use solfege::analysis::{analyze, Analysis, AnalysisSettings};
use solfege::audio::{AudioBuffer, SourceIdentity};
use solfege::document::{EditDocument, EngineMode, FormantPolicy, QualityProfile};
use solfege::mapping::{AnchorKind, TimeMap, WarpAnchor};
use solfege::metrics;
use solfege::plan::{compile, CompileOptions};
use solfege::render::render_offline;
use solfege::stream::{PlayerConfig, RtPlayer, StreamHandle};
use solfege::RenderPlan;
use solfege::{fixtures, wav};
use std::sync::atomic::Ordering;
use std::sync::mpsc::{channel, Receiver, Sender};
use std::sync::Arc;
use std::time::Instant;

fn main() -> eframe::Result<()> {
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_inner_size([1180.0, 820.0])
            .with_min_inner_size([900.0, 600.0])
            .with_title("Solfege Stretching — demo"),
        ..Default::default()
    };
    eframe::run_native(
        "solfege-demo",
        options,
        Box::new(|cc| {
            cc.egui_ctx.set_visuals(egui::Visuals::dark());
            Ok(Box::new(DemoApp::new()))
        }),
    )
}

// ---------------------------------------------------------------- palette

const BG_PANEL: Color32 = Color32::from_rgb(24, 26, 31);
const WAVE_SRC: Color32 = Color32::from_rgb(122, 176, 255);
const WAVE_OUT: Color32 = Color32::from_rgb(126, 220, 168);
const ANCHOR_USER: Color32 = Color32::from_rgb(255, 186, 92);
const ANCHOR_END: Color32 = Color32::from_rgb(120, 126, 140);
const ONSET: Color32 = Color32::from_rgb(214, 110, 140);
const PLAYHEAD: Color32 = Color32::from_rgb(255, 255, 255);
const GRID: Color32 = Color32::from_rgb(44, 47, 55);
const TEXT_DIM: Color32 = Color32::from_rgb(150, 155, 168);
const ERR: Color32 = Color32::from_rgb(255, 122, 122);
const OK: Color32 = Color32::from_rgb(126, 220, 168);

// ---------------------------------------------------------------- workers

enum Msg {
    Analysed(Box<Analysis>),
    Rendered {
        audio: Box<AudioBuffer>,
        mode: EngineMode,
        auto_reason: Option<String>,
        expected: u64,
        produced: u64,
        ms: f64,
    },
    Failed(String),
}

// ---------------------------------------------------------------- playback

/// Bridges the engine's clock to the device's.
///
/// The library renders at the file's sample rate with the plan's channel count;
/// a device may want neither. Rate conversion and channel spreading are device
/// concerns, so they live here and not in the DSP: the numbers in the docs are
/// about the engine, and nothing in this bridge should be read as part of them.
/// It allocates once, at stream construction.
struct DeviceBridge {
    player: RtPlayer,
    plan_channels: usize,
    device_channels: usize,
    /// device frames -> engine frames.
    ratio: f64,
    acc: f64,
    /// Engine-rate scratch, plan channels, interleaved.
    scratch: Vec<f32>,
    /// Last engine frame, kept so interpolation can span callbacks.
    last: Vec<f32>,
    have_last: bool,
    max_device_frames: usize,
}

impl DeviceBridge {
    fn new(
        player: RtPlayer,
        plan_channels: usize,
        device_channels: usize,
        engine_rate: u32,
        device_rate: u32,
        max_device_frames: usize,
    ) -> Self {
        let ratio = engine_rate as f64 / device_rate as f64;
        let max_engine = (max_device_frames as f64 * ratio).ceil() as usize + 4;
        Self {
            player,
            plan_channels,
            device_channels,
            ratio,
            acc: 0.0,
            scratch: vec![0.0; max_engine * plan_channels],
            last: vec![0.0; plan_channels],
            have_last: false,
            max_device_frames,
        }
    }

    fn fill(&mut self, out: &mut [f32]) {
        let dch = self.device_channels;
        let pch = self.plan_channels;
        let frames = (out.len() / dch).min(self.max_device_frames);

        if (self.ratio - 1.0).abs() < 1e-9 {
            // Same rate: pull straight through, only spread the channels.
            self.player.fill(&mut self.scratch, frames);
            for f in 0..frames {
                for c in 0..dch {
                    out[f * dch + c] = self.scratch[f * pch + c.min(pch - 1)];
                }
            }
            return;
        }

        // How many engine frames does this block of device frames span?
        let need = ((frames as f64) * self.ratio + self.acc).ceil() as usize + 1;
        self.player.fill(&mut self.scratch, need);
        for f in 0..frames {
            let pos = self.acc;
            let i = pos.floor() as usize;
            let t = (pos - i as f64) as f32;
            for c in 0..dch {
                let sc = c.min(pch - 1);
                let a = if i == 0 && self.have_last {
                    self.last[sc]
                } else if i == 0 {
                    self.scratch[sc]
                } else {
                    self.scratch[(i - 1) * pch + sc]
                };
                let b = self.scratch[i.min(need - 1) * pch + sc];
                out[f * dch + c] = a + (b - a) * t;
            }
            self.acc += self.ratio;
        }
        let consumed = self.acc.floor() as usize;
        if consumed > 0 && consumed <= need {
            for c in 0..pch {
                self.last[c] = self.scratch[(consumed - 1) * pch + c];
            }
            self.have_last = true;
        }
        self.acc -= consumed as f64;
    }
}

/// A live stream: the cpal stream, the control handle for the worker, and what
/// it is playing.
struct Session {
    _stream: cpal::Stream,
    handle: StreamHandle,
    kind: Playing,
    source_frames: u64,
}

fn start_stream(
    source: Arc<AudioBuffer>,
    engine_rate: u32,
    plan: RenderPlan,
    from_source: u64,
) -> Result<Session, String> {
    let host = cpal::default_host();
    let device = host.default_output_device().ok_or("no output device")?;

    // Prefer running the device at the file's rate; the bridge only has to
    // interpolate when the device refuses.
    let mut chosen = device.default_output_config().map_err(|e| e.to_string())?;
    if let Ok(ranges) = device.supported_output_configs() {
        for r in ranges {
            if r.min_sample_rate().0 <= engine_rate
                && engine_rate <= r.max_sample_rate().0
                && r.sample_format() == cpal::SampleFormat::F32
            {
                chosen = r.with_sample_rate(cpal::SampleRate(engine_rate));
                break;
            }
        }
    }
    let device_rate = chosen.sample_rate().0;
    let device_channels = chosen.channels() as usize;
    let plan_channels = plan.cfg.channels;

    let max_block = 4096usize;
    let cfg = PlayerConfig { max_block, ..PlayerConfig::default() };
    let (player, handle) =
        solfege::stream::start(source, engine_rate, plan, from_source, cfg);
    let metrics = handle.metrics().clone();

    let mut bridge = DeviceBridge::new(
        player,
        plan_channels,
        device_channels,
        engine_rate,
        device_rate,
        max_block,
    );

    let stream = device
        .build_output_stream(
            &chosen.config(),
            move |data: &mut [f32], _: &cpal::OutputCallbackInfo| {
                let t0 = Instant::now();
                bridge.fill(data);
                metrics.record_call(t0.elapsed().as_micros() as u64);
            },
            move |e| eprintln!("audio stream error: {e}"),
            None,
        )
        .map_err(|e| e.to_string())?;
    stream.play().map_err(|e| e.to_string())?;
    Ok(Session { _stream: stream, handle, kind: Playing::None, source_frames: 0 })
}

// ---------------------------------------------------------------- app

#[derive(Copy, Clone, PartialEq, Eq)]
enum Playing {
    None,
    /// Streaming through the engine, live.
    Live,
    /// Streaming the untouched source, for A/B at the same level.
    Dry,
}

struct DemoApp {
    // material
    source: Arc<AudioBuffer>,
    sample_rate: u32,
    source_name: String,
    output: Option<Arc<AudioBuffer>>,
    analysis: Option<Arc<Analysis>>,
    source_peaks: Peaks,
    output_peaks: Peaks,
    /// Monitor trim in dB. Applies to what the device hears, never to a render.
    trim_db: f32,
    /// Pull the monitor down far enough to keep the device under 0 dBFS.
    ///
    /// This is a *monitoring* control, not normalisation: the rendered and
    /// exported audio is untouched, and the meter still reports the engine's
    /// real peak. Both phase propagation and overlap-add can put a peak above
    /// the source's — a mastered track at -0.2 dBFS came back at +1.7 dBFS
    /// through the phase vocoder — and listening to that through a clipping
    /// converter tells you nothing about the engine.
    clip_guard: bool,
    /// The trim the guard settled on, for display.
    guard_db: f32,

    // controls
    alpha: f64,
    semitones: f64,
    mode: EngineMode,
    formant_kind: usize,
    formant_shift: f64,
    quality: QualityProfile,
    block: usize,
    seed: u64,
    /// Transient handling: detection plus unity-rate locks around attacks.
    transients: bool,

    // time map
    anchors: Vec<WarpAnchor>,
    drag_anchor: Option<usize>,
    /// Off renders the identity map: the transpose still applies, nothing is
    /// stretched, and the anchors are kept so turning it back on restores them.
    warp_enabled: bool,

    // status
    status: String,
    status_is_error: bool,
    plan_line: String,
    render_line: String,
    busy: bool,

    // io
    tx: Sender<Msg>,
    rx: Receiver<Msg>,
    path_input: String,

    // live playback
    session: Option<Session>,
    playing: Playing,
    /// A control moved while streaming; the plan is re-sent once the user
    /// settles, rather than on every frame of a slider drag.
    plan_dirty: bool,
    last_swap: Instant,
    live_line: String,
    /// The plan currently playing was compiled with no analysis available.
    planned_without_analysis: bool,
}

impl DemoApp {
    fn new() -> Self {
        let (tx, rx) = channel();
        let rate = 48_000u32;
        let source = Arc::new(fixtures::mixed(rate as usize * 3, rate, 2));
        let mut app = Self {
            source,
            sample_rate: rate,
            source_name: "fixture: mixed".to_string(),
            output: None,
            analysis: None,
            source_peaks: Peaks::empty(),
            output_peaks: Peaks::empty(),
            trim_db: 0.0,
            clip_guard: true,
            guard_db: 0.0,
            alpha: 1.5,
            semitones: 0.0,
            mode: EngineMode::Auto,
            formant_kind: 0,
            formant_shift: 0.0,
            quality: QualityProfile::Offline,
            block: 1024,
            seed: 0,
            transients: true,
            anchors: Vec::new(),
            drag_anchor: None,
            warp_enabled: true,
            status: "loaded fixture: mixed".to_string(),
            status_is_error: false,
            plan_line: String::new(),
            render_line: String::new(),
            busy: false,
            tx,
            rx,
            path_input: String::new(),
            session: None,
            playing: Playing::None,
            plan_dirty: false,
            last_swap: Instant::now(),
            live_line: String::new(),
            planned_without_analysis: true,
        };
        app.reset_anchors();
        app.source_peaks = Peaks::build(&app.source);
        app.spawn_analysis();
        app
    }

    fn formant(&self) -> FormantPolicy {
        match self.formant_kind {
            1 => FormantPolicy::Preserve,
            2 => FormantPolicy::Shift(self.formant_shift),
            _ => FormantPolicy::FollowPitch,
        }
    }

    fn reset_anchors(&mut self) {
        let n = self.source.frames() as u64;
        let m = ((n as f64) * self.alpha).round().max(1.0) as u64;
        self.anchors = if n == 0 {
            Vec::new()
        } else {
            vec![WarpAnchor::endpoint(0, 0), WarpAnchor::endpoint(n, m)]
        };
    }

    /// Rescale interior anchors and move the endpoint when alpha changes, so a
    /// slider drag does not invalidate a map the user has already shaped.
    fn apply_alpha(&mut self) {
        let n = self.source.frames() as u64;
        if n == 0 {
            return;
        }
        let target = ((n as f64) * self.alpha).round().max(1.0) as u64;
        let Some(last) = self.anchors.last().copied() else {
            self.reset_anchors();
            return;
        };
        if last.output_frame == 0 {
            self.reset_anchors();
            return;
        }
        let scale = target as f64 / last.output_frame as f64;
        for a in self.anchors.iter_mut() {
            a.output_frame = ((a.output_frame as f64) * scale).round() as u64;
        }
        if let Some(l) = self.anchors.last_mut() {
            l.output_frame = target;
        }
        self.repair_anchors();
    }

    /// Keep the map strictly monotonic. The document would reject a crossed
    /// anchor anyway; this stops the UI from producing one in the first place.
    fn repair_anchors(&mut self) {
        self.anchors.sort_by_key(|a| a.source_frame);
        for i in 1..self.anchors.len() {
            let prev = self.anchors[i - 1];
            if self.anchors[i].source_frame <= prev.source_frame {
                self.anchors[i].source_frame = prev.source_frame + 1;
            }
            if self.anchors[i].output_frame <= prev.output_frame {
                self.anchors[i].output_frame = prev.output_frame + 1;
            }
        }
    }

    /// The anchors a render should use. With warp off this is the identity
    /// map, which is a different thing from Bypass: the engine still runs, so
    /// the transpose and the formant policy still apply.
    fn effective_anchors(&self) -> Vec<WarpAnchor> {
        if self.warp_enabled {
            return self.anchors.clone();
        }
        let n = self.source.frames() as u64;
        if n == 0 {
            Vec::new()
        } else {
            vec![WarpAnchor::endpoint(0, 0), WarpAnchor::endpoint(n, n)]
        }
    }

    fn document(&self) -> EditDocument {
        let id = SourceIdentity::of(&self.source, self.sample_rate);
        let mut doc = EditDocument::identity(id);
        doc.anchors = self.effective_anchors();
        doc.mode = self.mode;
        doc.quality = self.quality;
        doc.pitch_semitones = self.semitones;
        doc.formant = self.formant();
        doc.deterministic_seed = self.seed;
        doc
    }

    fn compile_options(&self) -> CompileOptions {
        CompileOptions {
            max_block: 8192,
            transient_protect: self.transients,
            stft_size: None,
        }
    }

    fn spawn_analysis(&mut self) {
        let tx = self.tx.clone();
        let src = self.source.clone();
        let rate = self.sample_rate;
        std::thread::spawn(move || {
            let a = analyze(&src, rate, "demo", &AnalysisSettings::default());
            let _ = tx.send(Msg::Analysed(Box::new(a)));
        });
    }

    fn spawn_render(&mut self) {
        if self.busy {
            return;
        }
        let doc = self.document();
        let analysis = self.analysis.clone();
        let src = self.source.clone();
        let opts = self.compile_options();
        let block = self.block;
        let tx = self.tx.clone();
        self.busy = true;
        self.status = "rendering…".into();
        self.status_is_error = false;
        std::thread::spawn(move || {
            let t0 = Instant::now();
            let plan = match compile(&doc, analysis.as_deref(), &opts) {
                Ok(p) => p,
                Err(e) => {
                    let _ = tx.send(Msg::Failed(e.to_string()));
                    return;
                }
            };
            match render_offline(&plan, &src, block) {
                Ok((audio, report)) => {
                    let _ = tx.send(Msg::Rendered {
                        audio: Box::new(audio),
                        mode: plan.mode,
                        auto_reason: plan.auto_reason.clone(),
                        expected: report.expected_frames,
                        produced: report.produced_frames,
                        ms: t0.elapsed().as_secs_f64() * 1000.0,
                    });
                }
                Err(e) => {
                    let _ = tx.send(Msg::Failed(e.to_string()));
                }
            }
        });
    }

    fn pump(&mut self) {
        while let Ok(msg) = self.rx.try_recv() {
            match msg {
                Msg::Analysed(a) => {
                    self.status = format!(
                        "analysis: {} — {} onsets, {} notes, suggests {}",
                        a.class.label(),
                        a.onsets.len(),
                        a.notes.len(),
                        a.class.suggested_mode().title()
                    );
                    self.status_is_error = false;
                    self.analysis = Some(Arc::from(*a));
                    // A plan compiled before this point routed Auto with no
                    // evidence and got no transient protections. Re-plan rather
                    // than leave a live stream on a mode the analysis
                    // contradicts.
                    if self.planned_without_analysis {
                        self.plan_dirty = true;
                        self.last_swap = Instant::now()
                            - std::time::Duration::from_millis(200);
                    }
                }
                Msg::Rendered { audio, mode, auto_reason, expected, produced, ms } => {
                    let lvl = metrics::level(&audio);
                    let secs = audio.frames() as f64 / self.sample_rate as f64;
                    self.render_line = format!(
                        "{} frames (expected {}) · peak {:.3} · rms {:.1} dBFS · {:.0} ms ({:.0}x realtime)",
                        produced,
                        expected,
                        lvl.peak,
                        lvl.rms_dbfs,
                        ms,
                        secs / (ms / 1000.0).max(1e-9)
                    );
                    self.plan_line = match auto_reason {
                        Some(r) => format!("engine {} — auto chose it: {r}", mode.title()),
                        None => format!("engine {} — {}", mode.title(), mode.hint()),
                    };
                    self.status = "render complete".into();
                    self.status_is_error = false;
                    self.output_peaks = Peaks::build(&audio);
                    self.output = Some(Arc::from(*audio));
                    self.busy = false;
                }
                Msg::Failed(e) => {
                    self.status = e;
                    self.status_is_error = true;
                    self.busy = false;
                }
            }
        }
    }

    fn load_path(&mut self, path: &str) {
        self.stop();
        match wav::read(path) {
            Ok(f) => {
                if let Err(e) = f.audio.check_finite() {
                    self.status = format!("{path}: {e}");
                    self.status_is_error = true;
                    return;
                }
                self.sample_rate = f.sample_rate;
                self.source = Arc::new(f.audio);
                self.source_peaks = Peaks::build(&self.source);
                self.output_peaks = Peaks::empty();
                self.source_name = path.to_string();
                self.output = None;
                self.analysis = None;
                self.reset_anchors();
                self.spawn_analysis();
                self.status = format!(
                    "loaded {} ({} Hz, {} ch, {} frames)",
                    path,
                    self.sample_rate,
                    self.source.channel_count(),
                    self.source.frames()
                );
                self.status_is_error = false;
            }
            Err(e) => {
                self.status = format!("{path}: {e}");
                self.status_is_error = true;
            }
        }
    }

    fn load_fixture(&mut self, name: &str) {
        self.stop();
        let rate = 48_000u32;
        let n = rate as usize * 3;
        let buf = match name {
            "sine 440" => fixtures::sine(n, rate, 440.0, 0.5, 1),
            "harmonics 220" => fixtures::harmonics(n, rate, 220.0, 8, 1),
            "chirp" => fixtures::chirp(n, rate, 40.0, 12_000.0, 1),
            "impulses" => fixtures::impulse_train(n, rate as usize / 4, 1, 0),
            "drums" => fixtures::drum_hits(n, rate, rate as usize / 4, 1),
            "vowel /a/" => fixtures::vowel(n, rate, 130.0, [730.0, 1090.0, 2440.0], 1),
            "vowel /i/" => fixtures::vowel(n, rate, 130.0, [270.0, 2290.0, 3010.0], 1),
            "stereo inverted" => {
                fixtures::inverted_stereo(&fixtures::harmonics(n, rate, 220.0, 8, 1))
            }
            "stereo delayed" => fixtures::delayed_stereo(
                &fixtures::drum_hits(n, rate, rate as usize / 4, 1),
                24,
            ),
            _ => fixtures::mixed(n, rate, 2),
        };
        self.sample_rate = rate;
        self.source = Arc::new(buf);
        self.source_peaks = Peaks::build(&self.source);
        self.output_peaks = Peaks::empty();
        self.source_name = format!("fixture: {name}");
        self.output = None;
        self.analysis = None;
        self.reset_anchors();
        self.spawn_analysis();
        self.status = format!("loaded fixture: {name}");
        self.status_is_error = false;
    }

    /// Build the plan the live stream should run. `dry` gives a Bypass plan for
    /// A/B against the untouched source at the same level.
    fn live_plan(&mut self, dry: bool) -> Result<RenderPlan, String> {
        let doc = if dry {
            EditDocument::identity(SourceIdentity::of(&self.source, self.sample_rate))
        } else {
            self.document()
        };
        self.planned_without_analysis = self.analysis.is_none();
        compile(&doc, self.analysis.as_deref(), &self.compile_options()).map_err(|e| e.to_string())
    }

    /// One line describing what the compiler settled on, including whether it
    /// had anything to go on.
    fn describe(&self, plan: &RenderPlan) -> String {
        match &plan.auto_reason {
            Some(r) => format!("engine {} — auto chose it: {r}", plan.mode.title()),
            None => format!("engine {} — {}", plan.mode.title(), plan.mode.hint()),
        }
    }

    fn play(&mut self, which: Playing) {
        self.session = None;
        self.playing = Playing::None;
        let plan = match self.live_plan(which == Playing::Dry) {
            Ok(p) => p,
            Err(e) => {
                self.status = e;
                self.status_is_error = true;
                return;
            }
        };
        self.plan_line = self.describe(&plan);
        match start_stream(self.source.clone(), self.sample_rate, plan, 0) {
            Ok(mut s) => {
                s.kind = which;
                self.guard_db = 0.0;
                s.handle.set_trim_db(self.trim_db);
                s.source_frames = self.source.frames() as u64;
                self.session = Some(s);
                self.playing = which;
                self.plan_dirty = false;
                self.status = match which {
                    Playing::Dry => "streaming the dry source".into(),
                    _ => "streaming through the engine".into(),
                };
                self.status_is_error = false;
            }
            Err(e) => {
                self.status = format!("playback: {e}");
                self.status_is_error = true;
            }
        }
    }

    fn stop(&mut self) {
        self.session = None;
        self.playing = Playing::None;
        self.live_line.clear();
    }

    /// Push the edited plan to a running stream.
    ///
    /// Within one mode the running engine is retargeted in place - new map,
    /// pitch and formant, same phase state and source position - so this can
    /// follow a slider while it is being dragged. Only a mode change builds a
    /// new engine, which the callback crossfades in.
    fn push_plan_if_dirty(&mut self) {
        if !self.plan_dirty || self.playing != Playing::Live {
            return;
        }
        if self.last_swap.elapsed() < std::time::Duration::from_millis(40) {
            return;
        }
        let Some(session) = self.session.as_ref() else { return };
        let at = session.handle.source_frame();
        match self.live_plan(false) {
            Ok(plan) => {
                self.plan_line = self.describe(&plan);
                let handle = &self.session.as_ref().expect("checked above").handle;
                handle.set_plan(plan, at);
                self.status = "live".into();
                self.status_is_error = false;
            }
            Err(e) => {
                self.status = e;
                self.status_is_error = true;
            }
        }
        self.plan_dirty = false;
        self.last_swap = Instant::now();
    }

    fn poll_stream(&mut self) {
        let Some(session) = self.session.as_ref() else {
            self.live_line.clear();
            return;
        };
        if let Some(e) = session.handle.take_error() {
            self.status = format!("stream: {e}");
            self.status_is_error = true;
        }
        let m = session.handle.metrics();
        let under = m.underruns.load(Ordering::Relaxed);
        let peak = session.handle.peak();
        let device_peak = session.handle.device_peak();
        let clipped = session.handle.clipped_frames();

        // Only ever pull down, and only by what the measured peak asks for, so
        // the guard settles instead of pumping.
        if self.clip_guard && peak > 1.0 {
            let needed = -(20.0 * peak.log10()) - 0.5;
            if needed < self.guard_db {
                self.guard_db = needed;
                session.handle.set_trim_db(self.trim_db + self.guard_db);
            }
        }
        let db = |v: f32| if v > 0.0 { 20.0 * v.log10() } else { -99.0 };
        let clip_note = if clipped > 0 {
            format!(" · CLIPPING {clipped} frames at the device")
        } else {
            String::new()
        };
        self.live_line = format!(
            "live · p99 {:.2} ms (max {:.2}) · underruns {} · starved {} · changes {} in place, {} rebuilt · engine {:+.1} dBFS → device {:+.1} dBFS{} · source {:.2}s",
            m.quantile_ms(0.99),
            m.max_ms(),
            under,
            m.starved_frames.load(Ordering::Relaxed),
            m.retargets.load(Ordering::Relaxed),
            m.swaps.load(Ordering::Relaxed).saturating_sub(1),
            db(peak),
            db(device_peak),
            clip_note,
            session.handle.source_frame() as f64 / self.sample_rate as f64,
        );
        if session.handle.is_finished() {
            self.stop();
        }
    }

    /// Playback progress 0..1, or `None` when nothing is playing.
    fn play_progress(&self) -> Option<(Playing, f32)> {
        let s = self.session.as_ref()?;
        let total = s.source_frames.max(1);
        Some((self.playing, (s.handle.source_frame() as f32 / total as f32).clamp(0.0, 1.0)))
    }

    fn time_map(&self) -> Option<TimeMap> {
        TimeMap::new(self.effective_anchors()).ok()
    }
}

// ---------------------------------------------------------------- drawing

/// Precomputed min/max envelope.
///
/// Scanning the samples on every repaint is fine for a three-second fixture and
/// ruinous for a five-minute track: a 333-second stereo file is 32 million
/// samples, and touching all of them at 30 fps pegs a core and starves the
/// audio callback that is doing the actual work. Bucketing once on load turns
/// every later repaint into a walk over a few thousand pairs.
struct Peaks {
    min: Vec<f32>,
    max: Vec<f32>,
    frames: usize,
}

impl Peaks {
    const BUCKETS: usize = 4096;

    fn build(buf: &AudioBuffer) -> Self {
        let frames = buf.frames();
        let buckets = Self::BUCKETS.min(frames.max(1));
        let mut min = vec![0.0f32; buckets];
        let mut max = vec![0.0f32; buckets];
        for b in 0..buckets {
            let a = frames * b / buckets;
            let e = (frames * (b + 1) / buckets).max(a + 1).min(frames);
            let mut lo = f32::INFINITY;
            let mut hi = f32::NEG_INFINITY;
            for c in 0..buf.channel_count() {
                for v in &buf.channel(c)[a..e] {
                    lo = lo.min(*v);
                    hi = hi.max(*v);
                }
            }
            if lo.is_finite() {
                min[b] = lo;
                max[b] = hi;
            }
        }
        Self { min, max, frames }
    }

    fn empty() -> Self {
        Self { min: Vec::new(), max: Vec::new(), frames: 0 }
    }
}

fn draw_wave(painter: &egui::Painter, rect: Rect, peaks: &Peaks, color: Color32) {
    painter.rect_filled(rect, CornerRadius::same(3), BG_PANEL);
    let mid = rect.center().y;
    painter.line_segment(
        [pos2(rect.left(), mid), pos2(rect.right(), mid)],
        Stroke::new(1.0_f32, GRID),
    );
    let n = peaks.min.len();
    if n == 0 || peaks.frames == 0 {
        return;
    }
    let cols = rect.width().max(1.0) as usize;
    let half = rect.height() * 0.5 - 2.0;
    for x in 0..cols {
        let a = n * x / cols;
        let b = (n * (x + 1) / cols).max(a + 1).min(n);
        let mut lo = f32::INFINITY;
        let mut hi = f32::NEG_INFINITY;
        for i in a..b {
            lo = lo.min(peaks.min[i]);
            hi = hi.max(peaks.max[i]);
        }
        if !lo.is_finite() {
            continue;
        }
        let px = rect.left() + x as f32;
        painter.line_segment(
            [pos2(px, mid - hi * half), pos2(px, mid - lo * half)],
            Stroke::new(1.0_f32, color),
        );
    }
}

fn draw_marker(painter: &egui::Painter, rect: Rect, x: f32, color: Color32, label: Option<&str>) {
    if x < rect.left() - 1.0 || x > rect.right() + 1.0 {
        return;
    }
    painter.line_segment(
        [pos2(x, rect.top()), pos2(x, rect.bottom())],
        Stroke::new(1.5_f32, color),
    );
    if let Some(t) = label {
        painter.text(
            pos2(x + 3.0, rect.top() + 2.0),
            Align2::LEFT_TOP,
            t,
            FontId::monospace(10.0),
            color,
        );
    }
}

impl eframe::App for DemoApp {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        self.pump();
        self.poll_stream();
        self.push_plan_if_dirty();
        if self.session.is_some() {
            ctx.request_repaint_after(std::time::Duration::from_millis(33));
        }

        // Dropped files load like a path typed into the box.
        let dropped: Vec<String> = ctx.input(|i| {
            i.raw
                .dropped_files
                .iter()
                .filter_map(|f| f.path.as_ref().map(|p| p.display().to_string()))
                .collect()
        });
        if let Some(p) = dropped.first() {
            let p = p.clone();
            self.load_path(&p);
        }

        egui::SidePanel::left("controls").exact_width(300.0).show(ctx, |ui| {
            ui.add_space(6.0);
            ui.heading("Solfege Stretching");
            ui.label(
                egui::RichText::new("time map · engines · anchors")
                    .color(TEXT_DIM)
                    .size(11.0),
            );
            ui.separator();

            ui.label(egui::RichText::new("SOURCE").size(11.0).color(TEXT_DIM));
            ui.horizontal(|ui| {
                ui.add(
                    egui::TextEdit::singleline(&mut self.path_input)
                        .hint_text("path to .wav (or drop a file)")
                        .desired_width(190.0),
                );
                if ui.button("Load").clicked() {
                    let p = self.path_input.clone();
                    self.load_path(&p);
                }
            });
            egui::ComboBox::from_id_salt("fixture")
                .selected_text("load a fixture…")
                .width(268.0)
                .show_ui(ui, |ui| {
                    for name in [
                        "mixed",
                        "sine 440",
                        "harmonics 220",
                        "chirp",
                        "impulses",
                        "drums",
                        "vowel /a/",
                        "vowel /i/",
                        "stereo inverted",
                        "stereo delayed",
                    ] {
                        if ui.selectable_label(false, name).clicked() {
                            self.load_fixture(name);
                        }
                    }
                });
            ui.label(
                egui::RichText::new(format!(
                    "{}  ·  {} Hz  ·  {} ch  ·  {:.2} s",
                    self.source_name,
                    self.sample_rate,
                    self.source.channel_count(),
                    self.source.frames() as f64 / self.sample_rate as f64
                ))
                .size(11.0)
                .color(TEXT_DIM),
            );

            ui.add_space(8.0);
            ui.label(egui::RichText::new("MODE").size(11.0).color(TEXT_DIM));
            egui::ComboBox::from_id_salt("mode")
                .selected_text(self.mode.title())
                .width(268.0)
                .show_ui(ui, |ui| {
                    for m in EngineMode::ALL {
                        let r = ui
                            .selectable_value(&mut self.mode, m, m.title())
                            .on_hover_text(m.hint());
                        if r.clicked() {
                            self.plan_dirty = true;
                        }
                    }
                });
            ui.label(egui::RichText::new(self.mode.hint()).size(10.0).color(TEXT_DIM));

            ui.add_space(8.0);
            ui.label(egui::RichText::new("TIME AND PITCH").size(11.0).color(TEXT_DIM));
            if ui
                .checkbox(&mut self.warp_enabled, "warp (time map)")
                .changed()
            {
                self.plan_dirty = true;
            }
            if !self.warp_enabled {
                ui.label(
                    egui::RichText::new(
                        "warp off: identity map, transpose still applies. Anchors are kept.",
                    )
                    .size(10.0)
                    .color(ANCHOR_USER),
                );
            }
            let mut alpha = self.alpha;
            let warp_on = self.warp_enabled;
            if ui
                .add_enabled(
                    warp_on,
                    egui::Slider::new(&mut alpha, 0.25..=4.0)
                        .logarithmic(true)
                        .text("length ×"),
                )
                .changed()
            {
                self.alpha = alpha;
                self.apply_alpha();
                self.plan_dirty = true;
            }
            ui.horizontal(|ui| {
                ui.label(
                    egui::RichText::new(format!("tempo {:.1}%", 100.0 / self.alpha))
                        .size(11.0)
                        .color(TEXT_DIM),
                );
                for (label, a) in [("50%", 2.0), ("75%", 4.0 / 3.0), ("100%", 1.0), ("125%", 0.8), ("150%", 2.0 / 3.0)] {
                    if ui.add_enabled(warp_on, egui::Button::new(label).small()).clicked() {
                        self.alpha = a;
                        self.apply_alpha();
                        self.plan_dirty = true;
                    }
                }
            });
            let pitch_ok = self.mode.independent_pitch();
            if ui
                .add_enabled(
                    pitch_ok,
                    egui::Slider::new(&mut self.semitones, -12.0..=12.0)
                        .step_by(0.01)
                        .text("transpose (st)"),
                )
                .changed()
            {
                self.plan_dirty = true;
            }
            ui.horizontal(|ui| {
                for (label, st) in [("-12", -12.0), ("-7", -7.0), ("-1", -1.0), ("0", 0.0), ("+1", 1.0), ("+7", 7.0), ("+12", 12.0)] {
                    if ui.add_enabled(pitch_ok, egui::Button::new(label).small()).clicked() {
                        self.semitones = st;
                        self.plan_dirty = true;
                    }
                }
            });
            if !pitch_ok && self.mode == EngineMode::Varispeed {
                ui.label(
                    egui::RichText::new(format!(
                        "varispeed: pitch follows speed ({:+.2} st)",
                        -12.0 * self.alpha.log2()
                    ))
                    .size(10.0)
                    .color(ANCHOR_USER),
                );
                if self.semitones != 0.0 {
                    self.semitones = 0.0;
                    self.plan_dirty = true;
                }
            }

            let formant_ok = solfege::plan::capability_of(self.mode).formant_control
                || self.mode == EngineMode::Auto;
            ui.add_enabled_ui(formant_ok, |ui| {
                egui::ComboBox::from_id_salt("formant")
                    .selected_text(match self.formant_kind {
                        1 => "formants: preserve",
                        2 => "formants: shift",
                        _ => "formants: follow pitch",
                    })
                    .width(268.0)
                    .show_ui(ui, |ui| {
                        let a = ui.selectable_value(&mut self.formant_kind, 0, "follow pitch (like tape)");
                        let b = ui.selectable_value(&mut self.formant_kind, 1, "preserve (natural voice)");
                        let c = ui.selectable_value(&mut self.formant_kind, 2, "shift by semitones");
                        if a.clicked() || b.clicked() || c.clicked() {
                            self.plan_dirty = true;
                        }
                    });
                if self.formant_kind == 2
                    && ui
                        .add(
                            egui::Slider::new(&mut self.formant_shift, -12.0..=12.0)
                                .text("formant shift"),
                        )
                        .changed()
                {
                    self.plan_dirty = true;
                }
            });
            if !formant_ok && self.formant_kind != 0 {
                self.formant_kind = 0;
                self.plan_dirty = true;
            }
            if ui
                .checkbox(&mut self.transients, "keep transients sharp")
                .on_hover_text(
                    "Plays each detected attack at its original rate and makes up the time \
                     around it, so hits stay crisp and land on the map. Changes where frames \
                     are read, never a gain.",
                )
                .changed()
            {
                self.plan_dirty = true;
            }
            egui::CollapsingHeader::new(egui::RichText::new("advanced").size(11.0).color(TEXT_DIM))
                .default_open(false)
                .show(ui, |ui| {
                    ui.horizontal(|ui| {
                        let a = ui.selectable_value(&mut self.quality, QualityProfile::Offline, "offline");
                        let b = ui.selectable_value(&mut self.quality, QualityProfile::Realtime, "realtime");
                        if a.clicked() || b.clicked() {
                            self.plan_dirty = true;
                        }
                    });
                    ui.horizontal(|ui| {
                        ui.label("render block");
                        ui.add(egui::DragValue::new(&mut self.block).range(1..=8192));
                        ui.label("seed");
                        ui.add(egui::DragValue::new(&mut self.seed).range(0..=u64::MAX));
                    });
                });

            ui.add_space(10.0);
            ui.horizontal(|ui| {
                let btn = egui::Button::new(if self.busy { "rendering…" } else { "Render" })
                    .min_size(vec2(120.0, 28.0));
                if ui.add_enabled(!self.busy, btn).clicked() {
                    self.spawn_render();
                }
                if ui.button("Reset map").clicked() {
                    self.reset_anchors();
                    self.plan_dirty = true;
                }
            });
            ui.horizontal(|ui| {
                let live = egui::Button::new("▶ live").min_size(vec2(84.0, 24.0));
                if ui.add(live).clicked() {
                    self.play(Playing::Live);
                }
                if ui.button("▶ dry").clicked() {
                    self.play(Playing::Dry);
                }
                if ui.button("■ stop").clicked() {
                    self.stop();
                }
            });
            if ui
                .add(egui::Slider::new(&mut self.trim_db, -24.0..=6.0).text("monitor trim (dB)"))
                .changed()
            {
                if let Some(s) = self.session.as_ref() {
                    s.handle.set_trim_db(self.trim_db + self.guard_db);
                }
            }
            if ui
                .checkbox(&mut self.clip_guard, "clip guard (monitor only)")
                .changed()
            {
                self.guard_db = 0.0;
                if let Some(s) = self.session.as_ref() {
                    s.handle.set_trim_db(self.trim_db);
                }
            }
            if self.guard_db < 0.0 {
                ui.label(
                    egui::RichText::new(format!(
                        "clip guard is holding the monitor {:.1} dB down; the render itself is untouched",
                        self.guard_db
                    ))
                    .size(10.0)
                    .color(ANCHOR_USER),
                );
            }
            ui.label(
                egui::RichText::new(
                    "▶ live runs the engine in the audio callback. Moving a control retargets the running engine in place - no rebuild, no crossfade; only a mode change builds a new one. Renders are never normalised; the clip guard trims the monitor only.",
                )
                .size(10.0)
                .color(TEXT_DIM),
            );

            ui.add_space(8.0);
            ui.separator();
            ui.label(egui::RichText::new("ANCHORS").size(11.0).color(TEXT_DIM));
            ui.label(
                egui::RichText::new(
                    "click the source waveform to add one; drag it on the output waveform",
                )
                .size(10.0)
                .color(TEXT_DIM),
            );
            ui.horizontal(|ui| {
                let can_edit = self.warp_enabled;
                if ui
                    .add_enabled(
                        can_edit && self.analysis.is_some(),
                        egui::Button::new("from onsets"),
                    )
                    .clicked()
                {
                    if let Some(a) = self.analysis.clone() {
                        let map = self.time_map();
                        for o in a.onsets.iter() {
                            if o.frame == 0 || o.frame >= self.source.frames() as u64 {
                                continue;
                            }
                            let t = map
                                .as_ref()
                                .map(|m| m.forward(o.frame as f64).round() as u64)
                                .unwrap_or(o.frame);
                            self.anchors.push(WarpAnchor::new(
                                o.frame,
                                t,
                                AnchorKind::PromotedAnalysis,
                            ));
                        }
                        self.anchors.dedup_by_key(|a| a.source_frame);
                        self.repair_anchors();
                        self.plan_dirty = true;
                    }
                }
                if ui
                    .add_enabled(can_edit, egui::Button::new("clear interior"))
                    .clicked()
                {
                    self.anchors.retain(|a| a.kind == AnchorKind::Endpoint);
                    self.plan_dirty = true;
                }
            });

            let sr = self.sample_rate as f64;
            let mut remove: Option<usize> = None;
            egui::ScrollArea::vertical().max_height(180.0).show(ui, |ui| {
                for i in 0..self.anchors.len() {
                    let kind = self.anchors[i].kind;
                    let is_end = kind == AnchorKind::Endpoint;
                    ui.horizontal(|ui| {
                        ui.label(
                            egui::RichText::new(format!("{:>7.3}s", self.anchors[i].source_frame as f64 / sr))
                                .monospace()
                                .size(11.0)
                                .color(if is_end { ANCHOR_END } else { ANCHOR_USER }),
                        );
                        ui.label("→");
                        let mut out = self.anchors[i].output_frame;
                        if ui
                            .add(
                                egui::DragValue::new(&mut out)
                                    .speed(50.0)
                                    .custom_formatter(|v, _| format!("{:.3}s", v / sr)),
                            )
                            .changed()
                        {
                            self.anchors[i].output_frame = out;
                            self.repair_anchors();
                            self.plan_dirty = true;
                        }
                        if !is_end && ui.small_button("×").clicked() {
                            remove = Some(i);
                        }
                    });
                }
            });
            if let Some(i) = remove {
                self.anchors.remove(i);
                self.plan_dirty = true;
            }
        });

        egui::TopBottomPanel::bottom("status").show(ctx, |ui| {
            ui.add_space(2.0);
            if !self.plan_line.is_empty() {
                ui.label(egui::RichText::new(&self.plan_line).size(12.0).color(OK));
            }
            if !self.render_line.is_empty() {
                ui.label(egui::RichText::new(&self.render_line).monospace().size(11.0).color(TEXT_DIM));
            }
            if !self.live_line.is_empty() {
                let bad = self
                    .session
                    .as_ref()
                    .map(|s| {
                        s.handle.metrics().underruns.load(Ordering::Relaxed) > 0
                            || s.handle.clipped_frames() > 0
                    })
                    .unwrap_or(false);
                ui.label(
                    egui::RichText::new(&self.live_line)
                        .monospace()
                        .size(11.0)
                        .color(if bad { ERR } else { OK }),
                );
            }
            ui.label(
                egui::RichText::new(&self.status)
                    .size(12.0)
                    .color(if self.status_is_error { ERR } else { TEXT_DIM }),
            );
            ui.add_space(2.0);
        });

        egui::CentralPanel::default().show(ctx, |ui| {
            let progress = self.play_progress();
            let src_frames = self.source.frames();
            let out_frames = self.output.as_ref().map(|o| o.frames()).unwrap_or(0);
            let map = self.time_map();

            // ---- source waveform: click to add an anchor
            ui.label(egui::RichText::new("SOURCE (click to add an anchor)").size(11.0).color(TEXT_DIM));
            let h = ((ui.available_height() - 90.0) * 0.5).max(90.0);
            let (resp, painter) =
                ui.allocate_painter(vec2(ui.available_width(), h), Sense::click());
            let rect = resp.rect;
            draw_wave(&painter, rect, &self.source_peaks, WAVE_SRC);
            if let Some(a) = &self.analysis {
                for o in &a.onsets {
                    let x = rect.left() + rect.width() * (o.frame as f32 / src_frames.max(1) as f32);
                    painter.line_segment(
                        [pos2(x, rect.bottom() - 8.0), pos2(x, rect.bottom())],
                        Stroke::new(1.0_f32, ONSET),
                    );
                }
            }
            for a in &self.anchors {
                let x = rect.left() + rect.width() * (a.source_frame as f32 / src_frames.max(1) as f32);
                let c = if a.kind == AnchorKind::Endpoint { ANCHOR_END } else { ANCHOR_USER };
                draw_marker(&painter, rect, x, c, None);
            }
            if let Some((_, p)) = progress {
                draw_marker(&painter, rect, rect.left() + rect.width() * p, PLAYHEAD, None);
            }
            if resp.clicked() && self.warp_enabled {
                if let Some(pos) = resp.interact_pointer_pos() {
                    let f = ((pos.x - rect.left()) / rect.width()).clamp(0.0, 1.0) as f64
                        * src_frames as f64;
                    let s = f.round() as u64;
                    if s > 0 && s < src_frames as u64 {
                        let t = map
                            .as_ref()
                            .map(|m| m.forward(s as f64).round() as u64)
                            .unwrap_or(s);
                        self.anchors.push(WarpAnchor::user(s, t));
                        self.repair_anchors();
                        self.plan_dirty = true;
                    }
                }
            }

            ui.add_space(6.0);
            // ---- output waveform: drag anchors along the output axis
            ui.label(
                egui::RichText::new("OUTPUT (drag an anchor left/right to move it in time)")
                    .size(11.0)
                    .color(TEXT_DIM),
            );
            let (resp2, painter2) =
                ui.allocate_painter(vec2(ui.available_width(), h), Sense::click_and_drag());
            let rect2 = resp2.rect;
            let total_out = self
                .anchors
                .last()
                .map(|a| a.output_frame as usize)
                .unwrap_or(out_frames)
                .max(1);
            match &self.output {
                Some(_) => draw_wave(&painter2, rect2, &self.output_peaks, WAVE_OUT),
                None => {
                    painter2.rect_filled(rect2, CornerRadius::same(3), BG_PANEL);
                    painter2.text(
                        rect2.center(),
                        Align2::CENTER_CENTER,
                        "press Render",
                        FontId::proportional(13.0),
                        TEXT_DIM,
                    );
                }
            }
            for (i, a) in self.anchors.iter().enumerate() {
                let x = rect2.left() + rect2.width() * (a.output_frame as f32 / total_out as f32);
                let c = if a.kind == AnchorKind::Endpoint { ANCHOR_END } else { ANCHOR_USER };
                let label = if a.kind == AnchorKind::Endpoint { None } else { Some(i) };
                draw_marker(&painter2, rect2, x, c, label.map(|_| "").as_deref());
            }

            if resp2.drag_started() && self.warp_enabled {
                if let Some(pos) = resp2.interact_pointer_pos() {
                    let mut best: Option<(usize, f32)> = None;
                    for (i, a) in self.anchors.iter().enumerate() {
                        if a.kind == AnchorKind::Endpoint {
                            continue;
                        }
                        let x = rect2.left()
                            + rect2.width() * (a.output_frame as f32 / total_out as f32);
                        let d = (x - pos.x).abs();
                        if d < 10.0 && best.map(|(_, bd)| d < bd).unwrap_or(true) {
                            best = Some((i, d));
                        }
                    }
                    self.drag_anchor = best.map(|(i, _)| i);
                }
            }
            if resp2.dragged() {
                if let (Some(i), Some(pos)) = (self.drag_anchor, resp2.interact_pointer_pos()) {
                    let f = ((pos.x - rect2.left()) / rect2.width()).clamp(0.0, 1.0) as f64
                        * total_out as f64;
                    self.anchors[i].output_frame = f.round() as u64;
                    self.repair_anchors();
                    // Live streams follow the drag: each update retargets
                    // the running engine in place.
                    self.plan_dirty = true;
                }
            }
            if resp2.drag_stopped() {
                if self.drag_anchor.is_some() {
                    self.plan_dirty = true;
                }
                self.drag_anchor = None;
            }

            // ---- the map itself, drawn as a curve
            ui.add_space(6.0);
            ui.label(
                egui::RichText::new(if self.warp_enabled {
                    "TIME MAP  W(s)"
                } else {
                    "TIME MAP  W(s) — warp off, identity"
                })
                .size(11.0)
                .color(TEXT_DIM),
            );
            let (_r3, p3) =
                ui.allocate_painter(vec2(ui.available_width(), 70.0), Sense::hover());
            let r3 = _r3.rect;
            p3.rect_filled(r3, CornerRadius::same(3), BG_PANEL);
            if let Some(m) = &map {
                let n = m.source_frames().as_f64().max(1.0);
                let mm = m.output_frames().as_f64().max(1.0);
                let mut prev: Option<Pos2> = None;
                let cols = r3.width() as usize;
                for x in 0..=cols {
                    let s = n * x as f64 / cols.max(1) as f64;
                    let t = m.forward(s);
                    let p = pos2(
                        r3.left() + r3.width() * (s / n) as f32,
                        r3.bottom() - r3.height() * (t / mm) as f32,
                    );
                    if let Some(q) = prev {
                        p3.line_segment([q, p], Stroke::new(1.5_f32, ANCHOR_USER));
                    }
                    prev = Some(p);
                }
                // identity reference
                p3.line_segment(
                    [pos2(r3.left(), r3.bottom()), pos2(r3.right(), r3.top())],
                    Stroke::new(1.0_f32, GRID),
                );
                let (lo, hi) = m.ratio_range();
                let p = 2f64.powf(self.semitones / 12.0);
                p3.text(
                    pos2(r3.left() + 6.0, r3.top() + 4.0),
                    Align2::LEFT_TOP,
                    format!(
                        "alpha {lo:.3}..{hi:.3}   p x{p:.4}   internal {:.3}..{:.3}   M = {} frames",
                        lo * p,
                        hi * p,
                        m.output_frames().get()
                    ),
                    FontId::monospace(11.0),
                    TEXT_DIM,
                );
            } else {
                p3.text(
                    r3.center(),
                    Align2::CENTER_CENTER,
                    "invalid map",
                    FontId::proportional(12.0),
                    ERR,
                );
            }
            let _ = Vec2::ZERO;
        });
    }
}
