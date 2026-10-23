//! Pruebas del binario real `teatrocue-daemon` como proceso: arranque, IPC, apagado limpio por
//! señales (SIGINT/SIGTERM) y línea de órdenes. En Windows el equivalente (Ctrl+C) se valida en la VM.
#![cfg(unix)]

use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use qlab_protocol::cue_dto::{CueBasicsData, CueId, MediaTarget};
use qlab_protocol::ipc_client::IpcClient;
use qlab_protocol::ipc_msg::{ClientRequest, ServerEvent, ServerResponse};
use qlab_protocol::show_format::{AudioBody, CueBody, CueNodeDto, ShowFile, SHOW_FORMAT};

const BIN: &str = env!("CARGO_BIN_EXE_teatrocue-daemon");
const WAIT: Duration = Duration::from_secs(5);

fn temp(tag: &str, ext: &str) -> PathBuf {
    std::env::temp_dir().join(format!("qlab-proc-{}-{tag}.{ext}", std::process::id()))
}

fn write_constant_wav(path: &Path, frames: u32, level: f32) {
    let mut data = Vec::new();
    for _ in 0..frames * 2 {
        data.extend_from_slice(&level.to_le_bytes());
    }
    let mut w = Vec::new();
    w.extend_from_slice(b"RIFF");
    w.extend_from_slice(&(36 + data.len() as u32).to_le_bytes());
    w.extend_from_slice(b"WAVEfmt ");
    w.extend_from_slice(&16u32.to_le_bytes());
    w.extend_from_slice(&3u16.to_le_bytes());
    w.extend_from_slice(&2u16.to_le_bytes());
    w.extend_from_slice(&48_000u32.to_le_bytes());
    w.extend_from_slice(&(48_000u32 * 8).to_le_bytes());
    w.extend_from_slice(&8u16.to_le_bytes());
    w.extend_from_slice(&32u16.to_le_bytes());
    w.extend_from_slice(b"data");
    w.extend_from_slice(&(data.len() as u32).to_le_bytes());
    w.extend_from_slice(&data);
    std::fs::write(path, w).unwrap();
}

struct Proc {
    child: Child,
    socket: String,
    shm: String,
    files: Vec<PathBuf>,
}

impl Drop for Proc {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
        for f in &self.files {
            let _ = std::fs::remove_file(f);
        }
        let _ = std::fs::remove_file(&self.socket);
        let _ = std::fs::remove_file(Path::new("/dev/shm").join(&self.shm));
    }
}

fn spawn_daemon(tag: &str, with_show: bool) -> Proc {
    let socket = temp(tag, "sock").to_string_lossy().into_owned();
    let shm = format!("qlab_proc_{}_{tag}", std::process::id());
    let mut files = Vec::new();
    let mut cmd = Command::new(BIN);
    cmd.args(["--backend", "null", "--no-mlock", "--socket", &socket, "--shm", &shm]).stdout(Stdio::null()).stderr(Stdio::piped());
    if with_show {
        let wav = temp(tag, "wav");
        write_constant_wav(&wav, 480_000, 0.2); // 10 s: sigue sonando cuando llega la señal
        let show = ShowFile {
            format: SHOW_FORMAT.into(),
            name: "proceso".into(),
            sample_rate: 48_000,
            carts: Default::default(),
            cues: vec![CueNodeDto {
                triggers: Default::default(),
                basics: CueBasicsData {
                    unique_id: CueId(1),
                    file_target: Some(MediaTarget { absolute_path: wav.to_string_lossy().into(), ..Default::default() }),
                    ..Default::default()
                },
                dead: false,
                body: CueBody::Audio(AudioBody { time_loops: Default::default(), levels: None, eq: Default::default() }),
            }],
        };
        let path = temp(tag, "json");
        std::fs::write(&path, show.to_json()).unwrap();
        cmd.arg("--show").arg(&path);
        files.extend([wav, path]);
    }
    let child = cmd.spawn().expect("no se pudo lanzar el daemon");
    let mut p = Proc { child, socket, shm, files };
    let t0 = Instant::now();
    loop {
        if IpcClient::connect(&p.socket).is_ok() {
            return p;
        }
        if let Some(status) = p.child.try_wait().unwrap() {
            panic!("el daemon terminó al arrancar: {status}");
        }
        assert!(t0.elapsed() < WAIT, "el daemon no abrió su socket");
        std::thread::sleep(Duration::from_millis(20));
    }
}

fn wait_exit(p: &mut Proc) -> std::process::ExitStatus {
    let t0 = Instant::now();
    loop {
        if let Some(s) = p.child.try_wait().unwrap() {
            return s;
        }
        assert!(t0.elapsed() < WAIT, "el daemon no terminó tras la señal");
        std::thread::sleep(Duration::from_millis(10));
    }
}

fn signal_cleanly(sig: libc::c_int, tag: &str) {
    let mut p = spawn_daemon(tag, true);
    let c = IpcClient::connect(&p.socket).unwrap();
    assert_eq!(c.request(ClientRequest::Subscribe { events: true }, WAIT).unwrap(), ServerResponse::Ok);
    assert_eq!(c.request(ClientRequest::Go, WAIT).unwrap(), ServerResponse::Ok);
    c.wait_event(WAIT, |e| matches!(e, ServerEvent::CueStateChanged { state, .. } if state == "Running")).expect("la cue suena");

    // SAFETY: señal a un proceso hijo que acabamos de lanzar.
    assert_eq!(unsafe { libc::kill(p.child.id() as libc::pid_t, sig) }, 0);
    let status = wait_exit(&mut p);
    assert!(status.success(), "salida limpia, no {status}");
    assert!(c.wait_event(WAIT, |e| matches!(e, ServerEvent::ShuttingDown)).is_some(), "avisó a los clientes antes de cerrar");
    assert!(!Path::new(&p.socket).exists(), "el socket se elimina");
    assert!(!Path::new("/dev/shm").join(&p.shm).exists(), "la memoria compartida se elimina");
}

#[test]
fn sigterm_shuts_the_daemon_down_cleanly_while_playing() {
    signal_cleanly(libc::SIGTERM, "term");
}

#[test]
fn sigint_shuts_the_daemon_down_cleanly_while_playing() {
    signal_cleanly(libc::SIGINT, "int");
}

#[test]
fn the_shutdown_command_also_exits_with_success() {
    let mut p = spawn_daemon("cmd", false);
    let c = IpcClient::connect(&p.socket).unwrap();
    assert_eq!(c.request(ClientRequest::Shutdown, WAIT).unwrap(), ServerResponse::Ok);
    assert!(wait_exit(&mut p).success());
}

#[test]
fn command_line_help_version_and_errors() {
    let out = Command::new(BIN).arg("--help").output().unwrap();
    assert!(out.status.success());
    assert!(String::from_utf8_lossy(&out.stdout).contains("--exclusive"));
    let out = Command::new(BIN).arg("--version").output().unwrap();
    assert!(String::from_utf8_lossy(&out.stdout).starts_with("teatrocue-daemon "));
    let out = Command::new(BIN).args(["--rate", "12"]).output().unwrap();
    assert_eq!(out.status.code(), Some(2));
    assert!(String::from_utf8_lossy(&out.stderr).contains("--rate"));
    let out = Command::new(BIN).args(["--backend", "null", "--no-mlock", "--show", "/no/existe.json", "--socket", "/tmp/qlab-proc-bad.sock", "--shm", "qlab_proc_bad"]).output().unwrap();
    assert_eq!(out.status.code(), Some(1), "un show inicial roto impide arrancar");
    assert!(String::from_utf8_lossy(&out.stderr).contains("show inicial"));
    let out = Command::new(BIN).arg("--list-devices").output().unwrap();
    assert!(out.status.success());
}

#[test]
fn a_second_instance_on_the_same_socket_refuses_to_start() {
    let p = spawn_daemon("dup", false);
    let out = Command::new(BIN)
        .args(["--backend", "null", "--no-mlock", "--socket", &p.socket, "--shm", "qlab_proc_dup2"])
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(1));
    assert!(String::from_utf8_lossy(&out.stderr).contains("ya hay un daemon"));
    // El primero sigue vivo.
    let c = IpcClient::connect(&p.socket).unwrap();
    assert!(matches!(c.request(ClientRequest::Ping { nonce: 1 }, WAIT).unwrap(), ServerResponse::Pong { .. }));
    let _ = std::fs::remove_file("/dev/shm/qlab_proc_dup2");
}
