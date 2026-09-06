//! `solfege` — command line front end.
//!
//! IO and argument parsing live here so the library never reads a file or
//! prints anything (system-design.md sec.3).

use solfege::analysis::{analyze, Analysis, AnalysisSettings};
use solfege::audio::{AudioBuffer, SourceIdentity};
use solfege::document::{EditDocument, EngineMode, FormantPolicy, QualityProfile};
use solfege::mapping::{AnchorKind, WarpAnchor};
use solfege::metrics;
use solfege::plan::{compile, CompileOptions};
use solfege::render::{render_offline, render_offline_random_blocks};
use solfege::{fixtures, wav};
use std::collections::HashMap;
use std::path::PathBuf;
use std::process::ExitCode;
use std::time::Instant;

const USAGE: &str = "\
solfege — time stretching, pitch shifting and warping

USAGE:
  solfege render   --in <wav> --out <wav> [options]
  solfege analyze  --in <wav> [--json]
  solfege fixtures --out <dir> [--rate 48000]
  solfege selftest [--rate 48000] [--json]
  solfege bench    [--mode <name>] [--block 256] [--alpha 1.5] [--json]
  solfege stability [--hz 65] [--mode <name>] [--semitones 3] [--fft 2048]
                   steadiness of a known tone through the engine
  solfege quality  [--mode polyphonic] [--semitones 3] [--alpha 1.0] [--fft n]
                   the full-mix battery: bass, vocal, transient, tail, image
  solfege protect  [--mode polyphonic] [--protect 12] [--alpha 1.0]
                   does attack protect duck the level? on vs off, block by block
  solfege play     --in <wav> [--alpha 1.5] [--mode auto] [--seconds 10] [--trim -2]
                   [--sweep 0.5] exercise live plan swaps every N seconds
                   (needs --features playback)
  solfege doc      --in <wav> --out <json> [options]

RENDER OPTIONS:
  --alpha <f>          duration ratio; >1 is longer          (default 1.0)
  --semitones <f>      independent transpose                 (default 0)
  --mode <name>        auto|bypass|tape|percussive|monophonic|polyphonic|
                       hybrid|texture                        (default auto)
  --formant <name>     follow|preserve|shift:<semitones>     (default follow)
  --quality <name>     offline|realtime                      (default offline)
  --block <n>          caller block size                     (default 1024)
  --anchor <s>:<t>     add a hard anchor, source:output frames (repeatable)
  --float              write 32-bit float instead of 16-bit PCM
  --seed <n>           deterministic seed for FX engines     (default 0)
  --json               print a machine-readable report
";

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.is_empty() || args[0] == "-h" || args[0] == "--help" {
        print!("{USAGE}");
        return ExitCode::SUCCESS;
    }
    let cmd = args[0].clone();
    let opts = match Options::parse(&args[1..]) {
        Ok(o) => o,
        Err(e) => {
            eprintln!("solfege: {e}");
            return ExitCode::FAILURE;
        }
    };
    let result = match cmd.as_str() {
        "render" => cmd_render(&opts),
        "analyze" => cmd_analyze(&opts),
        "fixtures" => cmd_fixtures(&opts),
        "selftest" => cmd_selftest(&opts),
        "bench" => cmd_bench(&opts),
        "stability" => cmd_stability(&opts),
        "quality" => cmd_quality(&opts),
        "protect" => cmd_protect(&opts),
        "play" => cmd_play(&opts),
        "doc" => cmd_doc(&opts),
        other => Err(format!("unknown command `{other}`\n\n{USAGE}")),
    };
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("solfege: {e}");
            ExitCode::FAILURE
        }
    }
}

struct Options {
    flags: HashMap<String, String>,
    switches: Vec<String>,
    anchors: Vec<(u64, u64)>,
}

impl Options {
    fn parse(args: &[String]) -> Result<Self, String> {
        let mut flags = HashMap::new();
        let mut switches = Vec::new();
        let mut anchors = Vec::new();
        let mut i = 0;
        while i < args.len() {
            let a = &args[i];
            if let Some(name) = a.strip_prefix("--") {
                let takes_value = !matches!(name, "json" | "float" | "lowgate");
                if takes_value {
                    let v = args
                        .get(i + 1)
                        .ok_or_else(|| format!("--{name} needs a value"))?;
                    if name == "anchor" {
                        let (s, t) = v
                            .split_once(':')
                            .ok_or_else(|| "--anchor wants source:output".to_string())?;
                        anchors.push((
                            s.parse::<u64>().map_err(|e| e.to_string())?,
                            t.parse::<u64>().map_err(|e| e.to_string())?,
                        ));
                    } else {
                        flags.insert(name.to_string(), v.clone());
                    }
                    i += 2;
                } else {
                    switches.push(name.to_string());
                    i += 1;
                }
            } else {
                return Err(format!("unexpected argument `{a}`"));
            }
        }
        Ok(Self { flags, switches, anchors })
    }

    fn get(&self, k: &str) -> Option<&str> {
        self.flags.get(k).map(|s| s.as_str())
    }
    fn req(&self, k: &str) -> Result<&str, String> {
        self.get(k).ok_or_else(|| format!("--{k} is required"))
    }
    fn f64_or(&self, k: &str, d: f64) -> Result<f64, String> {
        match self.get(k) {
            None => Ok(d),
            Some(v) => v.parse().map_err(|_| format!("--{k} wants a number, got `{v}`")),
        }
    }
    fn usize_or(&self, k: &str, d: usize) -> Result<usize, String> {
        match self.get(k) {
            None => Ok(d),
            Some(v) => v.parse().map_err(|_| format!("--{k} wants an integer, got `{v}`")),
        }
    }
    fn u64_or(&self, k: &str, d: u64) -> Result<u64, String> {
        match self.get(k) {
            None => Ok(d),
            Some(v) => v.parse().map_err(|_| format!("--{k} wants an integer, got `{v}`")),
        }
    }
    fn has(&self, k: &str) -> bool {
        self.switches.iter().any(|s| s == k)
    }
}

fn parse_formant(s: &str) -> Result<FormantPolicy, String> {
    match s {
        "follow" | "follow_pitch" => Ok(FormantPolicy::FollowPitch),
        "preserve" => Ok(FormantPolicy::Preserve),
        other => {
            if let Some(v) = other.strip_prefix("shift:") {
                v.parse::<f64>()
                    .map(FormantPolicy::Shift)
                    .map_err(|_| format!("bad formant shift `{v}`"))
            } else {
                Err(format!("unknown formant policy `{other}`"))
            }
        }
    }
}

/// How steady is a known tone after the engine has been through it?
///
/// Two numbers, because "not smooth" can mean either and the fixes differ:
///
/// * **pitch wobble** - the fundamental wandering, in cents. A phase vocoder
///   whose window cannot resolve a low fundamental estimates its frequency from
///   a bin that holds several partials at once, and the output warbles.
/// * **level wobble** - the envelope breathing, in dB. That is phasiness:
///   partials drifting out of their original phase relationship and summing to
///   a different amplitude frame by frame.
///
/// The input is a synthetic harmonic tone, so both are zero before the engine
/// touches it and anything measured here is the engine's (validation.md sec.5).
fn cmd_stability(opts: &Options) -> Result<(), String> {
    let rate = opts.usize_or("rate", 48_000)? as u32;
    let seconds = opts.f64_or("seconds", 3.0)?;
    let alpha = opts.f64_or("alpha", 1.0)?;
    let semitones = opts.f64_or("semitones", 3.0)?;
    let fft = opts.get("fft").map(|v| v.parse::<usize>()).transpose()
        .map_err(|e| e.to_string())?;
    let freqs: Vec<f64> = match opts.get("hz") {
        Some(v) => vec![v.parse().map_err(|_| "--hz wants a number".to_string())?],
        None => vec![55.0, 82.5, 110.0, 220.0, 440.0],
    };
    let modes: Vec<EngineMode> = match opts.get("mode") {
        Some(m) => vec![EngineMode::parse(m).ok_or_else(|| format!("unknown mode `{m}`"))?],
        None => vec![EngineMode::Monophonic, EngineMode::Polyphonic, EngineMode::Hybrid],
    };
    let quality = match opts.get("quality").unwrap_or("offline") {
        "offline" => QualityProfile::Offline,
        "realtime" => QualityProfile::Realtime,
        other => return Err(format!("unknown quality `{other}`")),
    };

    let n = (rate as f64 * seconds) as usize;
    let p = 2f64.powf(semitones / 12.0);
    println!(
        "alpha {alpha}  ·  {semitones:+} semitones (x{p:.4})  ·  {} Hz  ·  fft {}",
        rate,
        fft.map(|f| f.to_string()).unwrap_or_else(|| "engine default".into())
    );
    println!(
        "{:<12} {:>7}  {:>10} {:>10}   {:>10} {:>10}",
        "engine", "f0", "cents rms", "cents max", "dB rms", "dB max"
    );

    for mode in modes {
        for &hz in &freqs {
            let src = fixtures::harmonics(n, rate, hz, 8, 1);
            let id = SourceIdentity::of(&src, rate);
            let mut doc = EditDocument::constant(id, alpha, semitones, mode);
            doc.quality = quality;
            let opts_c = CompileOptions {
                stft_size: fft,
                protect_in_spectral_modes: false,
                ..CompileOptions::default()
            };
            let plan = match compile(&doc, None, &opts_c) {
                Ok(p) => p,
                Err(e) => {
                    println!("{:<12} {hz:>7.1}  rejected: {e}", mode.label());
                    continue;
                }
            };
            let (out, _) = render_offline(&plan, &src, 1024).map_err(|e| e.to_string())?;

            // Skip the first and last quarter second: window ramps are not the
            // steady-state behaviour being measured.
            let win = 4096usize;
            let hop = 2048usize;
            let skip = rate as usize / 4;
            let expected = hz * p;
            let mut cents: Vec<f64> = Vec::new();
            let mut levels: Vec<f64> = Vec::new();
            let mut at = skip;
            while at + win + skip < out.frames() {
                if let Some(f) = metrics::dominant_hz(&out, rate, at, win) {
                    cents.push(metrics::pitch_error_cents(f, expected));
                }
                let mut acc = 0.0f64;
                for c in 0..out.channel_count() {
                    for v in &out.channel(c)[at..at + win] {
                        acc += (*v as f64) * (*v as f64);
                    }
                }
                let rms = (acc / (win * out.channel_count()) as f64).sqrt();
                if rms > 1e-9 {
                    levels.push(20.0 * rms.log10());
                }
                at += hop;
            }
            if cents.is_empty() || levels.is_empty() {
                println!("{:<12} {hz:>7.1}  no steady region", mode.label());
                continue;
            }
            // Deviation about the mean: a constant offset is a tuning error,
            // which the pitch gate already covers. Wobble is the variation.
            let cm = cents.iter().sum::<f64>() / cents.len() as f64;
            let c_rms =
                (cents.iter().map(|v| (v - cm) * (v - cm)).sum::<f64>() / cents.len() as f64).sqrt();
            let c_max = cents.iter().fold(0.0f64, |a, v| a.max((v - cm).abs()));
            let lm = levels.iter().sum::<f64>() / levels.len() as f64;
            let l_rms =
                (levels.iter().map(|v| (v - lm) * (v - lm)).sum::<f64>() / levels.len() as f64).sqrt();
            let l_max = levels.iter().fold(0.0f64, |a, v| a.max((v - lm).abs()));
            println!(
                "{:<12} {hz:>7.1}  {c_rms:>10.2} {c_max:>10.2}   {l_rms:>10.2} {l_max:>10.2}",
                mode.label()
            );
        }
    }
    println!("\ncents = fundamental wandering about its own mean; dB = envelope breathing.");
    println!("The source is a synthetic harmonic tone: both are zero going in.");
    Ok(())
}


/// The battery a mastered full mix actually exercises.
///
/// Each row isolates one failure the design already names, so a change can be
/// argued from a number rather than an impression (validation.md sec.5). None
/// of this replaces listening; it just stops the listening from being the only
/// evidence.
fn cmd_quality(opts: &Options) -> Result<(), String> {
    let rate = 48_000u32;
    let alpha = opts.f64_or("alpha", 1.0)?;
    let semitones = opts.f64_or("semitones", 3.0)?;
    let fft = opts
        .get("fft")
        .map(|v| v.parse::<usize>())
        .transpose()
        .map_err(|e| e.to_string())?;
    let lowfft = opts
        .get("lowfft")
        .map(|v| v.parse::<usize>())
        .transpose()
        .map_err(|e| e.to_string())?;
    let protect_s = opts.f64_or("protect", 12.0)? / 1000.0;
    let modes: Vec<EngineMode> = match opts.get("mode") {
        Some(m) => vec![EngineMode::parse(m).ok_or_else(|| format!("unknown mode `{m}`"))?],
        None => vec![EngineMode::Polyphonic, EngineMode::Hybrid],
    };
    let p = 2f64.powf(semitones / 12.0);
    let secs = 3.0f64;
    let n = (rate as f64 * secs) as usize;

    let render = |src: &AudioBuffer, mode: EngineMode| -> Result<(AudioBuffer, usize), String> {
        let id = SourceIdentity::of(src, rate);
        let mut doc = EditDocument::constant(id, alpha, semitones, mode);
        doc.quality = QualityProfile::Offline;
        let a = analyze(src, rate, "quality", &AnalysisSettings::default());
        let o = CompileOptions {
            stft_size: fft,
            low_stft_size: lowfft,
            low_gate: opts.has("lowgate"),
            protect_seconds: protect_s,
            ..CompileOptions::default()
        };
        let plan = compile(&doc, Some(&a), &o).map_err(|e| e.to_string())?;
        let win = plan.cfg.stft_size.unwrap_or(0);
        let (out, _) = render_offline(&plan, src, 1024).map_err(|e| e.to_string())?;
        Ok((out, win))
    };

    let show = |w: usize| if w == 0 { "default".to_string() } else { w.to_string() };
    println!(
        "alpha {alpha}  ·  {semitones:+} semitones  ·  fft {}\n",
        fft.map(|f| f.to_string()).unwrap_or_else(|| "engine default".into())
    );

    for mode in modes {
        println!("=== {} ===", mode.label());

        // --- bass: does the fundamental hold still ----------------------
        for hz in [55.0f64, 82.5] {
            let src = fixtures::harmonics(n, rate, hz, 8, 2);
            let (out, win) = render(&src, mode)?;
            let expect = hz * p;
            let mut cents: Vec<f64> = Vec::new();
            let mut at = rate as usize / 4;
            while at + 4096 + rate as usize / 4 < out.frames() {
                if let Some(f) = metrics::dominant_hz(&out, rate, at, 4096) {
                    cents.push(metrics::pitch_error_cents(f, expect));
                }
                at += 2048;
            }
            let m = cents.iter().sum::<f64>() / cents.len().max(1) as f64;
            let rms = (cents.iter().map(|v| (v - m) * (v - m)).sum::<f64>()
                / cents.len().max(1) as f64)
                .sqrt();
            println!("  bass {hz:>5.1} Hz     pitch wobble {rms:>6.2} cents rms   window {}", show(win));
        }

        // --- vocal: a source-filter vowel -------------------------------
        {
            let src = fixtures::vowel(n, rate, 130.0, [730.0, 1090.0, 2440.0], 2);
            let (out, win) = render(&src, mode)?;
            let expect = 130.0 * p;
            let mut cents: Vec<f64> = Vec::new();
            let mut at = rate as usize / 4;
            while at + 4096 + rate as usize / 4 < out.frames() {
                if let Some(f) = metrics::dominant_hz(&out, rate, at, 4096) {
                    cents.push(metrics::pitch_error_cents(f, expect));
                }
                at += 2048;
            }
            let m = cents.iter().sum::<f64>() / cents.len().max(1) as f64;
            let rms = (cents.iter().map(|v| (v - m) * (v - m)).sum::<f64>()
                / cents.len().max(1) as f64)
                .sqrt();
            let env = metrics::envelope_db(&out, 2048, 512);
            let a = env.len() / 6;
            let b = env.len() - env.len() / 6;
            let em = env[a..b].iter().sum::<f64>() / (b - a) as f64;
            let e_rms =
                (env[a..b].iter().map(|v| (v - em) * (v - em)).sum::<f64>() / (b - a) as f64).sqrt();
            println!(
                "  vowel /a/ 130 Hz  pitch wobble {rms:>6.2} cents rms   level wobble {e_rms:>5.2} dB rms   window {}",
                show(win)
            );
        }

        // --- drum transient ---------------------------------------------
        {
            let period = rate as usize / 4;
            let src = fixtures::drum_hits(n, rate, period, 2);
            let (out, win) = render(&src, mode)?;
            let src_peaks = metrics::peak_positions(&src, 0.5, period / 2);
            let out_peaks = metrics::peak_positions(&out, 0.5, period / 2);
            let mut rise_src = Vec::new();
            let mut rise_out = Vec::new();
            let mut pre_src = Vec::new();
            let mut pre_out = Vec::new();
            for q in src_peaks.iter().skip(1) {
                if let Some(v) = metrics::attack_rise_ms(&src, *q, rate) {
                    rise_src.push(v);
                }
                if let Some(v) = metrics::pre_attack_db(&src, *q, rate) {
                    pre_src.push(v);
                }
            }
            for q in out_peaks.iter().skip(1) {
                if let Some(v) = metrics::attack_rise_ms(&out, *q, rate) {
                    rise_out.push(v);
                }
                if let Some(v) = metrics::pre_attack_db(&out, *q, rate) {
                    pre_out.push(v);
                }
            }
            let mean = |v: &Vec<f64>| {
                if v.is_empty() { f64::NAN } else { v.iter().sum::<f64>() / v.len() as f64 }
            };
            // Onset placement: compare each output hit with where the map put it.
            let mut err_ms: Vec<f64> = Vec::new();
            for (i, q) in out_peaks.iter().enumerate() {
                if let Some(s) = src_peaks.get(i) {
                    let want = (*s as f64) * alpha;
                    err_ms.push((*q as f64 - want) * 1000.0 / rate as f64);
                }
            }
            let worst = err_ms.iter().fold(0.0f64, |a, v| a.max(v.abs()));
            println!(
                "  drum transient    rise {:>5.2} -> {:>5.2} ms   pre-echo {:>6.1} -> {:>6.1} dB   onset err {:>5.2} ms   window {}",
                mean(&rise_src),
                mean(&rise_out),
                mean(&pre_src),
                mean(&pre_out),
                worst,
                show(win)
            );
        }

        // --- bass sustained under drum hits -------------------------------
        {
            let src = fixtures::bass_under_hits(n, rate, 62.0, rate as usize / 4, 2);
            let (out, win) = render(&src, mode)?;
            let r_src = metrics::bass_roughness_db(&src, rate, 200.0);
            let r_out = metrics::bass_roughness_db(&out, rate, 200.0);
            let expect = 62.0 * p;
            let lp = metrics::lowpass(&out, 300.0, rate);
            let mut cents: Vec<f64> = Vec::new();
            let mut at = rate as usize / 4;
            while at + 8192 + rate as usize / 4 < lp.frames() {
                if let Some(f) = metrics::dominant_hz(&lp, rate, at, 8192) {
                    cents.push(metrics::pitch_error_cents(f, expect));
                }
                at += 4096;
            }
            let m = cents.iter().sum::<f64>() / cents.len().max(1) as f64;
            let c_rms = (cents.iter().map(|v| (v - m) * (v - m)).sum::<f64>()
                / cents.len().max(1) as f64)
                .sqrt();
            println!(
                "  bass under drums  roughness {r_src:>5.2} -> {r_out:>5.2} dB rms   pitch wobble {c_rms:>6.2} cents   window {}",
                show(win)
            );
        }

        // --- reverb tail --------------------------------------------------
        {
            let src = fixtures::noise_tail(n, rate, 1.8, 2);
            let (out, _) = render(&src, mode)?;
            let r_src = metrics::tail_roughness_db(&src, rate, 0.2, 1.2);
            let r_out = metrics::tail_roughness_db(&out, rate, 0.2, 1.2);
            println!("  reverb tail       roughness {r_src:>5.2} -> {r_out:>5.2} dB rms");
        }

        // --- stereo image -------------------------------------------------
        {
            let mono = fixtures::drum_hits(n, rate, rate as usize / 4, 1);
            let delay = 24usize;
            let src = fixtures::delayed_stereo(&mono, delay);
            let (out, _) = render(&src, mode)?;
            let lag = metrics::interchannel_lag(&out, rate as usize / 2, 16384, 200);
            let corr = metrics::channel_correlation(&out).unwrap_or(0.0);

            let ident = fixtures::harmonics(n, rate, 220.0, 8, 2);
            let (out_i, _) = render(&ident, mode)?;
            let same = out_i.channel(0) == out_i.channel(1);

            let inv = fixtures::inverted_stereo(&fixtures::harmonics(n, rate, 220.0, 8, 1));
            let (out_v, _) = render(&inv, mode)?;
            let corr_v = metrics::channel_correlation(&out_v).unwrap_or(0.0);

            println!(
                "  stereo image      mic delay {delay} -> {} frames   corr {corr:+.3}   identical {same}   inverted {corr_v:+.3}",
                lag.map(|v| v.to_string()).unwrap_or_else(|| "?".into())
            );
        }
        println!();
    }
    Ok(())
}


/// Does Attack Protect change the level?
///
/// It must not. Protection is a statement about *how* the engine treats a
/// transient - how it locks phase, where it re-anchors - and none of that is a
/// gain decision. So the honest test is to render the same material twice, with
/// protection off and on, and compare the two short-time envelopes block by
/// block: a correct implementation differs in phase detail and not in level,
/// and anything that ducks shows up here as a negative excursion clustered on
/// the transients.
fn cmd_protect(opts: &Options) -> Result<(), String> {
    let rate = 48_000u32;
    let alpha = opts.f64_or("alpha", 1.0)?;
    let semitones = opts.f64_or("semitones", 2.0)?;
    let protect_ms = opts.f64_or("protect", 12.0)?;
    let modes: Vec<EngineMode> = match opts.get("mode") {
        Some(m) => vec![EngineMode::parse(m).ok_or_else(|| format!("unknown mode `{m}`"))?],
        None => vec![EngineMode::Polyphonic, EngineMode::Hybrid],
    };
    let n = rate as usize * 3;

    let cases: Vec<(&str, AudioBuffer)> = vec![
        ("drums", fixtures::drum_hits(n, rate, rate as usize / 4, 2)),
        ("bass+drums", fixtures::bass_under_hits(n, rate, 62.0, rate as usize / 4, 2)),
        ("mixed", fixtures::mixed(n, rate, 2)),
    ];

    let render = |src: &AudioBuffer, mode: EngineMode, ms: f64| -> Result<AudioBuffer, String> {
        let id = SourceIdentity::of(src, rate);
        let mut doc = EditDocument::constant(id, alpha, semitones, mode);
        doc.quality = QualityProfile::Offline;
        let a = analyze(src, rate, "protect", &AnalysisSettings::default());
        let o = CompileOptions { protect_seconds: ms / 1000.0, ..CompileOptions::default() };
        let plan = compile(&doc, Some(&a), &o).map_err(|e| e.to_string())?;
        let (out, _) = render_offline(&plan, src, 1024).map_err(|e| e.to_string())?;
        Ok(out)
    };

    println!(
        "alpha {alpha}  ·  {semitones:+} semitones  ·  protect {protect_ms} ms vs 0 ms\n"
    );
    println!(
        "{:<12} {:<12} {:>10} {:>10} {:>10} {:>10}",
        "engine", "material", "total dB", "hit mean", "hit worst", "peak dB"
    );

    for mode in modes {
        for (name, src) in &cases {
            let off = render(src, mode, 0.0)?;
            let on = render(src, mode, protect_ms)?;
            // Short blocks: a duck that lasts a couple of window lengths has to
            // be visible, so the block cannot be longer than the artifact.
            let win = 512usize;
            let hop = 128usize;
            let a = metrics::envelope_db(&off, win, hop);
            let b = metrics::envelope_db(&on, win, hop);
            let len = a.len().min(b.len());
            let skip = rate as usize / 4 / hop;
            if len <= skip * 2 + 4 {
                println!("{:<12} {:<12} not enough material", mode.label(), name);
                continue;
            }
            // Ducking is a *level* question, so it has to be measured over a
            // span long enough that moving an attack by a few milliseconds -
            // which is the whole point of protection - does not read as a dip.
            // A block-by-block compare on sparse drums cannot tell the two
            // apart: it showed -25 dB "dips" beside +14 dB "lifts", which was
            // one attack landing in the next block, not a gate.
            let energy = |env: &[f64], from: usize, to: usize| -> f64 {
                let mut acc = 0.0;
                for v in &env[from.min(env.len())..to.min(env.len())] {
                    acc += 10f64.powf(v / 10.0);
                }
                acc
            };
            // Windows centred on the transients themselves, -50 to +150 ms:
            // wide enough that the attack cannot leave the window it is in.
            let pre = (rate as usize / 20) / hop;
            let post = (rate as usize * 3 / 20) / hop;
            let hits = metrics::peak_positions(&off, 0.45, rate as usize / 8);
            let mut worst_hit = 0.0f64;
            let mut hit_sum = 0.0f64;
            let mut hit_n = 0usize;
            for h in hits.iter() {
                let c = h / hop;
                if c < pre + skip || c + post + skip >= len {
                    continue;
                }
                let ea = energy(&a, c - pre, c + post);
                let eb = energy(&b, c - pre, c + post);
                if ea <= 1e-9 {
                    continue;
                }
                let d = 10.0 * (eb / ea).log10();
                hit_sum += d;
                hit_n += 1;
                if d < worst_hit {
                    worst_hit = d;
                }
            }
            let mean_hit = if hit_n == 0 { 0.0 } else { hit_sum / hit_n as f64 };
            let total_a = energy(&a, skip, len - skip);
            let total_b = energy(&b, skip, len - skip);
            let total_db = 10.0 * (total_b / total_a.max(1e-12)).log10();
            let peak_db = 20.0
                * ((on.peak().max(1e-9) as f64) / (off.peak().max(1e-9) as f64)).log10();
            println!(
                "{:<12} {:<12} {total_db:>10.2} {mean_hit:>10.2} {worst_hit:>10.2} {peak_db:>10.2}",
                mode.label(),
                name
            );
        }
    }
    println!(
        "
total = whole-render energy. hit mean/worst = energy in a -50..+150 ms"
    );
    println!("window around each transient, protect-on minus protect-off, in dB.");
    println!(
        "Attack Protect is a phase decision: it may move an attack, never duck one."
    );
    Ok(())
}

fn build_document(
    source: &AudioBuffer,
    rate: u32,
    opts: &Options,
) -> Result<EditDocument, String> {
    let alpha = opts.f64_or("alpha", 1.0)?;
    let semitones = opts.f64_or("semitones", 0.0)?;
    let mode = EngineMode::parse(opts.get("mode").unwrap_or("auto"))
        .ok_or_else(|| format!("unknown mode `{}`", opts.get("mode").unwrap_or("")))?;
    let quality = match opts.get("quality").unwrap_or("offline") {
        "offline" => QualityProfile::Offline,
        "realtime" => QualityProfile::Realtime,
        other => return Err(format!("unknown quality `{other}`")),
    };
    let id = SourceIdentity::of(source, rate);
    let mut doc = EditDocument::constant(id, alpha, semitones, mode);
    doc.quality = quality;
    doc.formant = parse_formant(opts.get("formant").unwrap_or("follow"))?;
    doc.deterministic_seed = opts.u64_or("seed", 0)?;

    for (s, t) in &opts.anchors {
        doc.anchors.push(WarpAnchor::new(*s, *t, AnchorKind::User));
    }
    if !opts.anchors.is_empty() {
        doc.anchors.sort_by_key(|a| a.source_frame);
        doc.anchors.dedup_by_key(|a| a.source_frame);
    }
    Ok(doc)
}

fn cmd_render(opts: &Options) -> Result<(), String> {
    let input = PathBuf::from(opts.req("in")?);
    let output = PathBuf::from(opts.req("out")?);
    let block = opts.usize_or("block", 1024)?;

    let file = wav::read(&input).map_err(|e| format!("{}: {e}", input.display()))?;
    file.audio.check_finite().map_err(|e| e.to_string())?;
    let doc = build_document(&file.audio, file.sample_rate, opts)?;

    let t0 = Instant::now();
    let needs_analysis = matches!(
        doc.mode,
        EngineMode::Auto | EngineMode::Percussive | EngineMode::Polyphonic | EngineMode::Hybrid
    );
    let analysis = if needs_analysis {
        Some(analyze(&file.audio, file.sample_rate, &doc.source.id, &AnalysisSettings::default()))
    } else {
        None
    };
    let analysis_ms = t0.elapsed().as_secs_f64() * 1000.0;

    let copts = CompileOptions {
        stft_size: opts.get("fft").map(|v| v.parse::<usize>()).transpose()
            .map_err(|e: std::num::ParseIntError| e.to_string())?,
        low_stft_size: opts.get("lowfft").map(|v| v.parse::<usize>()).transpose()
            .map_err(|e: std::num::ParseIntError| e.to_string())?,
        low_gate: opts.has("lowgate"),
        ..CompileOptions::default()
    };
    let plan = compile(&doc, analysis.as_ref(), &copts).map_err(|e| e.to_string())?;
    let t1 = Instant::now();
    let (out, report) = render_offline(&plan, &file.audio, block).map_err(|e| e.to_string())?;
    let render_ms = t1.elapsed().as_secs_f64() * 1000.0;

    let format =
        if opts.has("float") { wav::WriteFormat::Float32 } else { wav::WriteFormat::Pcm16 };
    wav::write(&output, &out, file.sample_rate, format)
        .map_err(|e| format!("{}: {e}", output.display()))?;

    let lvl = metrics::level(&out);
    let audio_seconds = out.frames() as f64 / file.sample_rate as f64;
    if opts.has("json") {
        let v = serde_json::json!({
            "status": "ok",
            "engine": plan.mode.label(),
            "auto_reason": plan.auto_reason,
            "render_key": plan.render_key,
            "sample_rate": file.sample_rate,
            "channels": out.channel_count(),
            "source_frames": file.audio.frames(),
            "expected_frames": report.expected_frames,
            "actual_frames": report.produced_frames,
            "alpha_range": [plan.alpha_range().0, plan.alpha_range().1],
            "internal_ratio_range": [plan.internal_ratio_range().0, plan.internal_ratio_range().1],
            "pitch_semitones": doc.pitch_semitones,
            "peak": lvl.peak,
            "rms_dbfs": lvl.rms_dbfs,
            "analysis_ms": analysis_ms,
            "render_ms": render_ms,
            "realtime_factor": audio_seconds / (render_ms / 1000.0).max(1e-9),
            "process_calls": report.process_calls,
        });
        println!("{}", serde_json::to_string_pretty(&v).unwrap());
    } else {
        println!("engine        {}", plan.mode.label());
        if let Some(r) = &plan.auto_reason {
            println!("auto chose it {r}");
        }
        if let Some(w) = plan.cfg.stft_size {
            println!(
                "window        {w} frames ({:.1} ms) chosen from the analysis",
                w as f64 * 1000.0 / file.sample_rate as f64
            );
        }
        match plan.cfg.low_stft_size {
            Some(0) | None => {}
            Some(w) => println!(
                "low path      {w} frames ({:.1} ms) below {:.0}-{:.0} Hz",
                w as f64 * 1000.0 / file.sample_rate as f64,
                solfege::engines::pv::CROSSOVER_LO_HZ,
                solfege::engines::pv::CROSSOVER_HI_HZ
            ),
        }
        println!(
            "length        {} -> {} frames (expected {})",
            file.audio.frames(),
            report.produced_frames,
            report.expected_frames
        );
        println!(
            "ratio         {:.4}..{:.4} (internal {:.4}..{:.4})",
            plan.alpha_range().0,
            plan.alpha_range().1,
            plan.internal_ratio_range().0,
            plan.internal_ratio_range().1
        );
        println!("level         peak {:.4}  rms {:.1} dBFS", lvl.peak, lvl.rms_dbfs);
        if lvl.peak > 1.0 {
            // Not normalised on purpose (system-design.md sec.12). Say what it
            // would take, and leave the decision where it belongs.
            println!(
                "              above full scale by {:.2} dB — phase changes and overlap-add \
                 can both exceed the source peak",
                20.0 * (lvl.peak as f64).log10()
            );
            println!(
                "              nothing was normalised for you; -{:.2} dB of gain would fit it",
                20.0 * (lvl.peak as f64).log10() + 0.1
            );
        }
        println!(
            "time          analysis {analysis_ms:.0} ms, render {render_ms:.0} ms ({:.1}x realtime)",
            audio_seconds / (render_ms / 1000.0).max(1e-9)
        );
        println!("wrote         {}", output.display());
    }
    Ok(())
}

fn cmd_analyze(opts: &Options) -> Result<(), String> {
    let input = PathBuf::from(opts.req("in")?);
    let file = wav::read(&input).map_err(|e| format!("{}: {e}", input.display()))?;
    let id = SourceIdentity::of(&file.audio, file.sample_rate);
    let a = analyze(&file.audio, file.sample_rate, &id.id, &AnalysisSettings::default());
    if opts.has("json") {
        println!("{}", serde_json::to_string_pretty(&summarize(&a, &id.id)).unwrap());
    } else {
        println!("source        {}", id.id);
        println!(
            "format        {} Hz, {} ch, {} frames ({:.2} s)",
            file.sample_rate,
            file.audio.channel_count(),
            file.audio.frames(),
            file.audio.frames() as f64 / file.sample_rate as f64
        );
        println!("class         {}", a.class.label());
        println!("suggests      {}", a.class.suggested_mode().label());
        println!("percussivity  {:.3}", a.percussivity);
        println!("tonality      {:.3}", a.tonality);
        println!("onsets        {}", a.onsets.len());
        println!("notes         {}", a.notes.len());
        for n in a.notes.iter().take(12) {
            println!(
                "  note {:>3}  {:>8.3}s..{:>8.3}s  {:>7.1} Hz  midi {:>5.1}  conf {:.2}",
                n.id.0,
                n.source_start as f64 / file.sample_rate as f64,
                n.source_end as f64 / file.sample_rate as f64,
                n.median_hz,
                n.midi(),
                n.confidence
            );
        }
        if a.notes.len() > 12 {
            println!("  ... {} more", a.notes.len() - 12);
        }
    }
    Ok(())
}

fn summarize(a: &Analysis, id: &str) -> serde_json::Value {
    serde_json::json!({
        "analyzer_version": a.analyzer_version,
        "source_id": id,
        "sample_rate": a.sample_rate,
        "frames": a.frames,
        "class": a.class.label(),
        "suggested_mode": a.class.suggested_mode().label(),
        "percussivity": a.percussivity,
        "tonality": a.tonality,
        "onsets": a.onsets.iter().map(|o| o.frame).collect::<Vec<_>>(),
        "notes": a.notes,
    })
}

fn cmd_doc(opts: &Options) -> Result<(), String> {
    let input = PathBuf::from(opts.req("in")?);
    let output = PathBuf::from(opts.req("out")?);
    let file = wav::read(&input).map_err(|e| format!("{}: {e}", input.display()))?;
    let doc = build_document(&file.audio, file.sample_rate, opts)?;
    doc.validate().map_err(|e| e.to_string())?;
    std::fs::write(&output, doc.to_json()).map_err(|e| e.to_string())?;
    println!("wrote {} (canonical hash {})", output.display(), doc.canonical_hash());
    Ok(())
}

fn cmd_fixtures(opts: &Options) -> Result<(), String> {
    let dir = PathBuf::from(opts.req("out")?);
    let rate = opts.usize_or("rate", 48_000)? as u32;
    std::fs::create_dir_all(&dir).map_err(|e| e.to_string())?;
    let n = rate as usize * 3;

    let items: Vec<(&str, AudioBuffer)> = vec![
        ("silence", fixtures::silence(2, n)),
        ("sine-440", fixtures::sine(n, rate, 440.0, 0.5, 1)),
        ("sine-110", fixtures::sine(n, rate, 110.0, 0.5, 1)),
        ("harmonics-220", fixtures::harmonics(n, rate, 220.0, 8, 1)),
        ("chirp", fixtures::chirp(n, rate, 40.0, 12_000.0, 1)),
        ("impulses", fixtures::impulse_train(n, rate as usize / 4, 1, 0)),
        ("drums", fixtures::drum_hits(n, rate, rate as usize / 4, 1)),
        ("vowel-a", fixtures::vowel(n, rate, 130.0, [730.0, 1090.0, 2440.0], 1)),
        ("vowel-i", fixtures::vowel(n, rate, 130.0, [270.0, 2290.0, 3010.0], 1)),
        ("mixed", fixtures::mixed(n, rate, 2)),
        (
            "stereo-inverted",
            fixtures::inverted_stereo(&fixtures::harmonics(n, rate, 220.0, 8, 1)),
        ),
        (
            "stereo-delayed",
            fixtures::delayed_stereo(&fixtures::drum_hits(n, rate, rate as usize / 4, 1), 24),
        ),
    ];
    for (name, buf) in &items {
        let path = dir.join(format!("{name}.wav"));
        wav::write(&path, buf, rate, wav::WriteFormat::Float32)
            .map_err(|e| format!("{}: {e}", path.display()))?;
        println!("{:<18} {} frames", name, buf.frames());
    }
    println!("wrote {} fixtures to {}", items.len(), dir.display());
    Ok(())
}

/// Runs the correctness gates from validation.md sec.4 that do not need a
/// listener. A failure prints what was measured, not just a verdict.
fn cmd_selftest(opts: &Options) -> Result<(), String> {
    let rate = opts.usize_or("rate", 48_000)? as u32;
    let json = opts.has("json");
    let mut rows: Vec<serde_json::Value> = Vec::new();
    let mut failures = 0usize;

    let mut record = |name: &str, pass: bool, detail: String| {
        if !pass {
            failures += 1;
        }
        if json {
            rows.push(serde_json::json!({ "gate": name, "pass": pass, "detail": detail }));
        } else {
            println!("[{}] {:<26} {}", if pass { "pass" } else { "FAIL" }, name, detail);
        }
    };

    let n = rate as usize;
    let tone = fixtures::harmonics(n, rate, 220.0, 8, 1);
    let stereo = fixtures::inverted_stereo(&tone);
    let drums = fixtures::drum_hits(n, rate, rate as usize / 4, 1);

    // Identity: bypass must be sample-exact.
    {
        let id = SourceIdentity::of(&tone, rate);
        let doc = EditDocument::identity(id);
        let plan = compile(&doc, None, &CompileOptions::default()).map_err(|e| e.to_string())?;
        let (out, _) = render_offline(&plan, &tone, 1024).map_err(|e| e.to_string())?;
        let exact = metrics::is_bit_identical(&tone, &out);
        record("identity bypass", exact, format!("bit-identical: {exact}"));
    }

    // Endpoint: every mode delivers exactly M frames, at several ratios.
    for mode in [
        EngineMode::Tape,
        EngineMode::Monophonic,
        EngineMode::Polyphonic,
        EngineMode::Percussive,
        EngineMode::Hybrid,
        EngineMode::Texture,
    ] {
        let mut worst = String::new();
        let mut ok = true;
        for alpha in [0.5, 0.75, 1.0, 1.5, 2.0] {
            let src = if mode == EngineMode::Percussive { &drums } else { &tone };
            let id = SourceIdentity::of(src, rate);
            let mut doc = EditDocument::constant(id, alpha, 0.0, mode);
            doc.quality = QualityProfile::Offline;
            let a = analyze(src, rate, "selftest", &AnalysisSettings::default());
            let plan = match compile(&doc, Some(&a), &CompileOptions::default()) {
                Ok(p) => p,
                Err(e) => {
                    ok = false;
                    worst = format!("alpha {alpha}: {e}");
                    break;
                }
            };
            match render_offline(&plan, src, 1024) {
                Ok((out, rep)) => {
                    if out.frames() as u64 != rep.expected_frames {
                        ok = false;
                        worst = format!("alpha {alpha}: {} != {}", out.frames(), rep.expected_frames);
                    }
                    if metrics::any_non_finite(&out) {
                        ok = false;
                        worst = format!("alpha {alpha}: non-finite output");
                    }
                }
                Err(e) => {
                    ok = false;
                    worst = format!("alpha {alpha}: {e}");
                }
            }
        }
        record(
            &format!("endpoint {}", mode.label()),
            ok,
            if ok { "exact at every ratio".into() } else { worst },
        );
    }

    // Block invariance: one plan, two very different block schedules.
    {
        let id = SourceIdentity::of(&tone, rate);
        let doc = EditDocument::constant(id, 1.5, 0.0, EngineMode::Monophonic);
        let plan = compile(&doc, None, &CompileOptions::default()).map_err(|e| e.to_string())?;
        let (a, _) = render_offline(&plan, &tone, 1024).map_err(|e| e.to_string())?;
        let (b, _) = render_offline(&plan, &tone, 17).map_err(|e| e.to_string())?;
        let (c, _) =
            render_offline_random_blocks(&plan, &tone, 0xC0FFEE, 512).map_err(|e| e.to_string())?;
        let d1 = metrics::rms_difference_db(&a, &b);
        let d2 = metrics::rms_difference_db(&a, &c);
        let pass = d1 <= -100.0 && d2 <= -100.0;
        record(
            "block invariance",
            pass,
            format!("fixed-17 {d1:.1} dBFS, random {d2:.1} dBFS (target <= -100)"),
        );
    }

    // Steady pitch through the transpose path.
    {
        let sine = fixtures::sine(n, rate, 440.0, 0.5, 1);
        let id = SourceIdentity::of(&sine, rate);
        let doc = EditDocument::constant(id, 1.0, 7.0, EngineMode::Polyphonic);
        let plan = compile(&doc, None, &CompileOptions::default()).map_err(|e| e.to_string())?;
        let (out, _) = render_offline(&plan, &sine, 1024).map_err(|e| e.to_string())?;
        let expected = 440.0 * 2f64.powf(7.0 / 12.0);
        match metrics::dominant_hz(&out, rate, rate as usize / 4, 8192) {
            Some(hz) => {
                let cents = metrics::pitch_error_cents(hz, expected);
                record(
                    "pitch +7 semitones",
                    cents.abs() <= 5.0,
                    format!("{hz:.2} Hz vs {expected:.2} Hz ({cents:+.2} cents)"),
                );
            }
            None => record("pitch +7 semitones", false, "no dominant peak found".into()),
        }
    }

    // Stereo: an inverted pair must stay inverted.
    {
        let id = SourceIdentity::of(&stereo, rate);
        let doc = EditDocument::constant(id, 1.5, 0.0, EngineMode::Polyphonic);
        let plan = compile(&doc, None, &CompileOptions::default()).map_err(|e| e.to_string())?;
        let (out, _) = render_offline(&plan, &stereo, 1024).map_err(|e| e.to_string())?;
        let corr = metrics::channel_correlation(&out).unwrap_or(0.0);
        record(
            "stereo inversion kept",
            corr < -0.95,
            format!("inter-channel correlation {corr:+.4} (want < -0.95)"),
        );
    }

    // Anchors: a hard anchor lands where it was put.
    {
        let src = fixtures::impulse_train(n, rate as usize / 4, 1, 0);
        let id = SourceIdentity::of(&src, rate);
        let mut doc = EditDocument::constant(id, 1.0, 0.0, EngineMode::Percussive);
        let mid_src = rate as u64 / 2;
        let mid_out = mid_src + rate as u64 / 20;
        doc.anchors = vec![
            WarpAnchor::endpoint(0, 0),
            WarpAnchor::user(mid_src, mid_out),
            WarpAnchor::endpoint(n as u64, n as u64),
        ];
        let a = analyze(&src, rate, "selftest", &AnalysisSettings::default());
        let plan = compile(&doc, Some(&a), &CompileOptions::default()).map_err(|e| e.to_string())?;
        let (out, _) = render_offline(&plan, &src, 1024).map_err(|e| e.to_string())?;
        let peaks = metrics::peak_positions(&out, 0.4, rate as usize / 16);
        let want = mid_out as usize;
        let found = peaks.iter().map(|p| (*p as i64 - want as i64).abs()).min().unwrap_or(i64::MAX);
        let ms = found as f64 * 1000.0 / rate as f64;
        record(
            "hard anchor placement",
            ms <= 1.0,
            format!("nearest impulse {ms:.3} ms from the anchor (target <= 1 ms)"),
        );
    }

    // Typed rejections rather than silent repair.
    {
        let id = SourceIdentity::of(&tone, rate);
        let doc = EditDocument::constant(id, 1.5, 3.0, EngineMode::Tape);
        let rejected = compile(&doc, None, &CompileOptions::default()).is_err();
        record("tape + transpose rejected", rejected, format!("rejected: {rejected}"));
    }
    {
        let id = SourceIdentity::of(&tone, rate);
        let mut doc = EditDocument::constant(id, 1.0, 0.0, EngineMode::Monophonic);
        doc.anchors = vec![WarpAnchor::endpoint(0, 0), WarpAnchor::user(100, 50), WarpAnchor::endpoint(n as u64, n as u64)];
        doc.anchors[1].output_frame = 0; // crossed
        let rejected = doc.validate().is_err();
        record("crossed anchors rejected", rejected, format!("rejected: {rejected}"));
    }
    {
        let id = SourceIdentity::of(&tone, rate);
        let mut doc = EditDocument::constant(id, 4.0, 12.0, EngineMode::Monophonic);
        doc.quality = QualityProfile::Offline;
        // alpha 4 x pitch 2 = an internal 8x: outside the WSOLA range even
        // though each control is inside its own.
        let err = compile(&doc, None, &CompileOptions::default()).err();
        let ok = matches!(err, Some(solfege::PlanError::InternalRatio { .. }));
        record("internal ratio rejected", ok, format!("{err:?}"));
    }

    // Empty and one-frame sources.
    {
        let empty = AudioBuffer::silence(1, 0);
        let id = SourceIdentity::of(&empty, rate);
        let doc = EditDocument::identity(id);
        let plan = compile(&doc, None, &CompileOptions::default()).map_err(|e| e.to_string())?;
        let out = render_offline(&plan, &empty, 64);
        let ok = out.map(|(b, _)| b.frames() == 0).unwrap_or(false);
        record("empty source", ok, "0 frames in, 0 frames out".into());
    }

    if json {
        println!(
            "{}",
            serde_json::to_string_pretty(&serde_json::json!({
                "sample_rate": rate,
                "gates": rows,
                "failures": failures,
            }))
            .unwrap()
        );
    } else {
        println!("\n{} gate(s) failed", failures);
    }
    if failures > 0 {
        return Err(format!("{failures} gate(s) failed"));
    }
    Ok(())
}

/// Per-call timing against the block deadline (validation.md sec.6).
///
/// This measures the offline path on this machine, with no reference machine
/// chosen and no real-time thread involved, so it is a working number to
/// optimise against — not a latency promise.
fn cmd_bench(opts: &Options) -> Result<(), String> {
    let rate = opts.usize_or("rate", 48_000)? as u32;
    let block = opts.usize_or("block", 256)?;
    let alpha = opts.f64_or("alpha", 1.5)?;
    let semitones = opts.f64_or("semitones", 0.0)?;
    let seconds = opts.f64_or("seconds", 4.0)?;
    let modes: Vec<EngineMode> = match opts.get("mode") {
        Some(m) => vec![EngineMode::parse(m).ok_or_else(|| format!("unknown mode `{m}`"))?],
        None => vec![
            EngineMode::Tape,
            EngineMode::Monophonic,
            EngineMode::Polyphonic,
            EngineMode::Hybrid,
            EngineMode::Percussive,
            EngineMode::Texture,
        ],
    };

    let n = (rate as f64 * seconds) as usize;
    let src = fixtures::mixed(n, rate, 2);
    let deadline_ms = block as f64 * 1000.0 / rate as f64;
    let analysis = analyze(&src, rate, "bench", &AnalysisSettings::default());
    let mut rows = Vec::new();

    for mode in modes {
        let id = SourceIdentity::of(&src, rate);
        let mut doc = EditDocument::constant(id, alpha, semitones, mode);
        doc.quality = QualityProfile::Realtime;
        let plan = match compile(&doc, Some(&analysis), &CompileOptions::default()) {
            Ok(p) => p,
            Err(e) => {
                println!("{:<12} rejected: {e}", mode.label());
                continue;
            }
        };
        let mut engine = solfege::build_engine(&plan).map_err(|e| e.to_string())?;
        let expected = plan.output_frames();
        let channels = plan.cfg.channels;
        let mut scratch: Vec<Vec<f32>> = vec![vec![0.0; block]; channels];

        let mut times: Vec<f64> = Vec::new();
        let mut in_pos = 0usize;
        let mut out_pos = 0u64;
        while out_pos < expected {
            let want = (src.frames() - in_pos).min(block);
            let eoi = in_pos + want >= src.frames();
            let room = ((expected - out_pos) as usize).min(block);
            let t = Instant::now();
            let report = {
                let view = solfege::AudioViewMut::from_planar(&mut scratch, 0, room)
                    .map_err(|e| e.to_string())?;
                engine
                    .process(
                        solfege::AudioView::from_planar(src.planes(), in_pos, want)
                            .map_err(|e| e.to_string())?,
                        view,
                        eoi,
                    )
                    .map_err(|e| e.to_string())?
            };
            times.push(t.elapsed().as_secs_f64() * 1000.0);
            in_pos += report.consumed_frames;
            out_pos += report.produced_frames as u64;
            if report.produced_frames == 0 && report.consumed_frames == 0 {
                break;
            }
        }
        times.sort_by(|a, b| a.partial_cmp(b).unwrap());
        let pick = |q: f64| times[((times.len() as f64 - 1.0) * q).round() as usize];
        let (p50, p99, max) = (pick(0.5), pick(0.99), *times.last().unwrap_or(&0.0));
        let over = times.iter().filter(|t| **t > deadline_ms).count();
        rows.push(serde_json::json!({
            "engine": mode.label(),
            "block": block,
            "deadline_ms": deadline_ms,
            "p50_ms": p50,
            "p99_ms": p99,
            "max_ms": max,
            "calls": times.len(),
            "over_deadline": over,
            "p99_of_deadline": p99 / deadline_ms,
        }));
        if !opts.has("json") {
            println!(
                "{:<12} p50 {:>7.3} ms  p99 {:>7.3} ms  max {:>7.3} ms  p99/deadline {:>5.1}%  over {}/{}",
                mode.label(),
                p50,
                p99,
                max,
                100.0 * p99 / deadline_ms,
                over,
                times.len()
            );
        }
    }

    if opts.has("json") {
        println!(
            "{}",
            serde_json::to_string_pretty(&serde_json::json!({
                "sample_rate": rate,
                "block": block,
                "deadline_ms": deadline_ms,
                "alpha": alpha,
                "note": "offline path on this machine; no reference machine chosen, not a latency promise",
                "engines": rows,
            }))
            .unwrap()
        );
    } else {
        println!(
            "
block {block} at {rate} Hz = {deadline_ms:.3} ms per callback; target p99 <= 50% of that."
        );
        println!("No reference machine has been chosen, so these are working numbers, not a promise.");
    }
    Ok(())
}

/// Stream a file through the engine in real time and report what the callback
/// actually did. This is the same path the GUI uses: the engine runs inside the
/// audio callback, a worker feeds it, and underruns are counted rather than
/// hidden.
#[cfg(feature = "playback")]
fn cmd_play(opts: &Options) -> Result<(), String> {
    use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
    use solfege::stream::PlayerConfig;

    let input = PathBuf::from(opts.req("in")?);
    let seconds = opts.f64_or("seconds", 10.0)?;
    let file = wav::read(&input).map_err(|e| format!("{}: {e}", input.display()))?;
    file.audio.check_finite().map_err(|e| e.to_string())?;
    let rate = file.sample_rate;
    let doc = build_document(&file.audio, rate, opts)?;

    let needs_analysis = matches!(
        doc.mode,
        EngineMode::Auto | EngineMode::Percussive | EngineMode::Polyphonic | EngineMode::Hybrid
    );
    let analysis = if needs_analysis {
        Some(analyze(&file.audio, rate, &doc.source.id, &AnalysisSettings::default()))
    } else {
        None
    };
    let plan = compile(&doc, analysis.as_ref(), &CompileOptions::default())
        .map_err(|e| e.to_string())?;
    let plan_channels = plan.cfg.channels;
    println!("engine        {}", plan.mode.label());
    if let Some(r) = &plan.auto_reason {
        println!("auto chose it {r}");
    }

    let host = cpal::default_host();
    let device = host.default_output_device().ok_or("no output device")?;
    let mut chosen = device.default_output_config().map_err(|e| e.to_string())?;
    if let Ok(ranges) = device.supported_output_configs() {
        for r in ranges {
            if r.min_sample_rate().0 <= rate
                && rate <= r.max_sample_rate().0
                && r.sample_format() == cpal::SampleFormat::F32
            {
                chosen = r.with_sample_rate(cpal::SampleRate(rate));
                break;
            }
        }
    }
    let device_rate = chosen.sample_rate().0;
    let device_channels = chosen.channels() as usize;
    if device_rate != rate {
        println!(
            "note          device runs at {device_rate} Hz, file at {rate} Hz;              the CLI does not resample, so pitch and speed will be off by              {:.3}x. Use a device at the file rate for a fair listen.",
            rate as f64 / device_rate as f64
        );
    }
    println!(
        "device        {device_rate} Hz, {device_channels} ch  ·  engine {rate} Hz, {plan_channels} ch"
    );

    let max_block = 4096usize;
    let source = std::sync::Arc::new(file.audio);
    let (mut player, handle) = solfege::stream::start(
        source,
        rate,
        plan,
        0,
        PlayerConfig { max_block, ..PlayerConfig::default() },
    );
    // Monitor trim only: the engine output is untouched and `peak` below is
    // still measured before this is applied.
    let trim = opts.f64_or("trim", 0.0)? as f32;
    handle.set_trim_db(trim);
    if trim != 0.0 {
        println!("monitor trim  {trim:+.2} dB (does not change what the engine produced)");
    }
    let metrics = handle.metrics().clone();
    let cb_metrics = metrics.clone();
    let mut scratch = vec![0.0f32; max_block * plan_channels];

    let stream = device
        .build_output_stream(
            &chosen.config(),
            move |data: &mut [f32], _: &cpal::OutputCallbackInfo| {
                let t0 = Instant::now();
                let frames = (data.len() / device_channels).min(max_block);
                player.fill(&mut scratch, frames);
                for f in 0..frames {
                    for c in 0..device_channels {
                        data[f * device_channels + c] =
                            scratch[f * plan_channels + c.min(plan_channels - 1)];
                    }
                }
                cb_metrics.record_call(t0.elapsed().as_micros() as u64);
            },
            move |e| eprintln!("audio stream error: {e}"),
            None,
        )
        .map_err(|e| e.to_string())?;
    stream.play().map_err(|e| e.to_string())?;

    // Optionally exercise the live plan swap: recompile with a moving alpha and
    // hand it to the worker, which warms a new voice for the callback to
    // crossfade in without stopping.
    let sweep = opts.f64_or("sweep", 0.0)?;
    let mut next_swap = Instant::now() + std::time::Duration::from_secs_f64(sweep.max(0.001));
    let mut swap_n = 0u32;

    let deadline = Instant::now() + std::time::Duration::from_secs_f64(seconds);
    while Instant::now() < deadline && !handle.is_finished() {
        if sweep > 0.0 && Instant::now() >= next_swap {
            swap_n += 1;
            let a = alpha_for_sweep(swap_n);
            let mut d = doc.clone();
            d.set_output_frames(((d.source.frames as f64) * a).round().max(1.0) as u64)
                .map_err(|e| e.to_string())?;
            match compile(&d, analysis.as_ref(), &CompileOptions::default()) {
                Ok(p) => handle.set_plan(p, handle.source_frame()),
                Err(e) => println!("
swap rejected: {e}"),
            }
            next_swap = Instant::now() + std::time::Duration::from_secs_f64(sweep);
        }
        std::thread::sleep(std::time::Duration::from_millis(200));
        print!(
            "\rplaying {:>6.2}s  p99 {:>6.2} ms  max {:>6.2} ms  underruns {}   ",
            handle.source_frame() as f64 / rate as f64,
            metrics.quantile_ms(0.99),
            metrics.max_ms(),
            metrics.underruns.load(std::sync::atomic::Ordering::Relaxed)
        );
        use std::io::Write;
        let _ = std::io::stdout().flush();
    }
    println!();
    drop(stream);

    let under = metrics.underruns.load(std::sync::atomic::Ordering::Relaxed);
    println!(
        "callbacks     {}  ·  p99 {:.2} ms  ·  max {:.2} ms",
        metrics.callbacks.load(std::sync::atomic::Ordering::Relaxed),
        metrics.quantile_ms(0.99),
        metrics.max_ms()
    );
    println!(
        "underruns     {under}  ·  starved {} frames  ·  plan swaps {}",
        metrics.starved_frames.load(std::sync::atomic::Ordering::Relaxed),
        metrics.swaps.load(std::sync::atomic::Ordering::Relaxed)
    );
    // Nothing is normalised on the way out, so a hot source can leave the
    // engine above full scale and clip at the device. Report it rather than
    // quietly turn it down (system-design.md sec.12).
    let peak = handle.peak();
    let dev = handle.device_peak();
    let clipped = handle.clipped_frames();
    let db = |v: f32| if v > 0.0 { 20.0 * (v as f64).log10() } else { -99.0 };
    println!(
        "engine peak   {peak:.4} ({:+.2} dBFS)   what the render contains",
        db(peak)
    );
    println!(
        "device peak   {dev:.4} ({:+.2} dBFS)   what the converter received  ·  clipped {clipped} frames",
        db(dev)
    );
    if clipped > 0 {
        println!(
            "              the engine went above 0 dBFS; overlap-add can exceed the source peak."
        );
        println!("              Nothing here normalises for you: trim the level.");
    }
    if under > 0 {
        println!("              (an underrun is faded to silence and counted, never patched with the dry signal)");
    }
    Ok(())
}

/// Alpha for the nth live swap: a small walk around 1.0 so the crossfade is
/// audible without leaving any engine's supported range.
#[cfg(feature = "playback")]
fn alpha_for_sweep(n: u32) -> f64 {
    const STEPS: [f64; 6] = [1.5, 0.8, 2.0, 1.0, 0.6, 1.25];
    STEPS[(n as usize - 1) % STEPS.len()]
}

#[cfg(not(feature = "playback"))]
fn cmd_play(_opts: &Options) -> Result<(), String> {
    Err("this build has no playback support; rebuild with --features playback".into())
}
