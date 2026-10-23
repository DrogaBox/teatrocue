//! `qlab-bench`: prueba de estrés del motor completo (daemon + secuenciador + motor RT + backend).
//!
//! Dispara N voces simultáneas (por defecto 64) de un archivo a 44,1 kHz sobre un dispositivo a 48 kHz,
//! de modo que cada voz pasa por el remuestreador cúbico (Catmull-Rom de 4 puntos), y mide durante
//! `--duration-sec` segundos: duración del callback, jitter de llegada, xruns, asignaciones en el hilo
//! de tiempo real, y CPU/RAM del proceso y del sistema. Escribe un informe JSON.

mod sysmetrics;

use std::sync::atomic::Ordering;
use std::time::{Duration, Instant};

use qlab_daemon::backend_audio::{list_output_devices, BackendKind};
use qlab_daemon::{AllocCheck, Daemon, DaemonConfig};
use qlab_protocol::cue_dto::{CueBasicsData, CueId, MediaTarget};
use qlab_protocol::ipc_client::IpcClient;
use qlab_protocol::ipc_msg::{ClientRequest, ServerResponse, IPC_PROTOCOL_VERSION};
use qlab_protocol::show_format::{AudioBody, CueBody, CueNodeDto, ShowFile, SHOW_FORMAT};

#[global_allocator]
static ALLOCATOR: qlab_engine::alloc_guard::RtGuardAllocator = qlab_engine::alloc_guard::RtGuardAllocator::system();

const USAGE: &str = "\
qlab-bench — estrés del motor de audio de TeatroCue

  --duration-sec N     duración de la medición con todas las voces sonando (60)
  --cues N             voces simultáneas (64)
  --backend cpal|null  dispositivo real o virtual (cpal)
  --host NOMBRE        host de cpal (wasapi, asio, pipewire, alsa…)
  --device NOMBRE      dispositivo de salida
  --exclusive          Windows: WASAPI en modo exclusivo
  --buffer FRAMES      tamaño de buffer (128)
  --rate HZ            frecuencia del dispositivo (48000)
  --file-rate HZ       frecuencia de los archivos (44100; distinta de --rate fuerza el remuestreo cúbico)
  --channels N         canales de salida (2)
  --json-report RUTA   escribe el informe JSON
  --list-devices       lista las salidas de audio y termina
  --write-demo-show DIR  escribe un show de demostración (tono + slice infinito) en DIR y termina
";

struct Args {
    duration: u64,
    cues: usize,
    backend: BackendKind,
    host: Option<String>,
    device: Option<String>,
    exclusive: bool,
    buffer: u32,
    rate: u32,
    file_rate: u32,
    channels: u16,
    report: Option<String>,
    list: bool,
    demo_dir: Option<String>,
}

fn parse() -> Result<Args, String> {
    let mut a = Args { duration: 60, cues: 64, backend: BackendKind::Cpal, host: None, device: None, exclusive: false, buffer: 128, rate: 48_000, file_rate: 44_100, channels: 2, report: None, list: false, demo_dir: None };
    let mut it = std::env::args().skip(1);
    while let Some(f) = it.next() {
        let mut val = |name: &str| it.next().ok_or_else(|| format!("{name} necesita un valor"));
        fn num<T: std::str::FromStr>(s: String, n: &str) -> Result<T, String> {
            s.parse().map_err(|_| format!("{n}: `{s}` no es válido"))
        }
        match f.as_str() {
            "--duration-sec" => a.duration = num(val(&f)?, &f)?,
            "--cues" => a.cues = num(val(&f)?, &f)?,
            "--backend" => {
                a.backend = match val(&f)?.as_str() {
                    "cpal" => BackendKind::Cpal,
                    "null" => BackendKind::Null,
                    o => return Err(format!("backend `{o}` desconocido")),
                }
            }
            "--host" => a.host = Some(val(&f)?),
            "--device" => a.device = Some(val(&f)?),
            "--exclusive" => a.exclusive = true,
            "--buffer" => a.buffer = num(val(&f)?, &f)?,
            "--rate" => a.rate = num(val(&f)?, &f)?,
            "--file-rate" => a.file_rate = num(val(&f)?, &f)?,
            "--channels" => a.channels = num(val(&f)?, &f)?,
            "--json-report" => a.report = Some(val(&f)?),
            "--list-devices" => a.list = true,
            "--write-demo-show" => a.demo_dir = Some(val(&f)?),
            "--help" | "-h" => {
                print!("{USAGE}");
                std::process::exit(0);
            }
            // Compatibilidad con el Makefile de la especificación.
            "--driver" | "--matrix-fades" | "--channels-in" | "--channels-out" => {
                let _ = val(&f)?;
            }
            "--assert-zero-xruns" | "--assert-zero-seams" => {}
            o => return Err(format!("opción desconocida `{o}`")),
        }
    }
    Ok(a)
}

fn write_wav(path: &std::path::Path, channels: usize, frames: u64, rate: u32, amp: f32) {
    use std::io::Write;
    let data_len = frames as u32 * channels as u32 * 4;
    let mut w = std::io::BufWriter::new(std::fs::File::create(path).expect("crear WAV"));
    w.write_all(b"RIFF").unwrap();
    w.write_all(&(36 + data_len).to_le_bytes()).unwrap();
    w.write_all(b"WAVEfmt ").unwrap();
    w.write_all(&16u32.to_le_bytes()).unwrap();
    w.write_all(&3u16.to_le_bytes()).unwrap();
    w.write_all(&(channels as u16).to_le_bytes()).unwrap();
    w.write_all(&rate.to_le_bytes()).unwrap();
    w.write_all(&(rate * channels as u32 * 4).to_le_bytes()).unwrap();
    w.write_all(&(channels as u16 * 4).to_le_bytes()).unwrap();
    w.write_all(&32u16.to_le_bytes()).unwrap();
    w.write_all(b"data").unwrap();
    w.write_all(&data_len.to_le_bytes()).unwrap();
    for f in 0..frames {
        let t = f as f32 / rate as f32;
        for c in 0..channels {
            // Una sinusoide por canal; amplitud baja para que 64 voces sumadas no saturen.
            let s = amp * (2.0 * std::f32::consts::PI * (220.0 + 55.0 * c as f32) * t).sin();
            w.write_all(&s.to_le_bytes()).unwrap();
        }
    }
    w.flush().unwrap();
}

/// Show de demostración para probar a mano: tono de 8 s, una cue simple, una con un slice infinito
/// (suena el primer segundo y luego repite el tramo de 1 s a 3 s hasta un Devamp) y otra simple.
fn write_demo(dir: &str, rate: u32) -> i32 {
    let dir = std::path::Path::new(dir);
    if let Err(e) = std::fs::create_dir_all(dir) {
        eprintln!("[bench] no se pudo crear {}: {e}", dir.display());
        return 1;
    }
    let wav = dir.join("demo-tone.wav");
    write_wav(&wav, 2, 8 * rate as u64, rate, 0.2);
    let mk = |id: u64, name: &str| CueNodeDto {
        basics: CueBasicsData {
            unique_id: CueId(id),
            number: id.to_string(),
            name: name.into(),
            file_target: Some(MediaTarget { absolute_path: wav.to_string_lossy().into(), ..Default::default() }),
            ..Default::default()
        },
        triggers: Default::default(),
        dead: false,
        body: CueBody::Audio(AudioBody { time_loops: Default::default(), levels: None, eq: Default::default() }),
    };
    let mut looped = mk(2, "Tono con slice infinito (Devamp)");
    if let CueBody::Audio(a) = &mut looped.body {
        a.time_loops.file_sample_rate = rate;
        a.time_loops.end_time_sec = 8.0;
        a.time_loops.slices = vec![
            qlab_protocol::cue_dto::AudioSliceMarker::new(rate as u64, qlab_protocol::cue_dto::INFINITE_SLICE_LOOP),
            qlab_protocol::cue_dto::AudioSliceMarker::new(3 * rate as u64, 1),
        ];
    }
    let show = ShowFile { format: SHOW_FORMAT.into(), name: "Demo TeatroCue".into(), sample_rate: rate, carts: Default::default(), cues: vec![mk(1, "Tono simple"), looped, mk(3, "Tono simple (final)")] };
    let path = dir.join("demo-show.teatroshow");
    match std::fs::write(&path, show.to_json()) {
        Ok(()) => {
            println!("{}", path.display());
            0
        }
        Err(e) => {
            eprintln!("[bench] no se pudo escribir {}: {e}", path.display());
            1
        }
    }
}

fn percentile(sorted: &[f64], p: f64) -> f64 {
    if sorted.is_empty() {
        return 0.0;
    }
    sorted[((sorted.len() - 1) as f64 * p).round() as usize]
}

fn main() {
    let args = match parse() {
        Ok(a) => a,
        Err(e) => {
            eprintln!("error: {e}\n\n{USAGE}");
            std::process::exit(2);
        }
    };
    if let Some(dir) = &args.demo_dir {
        std::process::exit(write_demo(dir, args.rate));
    }
    if args.list {
        for (host, device) in list_output_devices() {
            println!("{host}\t{device}");
        }
        return;
    }
    std::process::exit(run(args));
}

fn run(args: Args) -> i32 {
    let tmp = std::env::temp_dir();
    let pid = std::process::id();
    let wav = tmp.join(format!("qlab-bench-{pid}.wav"));
    let show_path = tmp.join(format!("qlab-bench-{pid}.json"));
    let secs = args.duration + 15;
    eprintln!("[bench] generando {} s de audio a {} Hz…", secs, args.file_rate);
    write_wav(&wav, args.channels as usize, secs * args.file_rate as u64, args.file_rate, 0.012);

    let cues: Vec<CueNodeDto> = (1..=args.cues as u64)
        .map(|id| CueNodeDto {
            basics: CueBasicsData {
                unique_id: CueId(id),
                number: id.to_string(),
                name: format!("voz {id}"),
                file_target: Some(MediaTarget { absolute_path: wav.to_string_lossy().into(), ..Default::default() }),
                ..Default::default()
            },
            triggers: Default::default(),
            dead: false,
            body: CueBody::Audio(AudioBody { time_loops: Default::default(), levels: None, eq: Default::default() }),
        })
        .collect();
    let show = ShowFile { format: SHOW_FORMAT.into(), name: "qlab-bench".into(), sample_rate: args.rate, carts: Default::default(), cues };
    std::fs::write(&show_path, show.to_json()).expect("escribir el show");

    let socket = if cfg!(windows) { format!(r"\\.\pipe\qlab_bench_{pid}") } else { tmp.join(format!("qlab-bench-{pid}.sock")).to_string_lossy().into_owned() };
    let mut cfg = DaemonConfig::default();
    cfg.socket = socket.clone();
    cfg.shm_name = format!("qlab_bench_{pid}");
    cfg.backend.kind = args.backend;
    cfg.backend.host = args.host.clone();
    cfg.backend.device = args.device.clone();
    cfg.backend.sample_rate = args.rate;
    cfg.backend.channels = args.channels;
    cfg.backend.buffer_frames = args.buffer;
    cfg.backend.exclusive = args.exclusive;
    cfg.backend.rt_alloc_check = true;
    cfg.alloc_check = AllocCheck::Count;
    cfg.lock_memory = false;
    cfg.disk_threads = 4;
    cfg.show = Some(show_path.clone());

    let cleanup = |code: i32| {
        let _ = std::fs::remove_file(&wav);
        let _ = std::fs::remove_file(&show_path);
        code
    };

    let mut daemon = match Daemon::start(cfg) {
        Ok(d) => d,
        Err(e) => {
            eprintln!("[bench] el daemon no arrancó: {e}");
            return cleanup(3);
        }
    };
    let info = daemon.backend_info().clone();
    eprintln!("[bench] backend: {} · {} · {} Hz · {} canales · buffer {} frames ({:.2} ms) · exclusivo: {}", info.backend, info.device, info.sample_rate, info.channels, info.buffer_frames, info.latency_ms(), info.exclusive_active);
    for w in &info.warnings {
        eprintln!("[bench] aviso: {w}");
    }
    let period_us = info.buffer_frames as f64 * 1e6 / info.sample_rate.max(1) as f64;

    let client = match IpcClient::connect(&socket) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("[bench] no se pudo conectar al IPC: {e:?}");
            return cleanup(3);
        }
    };
    let t = Duration::from_secs(5);
    let _ = client.request(ClientRequest::Hello { client: "qlab-bench".into(), protocol: IPC_PROTOCOL_VERSION }, t);
    // Margen para que el cargador deje todas las voces listas (pre-carga) antes del disparo.
    std::thread::sleep(Duration::from_millis(1500));

    let health = daemon.health().clone();
    let sys0 = sysmetrics::snapshot();
    let t_fire = Instant::now();
    let mut fired = 0usize;
    for _ in 0..args.cues {
        match client.request(ClientRequest::Go, t) {
            Ok(ServerResponse::Ok) => fired += 1,
            other => eprintln!("[bench] GO sin éxito: {other:?}"),
        }
    }
    let fire_ms = t_fire.elapsed().as_secs_f64() * 1e3;
    eprintln!("[bench] {fired}/{} GO enviados en {fire_ms:.1} ms", args.cues);

    // Las métricas del régimen estable parten de aquí (se descarta el transitorio del disparo).
    std::thread::sleep(Duration::from_millis(500));
    let base = Counters::read(&health);
    health.peak_cpu_permille.store(0, Ordering::Relaxed);
    health.callback_max_ns.store(0, Ordering::Relaxed);
    health.jitter_max_ns.store(0, Ordering::Relaxed);
    let sys_a = sysmetrics::snapshot();
    let t_a = Instant::now();

    let mut running_min = usize::MAX;
    let mut running_max = 0usize;
    let mut cpu_proc: Vec<f64> = Vec::new();
    let mut cpu_sys: Vec<f64> = Vec::new();
    let mut rss_peak = 0u64;
    let mut avail_min = u64::MAX;
    let mut prev = (sys_a, t_a);
    let ncpu = sysmetrics::logical_cpus() as f64;
    let end = t_a + Duration::from_secs(args.duration);
    while Instant::now() < end {
        std::thread::sleep(Duration::from_millis(1000).min(end.saturating_duration_since(Instant::now())));
        if let Ok(ServerResponse::Status(s)) = client.request(ClientRequest::GetStatus, t) {
            running_min = running_min.min(s.running.len());
            running_max = running_max.max(s.running.len());
        }
        let now = (sysmetrics::snapshot(), Instant::now());
        let dt = now.1.duration_since(prev.1).as_nanos() as f64;
        if dt > 0.0 {
            cpu_proc.push((now.0.process_cpu_ns.saturating_sub(prev.0.process_cpu_ns)) as f64 / dt * 100.0 / ncpu);
            let tot = now.0.system_total_ns.saturating_sub(prev.0.system_total_ns) as f64;
            let idle = now.0.system_idle_ns.saturating_sub(prev.0.system_idle_ns) as f64;
            if tot > 0.0 {
                cpu_sys.push((1.0 - idle / tot) * 100.0);
            }
        }
        rss_peak = rss_peak.max(now.0.process_rss);
        if now.0.mem_available > 0 {
            avail_min = avail_min.min(now.0.mem_available);
        }
        prev = now;
    }
    let measured = t_a.elapsed().as_secs_f64();
    let fin = Counters::read(&health);
    let sys_end = sysmetrics::snapshot();
    let status = match client.request(ClientRequest::GetStatus, t) {
        Ok(ServerResponse::Status(s)) => Some(s),
        _ => None,
    };
    let _ = client.request(ClientRequest::HardStopAll, t);
    std::thread::sleep(Duration::from_millis(100));
    let alloc_violations = health.rt_alloc_violations.load(Ordering::Relaxed);
    let peak_cpu_permille = health.peak_cpu_permille.load(Ordering::Relaxed);
    let max_cb_ns = health.callback_max_ns.load(Ordering::Relaxed);
    let max_jitter_ns = health.jitter_max_ns.load(Ordering::Relaxed);
    let audio_priority_ok = health.audio_priority_ok.load(Ordering::Relaxed);
    let seq_overruns = health.seq_overruns.load(Ordering::Relaxed);
    let seq_max_late_ns = health.seq_max_late_ns.load(Ordering::Relaxed);
    drop(client);
    daemon.stop();

    let d = |a: u64, b: u64| b.saturating_sub(a);
    let callbacks = d(base.callbacks, fin.callbacks);
    let xruns = d(base.xruns, fin.xruns);
    let cb_mean_us = if callbacks > 0 { d(base.cb_total, fin.cb_total) as f64 / callbacks as f64 / 1e3 } else { 0.0 };
    let jit_n = d(base.jit_n, fin.jit_n);
    let jit_mean_us = if jit_n > 0 { d(base.jit_total, fin.jit_total) as f64 / jit_n as f64 / 1e3 } else { 0.0 };
    let over_budget = d(base.over, fin.over);
    let max_cb_us = max_cb_ns as f64 / 1e3;
    cpu_proc.sort_by(|a, b| a.partial_cmp(b).unwrap());
    cpu_sys.sort_by(|a, b| a.partial_cmp(b).unwrap());
    let avg = |v: &[f64]| if v.is_empty() { 0.0 } else { v.iter().sum::<f64>() / v.len() as f64 };
    let mb = |b: u64| b as f64 / (1024.0 * 1024.0);
    let expected_cbs = measured * info.sample_rate as f64 / info.buffer_frames.max(1) as f64;
    let min_running = if running_min == usize::MAX { 0 } else { running_min };

    let pass = xruns == 0 && alloc_violations == 0 && max_cb_us < period_us && min_running >= args.cues && fired == args.cues;
    let report = serde_json::json!({
        "pass": pass,
        "platform": std::env::consts::OS,
        "backend": { "name": info.backend, "device": info.device, "sample_rate": info.sample_rate, "channels": info.channels,
                     "buffer_frames": info.buffer_frames, "period_us": period_us, "exclusive": info.exclusive_active, "warnings": info.warnings },
        "voices": { "requested": args.cues, "fired": fired, "running_min": min_running, "running_max": running_max,
                    "file_rate": args.file_rate, "resampler": if args.file_rate != info.sample_rate { "catmull-rom 4 puntos" } else { "ninguno (tasa nativa)" },
                    "fire_all_ms": fire_ms },
        "duration_sec": measured,
        "callbacks": { "count": callbacks, "expected": expected_cbs.round(), "missing_pct": if expected_cbs > 0.0 { (1.0 - callbacks as f64 / expected_cbs) * 100.0 } else { 0.0 },
                       "mean_us": cb_mean_us, "max_us": max_cb_us, "over_budget": over_budget, "peak_cpu_permille": peak_cpu_permille },
        "jitter_us": { "mean": jit_mean_us, "max": max_jitter_ns as f64 / 1e3, "samples": jit_n },
        "xruns": xruns,
        "xruns_total_run": fin.xruns,
        "seam_glitches": serde_json::Value::Null,
        "seam_check": "no medido (el motor no expone un contador de costuras)",
        "rt_allocations": alloc_violations,
        "max_callback_us": max_cb_us,
        "audio_priority_ok": audio_priority_ok,
        "sequencer": { "overruns": seq_overruns, "max_late_us": seq_max_late_ns as f64 / 1e3 },
        "cpu": { "logical_cpus": ncpu, "process_pct_of_machine_mean": avg(&cpu_proc), "process_pct_of_machine_p95": percentile(&cpu_proc, 0.95), "process_pct_of_machine_max": percentile(&cpu_proc, 1.0),
                 "system_pct_mean": avg(&cpu_sys), "system_pct_max": percentile(&cpu_sys, 1.0) },
        "memory_mb": { "process_rss_peak": mb(rss_peak.max(sys_end.process_peak_rss)), "process_rss_start": mb(sys0.process_rss),
                       "system_total": mb(sys_end.mem_total), "system_available_min": if avail_min == u64::MAX { 0.0 } else { mb(avail_min) } },
        "daemon_status": status.map(|s| serde_json::json!({ "xruns": s.xruns, "audio_stalled": s.audio_stalled, "sequencer_stalled": s.sequencer_stalled, "safe_mute": s.safe_mute, "rt_alloc_violations": s.rt_alloc_violations })),
    });
    let text = serde_json::to_string_pretty(&report).unwrap();
    println!("{text}");
    if let Some(p) = &args.report {
        if let Err(e) = std::fs::write(p, &text) {
            eprintln!("[bench] no se pudo escribir {p}: {e}");
        }
    }
    eprintln!("[bench] {}", if pass { "PASS" } else { "FAIL" });
    cleanup(if pass { 0 } else { 1 })
}

struct Counters {
    callbacks: u64,
    xruns: u64,
    cb_total: u64,
    over: u64,
    jit_total: u64,
    jit_n: u64,
}

impl Counters {
    fn read(h: &qlab_daemon::health::Health) -> Self {
        let l = |a: &std::sync::atomic::AtomicU64| a.load(Ordering::Relaxed);
        Self { callbacks: l(&h.audio_callbacks), xruns: l(&h.xruns), cb_total: l(&h.callback_total_ns), over: l(&h.callback_over_budget), jit_total: l(&h.jitter_total_ns), jit_n: l(&h.jitter_samples) }
    }
}
