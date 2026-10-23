//! Mezclador de tiempo real: callback de audio, matriz M×N con suavizado de 5 ms por muestra,
//! interpolador Catmull-Rom, FTZ/DAZ y detector de costuras (spec §2.2 y §4.2.4).
//!
//! Este archivo no asigna memoria, no toma bloqueos ni hace E/S. Todo el estado temporal vive en
//! el stack (`Scratch`, ~49 KiB). Los bucles de mezcla trabajan sobre búferes planares con la
//! forma que el autovectorizador convierte en AVX2/FMA con `target-cpu=x86-64-v3` (Linux, ver
//! `.cargo/config.toml`). El build de Windows usa CPU genérica (SSE2, sin FMA) para arrancar en PC
//! antiguas: el resultado es el mismo salvo diferencias de redondeo del orden de 1e-7.

use core::sync::atomic::Ordering;
use std::sync::Arc;

use arc_swap::ArcSwap;
use ringbuf::traits::{Consumer, Producer};

use crate::rt_command::{
    AudioCommand, AudioStatus, CurveTable, DevampMode, FadeMode, VoiceId, MAX_FADES_PER_VOICE, MAX_INPUT_CHANNELS,
    MAX_OUTPUT_CHANNELS, MAX_SLICES_PER_VOICE, MAX_VOLUME_DB, MIN_VOLUME_DB,
};
use crate::voice::{
    slew_coeff, CellFadeEntry, MatrixFadeSlot, Voice, VoiceRtState, VoiceState, SLEW_EPSILON,
};

/// Frames por trozo de mezcla; acota el tamaño de los búferes del stack.
pub const CHUNK_FRAMES: usize = 128;
/// Salto mínimo (amplitud) para considerar una costura.
pub const SEAM_THRESHOLD: f32 = 0.08;
/// Además, el salto debe superar este múltiplo del mayor paso entre muestras del bloque.
pub const SEAM_RATIO: f32 = 4.0;
/// Pause/Resume: mínimo 5 ms @ 48 kHz.
pub const MIN_PAUSE_FADE_FRAMES: u32 = 240;
/// Stop: mínimo 2 ms @ 48 kHz (HardStop).
pub const MIN_STOP_FADE_FRAMES: u32 = 96;
/// Fade de un Stop normal y del `stop_target_when_done`: 5 ms @ 48 kHz.
pub const STANDARD_STOP_FADE_FRAMES: u32 = 240;

const RATE_MIN: f32 = 0.03;
const RATE_MAX: f32 = 33.0;

// ---------------------------------------------------------------------------------------------
// Primitivas numéricas
// ---------------------------------------------------------------------------------------------

/// `true` si esta compilación ya trae FMA de hardware (el binario de Windows es genérico y no lo trae).
const STATIC_FMA: bool = cfg!(any(target_feature = "fma", target_arch = "aarch64"));

/// ¿Puede esta CPU ejecutar la rama acelerada (AVX2 + FMA)? `is_x86_feature_detected!` consulta CPUID una sola vez
/// y guarda el resultado en un atómico: después cuesta una lectura relajada.
#[inline]
pub fn cpu_has_avx2_fma() -> bool {
    #[cfg(any(target_arch = "x86_64", target_arch = "x86"))]
    {
        std::is_x86_feature_detected!("avx2") && std::is_x86_feature_detected!("fma")
    }
    #[cfg(not(any(target_arch = "x86_64", target_arch = "x86")))]
    {
        false
    }
}

/// `a * b + c`. Con `HW` usa FMA de hardware (solo se instancia dentro de las funciones con
/// `#[target_feature(enable = "avx2,fma")]` o cuando el objetivo ya lo tiene); sin `HW`, multiplicación y suma
/// separadas, que en el baseline genérico evitan la rutina de software de `mul_add`.
#[inline(always)]
fn fma_g<const HW: bool>(a: f32, b: f32, c: f32) -> f32 {
    if HW {
        a.mul_add(b, c)
    } else {
        a * b + c
    }
}

#[inline(always)]
fn catmull_g<const HW: bool>(y_m1: f32, y0: f32, y1: f32, y2: f32, frac: f32) -> f32 {
    let c0 = y0;
    let c1 = 0.5 * (y1 - y_m1);
    let c2 = fma_g::<HW>(0.5, -y2, fma_g::<HW>(2.0, y1, fma_g::<HW>(-2.5, y0, y_m1)));
    let c3 = fma_g::<HW>(1.5, y0 - y1, 0.5 * (y2 - y_m1));
    fma_g::<HW>(fma_g::<HW>(fma_g::<HW>(c3, frac, c2), frac, c1), frac, c0)
}

/// Interpolador cúbico Catmull-Rom de 4 puntos, sin asignaciones y en forma de Horner.
/// Interpola entre `y0` (frac = 0) e `y1` (frac = 1); `y_m1` e `y2` son los vecinos.
/// Usa FMA solo si la compilación lo trae; el camino del mezclador elige en tiempo de ejecución.
#[inline(always)]
pub fn catmull_rom_4pt(y_m1: f32, y0: f32, y1: f32, y2: f32, frac: f32) -> f32 {
    catmull_g::<STATIC_FMA>(y_m1, y0, y1, y2, frac)
}

/// Activa Flush-To-Zero (bit 15) y Denormals-Are-Zero (bit 6) de `MXCSR` en el hilo actual
/// (x86_64), o `FPCR.FZ` en aarch64. Se llama al inicio de cada callback: son dos instrucciones y
/// así también cubre drivers que reinician el estado de coma flotante del hilo.
#[inline(always)]
pub fn enable_ftz_daz_on_current_thread() {
    #[cfg(target_arch = "x86_64")]
    // SAFETY: `stmxcsr`/`ldmxcsr` solo leen/escriben `csr`, una variable local, y el registro MXCSR
    // del hilo. Se usa ensamblador porque `_mm_getcsr`/`_mm_setcsr` están obsoletos.
    unsafe {
        let mut csr: u32 = 0;
        core::arch::asm!("stmxcsr [{p}]", p = in(reg) &mut csr, options(nostack, preserves_flags));
        csr |= 0x8040;
        core::arch::asm!("ldmxcsr [{p}]", p = in(reg) &csr, options(nostack, readonly, preserves_flags));
    }
    #[cfg(target_arch = "aarch64")]
    // SAFETY: lee y escribe el registro FPCR del hilo; activa FZ (bit 24).
    unsafe {
        let fpcr: u64;
        core::arch::asm!("mrs {}, fpcr", out(reg) fpcr, options(nomem, nostack, preserves_flags));
        core::arch::asm!("msr fpcr, {}", in(reg) fpcr | (1 << 24), options(nomem, nostack));
    }
}

// ---------------------------------------------------------------------------------------------
// Callback
// ---------------------------------------------------------------------------------------------

struct Scratch {
    /// Acumulador planar por canal de salida.
    planar: [[f32; CHUNK_FRAMES]; MAX_OUTPUT_CHANNELS],
    /// Muestras de la voz actual, por canal de entrada del archivo.
    src: [[f32; CHUNK_FRAMES]; MAX_INPUT_CHANNELS],
    /// Envolvente de transporte × ducking por frame.
    env: [f32; CHUNK_FRAMES],
}

impl Scratch {
    fn new() -> Self {
        Self {
            planar: [[0.0; CHUNK_FRAMES]; MAX_OUTPUT_CHANNELS],
            src: [[0.0; CHUNK_FRAMES]; MAX_INPUT_CHANNELS],
            env: [0.0; CHUNK_FRAMES],
        }
    }
}

#[inline(always)]
fn emit<P: Producer<Item = AudioStatus>>(status: &mut P, event: AudioStatus) {
    // Anillo lleno = el evento se pierde; el hilo RT nunca espera.
    let _ = status.try_push(event);
}

/// Callback principal de tiempo real (cpal WASAPI/ASIO o PipeWire).
///
/// Invariantes: 0 asignaciones, 0 bloqueos, 0 syscalls, 0 E/S, FTZ/DAZ activo. Los comandos se
/// aplican al inicio del bloque. `last_block_tail` guarda la última muestra de cada canal del
/// bloque anterior para el detector de costuras.
pub fn process_rt_callback<C, P>(
    out_interleaved: &mut [f32],
    out_channels: usize,
    host_sample_rate: u32,
    voices_swap: &ArcSwap<Vec<Arc<Voice>>>,
    cmd_consumer: &mut C,
    status_producer: &mut P,
    last_block_tail: &mut [f32; MAX_OUTPUT_CHANNELS],
) where
    C: Consumer<Item = AudioCommand>,
    P: Producer<Item = AudioStatus>,
{
    // 5. FTZ/DAZ
    enable_ftz_daz_on_current_thread();

    // 1. Limpiar la salida
    out_interleaved.fill(0.0);
    if out_channels == 0 {
        return;
    }
    let frames = out_interleaved.len() / out_channels;

    // 2. Instantánea wait-free de voces. El `VoicePool` retiene cada instantánea reemplazada, así
    //    que soltar este guard jamás libera memoria.
    let guard = voices_swap.load();
    let voices: &[Arc<Voice>] = guard.as_slice();

    // 3. Drenar comandos: O(comandos)
    while let Some(cmd) = cmd_consumer.try_pop() {
        apply_command(voices, cmd, host_sample_rate, status_producer);
    }
    if frames == 0 {
        return;
    }

    // 4. Mezcla por trozos
    let mix_channels = out_channels.min(MAX_OUTPUT_CHANNELS);
    let coeff = slew_coeff(host_sample_rate.max(1) as f32);
    let mut scratch = Scratch::new();
    let mut start = 0;
    while start < frames {
        let n = (frames - start).min(CHUNK_FRAMES);
        for plane in scratch.planar.iter_mut().take(mix_channels) {
            plane[..n].fill(0.0);
        }
        for voice in voices {
            if !matches!(voice.state(), VoiceState::Playing | VoiceState::TailingOut) {
                continue;
            }
            // SAFETY: este es el único hilo de callback de audio, y cada guard se suelta antes de
            // pedir el de la siguiente voz.
            let mut rt = unsafe { voice.rt_mut() };
            render_voice_chunk(voice, &mut rt, &mut scratch, n, mix_channels, host_sample_rate, coeff, status_producer);
        }
        for (ch, plane) in scratch.planar.iter().enumerate().take(mix_channels) {
            for (f, &s) in plane[..n].iter().enumerate() {
                out_interleaved[(start + f) * out_channels + ch] = s;
            }
        }
        start += n;
    }

    // 7. Detector de costuras y guardado de la cola del bloque
    detect_seams(out_interleaved, frames, out_channels, last_block_tail, status_producer);
}

/// Compara la primera muestra del bloque con la última del anterior. Hay costura si el salto supera
/// [`SEAM_THRESHOLD`] y además [`SEAM_RATIO`] veces el mayor paso entre muestras dentro del bloque
/// (así el contenido agudo y fuerte, con pasos grandes de forma natural, no dispara falsos avisos).
fn detect_seams<P: Producer<Item = AudioStatus>>(
    out: &[f32],
    frames: usize,
    oc: usize,
    last_block_tail: &mut [f32; MAX_OUTPUT_CHANNELS],
    status: &mut P,
) {
    for ch in 0..oc.min(MAX_OUTPUT_CHANNELS) {
        let jump = (out[ch] - last_block_tail[ch]).abs();
        if jump > SEAM_THRESHOLD {
            let mut max_step = 0.0f32;
            let mut prev = out[ch];
            for f in 1..frames {
                let s = out[f * oc + ch];
                max_step = max_step.max((s - prev).abs());
                prev = s;
            }
            if jump > SEAM_RATIO * max_step {
                emit(status, AudioStatus::SeamDetected { output_channel: ch as u8, jump_delta: jump });
            }
        }
        last_block_tail[ch] = out[(frames - 1) * oc + ch];
    }
}

// ---------------------------------------------------------------------------------------------
// Comandos
// ---------------------------------------------------------------------------------------------

fn find_voice(voices: &[Arc<Voice>], id: VoiceId) -> Option<&Arc<Voice>> {
    voices.iter().find(|v| v.id == id)
}

fn begin_transport_fade(rt: &mut VoiceRtState, target: f32, frames: u32, curve: CurveTable, on_done: VoiceState) {
    rt.transport_fade_start = rt.transport_fade_current;
    rt.transport_fade_target = target;
    rt.transport_fade_t = 0.0;
    rt.transport_fade_step = 1.0 / frames.max(1) as f32;
    rt.transport_curve = curve;
    rt.transport_fade_action_on_zero = on_done;
}

fn reset_playback(voice: &Voice, rt: &mut VoiceRtState) {
    rt.cursor_frame = voice.start_frame;
    rt.sub_frame_frac = 0.0;
    rt.current_slice_idx = 0;
    for s in rt.slices.iter_mut().take(rt.slice_count) {
        s.remaining_loops = s.play_count;
    }
    rt.transport_fade_current = 1.0;
    rt.transport_fade_target = 1.0;
    rt.transport_fade_step = 0.0;
    // Reinicio desde cero: los filtros no arrastran la cola de la reproducción anterior.
    rt.eq.reset_state();
}

fn start_stop<P: Producer<Item = AudioStatus>>(voice: &Voice, rt: &mut VoiceRtState, frames: u32, curve: CurveTable, status: &mut P) {
    match voice.state() {
        VoiceState::Playing | VoiceState::TailingOut => {
            voice.set_state(VoiceState::TailingOut);
            begin_transport_fade(rt, 0.0, frames.max(MIN_STOP_FADE_FRAMES), curve, VoiceState::Stopped);
        }
        // Ya está en silencio: no hay nada que atenuar.
        VoiceState::Paused | VoiceState::Idle => finish_voice(voice, false, status),
        VoiceState::Stopped => {}
    }
}

fn finish_voice<P: Producer<Item = AudioStatus>>(voice: &Voice, natural_eof: bool, status: &mut P) {
    voice.set_state(VoiceState::Stopped);
    emit(status, AudioStatus::VoiceCompleted { voice_id: voice.id, natural_eof });
}

fn seek_voice(voice: &Voice, rt: &mut VoiceRtState, target: u64) {
    let target = target.clamp(voice.start_frame, voice.end_frame);
    rt.cursor_frame = target;
    rt.sub_frame_frac = 0.0;
    let count = rt.slice_count.max(1);
    let idx = (0..count)
        .find(|&i| target < rt.slices[i].end_frame.min(voice.end_frame))
        .unwrap_or(count - 1);
    rt.current_slice_idx = idx;
    rt.slices[idx].remaining_loops = rt.slices[idx].play_count;
}

fn begin_cell_fade(
    rt: &mut VoiceRtState,
    fade_id: u64,
    mode: FadeMode,
    row: u8,
    col: u8,
    target_db: f32,
    duration_frames: u32,
    curve: CurveTable,
    stop_target_when_done: bool,
) {
    let (r, c) = (row as usize, col as usize);
    if r > rt.matrix.in_channels || c > rt.matrix.out_channels || target_db.is_nan() {
        return;
    }
    let span = MAX_VOLUME_DB - MIN_VOLUME_DB;
    let target_db = match mode {
        FadeMode::Absolute => target_db.clamp(MIN_VOLUME_DB, MAX_VOLUME_DB),
        FadeMode::Relative => target_db.clamp(-span, span),
    };
    let (start, target) = match mode {
        FadeMode::Absolute => (rt.matrix.base_db[r][c], target_db),
        FadeMode::Relative => {
            let s = rt.matrix.rel_offset_db[r][c];
            (s, s + target_db)
        }
    };

    // Coalescencia: la celda pertenece solo al fade más reciente.
    for slot in rt.active_fades.iter_mut().filter(|s| s.in_use && s.fade_id != fade_id) {
        for e in slot.cells[..slot.cells_count].iter_mut() {
            if e.active && e.row == row && e.col == col {
                e.active = false;
            }
        }
    }

    let idx = rt
        .active_fades
        .iter()
        .position(|s| s.in_use && s.fade_id == fade_id)
        .or_else(|| rt.active_fades.iter().position(|s| !s.in_use));
    let Some(idx) = idx else { return }; // sin ranura libre: se descarta
    let slot = &mut rt.active_fades[idx];
    if !slot.in_use {
        *slot = MatrixFadeSlot {
            in_use: true,
            fade_id,
            mode,
            duration_frames: duration_frames.max(1) as u64,
            elapsed_frames: 0,
            stop_target_when_done,
            curve,
            cells_count: 0,
            cells: [CellFadeEntry::EMPTY; 64],
        };
    }
    let entry = CellFadeEntry { active: true, row, col, start_db: start, target_db: target };
    let existing = slot.cells[..slot.cells_count].iter().position(|e| e.row == row && e.col == col);
    match existing {
        Some(i) => slot.cells[i] = entry,
        None if slot.cells_count < slot.cells.len() => {
            slot.cells[slot.cells_count] = entry;
            slot.cells_count += 1;
        }
        None => {}
    }
}

/// `Play` / `PlayFadeIn`: arranca la voz; con `fade_in_frames > 0` entra desde silencio con un fundido lineal.
fn play_voice<P: Producer<Item = AudioStatus>>(voice: &Voice, rt: &mut VoiceRtState, status: &mut P, voice_id: VoiceId, start_host_time_ns: u64, fade_in_frames: u32) {
    let started = match voice.state() {
        VoiceState::Idle => true,
        VoiceState::Stopped => {
            reset_playback(voice, rt);
            true
        }
        VoiceState::Paused => {
            voice.set_state(VoiceState::Playing);
            begin_transport_fade(rt, 1.0, MIN_PAUSE_FADE_FRAMES, CurveTable::LINEAR, VoiceState::Playing);
            false
        }
        VoiceState::Playing | VoiceState::TailingOut => false,
    };
    if started {
        if fade_in_frames > 0 {
            rt.transport_fade_current = 0.0;
            begin_transport_fade(rt, 1.0, fade_in_frames, CurveTable::LINEAR, VoiceState::Playing);
        }
        voice.set_state(VoiceState::Playing);
        emit(status, AudioStatus::VoiceStarted { voice_id, host_time_ns: start_host_time_ns });
    }
}

fn apply_to_voice<P: Producer<Item = AudioStatus>>(voice: &Voice, cmd: AudioCommand, status: &mut P) {
    // SAFETY: hilo de callback único; el guard no sobrevive a esta función.
    let mut rt = unsafe { voice.rt_mut() };
    let rt = &mut *rt;
    match cmd {
        AudioCommand::Play { voice_id, start_host_time_ns } => play_voice(voice, rt, status, voice_id, start_host_time_ns, 0),
        AudioCommand::PlayFadeIn { voice_id, start_host_time_ns, fade_frames } => play_voice(voice, rt, status, voice_id, start_host_time_ns, fade_frames),
        AudioCommand::Pause { fade_frames, .. } => {
            if voice.state() == VoiceState::Playing {
                voice.set_state(VoiceState::TailingOut);
                begin_transport_fade(rt, 0.0, fade_frames.max(MIN_PAUSE_FADE_FRAMES), CurveTable::LINEAR, VoiceState::Paused);
            }
        }
        AudioCommand::Resume { fade_frames, .. } => {
            // También cancela un Pause aún en curso (estado TailingOut hacia Paused).
            let pausing = voice.state() == VoiceState::TailingOut && rt.transport_fade_action_on_zero == VoiceState::Paused;
            if voice.state() == VoiceState::Paused || pausing {
                voice.set_state(VoiceState::Playing);
                begin_transport_fade(rt, 1.0, fade_frames.max(MIN_PAUSE_FADE_FRAMES), CurveTable::LINEAR, VoiceState::Playing);
            }
        }
        AudioCommand::Stop { fade_frames, curve, .. } => start_stop(voice, rt, fade_frames, curve, status),
        AudioCommand::Seek { target_frame, .. } => seek_voice(voice, rt, target_frame),
        AudioCommand::SetCrosspointDb { row, col, db, .. } => {
            let (r, c) = (row as usize, col as usize);
            if r <= rt.matrix.in_channels && c <= rt.matrix.out_channels {
                rt.matrix.base_db[r][c] = if db.is_nan() { MIN_VOLUME_DB } else { db.clamp(MIN_VOLUME_DB, MAX_VOLUME_DB) };
                rt.matrix.matrix_dirty = true;
            }
        }
        AudioCommand::SetMuteSoloMask { mute_mask, solo_mask, .. } => {
            rt.matrix.mute_mask = mute_mask;
            rt.matrix.solo_mask = solo_mask;
            rt.matrix.matrix_dirty = true;
        }
        AudioCommand::SetRate { rate, .. } => {
            if rate.is_finite() {
                voice.set_rate(rate.clamp(RATE_MIN, RATE_MAX));
            }
        }
        AudioCommand::RequestDevamp { mode, trigger_next_cue, .. } => voice.request_devamp(mode, trigger_next_cue),
        AudioCommand::BeginCellFade {
            fade_id, mode, row, col, target_db, duration_frames, curve, stop_target_when_done, ..
        } => begin_cell_fade(rt, fade_id, mode, row, col, target_db, duration_frames, curve, stop_target_when_done),
        AudioCommand::CancelFade { fade_id, .. } => {
            for slot in rt.active_fades.iter_mut().filter(|s| s.in_use && s.fade_id == fade_id) {
                slot.in_use = false;
            }
        }
        AudioCommand::SetDuckEnvelope { target_duck_linear, slew_frames, .. } => {
            let target = if target_duck_linear.is_finite() { target_duck_linear.max(0.0) } else { 1.0 };
            rt.duck_target_lin = target;
            rt.duck_step = (target - rt.duck_current_lin) / slew_frames.max(1) as f32;
        }
        AudioCommand::SetEq { eq, .. } => rt.eq.set_target(eq),
        AudioCommand::PanicAll { .. } => {}
    }
}

fn apply_command<P: Producer<Item = AudioStatus>>(voices: &[Arc<Voice>], cmd: AudioCommand, _host_sample_rate: u32, status: &mut P) {
    use AudioCommand as C;
    let target = match cmd {
        C::Play { voice_id, .. }
        | C::PlayFadeIn { voice_id, .. }
        | C::Pause { voice_id, .. }
        | C::Resume { voice_id, .. }
        | C::Stop { voice_id, .. }
        | C::Seek { voice_id, .. }
        | C::SetCrosspointDb { voice_id, .. }
        | C::SetMuteSoloMask { voice_id, .. }
        | C::SetRate { voice_id, .. }
        | C::RequestDevamp { voice_id, .. }
        | C::BeginCellFade { voice_id, .. }
        | C::CancelFade { voice_id, .. }
        | C::SetDuckEnvelope { voice_id, .. }
        | C::SetEq { voice_id, .. } => Some(voice_id),
        C::PanicAll { .. } => None,
    };
    match (cmd, target) {
        (C::PanicAll { fade_frames }, _) => {
            for voice in voices {
                // SAFETY: hilo de callback único; el guard se suelta al terminar `start_stop`.
                let mut rt = unsafe { voice.rt_mut() };
                start_stop(voice, &mut rt, fade_frames, CurveTable::LINEAR, status);
            }
        }
        (cmd, Some(id)) => {
            if let Some(voice) = find_voice(voices, id) {
                apply_to_voice(voice, cmd, status);
            }
        }
        _ => {}
    }
}

// ---------------------------------------------------------------------------------------------
// Render de una voz
// ---------------------------------------------------------------------------------------------

fn render_voice_chunk<P: Producer<Item = AudioStatus>>(
    voice: &Voice,
    rt: &mut VoiceRtState,
    scratch: &mut Scratch,
    n: usize,
    out_channels: usize,
    host_sr: u32,
    coeff: f32,
    status: &mut P,
) {
    advance_matrix_fades(voice, rt, n as u64, status);
    if rt.matrix.matrix_dirty {
        // Duck y transporte se aplican por muestra en `env`, no dentro de los objetivos.
        rt.matrix.recompute_targets(1.0, 1.0);
    }
    let fade_done = build_envelope(rt, &mut scratch.env[..n]);

    let Scratch { planar, src, env } = scratch;
    let (produced, finished) = read_source(voice, rt, src, n, host_sr, status);
    // Ecualizador por voz, antes de la matriz de ruteo. Con el EQ apagado (o todas las bandas en 0 dB) es una
    // comprobación y nada más.
    let in_channels = rt.matrix.in_channels.min(MAX_INPUT_CHANNELS);
    rt.eq.process(&mut src[..], in_channels, produced, host_sr as f32);
    voice.note_peak(voice_peak(rt, src, env, produced));
    mix_voice(rt, src, env, planar, produced, out_channels, coeff);

    let idx = rt.current_slice_idx.min(rt.slice_count.saturating_sub(1));
    let slice = &rt.slices[idx];
    let loops_left = if slice.is_infinite() { u32::MAX } else { slice.remaining_loops };
    voice.publish_progress(rt.cursor_frame, loops_left);

    // `finished` ya dejó la voz en Stopped; el fade de transporte solo manda si seguía viva.
    if !finished {
        if let Some(next) = fade_done {
            match next {
                VoiceState::Stopped => finish_voice(voice, false, status),
                other => voice.set_state(other),
            }
        }
    }
}

/// Avanza la envolvente de transporte y el ducking por muestra. Devuelve el estado destino si el
/// fade de transporte terminó en este trozo.
fn build_envelope(rt: &mut VoiceRtState, env: &mut [f32]) -> Option<VoiceState> {
    let mut done = None;
    for e in env.iter_mut() {
        if rt.transport_fade_step != 0.0 {
            rt.transport_fade_t += rt.transport_fade_step as f64;
            if rt.transport_fade_t >= 1.0 {
                rt.transport_fade_t = 1.0;
                rt.transport_fade_current = rt.transport_fade_target;
                rt.transport_fade_step = 0.0;
                done = Some(rt.transport_fade_action_on_zero);
            } else {
                let k = rt.transport_curve.eval(rt.transport_fade_t as f32);
                rt.transport_fade_current = rt.transport_fade_start + (rt.transport_fade_target - rt.transport_fade_start) * k;
            }
        }
        if rt.duck_step != 0.0 {
            rt.duck_current_lin += rt.duck_step;
            if (rt.duck_step > 0.0 && rt.duck_current_lin >= rt.duck_target_lin)
                || (rt.duck_step < 0.0 && rt.duck_current_lin <= rt.duck_target_lin)
            {
                rt.duck_current_lin = rt.duck_target_lin;
                rt.duck_step = 0.0;
            }
        }
        *e = rt.transport_fade_current * rt.duck_current_lin;
    }
    done
}

/// Avanza los `FadeCue` de matriz (curva de 33 puntos) y marca la matriz como sucia.
fn advance_matrix_fades<P: Producer<Item = AudioStatus>>(voice: &Voice, rt: &mut VoiceRtState, frames: u64, status: &mut P) {
    let mut stop_after = false;
    {
        let VoiceRtState { active_fades, matrix, .. } = &mut *rt;
        for slot in active_fades.iter_mut().take(MAX_FADES_PER_VOICE).filter(|s| s.in_use) {
            slot.elapsed_frames += frames;
            let done = slot.elapsed_frames >= slot.duration_frames;
            let t = if done { 1.0 } else { slot.elapsed_frames as f32 / slot.duration_frames as f32 };
            let k = slot.curve.eval(t);
            for e in slot.cells[..slot.cells_count].iter().filter(|e| e.active) {
                let v = if done { e.target_db } else { e.start_db + (e.target_db - e.start_db) * k };
                let (r, c) = (e.row as usize, e.col as usize);
                match slot.mode {
                    FadeMode::Absolute => matrix.base_db[r][c] = v,
                    FadeMode::Relative => matrix.rel_offset_db[r][c] = v,
                }
            }
            matrix.matrix_dirty = true;
            if done {
                slot.in_use = false;
                stop_after |= slot.stop_target_when_done;
                emit(
                    status,
                    AudioStatus::FadeCompleted {
                        voice_id: voice.id,
                        fade_id: slot.fade_id,
                        stopped_target: slot.stop_target_when_done,
                    },
                );
            }
        }
    }
    if stop_after {
        start_stop(voice, rt, STANDARD_STOP_FADE_FRAMES, CurveTable::LINEAR, status);
    }
}

/// Cierra los segmentos que ya terminaron en `rt.cursor_frame`: repeticiones, avance de slice,
/// devamp y fin. `false` = la voz terminó (y ya quedó en `Stopped`).
fn resolve_boundaries<P: Producer<Item = AudioStatus>>(voice: &Voice, rt: &mut VoiceRtState, status: &mut P) -> bool {
    let mut guard = 0;
    loop {
        let idx = rt.current_slice_idx.min(rt.slice_count.saturating_sub(1));
        let slice = rt.slices[idx];
        let end = slice.end_frame.min(voice.end_frame);
        if rt.cursor_frame < end {
            return true;
        }
        guard += 1;
        if guard > 2 * MAX_SLICES_PER_VOICE + 2 {
            finish_voice(voice, true, status);
            return false;
        }

        let (devamp, trigger_next_cue) = voice.take_devamp();
        if devamp != DevampMode::None {
            emit(status, AudioStatus::SliceDevamped { voice_id: voice.id, slice_index: idx as u16, trigger_next_cue });
            if devamp == DevampMode::StopAtSliceEnd {
                finish_voice(voice, false, status);
                return false;
            }
        } else if slice.start_frame < end && (slice.is_infinite() || slice.remaining_loops > 1) {
            if !slice.is_infinite() {
                rt.slices[idx].remaining_loops -= 1;
            }
            rt.cursor_frame = slice.start_frame;
            continue;
        }

        // Siguiente slice, o fin del programa.
        if idx + 1 < rt.slice_count {
            rt.current_slice_idx = idx + 1;
            let next = &mut rt.slices[idx + 1];
            next.remaining_loops = next.play_count;
            rt.cursor_frame = next.start_frame;
        } else if voice.infinite_loop.load(Ordering::Relaxed) {
            rt.current_slice_idx = 0;
            rt.slices[0].remaining_loops = rt.slices[0].play_count;
            rt.cursor_frame = rt.slices[0].start_frame;
        } else {
            finish_voice(voice, true, status);
            return false;
        }
    }
}

/// Lee `n` frames del archivo a `src` (planar), con bucles/slices exactos al frame. Interpola con
/// Catmull-Rom cuando `rate != 1` o cambia la frecuencia de muestreo. Devuelve `(frames, terminó)`.
#[inline(always)]
fn read_source_g<const HW: bool, P: Producer<Item = AudioStatus>>(
    voice: &Voice,
    rt: &mut VoiceRtState,
    src: &mut [[f32; CHUNK_FRAMES]; MAX_INPUT_CHANNELS],
    n: usize,
    host_sr: u32,
    status: &mut P,
) -> (usize, bool) {
    // Solo RAM: `maintain` pide al disco lo que viene y libera lo recorrido; `view` fija qué se lee.
    voice.reader.maintain(rt.cursor_frame);
    let reader = voice.reader.view();
    let m = rt.matrix.in_channels.min(MAX_INPUT_CHANNELS);
    let rate = voice.rate().clamp(RATE_MIN, RATE_MAX);
    let host_sr = host_sr.max(1);
    let unity = rate == 1.0 && voice.file_sample_rate == host_sr;
    let step = rate as f64 * voice.file_sample_rate as f64 / host_sr as f64;
    let mut underrun_reported = false;

    for f in 0..n {
        if !resolve_boundaries(voice, rt, status) {
            return (f, true);
        }
        let cur = rt.cursor_frame as i64;
        if !reader.has_frame(rt.cursor_frame) {
            if !underrun_reported {
                underrun_reported = true;
                emit(status, AudioStatus::DiskBufferUnderrun { voice_id: voice.id, at_frame: rt.cursor_frame });
            }
            for plane in src.iter_mut().take(m) {
                plane[f] = 0.0;
            }
        } else if unity {
            for (ch, plane) in src.iter_mut().enumerate().take(m) {
                plane[f] = reader.sample(cur, ch);
            }
        } else {
            let frac = rt.sub_frame_frac as f32;
            for (ch, plane) in src.iter_mut().enumerate().take(m) {
                plane[f] = catmull_g::<HW>(
                    reader.sample(cur - 1, ch),
                    reader.sample(cur, ch),
                    reader.sample(cur + 1, ch),
                    reader.sample(cur + 2, ch),
                    frac,
                );
            }
        }
        if unity {
            rt.cursor_frame += 1;
        } else {
            rt.sub_frame_frac += step;
            let whole = rt.sub_frame_frac as u64;
            rt.sub_frame_frac -= whole as f64;
            rt.cursor_frame += whole;
        }
    }
    (n, false)
}

#[allow(clippy::too_many_arguments)]
#[cfg(any(target_arch = "x86_64", target_arch = "x86"))]
#[target_feature(enable = "avx2,fma")]
unsafe fn read_source_avx2<P: Producer<Item = AudioStatus>>(
    voice: &Voice,
    rt: &mut VoiceRtState,
    src: &mut [[f32; CHUNK_FRAMES]; MAX_INPUT_CHANNELS],
    n: usize,
    host_sr: u32,
    status: &mut P,
) -> (usize, bool) {
    read_source_g::<true, P>(voice, rt, src, n, host_sr, status)
}

fn read_source<P: Producer<Item = AudioStatus>>(
    voice: &Voice,
    rt: &mut VoiceRtState,
    src: &mut [[f32; CHUNK_FRAMES]; MAX_INPUT_CHANNELS],
    n: usize,
    host_sr: u32,
    status: &mut P,
) -> (usize, bool) {
    #[cfg(any(target_arch = "x86_64", target_arch = "x86"))]
    if cpu_has_avx2_fma() {
        // SAFETY: se acaba de comprobar en tiempo de ejecución que la CPU tiene AVX2 y FMA.
        return unsafe { read_source_avx2(voice, rt, src, n, host_sr, status) };
    }
    read_source_g::<STATIC_FMA, P>(voice, rt, src, n, host_sr, status)
}

/// Pico audible de la voz en este trozo: por canal de entrada, `max(|x|·envolvente)` por su mayor ganancia hacia
/// alguna salida (la matriz ya asentada). Sin asignaciones; son dos bucles cortos sobre datos que ya están en caché.
fn voice_peak(rt: &VoiceRtState, src: &[[f32; CHUNK_FRAMES]; MAX_INPUT_CHANNELS], env: &[f32; CHUNK_FRAMES], produced: usize) -> f32 {
    let m = rt.matrix.in_channels.min(MAX_INPUT_CHANNELS);
    let outs = rt.matrix.out_channels.min(MAX_OUTPUT_CHANNELS);
    let mut peak = 0.0f32;
    for i in 0..m {
        let gain = rt.matrix.current_crosspoint_lin[i][..outs].iter().fold(0.0f32, |g, &x| g.max(x.abs()));
        if gain == 0.0 {
            continue;
        }
        let ch = src[i][..produced].iter().zip(&env[..produced]).fold(0.0f32, |p, (&x, &e)| p.max((x * e).abs()));
        peak = peak.max(ch * gain);
    }
    peak
}

/// Suma la voz en los acumuladores planares aplicando la matriz con suavizado por muestra:
/// `g += (objetivo - g) * coeff` en cada frame (constante de tiempo de 5 ms). Las celdas ya
/// asentadas usan un bucle de ganancia constante que el compilador vectoriza.
#[inline(always)]
fn mix_voice_g<const HW: bool>(
    rt: &mut VoiceRtState,
    src: &[[f32; CHUNK_FRAMES]; MAX_INPUT_CHANNELS],
    env: &[f32; CHUNK_FRAMES],
    planar: &mut [[f32; CHUNK_FRAMES]; MAX_OUTPUT_CHANNELS],
    produced: usize,
    out_channels: usize,
    coeff: f32,
) {
    let m = rt.matrix.in_channels.min(MAX_INPUT_CHANNELS);
    let outs = rt.matrix.out_channels.min(out_channels).min(MAX_OUTPUT_CHANNELS);
    let env = &env[..produced];
    for i in 0..m {
        let s = &src[i][..produced];
        for o in 0..outs {
            let target = rt.matrix.target_crosspoint_lin[i][o];
            let current = &mut rt.matrix.current_crosspoint_lin[i][o];
            let dst = &mut planar[o][..produced];
            if (target - *current).abs() <= SLEW_EPSILON {
                *current = target;
                if target == 0.0 {
                    continue;
                }
                for ((d, &x), &e) in dst.iter_mut().zip(s).zip(env) {
                    *d = fma_g::<HW>(x, target * e, *d);
                }
            } else {
                let mut g = *current;
                for ((d, &x), &e) in dst.iter_mut().zip(s).zip(env) {
                    g = fma_g::<HW>(target - g, coeff, g);
                    *d = fma_g::<HW>(x, g * e, *d);
                }
                *current = if (target - g).abs() <= SLEW_EPSILON { target } else { g };
            }
        }
    }
}

/// Rama acelerada: el mismo bucle compilado con AVX2 (256 bits) y FMA fusionado.
#[cfg(any(target_arch = "x86_64", target_arch = "x86"))]
#[target_feature(enable = "avx2,fma")]
unsafe fn process_voices_avx2(
    rt: &mut VoiceRtState,
    src: &[[f32; CHUNK_FRAMES]; MAX_INPUT_CHANNELS],
    env: &[f32; CHUNK_FRAMES],
    planar: &mut [[f32; CHUNK_FRAMES]; MAX_OUTPUT_CHANNELS],
    produced: usize,
    out_channels: usize,
    coeff: f32,
) {
    mix_voice_g::<true>(rt, src, env, planar, produced, out_channels, coeff)
}

/// Rama portátil: SSE2 / lo que dé el baseline del objetivo; nunca emite instrucciones de 256 bits.
pub(crate) fn process_voices_fallback(
    rt: &mut VoiceRtState,
    src: &[[f32; CHUNK_FRAMES]; MAX_INPUT_CHANNELS],
    env: &[f32; CHUNK_FRAMES],
    planar: &mut [[f32; CHUNK_FRAMES]; MAX_OUTPUT_CHANNELS],
    produced: usize,
    out_channels: usize,
    coeff: f32,
) {
    mix_voice_g::<STATIC_FMA>(rt, src, env, planar, produced, out_channels, coeff)
}

fn mix_voice(
    rt: &mut VoiceRtState,
    src: &[[f32; CHUNK_FRAMES]; MAX_INPUT_CHANNELS],
    env: &[f32; CHUNK_FRAMES],
    planar: &mut [[f32; CHUNK_FRAMES]; MAX_OUTPUT_CHANNELS],
    produced: usize,
    out_channels: usize,
    coeff: f32,
) {
    #[cfg(any(target_arch = "x86_64", target_arch = "x86"))]
    if cpu_has_avx2_fma() {
        // SAFETY: se acaba de comprobar en tiempo de ejecución que la CPU tiene AVX2 y FMA.
        return unsafe { process_voices_avx2(rt, src, env, planar, produced, out_channels, coeff) };
    }
    process_voices_fallback(rt, src, env, planar, produced, out_channels, coeff)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::alloc_guard::{assert_no_alloc, set_violation_policy, violations_on_this_thread, ViolationPolicy};
    use crate::disk_streamer::StreamingVoiceReader;
    use crate::rt_command::{COMMAND_RING_CAPACITY, STATUS_RING_CAPACITY};
    use crate::voice::{AudioLevelMatrixRt, SliceSegment, VoiceInit};
    use crate::voice_pool::VoicePool;
    use ringbuf::traits::Split;
    use ringbuf::{HeapCons, HeapProd, HeapRb};

    const SR: u32 = 48_000;

    struct VoiceSpec {
        id: u64,
        channels: usize,
        file_frames: usize,
        out_channels: usize,
        file_sr: u32,
        end_frame: u64,
        slices: Vec<SliceSegment>,
        infinite_loop: bool,
    }

    impl VoiceSpec {
        fn new(id: u64, channels: usize, frames: usize) -> Self {
            Self {
                id,
                channels,
                file_frames: frames,
                out_channels: 2,
                file_sr: SR,
                end_frame: frames as u64,
                slices: Vec::new(),
                infinite_loop: false,
            }
        }

        fn build(&self, sample: impl Fn(usize, usize) -> f32) -> Arc<Voice> {
            let mut data = Vec::with_capacity(self.file_frames * self.channels);
            for f in 0..self.file_frames {
                for c in 0..self.channels {
                    data.push(sample(f, c));
                }
            }
            let reader = StreamingVoiceReader::from_interleaved(self.channels, 0, data.into_boxed_slice());
            let rt = VoiceRtState::new(0, &self.slices, AudioLevelMatrixRt::new(self.channels, self.out_channels)).unwrap();
            Arc::new(Voice::with_reader(
                VoiceInit {
                    id: self.id,
                    cue_unique_id: self.id as u128,
                    patch_id: 0,
                    file_sample_rate: self.file_sr,
                    start_frame: 0,
                    end_frame: self.end_frame,
                    infinite_loop: self.infinite_loop,
                    rate: 1.0,
                },
                rt,
                reader,
            ))
        }
    }

    struct Harness {
        pool: VoicePool,
        cmd_tx: HeapProd<AudioCommand>,
        cmd_rx: HeapCons<AudioCommand>,
        st_tx: HeapProd<AudioStatus>,
        st_rx: HeapCons<AudioStatus>,
        tail: [f32; MAX_OUTPUT_CHANNELS],
        handle: Arc<ArcSwap<Vec<Arc<Voice>>>>,
        sr: u32,
    }

    impl Harness {
        fn new() -> Self {
            set_violation_policy(ViolationPolicy::CountOnly);
            let pool = VoicePool::new(8);
            let handle = pool.rt_handle();
            drop(handle.load()); // warm-up de arc-swap en este hilo
            let (cmd_tx, cmd_rx) = HeapRb::<AudioCommand>::new(COMMAND_RING_CAPACITY).split();
            let (st_tx, st_rx) = HeapRb::<AudioStatus>::new(STATUS_RING_CAPACITY).split();
            Self { pool, cmd_tx, cmd_rx, st_tx, st_rx, tail: [0.0; MAX_OUTPUT_CHANNELS], handle, sr: SR }
        }

        fn add(&mut self, v: Arc<Voice>) -> Arc<Voice> {
            self.pool.insert_voice(Arc::clone(&v));
            v
        }

        fn cmd(&mut self, c: AudioCommand) {
            assert!(self.cmd_tx.try_push(c).is_ok());
        }

        fn run(&mut self, frames: usize, oc: usize) -> Vec<f32> {
            let mut out = vec![9.0f32; frames * oc];
            let before = violations_on_this_thread();
            assert_no_alloc(|| {
                process_rt_callback(&mut out, oc, self.sr, &self.handle, &mut self.cmd_rx, &mut self.st_tx, &mut self.tail)
            });
            assert_eq!(violations_on_this_thread(), before, "el callback asignó o liberó memoria");
            out
        }

        fn events(&mut self) -> Vec<AudioStatus> {
            let mut v = Vec::new();
            while let Some(e) = self.st_rx.try_pop() {
                v.push(e);
            }
            v
        }
    }

    fn ch(out: &[f32], oc: usize, c: usize) -> Vec<f32> {
        out.chunks(oc).map(|f| f[c]).collect()
    }

    fn play(id: u64) -> AudioCommand {
        AudioCommand::Play { voice_id: id, start_host_time_ns: 123 }
    }

    fn kernel_inputs() -> ([[f32; CHUNK_FRAMES]; MAX_INPUT_CHANNELS], [f32; CHUNK_FRAMES]) {
        let mut src = [[0.0f32; CHUNK_FRAMES]; MAX_INPUT_CHANNELS];
        let mut env = [0.0f32; CHUNK_FRAMES];
        for f in 0..CHUNK_FRAMES {
            env[f] = 0.25 + 0.75 * (f as f32 / CHUNK_FRAMES as f32);
            for (c, plane) in src.iter_mut().enumerate() {
                plane[f] = ((f as f32 * 0.37 + c as f32 * 1.3).sin() * 0.8) + (f as f32 * 0.011).cos() * 0.1;
            }
        }
        (src, env)
    }

    fn matrix_with_slew() -> VoiceRtState {
        let mut m = AudioLevelMatrixRt::new(2, 2);
        // Una celda asentada y otra en pleno suavizado, para ejercer los dos bucles.
        m.target_crosspoint_lin[0][0] = 0.7;
        m.current_crosspoint_lin[0][0] = 0.7;
        m.target_crosspoint_lin[1][1] = 0.9;
        m.current_crosspoint_lin[1][1] = 0.1;
        m.target_crosspoint_lin[0][1] = 0.3;
        m.current_crosspoint_lin[0][1] = 0.3;
        VoiceRtState::new(0, &[], m).expect("estado de voz")
    }

    #[test]
    fn fallback_and_avx2_mixers_agree_within_float_epsilon() {
        let (src, env) = kernel_inputs();
        let coeff = slew_coeff(48_000.0);
        let mut rt_a = matrix_with_slew();
        let mut planar_a = [[0.05f32; CHUNK_FRAMES]; MAX_OUTPUT_CHANNELS];
        process_voices_fallback(&mut rt_a, &src, &env, &mut planar_a, CHUNK_FRAMES, 2, coeff);
        assert!(planar_a[0].iter().any(|x| x.abs() > 0.1), "la rama portátil debe producir señal");

        if !cpu_has_avx2_fma() {
            eprintln!("esta CPU no tiene AVX2+FMA: solo se ejercita la rama portátil");
            return;
        }
        let mut rt_b = matrix_with_slew();
        let mut planar_b = [[0.05f32; CHUNK_FRAMES]; MAX_OUTPUT_CHANNELS];
        // SAFETY: la guarda anterior comprobó AVX2 y FMA.
        unsafe { process_voices_avx2(&mut rt_b, &src, &env, &mut planar_b, CHUNK_FRAMES, 2, coeff) };
        for o in 0..2 {
            for f in 0..CHUNK_FRAMES {
                assert!((planar_a[o][f] - planar_b[o][f]).abs() <= 1e-5, "salida {o} frame {f}: {} vs {}", planar_a[o][f], planar_b[o][f]);
            }
        }
        assert!((rt_a.matrix.current_crosspoint_lin[1][1] - rt_b.matrix.current_crosspoint_lin[1][1]).abs() <= 1e-6);
    }

    #[test]
    fn catmull_rom_fused_and_unfused_agree_within_float_epsilon() {
        for k in 0..=1000 {
            let t = k as f32 / 1000.0;
            let (a, b, c, d) = ((k as f32 * 0.7).sin(), (k as f32 * 0.3).cos(), (k as f32 * 0.11).sin(), 0.5 - t);
            let plain = catmull_g::<false>(a, b, c, d, t);
            let fused = catmull_g::<true>(a, b, c, d, t);
            assert!((plain - fused).abs() <= 1e-5, "t={t}: {plain} vs {fused}");
        }
    }

    #[test]
    fn catmull_rom_properties() {
        // Pasa por y0 e y1 en los extremos.
        assert_eq!(catmull_rom_4pt(1.0, 2.0, 3.0, 4.0, 0.0), 2.0);
        assert!((catmull_rom_4pt(-5.0, 2.0, 7.0, 1.0, 1.0) - 7.0).abs() < 1e-6);
        // Exacto en datos lineales.
        for k in 0..=10 {
            let f = k as f32 / 10.0;
            assert!((catmull_rom_4pt(0.0, 1.0, 2.0, 3.0, f) - (1.0 + f)).abs() < 1e-6);
        }
        // Coincide con la fórmula de la especificación.
        let (a, b, c, d) = (0.3f32, -0.7, 0.9, 0.1);
        for k in 0..=20 {
            let t = k as f32 / 20.0;
            let spec = {
                let c1 = 0.5 * (c - a);
                let c2 = a - 2.5 * b + 2.0 * c - 0.5 * d;
                let c3 = 0.5 * (d - a) + 1.5 * (b - c);
                ((c3 * t + c2) * t + c1) * t + b
            };
            assert!((catmull_rom_4pt(a, b, c, d, t) - spec).abs() < 1e-5);
        }
        set_violation_policy(ViolationPolicy::CountOnly);
        let before = violations_on_this_thread();
        let s = assert_no_alloc(|| (0..1000).map(|k| catmull_rom_4pt(0.0, 1.0, 2.0, 3.0, k as f32 / 1000.0)).sum::<f32>());
        assert!(s > 0.0);
        assert_eq!(violations_on_this_thread(), before);
    }

    #[test]
    fn ftz_daz_flush_denormals() {
        #[cfg(any(target_arch = "x86_64", target_arch = "aarch64"))]
        std::thread::spawn(|| {
            let tiny = std::hint::black_box(f32::MIN_POSITIVE);
            assert!(std::hint::black_box(tiny * 0.5) != 0.0, "sin FTZ el resultado subnormal no es cero");
            enable_ftz_daz_on_current_thread();
            let tiny = std::hint::black_box(f32::MIN_POSITIVE);
            assert_eq!(std::hint::black_box(tiny * 0.5), 0.0, "FTZ debe vaciar el resultado");
            let denormal = std::hint::black_box(f32::from_bits(1));
            assert_eq!(std::hint::black_box(denormal + 0.0), 0.0, "DAZ debe tratar la entrada como cero");
        })
        .join()
        .unwrap();
    }

    #[test]
    fn callback_enables_ftz_daz() {
        #[cfg(any(target_arch = "x86_64", target_arch = "aarch64"))]
        std::thread::spawn(|| {
            let mut h = Harness::new();
            h.run(16, 2);
            let denormal = std::hint::black_box(f32::from_bits(1));
            assert_eq!(std::hint::black_box(denormal + 0.0), 0.0);
        })
        .join()
        .unwrap();
    }

    #[test]
    fn mixes_constant_signal_and_reports_start() {
        let mut h = Harness::new();
        let v = h.add(VoiceSpec::new(1, 2, 48_000).build(|_, _| 0.5));
        h.cmd(play(1));
        let out = h.run(300, 2);
        assert!(out.iter().all(|&s| (s - 0.5).abs() < 1e-6), "{:?}", &out[..4]);
        assert_eq!(v.state(), VoiceState::Playing);
        assert_eq!(v.published_frame_pos.load(Ordering::Relaxed), 300);
        assert!(h.events().contains(&AudioStatus::VoiceStarted { voice_id: 1, host_time_ns: 123 }));
    }

    fn lowcut_1k() -> crate::biquad::EqParams {
        use crate::biquad::{BandParams, EqParams, FilterKind};
        let mut eq = EqParams::OFF;
        eq.enabled = true;
        eq.bands[0] = BandParams { kind: FilterKind::LowCut, freq_hz: 1_000.0, gain_db: 0.0, q: 0.707, enabled: true };
        eq
    }

    fn sine_voice(h: &mut Harness, id: u64) {
        h.add(VoiceSpec::new(id, 2, 192_000).build(|frame, _| 0.5 * (2.0 * std::f32::consts::PI * 100.0 * frame as f32 / 48_000.0).sin()));
    }

    #[test]
    fn a_voice_eq_filters_the_signal_before_routing_and_costs_no_allocations() {
        // Sin EQ: el seno de 100 Hz sale con su amplitud.
        let mut plain = Harness::new();
        sine_voice(&mut plain, 1);
        plain.cmd(play(1));
        plain.run(480, 2);
        let reference = plain.run(4_800, 2);
        assert!(ch(&reference, 2, 0).iter().fold(0.0f32, |m, s| m.max(s.abs())) > 0.45);
        // Con un paso alto en 1 kHz el mismo seno se atenúa mucho (el callback no asigna: `run` lo comprueba).
        let mut h = Harness::new();
        sine_voice(&mut h, 1);
        h.cmd(AudioCommand::SetEq { voice_id: 1, eq: lowcut_1k() });
        h.cmd(play(1));
        h.run(480, 2);
        let mut filtered = Vec::new();
        for _ in 0..4 {
            filtered.extend(h.run(4_800, 2));
        }
        let tail = &ch(&filtered, 2, 0)[14_400..];
        assert!(tail.iter().fold(0.0f32, |m, s| m.max(s.abs())) < 0.02, "el grave debería quedar ~40 dB abajo");
        // Los dos canales de entrada se filtran por igual.
        let right = &ch(&filtered, 2, 1)[14_400..];
        assert!(right.iter().fold(0.0f32, |m, s| m.max(s.abs())) < 0.02);
    }

    #[test]
    fn eq_off_or_flat_is_bit_identical_to_no_eq_and_turning_it_off_mid_play_returns_to_unity() {
        let mut a = Harness::new();
        sine_voice(&mut a, 1);
        a.cmd(play(1));
        let base = a.run(9_600, 2);
        // EQ activado pero con la banda desactivada: salida idéntica bit a bit.
        let mut b = Harness::new();
        sine_voice(&mut b, 1);
        let mut flat = lowcut_1k();
        flat.bands[0].enabled = false;
        b.cmd(AudioCommand::SetEq { voice_id: 1, eq: flat });
        b.cmd(play(1));
        assert_eq!(b.run(9_600, 2), base);
        // Con el paso alto activo y luego apagado en vivo, la señal vuelve a pasar completa.
        let mut c = Harness::new();
        sine_voice(&mut c, 1);
        c.cmd(AudioCommand::SetEq { voice_id: 1, eq: lowcut_1k() });
        c.cmd(play(1));
        c.run(9_600, 2);
        c.cmd(AudioCommand::SetEq { voice_id: 1, eq: crate::biquad::EqParams::OFF });
        let back = c.run(4_800, 2);
        assert!(back.iter().all(|v| v.is_finite()));
        let again = c.run(4_800, 2);
        let peak = ch(&again, 2, 0).iter().fold(0.0f32, |m, s| m.max(s.abs()));
        assert!(peak > 0.45, "sin EQ el seno de 100 Hz vuelve a su amplitud ({peak})");
    }

    #[test]
    fn idle_and_unknown_voices_stay_silent() {
        let mut h = Harness::new();
        h.add(VoiceSpec::new(1, 2, 1000).build(|_, _| 0.5));
        h.cmd(play(99)); // no existe
        let out = h.run(64, 2);
        assert!(out.iter().all(|&s| s == 0.0));
    }

    #[test]
    fn crosspoint_change_is_slewed_per_sample() {
        let mut h = Harness::new();
        h.add(VoiceSpec::new(1, 2, 48_000).build(|_, _| 0.5));
        h.cmd(play(1));
        h.cmd(AudioCommand::SetCrosspointDb { voice_id: 1, row: 1, col: 1, db: -20.0 });
        let out = h.run(4800, 2);
        let l = ch(&out, 2, 0);
        let r = ch(&out, 2, 1);
        let coeff = 1.0f32 / 240.0;
        for (f, &x) in l.iter().enumerate().take(64) {
            let g = 0.1 + 0.9 * (1.0 - coeff).powi(f as i32 + 1);
            assert!((x - 0.5 * g).abs() < 2e-5, "frame {f}: {x} vs {}", 0.5 * g);
        }
        assert!(l.windows(2).all(|w| w[1] <= w[0] + 1e-7), "no monótono");
        let max_step = l.windows(2).map(|w| (w[1] - w[0]).abs()).fold(0.0f32, f32::max);
        assert!(max_step < 0.5 * 0.9 / 240.0 * 1.01, "paso de {max_step}: zipper noise");
        assert!((l[4799] - 0.05).abs() < 1e-5);
        assert!(r.iter().all(|&x| (x - 0.5).abs() < 1e-6), "el canal 2 no debía cambiar");
    }

    #[test]
    fn play_with_fade_in_enters_from_silence_in_a_linear_ramp_and_reports_the_start_once() {
        let mut h = Harness::new();
        let v = h.add(VoiceSpec::new(1, 2, 48_000).build(|_, _| 0.5));
        h.cmd(AudioCommand::PlayFadeIn { voice_id: 1, start_host_time_ns: 7, fade_frames: 240 });
        let out = h.run(512, 2);
        let l = ch(&out, 2, 0);
        assert!(l[0] < 0.01, "no hay salto brusco al entrar: {}", l[0]);
        for f in 0..240 {
            let want = 0.5 * ((f + 1) as f32 / 240.0);
            assert!((l[f] - want).abs() < 2e-3, "frame {f}: {} vs {want}", l[f]);
        }
        assert!(l[260..].iter().all(|&x| (x - 0.5).abs() < 1e-6), "después del fundido suena a nivel completo");
        assert_eq!(v.state(), VoiceState::Playing);
        assert_eq!(h.events().iter().filter(|e| matches!(e, AudioStatus::VoiceStarted { voice_id: 1, host_time_ns: 7 })).count(), 1);
        // Un Play normal sigue entrando de golpe.
        let mut g = Harness::new();
        g.add(VoiceSpec::new(1, 2, 48_000).build(|_, _| 0.5));
        g.cmd(play(1));
        assert!((ch(&g.run(8, 2), 2, 0)[0] - 0.5).abs() < 1e-6);
    }

    #[test]
    fn stop_ramps_linearly_then_goes_silent() {
        let mut h = Harness::new();
        let v = h.add(VoiceSpec::new(1, 2, 48_000).build(|_, _| 0.5));
        h.cmd(play(1));
        h.run(128, 2);
        h.events();
        h.cmd(AudioCommand::Stop { voice_id: 1, fade_frames: 96, curve: CurveTable::LINEAR });
        let out = h.run(256, 2);
        let l = ch(&out, 2, 0);
        for f in 0..96 {
            let want = 0.5 * (1.0 - (f + 1) as f32 / 96.0);
            assert!((l[f] - want).abs() < 1e-4, "frame {f}: {} vs {want}", l[f]);
        }
        assert!(l[96..].iter().all(|&x| x == 0.0));
        assert_eq!(v.state(), VoiceState::Stopped);
        assert_eq!(h.events(), vec![AudioStatus::VoiceCompleted { voice_id: 1, natural_eof: false }]);
    }

    #[test]
    fn stop_fade_is_never_shorter_than_hard_stop_minimum() {
        let mut h = Harness::new();
        h.add(VoiceSpec::new(1, 1, 48_000).build(|_, _| 0.5));
        h.cmd(play(1));
        h.run(64, 2);
        h.cmd(AudioCommand::Stop { voice_id: 1, fade_frames: 0, curve: CurveTable::LINEAR });
        let out = ch(&h.run(200, 2), 2, 0);
        assert!(out[0] > 0.45, "debe haber rampa, no corte: {}", out[0]);
        assert!(out[94] > 0.0 && out[96] == 0.0);
    }

    #[test]
    fn natural_eof_completes_voice() {
        let mut h = Harness::new();
        let v = h.add(VoiceSpec::new(1, 2, 100).build(|_, _| 0.5));
        h.cmd(play(1));
        let l = ch(&h.run(256, 2), 2, 0);
        assert!(l[..100].iter().all(|&x| (x - 0.5).abs() < 1e-6));
        assert!(l[100..].iter().all(|&x| x == 0.0));
        assert_eq!(v.state(), VoiceState::Stopped);
        assert!(h.events().contains(&AudioStatus::VoiceCompleted { voice_id: 1, natural_eof: true }));
    }

    #[test]
    fn slice_loops_repeat_sample_accurately() {
        let mut h = Harness::new();
        let mut spec = VoiceSpec::new(1, 1, 100);
        spec.slices = vec![SliceSegment::new(0, 100, 3)];
        h.add(spec.build(|f, _| f as f32 / 1000.0));
        h.cmd(play(1));
        let l = ch(&h.run(400, 2), 2, 0);
        for f in 0..300 {
            assert!((l[f] - (f % 100) as f32 / 1000.0).abs() < 1e-6, "frame {f}");
        }
        assert!(l[300..].iter().all(|&x| x == 0.0));
        assert!(h.events().contains(&AudioStatus::VoiceCompleted { voice_id: 1, natural_eof: true }));
    }

    #[test]
    fn each_voice_reports_its_audible_peak_and_resets_on_read() {
        let mut h = Harness::new();
        let loud = h.add(VoiceSpec::new(1, 1, 4000).build(|_, _| 0.5));
        let quiet = h.add(VoiceSpec::new(2, 1, 4000).build(|_, _| 0.1));
        let silent = h.add(VoiceSpec::new(3, 1, 4000).build(|_, _| 0.9));
        // La tercera tiene su salida a -inf: suena «nada» aunque el archivo sea fuerte.
        h.cmd(AudioCommand::SetCrosspointDb { voice_id: 3, row: 1, col: 1, db: -120.0 });
        h.cmd(AudioCommand::SetCrosspointDb { voice_id: 3, row: 1, col: 2, db: -120.0 });
        for id in [1, 2, 3] {
            h.cmd(play(id));
        }
        h.run(1200, 2);
        // La bajada suave (5 ms) de la matriz de la tercera ya terminó; se descarta lo acumulado.
        let _ = (loud.take_peak(), quiet.take_peak(), silent.take_peak());
        h.run(256, 2);
        let (pl, pq, ps) = (loud.take_peak(), quiet.take_peak(), silent.take_peak());
        assert!((pl - 0.5).abs() < 0.02, "fuerte: {pl}");
        assert!((pq - 0.1).abs() < 0.01, "suave: {pq}");
        assert!(ps < 0.01, "una voz con las salidas a -inf no marca: {ps}");
        assert!(pl > pq);
        // Tras leerlo vuelve a cero hasta que la voz siga sonando.
        assert_eq!(loud.take_peak(), 0.0);
        h.run(32, 2);
        assert!(loud.take_peak() > 0.4);
    }

    #[test]
    fn devamp_exits_infinite_slice_at_its_end() {
        let mut h = Harness::new();
        let mut spec = VoiceSpec::new(1, 1, 200);
        spec.slices = vec![SliceSegment::new(0, 100, SliceSegment::INFINITE), SliceSegment::new(100, 200, 1)];
        h.add(spec.build(|f, _| f as f32 / 1000.0));
        h.cmd(play(1));
        h.run(250, 2); // 2.5 vueltas del slice infinito: cursor en 50
        h.events();
        h.cmd(AudioCommand::RequestDevamp { voice_id: 1, mode: DevampMode::ContinueToNextSlice, trigger_next_cue: true });
        let l = ch(&h.run(300, 2), 2, 0);
        for f in 0..150 {
            assert!((l[f] - (50 + f) as f32 / 1000.0).abs() < 1e-6, "frame {f}: {}", l[f]);
        }
        assert!(l[150..].iter().all(|&x| x == 0.0));
        let ev = h.events();
        assert_eq!(ev[0], AudioStatus::SliceDevamped { voice_id: 1, slice_index: 0, trigger_next_cue: true });
        assert!(ev.contains(&AudioStatus::VoiceCompleted { voice_id: 1, natural_eof: true }));
    }

    #[test]
    fn devamp_stop_at_slice_end() {
        let mut h = Harness::new();
        let mut spec = VoiceSpec::new(1, 1, 200);
        spec.slices = vec![SliceSegment::new(0, 100, SliceSegment::INFINITE), SliceSegment::new(100, 200, 1)];
        h.add(spec.build(|_, _| 0.5));
        h.cmd(play(1));
        h.cmd(AudioCommand::RequestDevamp { voice_id: 1, mode: DevampMode::StopAtSliceEnd, trigger_next_cue: false });
        let l = ch(&h.run(300, 2), 2, 0);
        assert!(l[..100].iter().all(|&x| x == 0.5) && l[100..].iter().all(|&x| x == 0.0));
        assert!(h.events().contains(&AudioStatus::VoiceCompleted { voice_id: 1, natural_eof: false }));
    }

    #[test]
    fn whole_program_infinite_loop() {
        let mut h = Harness::new();
        let mut spec = VoiceSpec::new(1, 1, 50);
        spec.infinite_loop = true;
        let v = h.add(spec.build(|f, _| f as f32 / 100.0));
        h.cmd(play(1));
        let l = ch(&h.run(500, 2), 2, 0);
        for f in 0..500 {
            assert!((l[f] - (f % 50) as f32 / 100.0).abs() < 1e-6);
        }
        assert_eq!(v.state(), VoiceState::Playing);
    }

    #[test]
    fn resampling_uses_catmull_rom() {
        let mut h = Harness::new();
        let v = h.add(VoiceSpec::new(1, 1, 4000).build(|f, _| f as f32 / 1000.0));
        h.cmd(play(1));
        h.cmd(AudioCommand::SetRate { voice_id: 1, rate: 1.5 });
        let l = ch(&h.run(1000, 2), 2, 0);
        for k in 2..1000 {
            let want = 1.5 * k as f32 / 1000.0;
            assert!((l[k] - want).abs() < 2e-5, "frame {k}: {} vs {want}", l[k]);
        }
        assert!((v.published_frame_pos.load(Ordering::Relaxed) as i64 - 1500).abs() <= 1);
    }

    #[test]
    fn sample_rate_conversion_44k1_to_48k() {
        let mut h = Harness::new();
        let mut spec = VoiceSpec::new(1, 1, 44_100);
        spec.file_sr = 44_100;
        let v = h.add(spec.build(|f, _| f as f32 / 44_100.0));
        h.cmd(play(1));
        let l = ch(&h.run(4800, 2), 2, 0);
        // 4800 frames de host = 4410 del archivo.
        assert!((l[4799] - 4799.0 * 44_100.0 / 48_000.0 / 44_100.0).abs() < 1e-4);
        assert!((v.published_frame_pos.load(Ordering::Relaxed) as i64 - 4410).abs() <= 2);
    }

    #[test]
    fn output_is_independent_of_block_size() {
        let render = |blocks: &[usize]| {
            let mut h = Harness::new();
            let mut spec = VoiceSpec::new(1, 2, 20_000);
            spec.slices = vec![SliceSegment::new(0, 777, 5)];
            h.add(spec.build(|f, c| ((f * (c + 3)) as f32 * 0.01).sin() * 0.7));
            h.cmd(play(1));
            h.cmd(AudioCommand::SetRate { voice_id: 1, rate: 1.3 });
            h.cmd(AudioCommand::SetCrosspointDb { voice_id: 1, row: 2, col: 2, db: -9.0 });
            let mut all = Vec::new();
            for &b in blocks {
                all.extend(h.run(b, 2));
            }
            all
        };
        let a = render(&[3000]);
        let b = render(&[100; 30]);
        let c = render(&[1, 127, 128, 129, 2615]);
        for (i, ((x, y), z)) in a.iter().zip(&b).zip(&c).enumerate() {
            assert!((x - y).abs() < 1e-6 && (x - z).abs() < 1e-6, "muestra {i}: {x} {y} {z}");
        }
    }

    #[test]
    fn multiple_voices_sum() {
        let mut h = Harness::new();
        h.add(VoiceSpec::new(1, 2, 1000).build(|_, _| 0.5));
        h.add(VoiceSpec::new(2, 2, 1000).build(|_, _| 0.25));
        h.cmd(play(1));
        h.cmd(play(2));
        let out = h.run(200, 2);
        assert!(out.iter().all(|&s| (s - 0.75).abs() < 1e-6));
    }

    #[test]
    fn mono_file_to_wider_output_and_extra_host_channels() {
        let mut h = Harness::new();
        h.add(VoiceSpec::new(1, 1, 1000).build(|_, _| 0.5));
        h.cmd(play(1));
        let out = h.run(100, 4); // el archivo solo enruta a la salida 1; 3 y 4 siguen en silencio
        assert!(ch(&out, 4, 0).iter().all(|&s| (s - 0.5).abs() < 1e-6));
        for c in 1..4 {
            assert!(ch(&out, 4, c).iter().all(|&s| s == 0.0), "canal {c}");
        }
    }

    #[test]
    fn pause_then_resume_keeps_position() {
        let mut h = Harness::new();
        let v = h.add(VoiceSpec::new(1, 1, 48_000).build(|_, _| 0.5));
        h.cmd(play(1));
        h.run(128, 2);
        h.cmd(AudioCommand::Pause { voice_id: 1, fade_frames: 240 });
        let l = ch(&h.run(400, 2), 2, 0);
        assert!(l[239] < 0.01 && l[240..].iter().all(|&x| x == 0.0));
        assert_eq!(v.state(), VoiceState::Paused);
        let pos = v.published_frame_pos.load(Ordering::Relaxed);
        h.run(100, 2);
        assert_eq!(v.published_frame_pos.load(Ordering::Relaxed), pos, "en pausa no avanza");
        h.cmd(AudioCommand::Resume { voice_id: 1, fade_frames: 240 });
        let l = ch(&h.run(400, 2), 2, 0);
        assert!(l[0] < 0.01 && (l[399] - 0.5).abs() < 1e-6);
        assert_eq!(v.state(), VoiceState::Playing);
        assert!(h.events().iter().all(|e| !matches!(e, AudioStatus::VoiceCompleted { .. })));
    }

    #[test]
    fn seek_jumps_and_selects_slice() {
        let mut h = Harness::new();
        let mut spec = VoiceSpec::new(1, 1, 200);
        spec.slices = vec![SliceSegment::new(0, 100, 1), SliceSegment::new(100, 200, 1)];
        h.add(spec.build(|f, _| f as f32 / 1000.0));
        h.cmd(play(1));
        h.cmd(AudioCommand::Seek { voice_id: 1, target_frame: 150 });
        let l = ch(&h.run(80, 2), 2, 0);
        for f in 0..50 {
            assert!((l[f] - (150 + f) as f32 / 1000.0).abs() < 1e-6);
        }
        assert!(l[50..].iter().all(|&x| x == 0.0));
    }

    #[test]
    fn absolute_cell_fade_with_stop_target() {
        let mut h = Harness::new();
        let v = h.add(VoiceSpec::new(1, 1, 96_000).build(|_, _| 0.5));
        h.cmd(play(1));
        h.cmd(AudioCommand::BeginCellFade {
            voice_id: 1,
            fade_id: 7,
            mode: FadeMode::Absolute,
            row: 1,
            col: 1,
            target_db: -20.0,
            duration_frames: 4800,
            curve: CurveTable::LINEAR,
            stop_target_when_done: true,
        });
        let l = ch(&h.run(4800 + 2400, 2), 2, 0);
        assert!(l.windows(2).take(4700).all(|w| w[1] <= w[0] + 1e-6), "la caída debe ser monótona");
        let ev = h.events();
        assert!(ev.contains(&AudioStatus::FadeCompleted { voice_id: 1, fade_id: 7, stopped_target: true }));
        // El Stop disparado al terminar el fade (240 frames) cae dentro del mismo bloque.
        assert!(ev.contains(&AudioStatus::VoiceCompleted { voice_id: 1, natural_eof: false }));
        assert_eq!(v.state(), VoiceState::Stopped);
        assert!(l[7199] == 0.0);
    }

    #[test]
    fn relative_cell_fade_adds_offset() {
        let mut h = Harness::new();
        h.add(VoiceSpec::new(1, 1, 96_000).build(|_, _| 0.5));
        h.cmd(play(1));
        h.cmd(AudioCommand::BeginCellFade {
            voice_id: 1,
            fade_id: 1,
            mode: FadeMode::Relative,
            row: 1,
            col: 1,
            target_db: -6.0,
            duration_frames: 1000,
            curve: CurveTable::s_curve(),
            stop_target_when_done: false,
        });
        let l = ch(&h.run(10_000, 2), 2, 0);
        let want = 0.5 * 10f32.powf(-6.0 / 20.0);
        assert!((l[9999] - want).abs() < 1e-5, "{} vs {want}", l[9999]);
    }

    #[test]
    fn newer_fade_takes_over_cell() {
        let mut h = Harness::new();
        let v = h.add(VoiceSpec::new(1, 1, 96_000).build(|_, _| 0.5));
        h.cmd(play(1));
        let fade = |id, db| AudioCommand::BeginCellFade {
            voice_id: 1,
            fade_id: id,
            mode: FadeMode::Absolute,
            row: 1,
            col: 1,
            target_db: db,
            duration_frames: 2000,
            curve: CurveTable::LINEAR,
            stop_target_when_done: false,
        };
        h.cmd(fade(1, -40.0));
        h.run(500, 2);
        h.cmd(fade(2, -6.0));
        let l = ch(&h.run(20_000, 2), 2, 0);
        assert!((l[19_999] - 0.5 * 10f32.powf(-6.0 / 20.0)).abs() < 1e-4);
        assert_eq!(v.state(), VoiceState::Playing);
    }

    #[test]
    fn duck_envelope_and_mute_mask() {
        let mut h = Harness::new();
        h.add(VoiceSpec::new(1, 2, 96_000).build(|_, _| 0.5));
        h.cmd(play(1));
        h.cmd(AudioCommand::SetDuckEnvelope { voice_id: 1, target_duck_linear: 0.25, slew_frames: 480 });
        let out = h.run(1000, 2);
        let l = ch(&out, 2, 0);
        assert!((l[479] - 0.5 * (1.0 - 0.75 * 480.0 / 480.0)).abs() < 1e-4 || (l[479] - 0.125).abs() < 1e-3);
        assert!((l[999] - 0.125).abs() < 1e-6);
        h.cmd(AudioCommand::SetMuteSoloMask { voice_id: 1, mute_mask: 0b01, solo_mask: 0 });
        let out = h.run(48_000, 2);
        assert!(ch(&out, 2, 0).last().unwrap().abs() < 1e-6, "canal silenciado");
        assert!((ch(&out, 2, 1).last().unwrap() - 0.125).abs() < 1e-6);
    }

    #[test]
    fn panic_all_stops_every_voice() {
        let mut h = Harness::new();
        let a = h.add(VoiceSpec::new(1, 1, 96_000).build(|_, _| 0.5));
        let b = h.add(VoiceSpec::new(2, 1, 96_000).build(|_, _| 0.5));
        h.cmd(play(1));
        h.cmd(play(2));
        h.run(100, 2);
        h.cmd(AudioCommand::PanicAll { fade_frames: 480 });
        let l = ch(&h.run(600, 2), 2, 0);
        assert!(l[480..].iter().all(|&x| x == 0.0));
        assert_eq!((a.state(), b.state()), (VoiceState::Stopped, VoiceState::Stopped));
        assert_eq!(h.events().iter().filter(|e| matches!(e, AudioStatus::VoiceCompleted { natural_eof: false, .. })).count(), 2);
    }

    #[test]
    fn restart_after_stop_rewinds() {
        let mut h = Harness::new();
        h.add(VoiceSpec::new(1, 1, 96_000).build(|f, _| f as f32 / 10_000.0));
        h.cmd(play(1));
        h.run(500, 2);
        h.cmd(AudioCommand::Stop { voice_id: 1, fade_frames: 96, curve: CurveTable::LINEAR });
        h.run(200, 2);
        h.cmd(play(1));
        let l = ch(&h.run(10, 2), 2, 0);
        assert!((l[5] - 0.0005).abs() < 1e-6, "debe volver al inicio: {}", l[5]);
    }

    #[test]
    fn underrun_is_reported_once_per_chunk() {
        let mut h = Harness::new();
        let mut spec = VoiceSpec::new(1, 1, 100); // datos para 100 frames
        spec.end_frame = 10_000; // pero la región es más larga
        h.add(spec.build(|_, _| 0.5));
        h.cmd(play(1));
        let l = ch(&h.run(256, 2), 2, 0);
        assert!(l[100..].iter().all(|&x| x == 0.0));
        let under: Vec<_> = h.events().into_iter().filter(|e| matches!(e, AudioStatus::DiskBufferUnderrun { .. })).collect();
        assert_eq!(under[0], AudioStatus::DiskBufferUnderrun { voice_id: 1, at_frame: 100 });
        assert_eq!(under.len(), 2); // trozos [0,128) y [128,256)
    }

    #[test]
    fn seam_detector_flags_abrupt_onset_only() {
        let mut h = Harness::new();
        h.add(VoiceSpec::new(1, 2, 96_000).build(|_, _| 0.5));
        h.cmd(play(1));
        h.run(256, 2);
        let seams: Vec<_> = h.events().into_iter().filter(|e| matches!(e, AudioStatus::SeamDetected { .. })).collect();
        assert_eq!(seams.len(), 2, "un aviso por canal en el bloque de arranque");
        match seams[0] {
            AudioStatus::SeamDetected { output_channel, jump_delta } => {
                assert_eq!(output_channel, 0);
                assert!((jump_delta - 0.5).abs() < 1e-6);
            }
            _ => unreachable!(),
        }
        h.run(256, 2);
        assert!(h.events().iter().all(|e| !matches!(e, AudioStatus::SeamDetected { .. })), "continuidad: sin avisos");
    }

    #[test]
    fn seam_detector_ignores_loud_treble() {
        let mut h = Harness::new();
        h.add(VoiceSpec::new(1, 2, 96_000).build(|f, _| 0.9 * (2.0 * std::f32::consts::PI * 5000.0 * f as f32 / 48_000.0).sin()));
        h.cmd(play(1));
        for _ in 0..8 {
            h.run(256, 2);
        }
        // Pasos de ~0.59 por muestra, muy por encima de 0.08, pero dentro de 4x el paso máximo.
        assert!(h.events().iter().all(|e| !matches!(e, AudioStatus::SeamDetected { .. })));
    }

    #[test]
    fn seam_detector_flags_hard_cut_in_continuous_signal() {
        let mut h = Harness::new();
        // Señal suave que salta de golpe en el frame 256 (límite de bloque).
        h.add(VoiceSpec::new(1, 1, 96_000).build(|f, _| if f < 256 { 0.0 } else { 0.4 }));
        h.cmd(play(1));
        h.run(256, 2);
        h.events();
        h.run(256, 2);
        assert!(h.events().contains(&AudioStatus::SeamDetected { output_channel: 0, jump_delta: 0.4 }));
    }

    #[test]
    fn nonmultiple_buffer_and_zero_frames() {
        let mut h = Harness::new();
        h.add(VoiceSpec::new(1, 1, 96_000).build(|_, _| 0.5));
        h.cmd(play(1));
        assert!(h.run(0, 2).is_empty());
        let out = h.run(129, 2); // un trozo de 128 + uno de 1
        assert!(ch(&out, 2, 0).iter().all(|&s| (s - 0.5).abs() < 1e-6));
        // out_channels = 0 no debe entrar en pánico
        let mut empty: [f32; 0] = [];
        process_rt_callback(&mut empty, 0, SR, &h.handle, &mut h.cmd_rx, &mut h.st_tx, &mut h.tail);
    }
}
