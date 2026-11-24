//! Zero-dependency procedural PCM sound effects synthesizer for `infimum-web`.
//!
//! Synthesizes crisp 44.1 kHz 16-bit mono sound effects (marble clack, hop, click,
//! spawn, crystal chime clear, error thud, win fanfare, and custom tones) and mixes
//! up to 8 polyphonic voices into a lazy PipeWire/PulseAudio (`paplay --raw`) stream.
//! Automatically stays silent during `--headless-check` and `cargo test`.

#[cfg(not(target_arch = "wasm32"))]
use std::io::Write;
#[cfg(not(target_arch = "wasm32"))]
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
#[cfg(not(target_arch = "wasm32"))]
use std::sync::mpsc::{self, Sender};
use std::sync::OnceLock;
#[cfg(not(target_arch = "wasm32"))]
use std::thread;
#[cfg(not(target_arch = "wasm32"))]
use std::time::{Duration, Instant};
use wasmi::{Engine, Linker, Module, Store};

const AUDIO_AXW: &[u8] = include_bytes!("../capsules/core/audio_core.ifw");
const PCM_BUF_WORD: usize = 64;
const MAX_SAMPLES: usize = 66_150;
const AUDIO_FUEL: u64 = 120_000_000;
#[cfg(not(target_arch = "wasm32"))]
const CHUNK_SAMPLES: usize = 512; // ~11.6 ms low-latency mix buffer

static HEADLESS_SILENT: AtomicBool = AtomicBool::new(false);

pub fn set_headless(headless: bool) {
    HEADLESS_SILENT.store(headless, Ordering::SeqCst);
}

#[cfg(target_arch = "wasm32")]
std::thread_local! {
    static WEB_PCM_QUEUE: std::cell::RefCell<Vec<Vec<f32>>> = const { std::cell::RefCell::new(Vec::new()) };
}

#[cfg(target_arch = "wasm32")]
pub fn drain_web_pcm() -> Vec<Vec<f32>> {
    WEB_PCM_QUEUE.with(|q| std::mem::take(&mut *q.borrow_mut()))
}

#[cfg(target_arch = "wasm32")]
pub fn play_preset(preset: i64, pitch_pct: i64, vol_pct: i64) {
    if HEADLESS_SILENT.load(Ordering::SeqCst) || vol_pct <= 0 {
        return;
    }
    let samples = synth_preset(
        preset.clamp(1, 18),
        pitch_pct.clamp(40, 250) as f32 / 100.0,
        vol_pct.clamp(0, 100) as f32 / 100.0,
    );
    if !samples.is_empty() {
        WEB_PCM_QUEUE.with(|q| {
            let mut v = q.borrow_mut();
            if v.len() < 8 {
                v.push(samples);
            }
        });
    }
}

#[cfg(target_arch = "wasm32")]
pub fn play_tone(freq_hz: i64, dur_ms: i64, wave: i64) {
    if HEADLESS_SILENT.load(Ordering::SeqCst) {
        return;
    }
    let samples = synth_tone(
        freq_hz.clamp(60, 4000) as f32,
        dur_ms.clamp(10, 1500) as f32,
        wave.clamp(0, 3),
        0.55,
    );
    if !samples.is_empty() {
        WEB_PCM_QUEUE.with(|q| {
            let mut v = q.borrow_mut();
            if v.len() < 8 {
                v.push(samples);
            }
        });
    }
}

#[cfg(not(target_arch = "wasm32"))]
#[derive(Clone, Copy, Debug)]
enum AudioCmd {
    Preset {
        preset: i64,
        pitch_pct: i64,
        vol_pct: i64,
    },
    Tone {
        freq_hz: i64,
        dur_ms: i64,
        wave: i64,
    },
}

#[cfg(not(target_arch = "wasm32"))]
struct Voice {
    samples: Vec<f32>,
    pos: usize,
}

#[cfg(not(target_arch = "wasm32"))]
fn audio_sender() -> &'static Sender<AudioCmd> {
    static TX: OnceLock<Sender<AudioCmd>> = OnceLock::new();
    TX.get_or_init(|| {
        let (tx, rx) = mpsc::channel::<AudioCmd>();
        thread::Builder::new()
            .name("infimum-audio-mixer".to_string())
            .spawn(move || mixer_loop(rx))
            .ok();
        tx
    })
}

#[cfg(not(target_arch = "wasm32"))]
pub fn play_preset(preset: i64, pitch_pct: i64, vol_pct: i64) {
    if cfg!(test) || HEADLESS_SILENT.load(Ordering::SeqCst) || vol_pct <= 0 {
        return;
    }
    let _ = audio_sender().send(AudioCmd::Preset {
        preset: preset.clamp(1, 18),
        pitch_pct: pitch_pct.clamp(40, 250),
        vol_pct: vol_pct.clamp(0, 100),
    });
}

#[cfg(not(target_arch = "wasm32"))]
pub fn play_tone(freq_hz: i64, dur_ms: i64, wave: i64) {
    if cfg!(test) || HEADLESS_SILENT.load(Ordering::SeqCst) {
        return;
    }
    let _ = audio_sender().send(AudioCmd::Tone {
        freq_hz: freq_hz.clamp(60, 4000),
        dur_ms: dur_ms.clamp(10, 1500),
        wave: wave.clamp(0, 3),
    });
}

#[cfg(not(target_arch = "wasm32"))]
fn spawn_pcm_sink() -> Option<Child> {
    // Prefer `paplay --raw` (PipeWire/PulseAudio native), fallback to `aplay`
    if let Ok(child) = Command::new("paplay")
        .args([
            "--raw",
            "--rate=44100",
            "--channels=1",
            "--format=s16le",
            "--latency-msec=25",
            "--client-name=Infimum Web Audio",
        ])
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
    {
        return Some(child);
    }
    Command::new("aplay")
        .args(["-q", "-t", "raw", "-f", "S16_LE", "-r", "44100", "-c", "1"])
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .ok()
}

#[cfg(not(target_arch = "wasm32"))]
fn mixer_loop(rx: mpsc::Receiver<AudioCmd>) {
    let mut voices: Vec<Voice> = Vec::with_capacity(8);
    let mut sink: Option<Child> = None;
    let mut last_active = Instant::now();
    let mut pcm_bytes = vec![0u8; CHUNK_SAMPLES * 2];

    loop {
        if voices.is_empty() {
            // If sink is open, keep draining silence briefly then close after 2.5s idle
            let timeout = if sink.is_some() {
                Duration::from_millis(80)
            } else {
                Duration::from_secs(60)
            };
            match rx.recv_timeout(timeout) {
                Ok(cmd) => {
                    push_voice(&mut voices, cmd);
                    last_active = Instant::now();
                }
                Err(mpsc::RecvTimeoutError::Timeout) => {
                    if sink.is_some() && last_active.elapsed() > Duration::from_millis(2500) {
                        if let Some(mut c) = sink.take() {
                            drop(c.stdin.take());
                            let _ = c.wait();
                        }
                    }
                    continue;
                }
                Err(mpsc::RecvTimeoutError::Disconnected) => break,
            }
        } else {
            while let Ok(cmd) = rx.try_recv() {
                push_voice(&mut voices, cmd);
                last_active = Instant::now();
            }
        }

        if sink.is_none() {
            sink = spawn_pcm_sink();
            if sink.is_none() {
                voices.clear();
                continue;
            }
        }

        // Mix active voices into CHUNK_SAMPLES s16le buffer
        for s_i in 0..CHUNK_SAMPLES {
            let mut acc = 0.0f32;
            for v in &mut voices {
                if v.pos < v.samples.len() {
                    acc += v.samples[v.pos];
                    v.pos += 1;
                }
            }
            // Soft tanh-like limiter so polyphonic bursts never clip harshly
            let limited = acc / (1.0 + acc.abs() * 0.45);
            let sample_i16 = (limited.clamp(-1.0, 1.0) * 30_000.0) as i16;
            let le = sample_i16.to_le_bytes();
            pcm_bytes[s_i * 2] = le[0];
            pcm_bytes[s_i * 2 + 1] = le[1];
        }
        voices.retain(|v| v.pos < v.samples.len());
        last_active = Instant::now();

        let write_ok = if let Some(child) = sink.as_mut() {
            if let Some(stdin) = child.stdin.as_mut() {
                stdin.write_all(&pcm_bytes).is_ok()
            } else {
                false
            }
        } else {
            false
        };
        if !write_ok {
            if let Some(mut c) = sink.take() {
                let _ = c.kill();
                let _ = c.wait();
            }
        }
    }

    if let Some(mut c) = sink.take() {
        drop(c.stdin.take());
        let _ = c.wait();
    }
}

#[cfg(not(target_arch = "wasm32"))]
fn push_voice(voices: &mut Vec<Voice>, cmd: AudioCmd) {
    if voices.len() >= 8 {
        voices.remove(0);
    }
    let samples = match cmd {
        AudioCmd::Preset {
            preset,
            pitch_pct,
            vol_pct,
        } => synth_preset(preset, pitch_pct as f32 / 100.0, vol_pct as f32 / 100.0),
        AudioCmd::Tone {
            freq_hz,
            dur_ms,
            wave,
        } => synth_tone(freq_hz as f32, dur_ms as f32, wave, 0.55),
    };
    voices.push(Voice { samples, pos: 0 });
}

fn audio_module() -> &'static (Engine, Module) {
    static CELL: OnceLock<(Engine, Module)> = OnceLock::new();
    CELL.get_or_init(|| crate::runtime::load_core_axw("audio_core.ifw", AUDIO_AXW))
}

fn read_pcm_samples(mem: &[u8], count: usize) -> Vec<f32> {
    let n = count.min(MAX_SAMPLES);
    let mut out = Vec::with_capacity(n);
    for idx in 0..n {
        let off = (PCM_BUF_WORD + idx) * 4;
        let v = if off + 4 <= mem.len() {
            i32::from_le_bytes(mem[off..off + 4].try_into().unwrap())
        } else {
            0
        };
        out.push((v as f32) / 10_000.0);
    }
    out
}

pub fn synth_preset(preset: i64, pitch: f32, vol: f32) -> Vec<f32> {
    let (engine, module) = audio_module();
    let mut store = Store::new(engine, ());
    if store.set_fuel(AUDIO_FUEL).is_err() {
        return Vec::new();
    }
    let linker = Linker::new(engine);
    let Ok(inst_pre) = linker.instantiate(&mut store, module) else {
        return Vec::new();
    };
    let Ok(inst) = inst_pre.start(&mut store) else {
        return Vec::new();
    };
    let Some(memory) = inst.get_memory(&store, "memory") else {
        return Vec::new();
    };
    let Ok(func) = inst.get_typed_func::<(i32, i32, i32), i32>(&store, "synth_preset_ifm") else {
        return Vec::new();
    };
    let pitch_pct = (pitch * 100.0).round() as i32;
    let vol_pct = (vol * 100.0).round() as i32;
    let Ok(n_raw) = func.call(&mut store, (preset as i32, pitch_pct, vol_pct)) else {
        return Vec::new();
    };
    read_pcm_samples(memory.data(&store), n_raw.max(0) as usize)
}

pub fn synth_tone(freq_hz: f32, dur_ms: f32, wave: i64, vol: f32) -> Vec<f32> {
    let (engine, module) = audio_module();
    let mut store = Store::new(engine, ());
    if store.set_fuel(AUDIO_FUEL).is_err() {
        return Vec::new();
    }
    let linker = Linker::new(engine);
    let Ok(inst_pre) = linker.instantiate(&mut store, module) else {
        return Vec::new();
    };
    let Ok(inst) = inst_pre.start(&mut store) else {
        return Vec::new();
    };
    let Some(memory) = inst.get_memory(&store, "memory") else {
        return Vec::new();
    };
    let Ok(func) = inst.get_typed_func::<(i32, i32, i32, i32), i32>(&store, "synth_tone_ifm") else {
        return Vec::new();
    };
    let vol_pct = (vol * 100.0).round() as i32;
    let Ok(n_raw) = func.call(
        &mut store,
        (
            freq_hz.round() as i32,
            dur_ms.round() as i32,
            wave as i32,
            vol_pct,
        ),
    ) else {
        return Vec::new();
    };
    read_pcm_samples(memory.data(&store), n_raw.max(0) as usize)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn procedural_presets_and_tones_synthesize_valid_pcm() {
        for preset in 1..=7 {
            let pcm = synth_preset(preset, 1.0, 0.8);
            assert!(pcm.len() >= 1000, "preset {preset} too short");
            assert!(
                pcm.iter().all(|s| s.is_finite() && *s >= -2.0 && *s <= 2.0),
                "preset {preset} produced non-finite or out-of-range samples"
            );
            assert!(
                pcm.iter().any(|s| s.abs() > 0.05),
                "preset {preset} is silent"
            );
        }
        let bell = synth_tone(880.0, 120.0, 3, 0.5);
        assert!(bell.len() > 4000);
    }
}
