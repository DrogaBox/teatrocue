//! WASAPI en **modo exclusivo** dirigido por eventos (Windows).
//!
//! `cpal` solo abre WASAPI compartido, que pasa por el mezclador del sistema (`audiodg.exe`). Aquí se
//! abre el endpoint con `AUDCLNT_SHAREMODE_EXCLUSIVE` + `AUDCLNT_STREAMFLAGS_EVENTCALLBACK`, de modo
//! que el hilo de renderizado recibe un evento por periodo y escribe directamente en el búfer del
//! dispositivo (latencias de 2,7 a 5,3 ms).
//!
//! Los dispositivos suelen aceptar en exclusivo solo su formato nativo, así que se prueban en orden
//! f32, i32, i24 en contenedor de 32 bits, i24 empaquetado e i16, convirtiendo desde el `f32` del motor.
//!
//! **Sin verificar en hardware:** este módulo compila para `x86_64-pc-windows-gnu`, pero no se ha
//! ejecutado contra un dispositivo real. Se valida en la VM Tiny11 (fase 6). Si algo falla al abrir,
//! el backend cae a WASAPI compartido y lo avisa.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::channel;
use std::sync::Arc;
use std::thread::JoinHandle;
use std::time::Duration;

use windows::core::PCWSTR;
use windows::Win32::Foundation::{CloseHandle, HANDLE, WAIT_OBJECT_0, WAIT_TIMEOUT};
use windows::Win32::Media::Audio::{
    eConsole, eRender, IAudioClient, IAudioRenderClient, IMMDevice, IMMDeviceEnumerator, MMDeviceEnumerator, AUDCLNT_BUFFERFLAGS_SILENT,
    AUDCLNT_E_BUFFER_SIZE_NOT_ALIGNED, AUDCLNT_SHAREMODE_EXCLUSIVE, AUDCLNT_STREAMFLAGS_EVENTCALLBACK, WAVEFORMATEXTENSIBLE,
    WAVEFORMATEXTENSIBLE_0, WAVEFORMATEX,
};
use windows::Win32::Media::KernelStreaming::{KSAUDIO_SPEAKER_DIRECTOUT, KSDATAFORMAT_SUBTYPE_PCM, WAVE_FORMAT_EXTENSIBLE};
use windows::Win32::Media::Multimedia::KSDATAFORMAT_SUBTYPE_IEEE_FLOAT;
use windows::Win32::System::Com::{CoCreateInstance, CoInitializeEx, CoUninitialize, CLSCTX_ALL, COINIT_MULTITHREADED};
use windows::Win32::System::Threading::{CreateEventW, WaitForSingleObject};

use super::{BackendConfig, BackendInfo, RtRenderer};

/// Formatos que se intentan, en orden de preferencia.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum SampleKind {
    F32,
    I32,
    /// 24 bits significativos en un contenedor de 32 (los 24 en los bits altos).
    I24In32,
    I24Packed,
    I16,
}

const TRY_ORDER: [SampleKind; 5] = [SampleKind::F32, SampleKind::I32, SampleKind::I24In32, SampleKind::I24Packed, SampleKind::I16];

impl SampleKind {
    fn container_bytes(self) -> usize {
        match self {
            Self::F32 | Self::I32 | Self::I24In32 => 4,
            Self::I24Packed => 3,
            Self::I16 => 2,
        }
    }

    fn valid_bits(self) -> u16 {
        match self {
            Self::F32 | Self::I32 => 32,
            Self::I24In32 | Self::I24Packed => 24,
            Self::I16 => 16,
        }
    }

    fn name(self) -> &'static str {
        match self {
            Self::F32 => "f32",
            Self::I32 => "i32",
            Self::I24In32 => "i24/32",
            Self::I24Packed => "i24",
            Self::I16 => "i16",
        }
    }

    /// Convierte muestras `f32` a bytes del formato del dispositivo (little endian).
    fn write(self, src: &[f32], dst: &mut [u8]) {
        let w = self.container_bytes();
        debug_assert!(dst.len() >= src.len() * w);
        for (s, out) in src.iter().zip(dst.chunks_exact_mut(w)) {
            let v = s.clamp(-1.0, 1.0);
            match self {
                Self::F32 => out.copy_from_slice(&v.to_le_bytes()),
                Self::I32 => out.copy_from_slice(&((v as f64 * 2_147_483_647.0) as i32).to_le_bytes()),
                Self::I24In32 => out.copy_from_slice(&(((v * 8_388_607.0) as i32) << 8).to_le_bytes()),
                Self::I24Packed => out.copy_from_slice(&((v * 8_388_607.0) as i32).to_le_bytes()[..3]),
                Self::I16 => out.copy_from_slice(&((v * 32_767.0) as i16).to_le_bytes()),
            }
        }
    }

    fn format(self, channels: u16, rate: u32) -> WAVEFORMATEXTENSIBLE {
        let bytes = self.container_bytes() as u16;
        WAVEFORMATEXTENSIBLE {
            Format: WAVEFORMATEX {
                wFormatTag: WAVE_FORMAT_EXTENSIBLE as u16,
                nChannels: channels,
                nSamplesPerSec: rate,
                nAvgBytesPerSec: rate * channels as u32 * bytes as u32,
                nBlockAlign: channels * bytes,
                wBitsPerSample: bytes * 8,
                cbSize: (std::mem::size_of::<WAVEFORMATEXTENSIBLE>() - std::mem::size_of::<WAVEFORMATEX>()) as u16,
            },
            Samples: WAVEFORMATEXTENSIBLE_0 { wValidBitsPerSample: self.valid_bits() },
            dwChannelMask: KSAUDIO_SPEAKER_DIRECTOUT,
            SubFormat: if self == Self::F32 { KSDATAFORMAT_SUBTYPE_IEEE_FLOAT } else { KSDATAFORMAT_SUBTYPE_PCM },
        }
    }
}

/// Todo lo que vive en el hilo de renderizado (los objetos COM no cruzan hilos).
struct Opened {
    client: IAudioClient,
    render: IAudioRenderClient,
    event: HANDLE,
    kind: SampleKind,
    buffer_frames: u32,
    channels: usize,
}

pub struct ExclusiveStream {
    stop: Arc<AtomicBool>,
    join: Option<JoinHandle<()>>,
}

impl ExclusiveStream {
    /// Abre el endpoint y arranca el hilo de renderizado. `make_renderer(canales, frecuencia)` crea el
    /// callback ya con el formato negociado.
    pub fn start(
        cfg: &BackendConfig,
        make_renderer: impl FnOnce(usize, u32) -> RtRenderer + Send + 'static,
    ) -> Result<(Self, BackendInfo), String> {
        let (tx, rx) = channel::<Result<BackendInfo, String>>();
        let stop = Arc::new(AtomicBool::new(false));
        let stop_flag = Arc::clone(&stop);
        let cfg = cfg.clone();
        let join = std::thread::Builder::new()
            .name("qlab-wasapi-exclusive".into())
            .spawn(move || {
                // SAFETY: inicialización COM propia de este hilo, deshecha al salir.
                let com = unsafe { CoInitializeEx(None, COINIT_MULTITHREADED) };
                let opened = unsafe { open_exclusive(&cfg) };
                match opened {
                    Ok((o, device_label)) => {
                        let info = BackendInfo {
                            backend: "wasapi-exclusive".into(),
                            device: device_label,
                            sample_rate: cfg.sample_rate,
                            channels: cfg.channels,
                            buffer_frames: o.buffer_frames,
                            exclusive_active: true,
                            warnings: vec![format!("formato del dispositivo: {}", o.kind.name())],
                            monitor_device: None,
                        };
                        let renderer = make_renderer(o.channels, cfg.sample_rate);
                        let _ = tx.send(Ok(info));
                        render_loop(o, renderer, &stop_flag);
                    }
                    Err(e) => {
                        let _ = tx.send(Err(e));
                    }
                }
                if com.is_ok() {
                    // SAFETY: empareja el `CoInitializeEx` de arriba.
                    unsafe { CoUninitialize() };
                }
            })
            .map_err(|e| e.to_string())?;
        match rx.recv_timeout(Duration::from_secs(5)) {
            Ok(Ok(info)) => Ok((Self { stop, join: Some(join) }, info)),
            Ok(Err(e)) => {
                let _ = join.join();
                Err(e)
            }
            Err(_) => Err("WASAPI exclusivo no respondió en 5 s".into()),
        }
    }
}

impl Drop for ExclusiveStream {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Release);
        if let Some(j) = self.join.take() {
            let _ = j.join();
        }
    }
}

/// Endpoint por ID (`{0.0.0.00000000}.{guid}`) o el predeterminado de reproducción.
unsafe fn endpoint(cfg: &BackendConfig) -> Result<(IMMDevice, String), String> {
    let enumerator: IMMDeviceEnumerator = CoCreateInstance(&MMDeviceEnumerator, None, CLSCTX_ALL).map_err(|e| format!("enumerador de dispositivos: {e}"))?;
    match cfg.device.as_deref() {
        None => {
            let d = enumerator.GetDefaultAudioEndpoint(eRender, eConsole).map_err(|e| format!("sin dispositivo predeterminado: {e}"))?;
            Ok((d, "dispositivo predeterminado".into()))
        }
        Some(id) if id.starts_with('{') => {
            let wide: Vec<u16> = id.encode_utf16().chain(std::iter::once(0)).collect();
            let d = enumerator.GetDevice(PCWSTR(wide.as_ptr())).map_err(|e| format!("endpoint `{id}`: {e}"))?;
            Ok((d, id.to_string()))
        }
        Some(name) => Err(format!("`{name}` no es un ID de endpoint; en modo exclusivo usa el ID (empieza por `{{`) o el dispositivo predeterminado")),
    }
}

unsafe fn open_exclusive(cfg: &BackendConfig) -> Result<(Opened, String), String> {
    let (device, label) = endpoint(cfg)?;
    let channels = cfg.channels;
    let rate = cfg.sample_rate;

    let mut client: IAudioClient = device.Activate(CLSCTX_ALL, None).map_err(|e| format!("IAudioClient: {e}"))?;
    let kind = TRY_ORDER
        .into_iter()
        .find(|k| {
            let f = k.format(channels, rate);
            // En exclusivo `ppClosestMatch` debe ser NULL; solo S_OK significa "soportado".
            client.IsFormatSupported(AUDCLNT_SHAREMODE_EXCLUSIVE, &f.Format, None).0 == 0
        })
        .ok_or_else(|| format!("el dispositivo no acepta {channels} canales a {rate} Hz en exclusivo (f32/i32/i24/i16)"))?;
    let format = kind.format(channels, rate);

    let (mut default_period, mut min_period) = (0i64, 0i64);
    client.GetDevicePeriod(Some(&mut default_period), Some(&mut min_period)).map_err(|e| format!("GetDevicePeriod: {e}"))?;
    let wanted = (cfg.buffer_frames as i64 * 10_000_000 + rate as i64 / 2) / rate as i64;
    let mut period = wanted.max(min_period);

    let init = |c: &IAudioClient, period: i64| {
        c.Initialize(AUDCLNT_SHAREMODE_EXCLUSIVE, AUDCLNT_STREAMFLAGS_EVENTCALLBACK, period, period, &format.Format, None)
    };
    if let Err(e) = init(&client, period) {
        if e.code() != AUDCLNT_E_BUFFER_SIZE_NOT_ALIGNED {
            return Err(format!("Initialize (exclusivo, {}): {e}", kind.name()));
        }
        // El dispositivo exige un tamaño alineado: se lee el que propone y se reintenta con un cliente nuevo.
        let aligned = client.GetBufferSize().map_err(|e| format!("GetBufferSize: {e}"))?;
        period = (10_000_000.0 / rate as f64 * aligned as f64 + 0.5) as i64;
        client = device.Activate(CLSCTX_ALL, None).map_err(|e| format!("IAudioClient: {e}"))?;
        init(&client, period).map_err(|e| format!("Initialize alineado (exclusivo, {}): {e}", kind.name()))?;
    }

    let buffer_frames = client.GetBufferSize().map_err(|e| format!("GetBufferSize: {e}"))?;
    let event = CreateEventW(None, false, false, None).map_err(|e| format!("CreateEventW: {e}"))?;
    client.SetEventHandle(event).map_err(|e| format!("SetEventHandle: {e}"))?;
    let render: IAudioRenderClient = client.GetService().map_err(|e| format!("IAudioRenderClient: {e}"))?;

    // Búfer inicial en silencio antes de arrancar.
    let first = render.GetBuffer(buffer_frames).map_err(|e| format!("GetBuffer inicial: {e}"))?;
    std::ptr::write_bytes(first, 0, buffer_frames as usize * format.Format.nBlockAlign as usize);
    render.ReleaseBuffer(buffer_frames, AUDCLNT_BUFFERFLAGS_SILENT.0 as u32).map_err(|e| format!("ReleaseBuffer inicial: {e}"))?;
    client.Start().map_err(|e| format!("Start: {e}"))?;

    Ok((Opened { client, render, event, kind, buffer_frames, channels: channels as usize }, label))
}

fn render_loop(o: Opened, mut renderer: RtRenderer, stop: &AtomicBool) {
    let frames = o.buffer_frames as usize;
    let mut scratch = vec![0.0f32; frames * o.channels];
    let block = o.kind.container_bytes() * o.channels;
    while !stop.load(Ordering::Acquire) {
        // SAFETY: `event` es un manejador propio y válido.
        let wait = unsafe { WaitForSingleObject(o.event, 500) };
        if wait == WAIT_TIMEOUT {
            continue;
        }
        if wait != WAIT_OBJECT_0 {
            break;
        }
        renderer.render(&mut scratch);
        // SAFETY: `GetBuffer` devuelve `frames * block` bytes escribibles hasta el `ReleaseBuffer`.
        unsafe {
            let Ok(ptr) = o.render.GetBuffer(o.buffer_frames) else { break };
            let dst = std::slice::from_raw_parts_mut(ptr, frames * block);
            o.kind.write(&scratch, dst);
            if o.render.ReleaseBuffer(o.buffer_frames, 0).is_err() {
                break;
            }
        }
    }
    // SAFETY: cierre ordenado de los objetos de este hilo.
    unsafe {
        let _ = o.client.Stop();
        let _ = CloseHandle(o.event);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn bytes(kind: SampleKind, v: &[f32]) -> Vec<u8> {
        let mut out = vec![0u8; v.len() * kind.container_bytes()];
        kind.write(v, &mut out);
        out
    }

    #[test]
    fn conversions_cover_full_scale_and_clip() {
        assert_eq!(bytes(SampleKind::F32, &[0.5]), 0.5f32.to_le_bytes());
        assert_eq!(bytes(SampleKind::I16, &[1.0, -1.0, 0.0, 5.0]), [0xFF, 0x7F, 0x01, 0x80, 0, 0, 0xFF, 0x7F]);
        assert_eq!(bytes(SampleKind::I32, &[1.0]), i32::MAX.to_le_bytes());
        assert_eq!(bytes(SampleKind::I24Packed, &[1.0, -1.0]), [0xFF, 0xFF, 0x7F, 0x01, 0x00, 0x80]);
        assert_eq!(bytes(SampleKind::I24In32, &[1.0]), (0x7F_FFFF_i32 << 8).to_le_bytes());
        assert_eq!(SampleKind::I24Packed.container_bytes() * 2, 6);
    }

    #[test]
    fn formats_describe_their_layout() {
        let f = SampleKind::I24Packed.format(2, 48_000);
        assert_eq!({ f.Format.nBlockAlign }, 6);
        assert_eq!({ f.Format.nAvgBytesPerSec }, 48_000 * 6);
        // SAFETY: el campo `wValidBitsPerSample` es el que `format` inicializa.
        assert_eq!(unsafe { f.Samples.wValidBitsPerSample }, 24);
        let f = SampleKind::F32.format(8, 96_000);
        assert_eq!(({ f.Format.nBlockAlign }, { f.Format.wBitsPerSample }), (32, 32));
    }
}
