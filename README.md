# TeatroCue

Reproductor de audio y vídeo para escena, escrito en Rust, inspirado en la sección de audio de QLab. Se usa en el Ballet Oficial de Córdoba (Teatro del Libertador San Martín). Funciona en Linux (PipeWire) y Windows (WASAPI), sin conexión a internet.

## Qué hace

- **Cues**: Audio (con slices, bucles y Devamp), Vídeo (el audio manda y la imagen lo sigue), Fade, Grupo (Timeline, Fire First, Fire All), **Control** (Stop con fundido, Pause, Resume, Devamp, Go to, Load, Arm, Disarm), **OSC** (envía un mensaje a otro equipo) y **Memo/Espera**.
- **Edición**: Inspector por pestañas, memorias (puntos con nombre y nota: rombos ámbar sobre la regla de la onda, panel lateral con filtro y tecla M para marcar al vuelo), matriz de niveles, deshacer/rehacer (Ctrl+Z) y duplicar cue (Ctrl+D), shows recientes, empaquetar show, arrastrar archivos.
- **Directo**: GO, Panic (suave y duro), volumen maestro (−60…+6 dB, también por OSC `/master <dB>`), vúmetros por cue, proyección en ventana aparte.
- **EQ paramétrico de 4 bandas por cue** (Low Cut, shelves, Peaking, High Cut), filtros biquad RBJ en tiempo real sin asignaciones, con suavizado antichasquidos; curva interactiva en la pestaña EQ.
- **Audio**: Configuración de audio en la interfaz (dispositivo, canales, buffer, frecuencia).
- **Control remoto OSC** (opcional): `/go /panic /stop /pause /resume /hardstop /next /prev /cue/<n>/start` y por cue `/cue/<n>/stop|pause|resume|devamp|load|arm|disarm` por UDP. Se activa en Ver → Configuración de audio o con `--osc-port`.
- **Licencia offline** con vencimiento, defensa contra retroceso de reloj y **reactivación de emergencia** por desafío-respuesta.

## Ingeniería de Audio en Tiempo Real

El motor está pensado para una cabina de teatro, donde un corte de audio en escena no tiene repetición. Ocho decisiones lo sostienen (el detalle, con el código que las respalda, está en [`docs/DOSSIER_TECNICO_MANUAL.md`](docs/DOSSIER_TECNICO_MANUAL.md), sección 8):

1. **Tiempo real sin asignaciones ni bloqueos.** El callback de audio no reserva memoria (un asignador vigilante lo comprueba en las pruebas), se comunica por colas SPSC sin bloqueos y publica la telemetría con un Seqlock. GO llega a la cola de audio en unos 157 µs (medido).
2. **Curvas de fade precalculadas.** Una tabla de 33 puntos evita `exp`/`pow` en caliente; la interpolación cúbica no rebasa nunca el objetivo.
3. **Slices y Devamp a nivel de frame.** Los puntos de bucle son enteros exactos y la salida del bucle espera al último frame del tramo: sin clics ni recortes.
4. **Remuestreo Catmull-Rom (Horner).** Solo se interpola cuando hace falta; a 1,000× y a la misma frecuencia el audio pasa sin tocarse.
5. **Sin «zipper noise».** Niveles, MASTER y coeficientes del EQ (biquads RBJ) se deslizan en unos 5–10 ms; los saltos a una memoria entran con un micro-fade de 50 ms.
6. **SIMD dinámico.** El ejecutable es genérico (SSE2) y detecta AVX2/FMA en tiempo de ejecución, con una ruta de respaldo que da el mismo resultado: se evita el cuelgue por «instrucción ilegal» en las PC sin AVX2.
7. **PFL multitarjeta.** Los auriculares pueden ir en otra placa de sonido; un controlador PI sobre un remuestreo continuo absorbe la deriva entre los dos relojes.
8. **Audio-Master en vídeo.** El audio manda el tiempo y la imagen lo sigue: ante cualquier retraso gráfico se descartan cuadros, nunca se detiene el sonido.

Frente a reproductores armados sobre Electron o páginas web (el audio comparte hilo y recolector de basura con la interfaz) y frente a los DAW de estudio (pensados para grabar y mezclar, no para disparar cues en vivo), aquí el hilo de audio está aislado, es predecible y se prueba como tal.

## Estructura del workspace

| Crate | Función |
|---|---|
| `qlab-protocol` | Formato de show, mensajes IPC, memoria compartida |
| `qlab-engine` | Motor de audio en tiempo real (sin asignaciones en el callback) |
| `qlab-core` | Modelo de cues, secuenciador, fades |
| `qlab-daemon` | `teatrocue-daemon`: audio, IPC, OSC |
| `qlab-ui` | `teatrocue-ui`: interfaz egui |
| `qlab-license` | Licencias Ed25519, Machine ID, marcas de reloj, activación de emergencia (`teatrocue-keygen`) |
| `installer-win` | `Instalar_TeatroCue.exe`: instala y renueva desde el pendrive |
| `installer-tool` | `installer-tool sign-challenge`: firma claves de reactivación (solo el desarrollador) |
| `qlab-bench` | Mediciones |

## Compilar y probar

```bash
cargo test --workspace
cargo check --workspace --all-targets
cargo check --workspace --all-targets --target x86_64-pc-windows-gnu
```

El build de Windows usa CPU genérica (`x86-64`, sin AVX2) a propósito: con `x86-64-v3` el programa no arrancaba en PC antiguas (código `-1073741795`). Ver `.cargo/config.toml`. Medido con `qlab-bench` (64 voces con remuestreo cúbico, backend nulo, 10 s): callback máximo 0,57 ms con CPU genérica frente a 0,60 ms con AVX2, sobre un presupuesto de 2,67 ms por bloque de 128 frames: no hay penalización apreciable.

## Pendrive de instalación

```bash
scripts/build_usb_installer.sh /ruta/al/pendrive
```

Compila para Windows con la clave pública incrustada y licencia exigida, descarga ffmpeg estático (SHA-256 verificado) y deja en el pendrive `Instalar_TeatroCue.exe`, `master.key.enc`, `payload/`, `SHA256SUMS.txt` y `LEEME.txt`. Las claves del desarrollador viven en `~/.config/teatrocue-keys/` (`master.key.enc` cifrada con la contraseña maestra y `public.key`). No se regeneran: invalidaría todas las licencias emitidas.

### Licencias

- Instalar/renovar: ejecutar `Instalar_TeatroCue.exe` en la PC del teatro (pide la contraseña maestra y la duración).
- Reactivar sin pendrive: la app muestra un código `REQ-…`; el desarrollador responde con
  `installer-tool sign-challenge --code REQ-… --days N` y el operador pega la clave `RESP-…`. Cada clave sirve una vez, para esa PC, y caduca a los 7 días si no se canjea.
- Si la fecha de Windows se retrasa, la app se bloquea (ver `LEEME.txt` del pendrive).

## Probar en Windows desde Linux

La VM de pruebas (`scripts/02_launch_headless_tiny11_vm.sh`) con los flags `hv_*` del script puede quedar en pantalla negra; arrancar con `-cpu host` (o `-cpu Nehalem` para comprobar que no se usan instrucciones AVX) funciona. Se maneja por el monitor QEMU (`sendkey`, `screendump`) y un servidor HTTP en el host (`10.0.2.2` desde la VM).

## Diagnóstico

Si un programa entra en pánico queda escrito en `%PROGRAMDATA%\TeatroCue\crash.log` (Windows) o `~/.local/state/teatrocue/crash.log` (Linux), con fecha, versión, hilo y lugar del error. Un cierre instantáneo sin ese registro suele ser del sistema (p. ej. instrucción ilegal, código `-1073741795`) y se ve con `echo %errorlevel%`.

## Seguridad

Es licenciamiento contractual y disuasivo, no una protección infalible: quien pueda modificar el ejecutable puede quitarlo. OSC está apagado por defecto porque cualquiera en la red con el puerto puede disparar la función.
