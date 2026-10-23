# TeatroCue — Dossier Técnico y Operativo

Fuente primaria para el manual definitivo. Todo lo que sigue se extrajo del código fuente del workspace (`crates/*`, `scripts/`, `README.md`, `GUIA-SONIDISTA.md`) el 2026-10-20. Cada dato indica el archivo de origen entre paréntesis. Donde el código y la documentación existente **no coinciden**, o donde algo **no existe en el código**, se señala en la sección 6.

Versión del paquete: `0.1.0` (`Cargo.toml`, `[workspace.package]`).

---

## 0. Visión general

| Concepto | Dato | Origen |
|---|---|---|
| Qué es | Reproductor de audio y vídeo para escena (inspirado en la sección de audio de QLab), sin conexión a internet | `README.md` |
| Usuario | Ballet Oficial de Córdoba (Teatro del Libertador San Martín) | `help_modal.rs`, `cue_sheet.rs` |
| Sistemas | Windows (WASAPI) y Linux (PipeWire) | `backend_audio.rs` |
| Ejecutables | `teatrocue-ui` (interfaz), `teatrocue-daemon` (motor), `Instalar_TeatroCue.exe`, `installer-tool`, `teatrocue-keygen`, `teatrocue-admin`, `qlab-bench` | `Cargo.toml` de cada crate |
| Arranque normal | La UI arranca el **motor embebido** (el daemon en un hilo, con audio). `--remote RUTA`/`--socket RUTA` se conecta a un daemon externo; `--standalone-null` arranca sin tarjeta de sonido; `--exit-after-ms N` es para pruebas | `qlab-ui/src/main.rs` |
| Extensión de show | `.teatroshow` (también se abren `.json` y `.qlabshow`). Formato interno `teatrocue/v1` (se lee también el heredado `qlab-rs/v1`) | `native_dialog.rs`, `show_format.rs` |
| Ventana | 1280×800 inicial, mínimo 900×560 | `main.rs` |

### 0.1 Tipos de cue (botones de la barra de herramientas)

| Botón | Tipo | Descripción |
|---|---|---|
| `+ Audio Cue` | audio | Archivo con región, slices, bucles, EQ y matriz de niveles |
| `+ Fade Cue` | fade | Fundido sobre otra cue |
| `+ Memo/Espera` | memo | No suena; dura el tiempo indicado; con Auto-continue encadena |
| `+ OSC Cue` | osc | Envía un mensaje OSC por UDP a otro equipo (`equipo:puerto`, `/direccion 3 0.5 "texto"`); si falla avisa pero no detiene el show |
| `+ Control Cue` | control | Stop (con fundido), Pause, Resume, Devamp, Go to, Load, Arm, Disarm sobre otra cue |
| `+ Video Cue` | video | El audio suena por el motor; la imagen va a la ventana de proyección |
| `+ Group Cue` | group | Modos: Timeline, Fire First (primera y sigue), Fire All (todas a la vez) |

Las cues nuevas se insertan **después de la seleccionada** (o al final si no hay selección) (`app.rs`, `add_cue`). Una cue de audio nueva nace **desarmada**; al asignarle archivo queda armada.

### 0.2 Pestañas del Inspector (panel inferior)

`Basics`, `Video` (solo vídeo), `Fade`, `Control`, `OSC` (solo su tipo), `Triggers`, `Time & Loops` (audio/vídeo), `Audio Levels` (audio/vídeo), `EQ` (audio/vídeo) (`inspector/mod.rs`). Alto inicial 380 px, mínimo 340 px. Los cambios se envían solos al motor cuando el operador deja de teclear o arrastrar.

---

## 1. Catálogo de atajos y gestos

### 1.1 Atajos de teclado globales

“Ctrl” equivale a `modifiers.command` de egui (Ctrl en Windows/Linux). Fuente: `scan_keys` y `handle_keys` en `qlab-ui/src/app.rs`.

| Atajo | Acción | Notas de comportamiento |
|---|---|---|
| **Espacio** | GO: dispara la cue en espera (playhead) y avanza | Sin modificadores; **no** repite al mantener pulsado. No hace nada si el playhead está vacío («FIN») o no hay conexión con el motor |
| **Esc** | Panic suave | Fundido de **1 s** (`panic_fade_frames = sample_rate`) de todo lo que suena (`scheduler.rs`). No repite al mantener |
| **Esc Esc** (2.ª pulsación en < **700 ms**) | Parada dura (`HardStopAll`) | Fundido de 96 frames (2 ms a 48 kHz); aparece el aviso «Parada dura» |
| **F1** | Abre/cierra el manual (Atajos / Operación de Escena / Autorización Legal y Licencia) | Funciona aunque haya un campo de texto con foco; no se abre si hay selector de archivos o confirmación abiertos |
| **F3** | Muestra/oculta la Botonera de Carts | Sin modificadores |
| **M** | Marca una memoria y pide su nombre | Ver §3.1. Bloqueado por Show Lock |
| **Enter** (también el del teclado numérico) | Igual que M pero **sin avisos** si no hay nada que marcar | Solo cuando ningún campo de texto tiene foco. Bloqueado por Show Lock |
| **. N .** (teclado numérico) | Salta a la memoria número N | Ver §3.1.4. Funciona con Show Lock activo |
| **↑ / ↓** | Cambia la selección; en cues de nivel superior mueve también el playhead (sin disparar) | Sin modificadores |
| **Supr / Retroceso** | Borra la cue seleccionada | Sin modificadores; bloqueado por Show Lock. Las cues no piden confirmación |
| **Ctrl+Z** | Deshacer última edición de una cue | Los cambios seguidos cuentan como uno. Bloqueado por Show Lock. Aviso «No hay nada que deshacer» si el historial está vacío |
| **Ctrl+Mayús+Z** / **Ctrl+Y** | Rehacer | Bloqueado por Show Lock |
| **Ctrl+D** | Duplica la cue seleccionada | Crea una cue del mismo tipo justo después y copia todos los datos; si la cue tenía nombre, la copia se llama «nombre (copia)». **Los grupos no se duplican**. Bloqueado por Show Lock |
| **Ctrl+L** | Activa/desactiva el Bloqueo de función (Show Lock) | Aviso grande «Bloqueo de función: ACTIVADO/desactivado». Cancela un cuadro de memoria que estuviera abierto |
| **Ctrl+N** | Show nuevo | Pide confirmar si hay cambios sin guardar («Descartar cambios» / «Cancelar»). Bloqueado por Show Lock |
| **Ctrl+O** | Abrir show… | Pide confirmar si hay cambios sin guardar. Bloqueado por Show Lock |
| **Ctrl+S** | Guardar | Sin ruta previa abre «Guardar como…» |
| **Ctrl+Mayús+S** | Guardar como… | Añade `.teatroshow` si falta una extensión de show |
| **Ctrl+Q** | Salir | Cierra la ventana (un cierre limpio borra los autoguardados) |
| **1…9, 0, F5…F12** | Dispara el pad de Carts que tenga asignada esa tecla | Sin modificadores; solo si el pad tiene archivo; no actúa mientras se escribe en un campo de texto. Funcionan aunque la ventana de Carts tenga el foco. Bloquear no los deshabilita |

Reglas generales:

- Los atajos de la aplicación se **suspenden** mientras hay un campo de texto con foco (`egui_wants_keyboard_input`), salvo F1 y Ctrl+L.
- Se suspenden también mientras hay abierto el manual/«Acerca de», el selector de archivos o un diálogo de confirmación.
- Con la licencia bloqueada (pantalla de bloqueo) no se atiende ningún atajo.
- Esc dentro del cuadro «Nombre de la memoria» solo cancela el cuadro (no dispara Panic).

### 1.2 Atajos de la ventana de proyección

(`projection_viewport.rs`, ventana secundaria)

| Atajo/gesto | Acción |
|---|---|
| **F** | Alterna pantalla completa |
| **Doble clic** sobre la imagen | Alterna pantalla completa |
| **Esc** | En pantalla completa: sale de ella. En modo ventana: cierra la ventana |
| Arrastrar con el botón izquierdo sobre el fondo negro | Mueve la ventana (solo en modo ventana) |
| Mover el ratón | Muestra la barra flotante (se oculta tras **3 s** con desvanecimiento de 400 ms; en pantalla completa también se oculta el cursor) |

### 1.3 Gestos de mouse

| Zona | Gesto | Resultado | Origen |
|---|---|---|---|
| Fila de la tabla de cues | Clic | Selecciona la cue; si es de nivel superior, mueve también el playhead (sin disparar) | `cue_table.rs` |
| | **Doble clic** | Dispara esa cue (`FireCue`). Solo cues de nivel superior; en las hijas de un grupo avisa «Solo se pueden disparar cues de nivel superior; usa el grupo» | `cue_table.rs`, `app.rs` |
| | Clic derecho | Menú: «Poner el playhead aquí», «Disparar esta cue» | `cue_table.rs` |
| | Clic en el triángulo del grupo | Pliega/despliega el grupo | `cue_table.rs` |
| | Clic en la columna «Sig.» | Cicla el modo de continuación (bloqueado por Show Lock) | `app.rs` |
| Deslizador MASTER | Arrastrar | −60 a +6 dB en pasos de 0,5 dB | `top_bar.rs` |
| | **Doble clic** | Vuelve a 0 dB | `top_bar.rs` |
| Celda de la matriz de niveles | Arrastrar el cuadro de dB o escribir | Cambia el nivel (−120 a +12 dB) en vivo | `widgets.rs` |
| | Clic/arrastre en el fader fino | Fija el nivel de forma absoluta | `widgets.rs` |
| | **Mayús + arrastre** en el fader | Ajuste fino relativo | `widgets.rs` |
| Onda (Time & Loops) | Clic | Coloca el cursor de edición | `tab_time_loops.rs` |
| | **Doble clic** | Añade un slice en ese punto (si cabe) | idem |
| | Arrastrar marcador verde/rojo | Mueve Inicio / Fin de la región | idem |
| | Clic en un marcador de slice | Alterna 1x → 2x → Infinite | idem |
| | Arrastrar un slice | Lo mueve | idem |
| | Clic derecho en un slice | Lo quita | idem |
| | Clic en un rombo ámbar (memoria) | Salta a ese punto (`activate_memory`) | idem |
| Velocidad | Doble clic en el valor, o en una velocidad de ensayo | Vuelve a 1.000× | idem |
| Gráfica EQ | Arrastrar un punto | Frecuencia (horizontal) y ganancia (vertical) | `tab_eq.rs` |
| | Rueda sobre un punto | Cambia la Q (no desplaza el Inspector) | idem |
| | **Doble clic** en un punto | Activa/desactiva esa banda | idem |
| Panel Memorias | Clic en una fila | Ir a esa memoria | `memories.rs` |
| | **Mayús + clic** en una fila | Aplica pre-roll de 2 s a ese salto aunque esté en «Off» | `memories.rs`, `app.rs` |
| | Botón `PFL` de la fila | Escucha desde ese punto en los auriculares | `memories.rs` |
| Pad de Carts | Pulsar (clic) | Dispara (según el modo) | `carts.rs` |
| | **Clic derecho** | Abre el editor del pad | idem |
| | Clic en un pad vacío | Abre el editor | idem |
| Ventana principal | Soltar archivos | Ver §1.4 | `app.rs` |

### 1.4 Arrastrar y soltar (DnD)

(`app.rs`, `handle_drops`, `show_drop_overlay`)

| Dónde se suelta | Resultado |
|---|---|
| **En la tabla / cualquier parte fuera del Inspector** | Crea **una cue nueva por archivo compatible**, en orden, detrás de la cue seleccionada. Nombre de la cue = nombre del archivo sin extensión. Las cues salen armadas, con región = archivo completo y sin slices. Al terminar: «N cue(s) importadas» |
| **Sobre el Inspector** con una cue de audio/vídeo seleccionada | Cambia el archivo de **esa** cue (solo el primer archivo si se sueltan varios; la región vuelve a «todo el archivo» y se borran los slices) |

Detalles: se aceptan `wav, flac, mp3, ogg, oga` (audio) y `mp4, mov, mkv` (vídeo). Los archivos no multimedia se ignoran con aviso. Mientras se arrastra aparece una capa oscura con borde punteado ámbar («Soltar archivos para crear Cues» o «Soltar para cambiar el archivo de la cue»). **Con Show Lock activo el soltado se rechaza.** Sin conexión con el motor avisa «Conéctate al daemon antes de soltar archivos».

---

## 2. Arquitectura de audio y especificaciones de hardware

### 2.1 Formatos y códecs admitidos

Decodificación de audio en el motor con **Symphonia 0.5** (sin ffmpeg para el audio) con las características `wav, pcm, mp3, flac, ogg, vorbis, isomp4, aac, mkv` (`Cargo.toml`, `disk_streamer.rs`).

| Categoría | Extensiones aceptadas (UI) | Códecs/contenedores habilitados en el motor |
|---|---|---|
| Audio | `.wav`, `.flac`, `.mp3`, `.ogg`, `.oga` | WAV/PCM, FLAC, MP3, Ogg Vorbis |
| Vídeo | `.mp4`, `.mov`, `.mkv` | Contenedores ISO-MP4/QuickTime y Matroska; se reproduce **la pista de audio** (AAC, MP3, Vorbis, FLAC, PCM según el archivo) |
| Imagen del vídeo | idem | Decodificada por **`ffmpeg`/`ffprobe`** como subproceso (cuadros RGBA por tubería), escalada a un máximo de **960×540** conservando la proporción |

Notas:

- El **sonido** de una cue de vídeo nunca depende de ffmpeg. Si `ffmpeg` no está en el PATH, la vista previa y la proyección no están disponibles («Vista previa no disponible: no se encontró ffmpeg (el audio suena igual)»).
- El instalador exige `ffmpeg.exe` en el paquete (`REQUIRED_FILES`); `ffprobe.exe` y las DLL son opcionales. El build usa una versión LGPL estática de BtbN con SHA-256 verificado (`scripts/build_usb_installer.sh`).
- Los pads de Carts aceptan los mismos formatos de audio (la ayuda menciona `.wav/.mp3/.ogg`).
- Las cues con audio reproducen desde disco con un streamer propio (precarga y liberación de lo recorrido); si el disco no responde a tiempo se emite `DiskBufferUnderrun` y se rellena con silencio.

### 2.2 Buffer, frecuencia de muestreo y canales

| Parámetro | Interfaz de usuario (Ver → Configuración de audio) | Límite del motor/CLI | Por defecto | Origen |
|---|---|---|---|---|
| Buffer | **64, 128, 256, 512, 1024, 2048** frames | `16 … 16 384` (`--buffer`); `audio.json` editado a mano se corrige a ese rango | **128** | `settings.rs`, `config.rs` |
| Frecuencia | **44 100, 48 000, 96 000 Hz** | `8 000 … 384 000` (`--rate`) | **48 000** | idem |
| Canales de salida | `1 … 64` | `1 … 64` (`--channels`) | **2** | idem |
| Canales de entrada por archivo | — | hasta **32** (`MAX_INPUT_CHANNELS`) | — | `matrix.rs` |
| Matriz de niveles | filas = canales del archivo (≤ 32), columnas = salidas (≤ 64) | niveles **−120 a +12 dB** (−120 = silencio) | — | `matrix.rs` |
| Pads de Carts | 16 | `CART_PADS = 16`; volumen −60 a +12 dB | — | `cue_dto.rs` |
| Procesado interno | bloques de **128 frames** (`CHUNK_FRAMES`) | — | — | `mixer_simd.rs` |

Referencia de latencia: un buffer de 128 frames a 48 kHz = **2,67 ms** (presupuesto por bloque citado en `README.md`).

El BackendConfig recorta un buffer fuera del rango que soporta el dispositivo y avisa («buffer de N frames fuera de [min, max]: se usa X»). Los ajustes se guardan en:

- Windows: `%APPDATA%\TeatroCue\audio.json`
- Linux: `~/.config/teatrocue/audio.json` (o `$XDG_CONFIG_HOME/teatrocue/audio.json`)

Campos de `audio.json`: `host`, `device`, `channels`, `buffer_frames`, `sample_rate`, `osc_port`, `monitor_first` (índice desde 0 del primer canal del par de auriculares), `monitor_device` (nombre de **otra placa** de sonido para los auriculares; si está, manda sobre `monitor_first`), `preroll_secs` (0, 2 o 4). Un archivo ausente o dañado se sustituye por los valores de fábrica sin fallar. En la misma carpeta, `recent.json` guarda los **8** shows recientes.

**Aplicar** cambios de audio reinicia el motor embebido (se corta lo que suena) y recarga el show abierto automáticamente.

### 2.3 Backends: WASAPI, PipeWire, prioridades

Crate `cpal` 0.18 (`Cargo.toml`); el backend lo implementa `qlab-daemon/src/backend_audio.rs`.

| Plataforma | Comportamiento |
|---|---|
| **Windows — por defecto** | WASAPI **compartido** a través de `cpal` (pasa por el mezclador del sistema) |
| **Windows — exclusivo** | Solo mediante la opción de línea de órdenes del **daemon** `--exclusive` (módulo `wasapi_exclusive.rs`): `AUDCLNT_SHAREMODE_EXCLUSIVE` + `EVENTCALLBACK`. Prueba formatos en orden **f32, i32, i24 en contenedor de 32 bits, i24 empaquetado, i16** (convierte desde el f32 del motor). Latencias citadas en el código: 2,7 a 5,3 ms. Si el dispositivo lo rechaza, **cae a WASAPI compartido y avisa**. El propio código declara que *no se ha verificado contra hardware real* (solo compila y se validó en VM) |
| ASIO | Opcional (característica `asio` de cpal, SDK en `.sdk/asiosdk`); seleccionable como host en `--host asio` |
| **Linux** | `cpal` con característica `pipewire` (PipeWire nativo), con ALSA como respaldo (`pipewire-alsa` lo redirige). Hosts nombrables con `--host`: `pipewire`, `alsa`, `wasapi`, `asio`, `jack` |
| Sin tarjeta | `--backend null` o `--standalone-null`: backend virtual para pruebas |

**Importante:** la interfaz **no expone** el modo exclusivo; el motor embebido (`embedded_config`) usa siempre `exclusive = false`. Para usar WASAPI exclusivo hay que lanzar el daemon aparte con `--exclusive`.

Elevación de prioridad (`rt_priority.rs`, `main.rs` de la UI, `daemon.rs`):

| Elemento | Windows | Linux |
|---|---|---|
| Proceso de la **UI** (`teatrocue-ui.exe`) | `SetPriorityClass(HIGH_PRIORITY_CLASS)` (0x80). Se usa HIGH y no REALTIME a propósito: sin administrador Windows degrada REALTIME a HIGH y, con él, REALTIME puede dejar sin CPU al mouse y teclado | — |
| Proceso del **daemon** | Intenta `REALTIME_PRIORITY_CLASS`; si no puede, `HIGH_PRIORITY_CLASS` | Sin clase de proceso |
| Hilos de audio y secuenciador | MMCSS `"Pro Audio"` con prioridad crítica; respaldo `THREAD_PRIORITY_TIME_CRITICAL` | `SCHED_FIFO`: **90** audio, **80** secuenciador (`--rt-priority`, `--seq-priority`); secuenciador con `PR_SET_TIMERSLACK = 1` ns |
| Memoria | Sin bloqueo | `mlockall(MCL_CURRENT \| MCL_FUTURE)` (desactivable con `--no-mlock`; el motor embebido lo desactiva) |

Nada de esto es fatal: sin privilegios el motor sigue con un aviso (en Linux se necesita el grupo `realtime`/`RLIMIT_RTPRIO`).

Otras protecciones del motor: callback de audio **sin asignaciones de memoria** (vigilado con `--alloc-check off|count|abort`; por defecto `count`), *watchdog* del secuenciador (umbral 250 ms por defecto, `--watchdog-ms`) que activa **Safe Mute** si el secuenciador se congela (`--no-safe-mute` lo desactiva). En la barra superior: `XR n` (xruns), `CPU %`, LED rojo «audio congelado», LED ámbar «safe mute activo».

### 2.4 Remuestreador y detección de CPU

- **Interpolador:** cúbico **Catmull-Rom de 4 puntos** (`catmull_rom_4pt`, `mixer_simd.rs`), forma de Horner, sin asignaciones. Se usa **solo cuando hace falta**: si `velocidad ≠ 1.0` o la frecuencia del archivo difiere de la del dispositivo. Con velocidad 1,0 y misma frecuencia se copia la muestra directamente (camino «unity»).
- Velocidad (rate) admitida: **0,03× a 33×** (`RATE_MIN`/`RATE_MAX`). Velocidades de ensayo: 0,90×, 0,95×, 1,00×, 1,05×; atajos ½, 1×, 2×. «Conservar tono» está **deshabilitado** (la velocidad cambia también el tono). El cambio de velocidad es en caliente con la voz sonando.
- La misma interpolación se usa en el controlador de deriva de reloj entre dominios de audio (`drift.rs`: controlador PI + resampler Catmull-Rom).
- **Detección de AVX2/FMA en caliente:** `cpu_has_avx2_fma()` usa `is_x86_feature_detected!("avx2") && ("fma")` (CPUID consultado una vez y cacheado). Si está disponible se ejecutan las variantes `read_source_avx2` / `process_voices_avx2` (`#[target_feature(enable = "avx2,fma")]`, con FMA de hardware); si no, el **baseline genérico** (multiplicación y suma separadas, para evitar la rutina de software de `mul_add`). Además se activan FTZ/DAZ en el hilo de audio.
- **Compilación:**
  - Linux: `target-cpu=x86-64-v3` (+avx2,+fma) (`.cargo/config.toml`).
  - **Windows: `target-cpu=x86-64` (solo SSE2)** a propósito: con `x86-64-v3` el programa moría con el código `0xC000001D` (`-1073741795`, instrucción ilegal) en la PC Acer del teatro. Verificado en VM con `-cpu Nehalem` (sin AVX). Medición del README (64 voces con remuestreo cúbico, backend nulo, 10 s): callback máximo **0,57 ms** con CPU genérica frente a 0,60 ms con AVX2, sobre un presupuesto de 2,67 ms (bloque de 128 frames).
- **EQ:** ver §3.3 (biquads de 4 bandas, también en `f64`).

---

## 3. Flujos operativos (guía de escena)

### 3.1 Memorias (tecla M)

Concepto: puntos con nombre y nota dentro de una cue de audio/vídeo (equivalentes a las «Ubicaciones de memoria» de Pro Tools). **No alteran la reproducción.** Máximos: 500 memorias por cue (`MAX_MEMORIES`); nombre y nota con longitud acotada (`MAX_MEMORY_NAME_CHARS`, `MAX_MEMORY_NOTE_CHARS`).

#### 3.1.1 Marcado en caliente

1. Con la cue **sonando**, pulsar **M** (o **Enter**). Se captura de forma atómica el frame de lectura actual.
   - Si suena más de una cue, usa la seleccionada si está sonando; si no, la de menor id.
   - Si aún no hay posición de lectura: «Todavía no hay posición de lectura: pulse M otra vez en un instante».
2. Con la cue **parada**: seleccionar la cue de audio, hacer **clic en la onda** (Time & Loops) para dejar el cursor de edición y pulsar M. Si no hay cursor, M avisa cómo marcar.
3. Aparece el cuadro modal **«Nombre de la memoria:»** ya enfocado, con la hora del punto («Punto: mm:ss.mmm · Enter guarda, Esc cancela»).
4. **Enter** guarda; campo vacío = «Memoria N» (N = cantidad existente + 1). **Esc** cancela.
5. La memoria queda ordenada por posición, y se abre el panel de memorias. También existe el botón «Marcar memoria (M)» en Time & Loops.

#### 3.1.2 Edición de nombres y notas

En Time & Loops → «Memorias (N)»: por fila `n`, nombre, tiempo (arrastrable/tecleable), nota, y botones **Ir** (lleva el cursor de edición), **Arrancar aquí** (la cue empezará desde ese punto al dispararla; respeta el pre-roll) y **x** (quitar). «Arranca en … ([nombre])» + **Desde el inicio** quita el punto de arranque.

#### 3.1.3 Navegación desde el panel lateral

**Ver → Memorias…** abre un panel acoplable (botón «A la derecha»/«A la izquierda»; redimensionable, ancho inicial 340 px, mínimo 240 px). Filas `#N  MM:SS.mmm  nombre`, más la cue a la que pertenece y la nota. Numeración correlativa por cue y posición; **no cambia al filtrar**. «Buscar memoria…» filtra por nombre, nota o cue (sin distinguir mayúsculas).

Efecto de un clic en una fila (`activate_memory`):

| Situación | Resultado |
|---|---|
| Cue **parada** | La cue queda seleccionada con el playhead sobre ella y con `start_override_secs` = el punto (menos pre-roll). **Espacio** la dispara desde ahí. Se abre la pestaña Time & Loops |
| Cue **sonando** | Se envía Stop a la cue y, en cuanto se guarda el nuevo punto de arranque, se relanza (≈150 ms después) y **sigue sonando** desde ese punto |

El punto de arranque se resalta en la lista.

#### 3.1.4 Atajo numérico «. N .»

En el teclado numérico: **`.`** inicia la secuencia (aviso «Memoria #…»), luego los dígitos (hasta **4**), y **`.`** o **Enter** confirman. Salta a la memoria con ese número de la lista. Si no existe: «No existe la memoria #N». Cualquier otra tecla cancela la secuencia y sigue su camino (Esc sigue siendo Panic). La secuencia caduca a los **6 s** sin teclas. Solo funciona si ningún campo de texto tiene el foco. Se admite con Show Lock activo.

#### 3.1.5 Pre-roll (2 s / 4 s)

Selector **Off / 2s / 4s** en el panel de memorias y en Time & Loops. Al saltar a una memoria arranca `N` segundos **antes** del marcador, nunca antes del inicio del archivo (`max(0, marca − N)`). Aparece el aviso «Pre-roll: -Xs -> Memoria [nombre]». **Mayús + clic** en una fila aplica 2 s aunque esté en Off. El valor se guarda en `audio.json` (`preroll_secs`) y persiste entre sesiones.

### 3.2 Slices y Vamping

Concepto: los **slices** son marcadores de corte dentro de la región de una cue de audio/vídeo; cada marcador abre un tramo hasta el siguiente y define cuántas veces se repite (`AudioSliceMarker { frame, play_count, label }`).

**Crear marcadores**

- Botón **Add Slice** en Time & Loops, o **doble clic en la onda**. El punto es: cursor de reproducción si suena; si no, cursor de edición; si no, la mitad de la región.
- Máximo **63** marcadores por voz (`MAX_SLICES_PER_VOICE = 64`, se guardan 63). Los marcadores deben ser estrictamente crecientes y estar dentro de la región (Inicio, Fin).
- Cada marcador tiene un **nombre del tramo** (≤ 64 caracteres, p. ej. «Entrada coro») que flota sobre el corte; es solo informativo.
- Se puede arrastrar el marcador, o editar su tiempo en la lista «Slices (N)».

**Repeticiones**

| Control | Efecto |
|---|---|
| Clic en el marcador (en la onda) | Cicla 1x → 2x → Infinite |
| Lista de slices | Botones `1x`, `2x`, `Infinite`; más de 2 repeticiones con un contador «N veces» (1 a 9 999) |
| Clic derecho en el marcador | Lo quita |
| «Repeticiones» y «Bucle infinito» (sección Bucles) | Repite **toda la región** (1 a 9 999 veces o infinito); la «Duración» mostrada será `inf` si es infinito |

**Indicación visual (columna de estado de la tabla)**

| Etiqueta | Aspecto | Significado |
|---|---|---|
| **VAMP** | Pastilla ámbar que **parpadea** (negro sobre ámbar ↔ ámbar sobre fondo) | La cue está atrapada en un tramo Infinite. Ayuda: «Atrapada en un bucle infinito: Devamp para salir» |
| **DEVAMP** | Pastilla cian **fija** | Se pidió salir: termina la pasada en curso. Ayuda: «Devamp: termina la pasada en curso y sigue» |

**Salida musical limpia (Devamp)** — la cue termina la pasada en curso y sigue con el tramo siguiente, sin saltos ni clics (`DevampMode::ContinueToNextSlice`). Formas de pedirlo:

1. Botón **Devamp** en Time & Loops (activo si la cue suena y tiene slices o bucle infinito).
2. **GO (Espacio) con el playhead sobre esa cue atrapada**: el secuenciador pide el Devamp en lugar de disparar (`cue_list.rs`, resultado `Devamped`).
3. Botón **Devamp** en la barra del Inspector, visible aunque haya **Show Lock** activo mientras la cue suena.
4. Cue de **Control → Devamp**, o por OSC `/cue/<n>/devamp`.

### 3.3 Ecualizador paramétrico (pestaña EQ)

Disponible en cues de audio y vídeo. Apagado por defecto («Ecualizador activo» desmarcado). Con el EQ apagado, o con una banda desactivada, el procesado se salta por completo (coste nulo).

**Especificaciones** (`cue_dto.rs`, `biquad.rs`)

| Parámetro | Rango |
|---|---|
| Bandas | **4**, en cascada |
| Frecuencia | **20 Hz – 20 000 Hz** |
| Ganancia | **−24 dB a +12 dB** (los cortes la ignoran) |
| Factor Q | **0,1 – 10** |
| Tipos de filtro disponibles por banda | `Low Cut` (paso alto), `Low Shelf`, `Peaking`, `High Shelf`, `High Cut` (paso bajo) |

Los tipos con ganancia son Low Shelf, Peaking y High Shelf. Aunque se mencionan como «LowCut, Shelving, Peaking, HighCut», el código tiene **cinco** tipos seleccionables en cada una de las 4 bandas.

**Valores por defecto al reiniciar («Restablecer bandas», 0 dB)**

| Banda | Tipo | Frecuencia | Q |
|---|---|---|---|
| 1 | Low Shelf | 100 Hz | 0,707 |
| 2 | Peaking | 500 Hz | 1,0 |
| 3 | Peaking | 2 000 Hz | 1,0 |
| 4 | High Shelf | 8 000 Hz | 0,707 |

**Implementación:** filtros biquad IIR con las fórmulas del *Audio EQ Cookbook* de RBJ, forma directa I con estado y coeficientes en `f64`. Antichasquidos: los parámetros se suavizan hacia su objetivo (**≈10 ms**) y los coeficientes se recalculan una vez por trozo de audio. Una banda de realce/shelf con ganancia < 0,01 dB se trata como identidad y se salta. Suena en vivo y se guarda con el show.

**Manejo de nodos en la gráfica** (eje horizontal logarítmico 20 Hz–20 kHz; vertical −24 a +12 dB; curva combinada calculada con el mismo código que filtra el audio):

| Acción | Resultado |
|---|---|
| Arrastrar un punto | Horizontal = frecuencia, vertical = ganancia. Ajuste fino: ganancia a 0,1 dB; frecuencia a 3 cifras significativas |
| Rueda sobre un punto | Cambia la Q (cada muesca ≈ ×1,17) |
| Doble clic en un punto | Activa/desactiva la banda (punto relleno = activa; hueco = desactivada) |
| Clic en un punto o en el número de la tabla | Selecciona la banda |
| Tabla inferior | Teclado: activa, tipo (menú), frecuencia (Hz), ganancia (dB), Q |

Colores de banda: cian, verde, ámbar, violeta. El punto de un Low/High Cut se dibuja en 0 dB.

### 3.4 PFL / audición en auriculares

**Configuración**

1. **Ver → Configuración de audio…**
2. En **Auriculares (PFL)** hay tres clases de opción:
   - **Sin salida de auriculares.**
   - **Canales 3-4, 5-6… de la placa principal**: solo si esa interfaz tiene **4 o más** canales de salida (los canales **1-2 son siempre la sala** y no se pueden elegir).
   - **Otras salidas del sistema** (el jack de la notebook, «Realtek High Definition Audio», una placa USB aparte…): los auriculares suenan por **su propia placa**, sin exigir 4 canales en la interfaz de la sala (§8.7).
3. **Aplicar** (reinicia el motor). Por línea de órdenes: `--monitor-first N` (N desde 1, canales de la misma placa) o `--monitor-device NOMBRE` (otra placa).

**Uso**

| Qué | Cómo |
|---|---|
| Escuchar una cue | Botón **PFL** en la barra del Inspector (cues de audio/vídeo); queda pulsado mientras suena |
| Escuchar desde una memoria | Botón **PFL** de la fila en el panel de Memorias (arranca en ese punto exacto, sin pre-roll) |
| Detener | Pulsar de nuevo el mismo PFL (misma cue y mismo punto), o termina solo al final del archivo |

La audición carga el audio en un reproductor ad-hoc reservado, a **0 dB**, con `monitor: true`: **no sale por la sala y el volumen MASTER no la afecta**, tanto si los auriculares usan canales de la misma placa como si van a otra. No mueve el playhead. Si no hay auriculares configurados, el motor devuelve un error («No se pudo escuchar en auriculares: …»).

### 3.5 Proyección de vídeo

1. Una cue de vídeo suena por la pista de audio de su archivo; la imagen **sigue al audio, no al revés.**
2. **Ver → Abrir pantalla de proyección** o botón **Proyección** de la barra de herramientas (o el botón de la pestaña Video). Se abre una **ventana secundaria** del sistema («Proyección — Pantalla Secundaria», 960×540, mínimo 320×180) posicionada a la derecha del monitor principal; se puede mover y redimensionar.
3. Arrastrarla al monitor/proyector correcto y pulsar **F** (o doble clic) para pantalla completa; **Esc** sale de ella. La ventana muestra la resolución del monitor actual (egui no enumera monitores).
4. Muestra la cue de vídeo que **empezó a sonar más recientemente** entre las que suenan; si no hay ninguna, negro. Ajuste de la imagen por cue: Fit, Fill o Stretch (`VideoFit`).

**Audio Master Clock** (`video.rs`):

- Se toma el frame de lectura del audio (`source_frame`) y se muestra el cuadro cuyo `pts` es el último ≤ reloj de audio.
- Si la interfaz/GPU se atrasa, los cuadros intermedios se **descartan** (no se ralentiza el audio). Ej. de las pruebas: el reloj avanza 0,5 s de golpe → se muestra el último cuadro y se saltan los 11 intermedios.
- Si el reloj retrocede más de **0,25 s** (vuelta de bucle/cambio de región) o se adelanta más de **1,0 s** al cuadro actual (la decodificación no da abasto), el decodificador de ffmpeg se **reinicia** en la nueva posición.
- La decodificación va 4 cuadros por delante (contrapresión sobre la tubería); la imagen se escala en ffmpeg a ≤ 960×540.
- Las cues de vídeo detenidas vuelven a su miniatura (primer cuadro).

### 3.6 Botonera Carts (F3)

Abre/cierra una ventana flotante «Botonera de Carts» con **16 pads** (cuadrícula 4×4). Los pads son reproductores independientes: suenan al instante **sin mover la cue en espera ni el playhead**. Se guardan **dentro del show**.

**Asignar un pad:** clic derecho (o clic en uno vacío) → editor del pad:

| Campo | Detalle |
|---|---|
| Nombre | Texto libre; si queda vacío se usa el nombre del archivo sin extensión |
| Archivo | Ruta o botón «Examinar…» (rellena el nombre si estaba vacío) |
| Color | Gris, Rojo, Naranja, Verde, Azul, Violeta |
| Volumen | −60 a +12 dB (barra con 1 decimal) |
| Atajo | Sin atajo, o una de **18 teclas**: `1 2 3 4 5 6 7 8 9 0 F5 F6 F7 F8 F9 F10 F11 F12` (se marca «(ya usado)» si otro pad la tiene). Se excluyen F1, F2, F3 y las letras |
| Modo | ver tabla |
| «Vaciar pad» | Borra el pad |

**Modos de disparo**

| Modo (etiqueta en pantalla) | Valor interno | Comportamiento |
|---|---|---|
| Disparar y reiniciar | `Retrigger` (por defecto) | Cada pulsación lo dispara; si ya suena, reinicia desde el principio |
| Alternar Play/Stop | `Toggle` | La 1.ª pulsación lo dispara; la siguiente, estando sonando, lo detiene |
| Pulsar para reproducir | `Hold` | Suena mientras se mantiene pulsado (clic o tecla); al soltar se detiene |

El pad muestra «SUENA» y se ilumina mientras suena (según la duración del archivo). Botón **Detener todos**. El paro individual usa un fundido de 480 frames (10 ms a 48 kHz). **Con Show Lock activo no se pueden editar pads, pero sí dispararlos.**

### 3.7 Modo «Show Lock» (Ctrl+L)

Etiqueta en la interfaz: **«Bloqueo de función»** (botón «Función BLOQUEADA» cuando está activo; el título de la ventana añade « [FUNCIÓN BLOQUEADA]» y la ventana dibuja un borde ámbar de 3 px).

**Bloquea** (muestra «Bloqueo de función activo: Ctrl+L para desbloquear»):

| Elemento |
|---|
| Soltar archivos (DnD) |
| Nuevo show (Ctrl+N), Abrir show (Ctrl+O), Abrir reciente, elegir archivo de audio |
| Crear cues (todos los botones `+ …`) |
| Borrar cue (Supr / Retroceso / papelera) |
| Duplicar (Ctrl+D) |
| Deshacer / Rehacer |
| Mover cues (subir / bajar) |
| Cambiar el modo de continuación en la tabla (columna «Sig.») |
| Marcar memorias (M y Enter) |
| **Todo el contenido de las pestañas del Inspector** (deshabilitado) |
| Edición de pads de Carts (clic derecho / editor) |

**Sigue operativo** (según el texto del propio botón y el código):

| Elemento |
|---|
| **GO** (Espacio / botón), **Panic** (Esc, Esc Esc), Pausar/Reanudar/Parar todo |
| Deslizador **MASTER** |
| **Saltos de memoria** (panel de Memorias, rombos de la onda, secuencia «. N .») |
| **Proyección** (abrir/cerrar, F) |
| **Devamp** (botón visible en la barra del Inspector mientras la cue suena) |
| Selección con ↑/↓ y clic; doble clic para disparar |
| **Disparo de pads de Carts** (clic o tecla) y «Detener todos» |
| PFL |
| Guardar (Ctrl+S) y Guardar como; empaquetar; exportar guion; Ayuda (F1) |
| Ctrl+L para desbloquear |

El Show Lock **no persiste**: al abrir TeatroCue empieza desbloqueado. No afecta al control remoto OSC.

---

## 4. Gestión de archivos y salvaguarda

### 4.1 Autoguardado continuo y recuperación

| Aspecto | Dato |
|---|---|
| Frecuencia | Cada **60 s** (`cfg.autosave_secs = Some(60)` en `main.rs`) |
| Condición | Solo si hay **cambios sin guardar**, el motor está conectado, no hay otro autoguardado en curso y no hay una recuperación pendiente |
| Archivo | `~<nombre>.autosave` en la **misma carpeta del show** (p. ej. `D:\Shows\~Giselle.autosave`). Un show sin guardar usa `~sin_titulo.autosave` en la carpeta de los ajustes (`%APPDATA%\TeatroCue\` o `~/.config/teatrocue/`) |
| Escritura | Mediante el motor, de forma **atómica** (archivo temporal + renombrado). No cambia el show abierto ni su marca de «cambios sin guardar» |
| Borrado | Al **guardar a mano** (Ctrl+S) se borra el de ese show, el del show anterior y el de «sin título». Al **cerrar bien** la aplicación se borran también |
| Detección al arrancar | Busca entre los 8 shows recientes y el show sin nombre; ofrece el autoguardado solo si es **más nuevo** que el último guardado manual y **no está vacío** |
| Diálogo | «Recuperar sesión»: «Se detectó una sesión previa cerrada abruptamente. ¿Desea recuperar los cambios autoguardados?» + «show … · autoguardado hace N min/h/días». Botones **Recuperar cambios** / **Descartar** (descartar borra el archivo) |
| Tras recuperar | El show vuelve a ser el original, con cambios sin guardar; aviso «Sesión recuperada «…»: N cues. Guarde con Ctrl+S» |

### 4.2 Empaquetar Show (Archivo → Empaquetar show…)

Se elige una **carpeta de destino**. Si no hay diálogo nativo disponible se usa `<carpeta del show>/<nombre>-paquete`. Operación en el motor (`ipc_server.rs`, `bundle_show`):

```
<carpeta elegida>/
├── <nombre del show>.teatroshow
└── audio/
    ├── cancion1.wav
    ├── cancion1-2.wav      (si dos archivos distintos se llaman igual)
    └── ...
```

1. Crea `audio/` y copia **cada archivo multimedia** vinculado (audio y vídeo) sin duplicar el que usan varias cues.
2. Si dos archivos distintos se llaman igual (sin distinguir mayúsculas) renombra a `nombre-2.ext`, `nombre-3.ext`…
3. Reescribe cada ruta del show: `absolute_path = relative_path = "audio/<archivo>"`.
4. Valida el show y lo guarda como `<nombre>.teatroshow` (caracteres no alfanuméricos distintos de espacio, `-`, `_`, `.` se sustituyen por `_`) con escritura atómica (`.tmp` + renombrado).
5. Resultado: «Show empaquetado en … : N archivo(s), X MB».
6. Los archivos que **no existen** no se copian: se anotan (hasta 3 avisos «No se encontró …: se dejó su ruta original») y conservan su ruta original. Un fallo de copia aborta con «no se pudo copiar …».
7. Los pads de Carts viajan en el show, pero **la copia solo recorre las cues** (`file_target` de las cues): los archivos de los pads no se copian a `audio/`.

**Rutas relativas al abrir** (`resolve_media_paths`): las rutas relativas se resuelven respecto a la carpeta del show; si la ruta absoluta ya no existe se prueba la relativa. Un show empaquetado funciona abierto desde cualquier carpeta o equipo (verificado por la prueba `bundling_copies_media_rewrites_relative_paths_and_the_bundle_opens_from_anywhere`).

### 4.3 Exportación de Guion Técnico (Archivo → Exportar Guion Técnico…)

Nombre sugerido: `<show> - guion tecnico.html` (o `cue_sheet.html`). Escritura atómica. Resultado: «Guion técnico exportado (N cues): ruta».

**Estructura del HTML** (`cue_sheet.rs`): archivo **autocontenido** (CSS y un poco de JS embebidos, sin recursos externos).

- `<header>`: empresa «Ballet Oficial de Córdoba» / «Teatro del Libertador San Martín», título «Guion técnico — <show>», número de cues y fecha «Generado».
- Barra de herramientas (no se imprime): filtro de texto en vivo, casilla «Blanco y negro», botón «Imprimir».
- Tabla con 9 columnas:

| Columna | Contenido |
|---|---|
| Cue | Número |
| Nombre | Nombre (o nombre por defecto), con sangría por nivel de grupo y ⚑ si está marcada |
| Destino / Audio | Archivo (audio/vídeo); destino y mensaje (OSC); «→ cue N» (Fade/Control) |
| Tipo | Audio, Vídeo, Fade, `Control · <acción>`, OSC, Memo, `Grupo · línea de tiempo / primera y sigue / todas a la vez` |
| Pre-Wait | Tiempo |
| Duración | La conocida por el motor; si falta, la del show |
| Sig. | Auto-continue / Auto-follow / vacío |
| Memorias | Lista `tiempo nombre — nota` |
| Notas de escena | Notas de la cue |

- Las cues desarmadas aparecen **tachadas y atenuadas**; los grupos con fondo y negrita.
- Los tiempos se muestran como `mm:ss` o `mm:ss.mmm` (`h:mm:ss` desde una hora).

**Optimización para impresión** (`@page`): **A4 apaisado**, márgenes 12 mm / 10 mm; en impresión se oculta la barra, se fuerza escala de grises (`grayscale(1)`), texto negro, cabecera de tabla repetida en cada página (`thead` como `table-header-group`) y las filas **no se parten** entre páginas (`break-inside: avoid`).

---

## 5. Administración, seguridad y licenciamiento

### 5.1 Instalación física con `Instalar_TeatroCue.exe`

Pendrive (generado con `scripts/build_usb_installer.sh /ruta/al/pendrive`):

```
Instalar_TeatroCue.exe
master.key.enc          (clave privada Ed25519 cifrada con la contraseña maestra)
payload/ { teatrocue-ui.exe, teatrocue-daemon.exe, ffmpeg.exe, [ffprobe.exe], [*.dll], licencias/ }
SHA256SUMS.txt
LEEME.txt
```

Las claves del desarrollador viven en `~/.config/teatrocue-keys/` (`master.key.enc` y `public.key`). **No se regeneran:** hacerlo invalidaría todas las licencias emitidas. El instalador y los programas llevan incrustada la clave pública y exigen licencia (`enforce-license`).

**Pasos** (`installer-win/src/lib.rs` y `main.rs`; orden diseñado para no dejar la PC a medias):

1. Ejecutar `Instalar_TeatroCue.exe` (doble clic) desde el pendrive. Muestra el **Machine ID** de la PC.
2. Comprueba que el paquete esté completo (`teatrocue-ui.exe`, `teatrocue-daemon.exe`, `ffmpeg.exe` obligatorios) **antes de tocar el disco**.
3. Si hay programas instalados abiertos, aborta sin cambiar nada (evita mezclar versiones).
4. Pide la **contraseña maestra** del desarrollador (sin eco; hasta **3 intentos**). La clave privada solo existe en memoria y se descarta tras firmar.
5. Menú de duración: **1** 1 mes (30 d) · **2** 3 meses (90 d) · **3** 6 meses (180 d) · **4** 1 año (365 d) · **5** sin vencimiento · **6** otra cantidad de días (1 a 3650).
6. Firma la licencia para ese Machine ID y la verifica con la clave pública.
7. Copia `payload/` a **`C:\TeatroCue`** (cada archivo por temporal + renombrado) y escribe `license.key`.
8. Crea el acceso directo **`TeatroCue.lnk`** en el Escritorio.
9. Mensaje final: «TeatroCue instalado y activado exitosamente para el Ballet Oficial de Córdoba.»

**Opciones:** `--days N`, `--perpetual`, `--solo-licencia` (renueva solo la licencia, sin copiar programas), `--reset-reloj`, `--target CARPETA`, `--no-shortcut`, `--no-pause`. Variable `TEATROCUE_MASTER_PASSWORD` para automatizar.

**Renovar / actualizar:** volver a ejecutar el instalador; detecta la instalación, muestra cuánto queda y pide nueva duración (cuenta desde hoy). Al actualizar programas: **cerrar TeatroCue** antes; si pregunta por la licencia, responder **n**. Los shows guardados no se tocan.

**Comprobación de licencia:** solo al **arrancar**; nunca corta el audio durante una función. Avisa desde **14 días** antes del vencimiento.

**Fecha de la PC (anti-retroceso):** TeatroCue guarda la fecha más alta que vio en tres sitios: `license.state` junto a la aplicación, `%ProgramData%\TeatroCue\clock.dat` y el registro (`HKCU\Software\TeatroCue\ClockMark`), con un sello HMAC atado a la PC y la licencia. Tolerancia **36 h** hacia atrás. También se usan como «anclas» las fechas de archivos de Windows (`System32\config\SYSTEM/SOFTWARE/SECURITY`, registros de eventos). Si se atrasa la fecha, o se borran las marcas, la aplicación se bloquea. Si la fecha se adelantó por error, ejecutar el instalador con la contraseña maestra y confirmar la fecha de hoy (o `--reset-reloj`). El *Machine ID* sale del `MachineGuid` del registro (`HKLM`, SHA-256 del GUID con dominio `TeatroCue/machine-id/v1`, formato `XXXX-XXXX-XXXX-XXXX`).

### 5.2 Reactivación de emergencia (desafío-respuesta, por teléfono/WhatsApp)

Escenario: licencia vencida y sin el pendrive (`activation.rs`).

**Paso 1 — Operador:** al abrir TeatroCue con licencia vencida aparece el bloque **«Reactivación de Emergencia»** (también en F1 → «Autorización Legal y Licencia»; no se muestra con licencia perpetua). Muestra el código y el botón **Copiar código**.

**Estructura del código de solicitud:**

```
REQ-XXXX-XXXX-XXXX-XXXX-EEC
    └─────Machine ID────┘ │└┘└ C = carácter de control
                          └ EE = época (≥ 2 símbolos Base32 Crockford)
```

- `XXXX-XXXX-XXXX-XXXX`: Machine ID (16 hex).
- `EE`: «época» = cantidad de reactivaciones de emergencia ya canjeadas en esa PC (mínimo 2 símbolos, ceros a la izquierda).
- `C`: carácter de control (suma ponderada módulo 32). Detecta una letra mal copiada **antes** de firmar.
- Alfabeto Crockford `0123456789ABCDEFGHJKMNPQRSTVWXYZ`; en la lectura `O`→0, `I`/`L`→1; espacios y guiones se ignoran.

**Paso 2 — Operador:** envía el código al desarrollador (por WhatsApp, por ejemplo).

**Paso 3 — Desarrollador:** firma con

```bash
installer-tool sign-challenge --code REQ-XXXX-XXXX-XXXX-XXXX-EEC --days N [--key ruta/master.key.enc]
```

- `--days`: **1 a 365** (`MAX_EMERGENCY_DAYS`).
- Busca `master.key.enc` en: `$TEATROCUE_MASTER_KEY`, junto al ejecutable, `~/.config/teatrocue-keys/master.key.enc`, o la carpeta actual. Pide la contraseña (o `TEATROCUE_MASTER_PASSWORD`). El código se valida **antes** de pedir la contraseña.
- Salida:

```
Solicitud: REQ-…
PC (Machine ID): XXXX-XXXX-XXXX-XXXX
Valida hasta: DD/MM/AAAA  (canjear antes del DD/MM/AAAA)

CLAVE DE ACTIVACIÓN GENERADA (Válida por N días): [RESP-XXXXXXXX-XXXXXXXX-…]
```

Alternativas equivalentes: `teatrocue-admin` → «Resolutor de Emergencias» (§5.3) y `tools/generador_licencia_movil.html` (generador para móvil; su firma se verifica byte a byte contra la de Rust con `tools/vector_firma.json`).

**Estructura de la clave `RESP-…`:** `RESP-` + Base32 Crockford agrupado de a 8 caracteres de un bloque de **85 bytes**: versión (1) + Machine ID (8) + época (4) + `valid_until` (4) + `redeem_by` (4) + **firma Ed25519 de 64 bytes** (mensaje con dominio `TeatroCue/activation/v1\n`).

**Paso 4 — Operador:** pega la clave en «Pegar clave de activación» y pulsa **Activar**. Si es correcta: «Licencia extendida hasta: DD/MM/AAAA» y TeatroCue queda habilitado sin reiniciar (el daemon recarga en caliente). Si hay error se explica en pantalla.

**Reglas de validación:**

| Regla | Detalle |
|---|---|
| Firma | Ed25519 verificada con la clave pública incrustada. La app **no puede firmar** |
| Misma PC | El Machine ID de la clave debe ser el de esta PC |
| Anti-replay | La época de la clave no puede ser anterior a la local: **cada clave sirve una vez** |
| Plazo | Debe canjearse dentro de **7 días** desde que se firma (`REDEEM_WINDOW_SECS`) |
| Fecha absoluta | `valid_until` es una fecha fija, no “N días desde que se canjea”; repetir una clave vieja nunca da más tiempo |
| Debe extender | Si la licencia ya vence igual o después, se rechaza |
| No perpetuas | Una licencia sin vencimiento no necesita reactivación |
| Reloj | Antes de canjear se comprueba que el reloj sea creíble |

Al canjear, la clave se **adjunta** a `license.key` (línea `activation:…`), la época sube y las marcas de reloj se renuevan. No se reescribe la licencia base.

Mensajes de error: firma inválida, “esta clave es para otra PC”, “ya fue usada o es de una solicitud anterior”, “la clave caducó el …”, “la clave no extiende la licencia”, “el reloj de esta PC no es creíble”, etc.

### 5.3 `teatrocue-admin` (administración de licencias, 100 % offline)

Aplicación de escritorio (egui) «TeatroCue · Administración de licencias» (1100×700). Pantalla de inicio: contraseña maestra → **Desbloquear** (la clave privada vive solo en RAM; botón «Bloquear») o «Continuar sin firmar (solo consulta)».

**Base de datos SQLite `licencias.db`**

Ubicación: Linux `~/.config/teatrocue-keys/licencias.db`; Windows junto al ejecutable (o en la carpeta superior si allí ya existe). Tablas:

| Tabla `equipos` | Tipo |
|---|---|
| `id` | INTEGER PK AUTOINCREMENT |
| `institucion` | TEXT |
| `puesto_sala` | TEXT |
| `machine_id` | TEXT UNIQUE NOT NULL |
| `created_at` | INTEGER (Unix) |
| `notas` | TEXT |

| Tabla `licencias_emitidas` | Tipo |
|---|---|
| `id` | INTEGER PK AUTOINCREMENT |
| `equipo_id` | INTEGER → `equipos(id)` (índice `idx_lic_equipo`) |
| `tipo_emision` | TEXT: `Pendrive Instalador` o `Emergencia REQ/RESP` |
| `fecha_emision` | INTEGER |
| `fecha_vencimiento` | INTEGER (**0 = perpetua**) |
| `dias_otorgados` | INTEGER |
| `activation_epoch` | INTEGER |
| `token_generado` | TEXT (el `license.key` completo o la clave `RESP-…`) |
| `notas` | TEXT |

**Pestañas**

| Pestaña | Función |
|---|---|
| **Inventario de Salas** | Lista de equipos con búsqueda (institución, puesto o Machine ID), filtros y detalle/historial. Semáforo: **Activa** (verde; también las perpetuas), **Por vencer** (ámbar, vence en **< 15 días**), **Vencida** (rojo), **Sin licencia** (nunca se le emitió nada). Filtros: Todas, Activas, «Por vencer en < 15 días», Vencidas. El vencimiento efectivo de un equipo es: perpetuo si alguna emisión lo es; si no, el más lejano |
| **Emitir Licencia Fija** | Institución, puesto/ubicación, Machine ID, vigencia (1 mes, 3 meses, 6 meses, 1 año, Perpetua o días personalizados 1–3650). «Generar y Guardar license.key» escribe el archivo y registra equipo y emisión |
| **Resolutor de Emergencias** | Pegar el `REQ-…`; identifica el equipo conocido (o pide institución para registrarlo), elegir extensión (1–365 días), **Firmar y Generar Clave**; **Copiar para WhatsApp** deja listo el mensaje con la clave. Registra la emisión (`Emergencia REQ/RESP`) con su época |
| **Mantenimiento y Respaldo** | Cuenta de equipos y licencias y ruta de la base. **Exportar respaldo (JSON)** (formato `teatrocue-licencias/v1`, equipos con su historial) y **Exportar respaldo (CSV)** (una fila por licencia emitida; neutraliza prefijos `= + - @` para Excel; columnas `institucion, puesto_sala, machine_id, equipo_notas, tipo_emision, fecha_emision, fecha_vencimiento, dias_otorgados, activation_epoch, token_generado, licencia_notas`) |

Si no se encuentra `master.key.enc` o la clave no corresponde a la pública del paquete, se rechaza («las licencias emitidas no serían válidas»).

### 5.4 Rutas de diagnóstico

| Archivo | Windows | Linux | Contenido |
|---|---|---|---|
| **`crash.log`** | `%PROGRAMDATA%\TeatroCue\crash.log` (normalmente `C:\ProgramData\TeatroCue\crash.log`) | `~/.local/state/teatrocue/crash.log` (o `$XDG_STATE_HOME/teatrocue/crash.log`) | Un bloque por pánico: `[unix <seg>] <programa> <versión> · hilo \`<nombre>\` · archivo:línea` + mensaje. Si supera **512 KB** se rota a `crash.old`. Lo instalan UI, daemon e instalador |
| **`audio.json`**, **`recent.json`** | `%APPDATA%\TeatroCue\` | `~/.config/teatrocue/` | Ajustes de audio y shows recientes |
| **`clock.dat`**, **`license.state`** | `%ProgramData%\TeatroCue\clock.dat`; `C:\TeatroCue\license.state` | — | Marcas anti-retroceso de reloj |
| **`license.key`** | Junto a `teatrocue-ui.exe` (`C:\TeatroCue\license.key`) | junto al ejecutable | Licencia firmada |
| **`daemon.log`** | **No existe en el código** — ver §6 | | |

Un cierre instantáneo **sin** entrada en `crash.log` suele ser del sistema (p. ej. instrucción ilegal, código `-1073741795`); se ve con `echo %errorlevel%`.

Para ver el código de solicitud sin interfaz: `teatrocue-daemon --license-request`; para canjear una clave: `teatrocue-daemon --license-apply RESP-…`.

---

## 6. Discrepancias y puntos a verificar antes de publicar el manual

Estos puntos surgieron al contrastar código y documentación; conviene decidir cómo se redactan.

1. **`daemon.log` no existe.** No hay ninguna referencia en el código a un archivo con ese nombre. El único `*.log` relacionado es `qlab-daemon.log` en `$XDG_RUNTIME_DIR` (o `/tmp`), creado únicamente por el script de desarrollo `scripts/run_local_pipewire.sh`, no por el programa instalado. El daemon lanzado desde la UI se arranca con la salida estándar descartada. En Windows el único registro persistente es `crash.log`.
2. **Tipos de filtro del EQ:** el pedido habla de «LowCut, Shelving, Peaking, HighCut»; el código ofrece cinco por banda (Low Cut, Low Shelf, Peaking, High Shelf, High Cut). Las 4 bandas por defecto son Low Shelf / Peaking / Peaking / High Shelf.
3. **WASAPI exclusivo no es accesible desde la interfaz** (solo `--exclusive` en el daemon aparte) y el propio código lo marca como no verificado en hardware real. La documentación debería presentar WASAPI **compartido** como el modo de trabajo.
4. **Atajos de pad:** el comentario de `CartPad.key` menciona `A…Z` y `F1…F12`, pero el selector de la interfaz solo ofrece 18 teclas (`1–9, 0, F5–F12`) y la guía del sonidista las lista igual. Vale lo de la interfaz.
5. **Master «Doble clic = 0 dB»** coincide con el código; `GUIA-SONIDISTA.md` no menciona que el volumen maestro **no se guarda** y siempre empieza en 0 dB (sí lo dice el manual interno).
6. **Show Lock y OSC:** el código no restringe el control remoto OSC con el bloqueo; si se desea blindar la consola, mantener OSC apagado (es el valor por defecto).
7. **Empaquetado y pads:** los archivos de los pads de Carts **no** se incluyen en `audio/` al empaquetar; si el show se mueve a otro equipo hay que copiarlos aparte.
8. **Tabla de atajos de F1 incompleta:** la lista `SHORTCUTS` del manual interno no incluye **F3** (Carts), **Ctrl+L** (Show Lock), la secuencia **«. N .»** ni **Enter** como alternativa de M; tampoco la barra de estado inferior. Este dossier sí los recoge.
9. **Versión mostrada:** «Acerca de» imprime `CARGO_PKG_VERSION` (0.1.0).
10. **«NaturalEndOfSlice» no existe como nombre en el código.** La salida limpia de un slice se implementa con `DevampMode::ContinueToNextSlice` y `DevampMode::StopAtSliceEnd` (`rt_command.rs`) y el evento `AudioStatus::SliceDevamped`. Describe lo mismo: la voz termina la pasada en curso hasta el último frame del slice y recién entonces sigue o se detiene (§8.3).
11. **Cifras de rendimiento (§8):** la latencia de GO (≈157 µs) y el callback máximo (0,57 ms) son mediciones de las pruebas del repositorio en la PC de desarrollo, no garantías. El PFL en otra placa se comprobó con las dos placas reales de Linux (abren a la vez) y con pruebas automáticas, pero **no se midió acústicamente** ni en Windows.

---

## 7. Apéndice — Control remoto OSC y línea de órdenes

### 7.1 OSC (UDP, apagado por defecto)

Se activa en Ver → Configuración de audio → «Activar en el puerto UDP» (por defecto **53000**, rango 1024–65535) o con `--osc-port N`. Cualquiera en la red con el puerto puede disparar la función: usar solo en red de confianza.

| Dirección | Acción |
|---|---|
| `/go` · `/panic` · `/stop` · `/pause` · `/resume` · `/hardstop` · `/next` · `/prev` | Transporte global |
| `/master <dB>` | Volumen maestro (−60 a +6) |
| `/cue/<n>/start` | Dispara la cue `n` (número o id) |
| `/cue/<n>/stop` · `pause` · `resume` · `devamp` · `load` · `arm` · `disarm` | Acciones por cue |

Admite mensajes sueltos y `#bundle` anidados; tipos de argumento `i`, `f`, `s`, `T/F/N/I`. Basura o paquetes mal formados se ignoran.

### 7.2 Opciones de `teatrocue-daemon`

`--socket`, `--shm`, `--show`, `--backend cpal|null`, `--host`, `--device`, `--rate`, `--channels`, `--buffer`, `--exclusive`, `--rt-priority`, `--seq-priority`, `--ui-ring`, `--no-mlock`, `--watchdog-ms`, `--no-safe-mute`, `--alloc-check off|count|abort`, `--monitor-first N`, `--monitor-device NOMBRE`, `--osc-port N`, `--list-devices`, `--license-request`, `--license-apply CLAVE`, `--version`, `--help`.

### 7.3 Opciones de `teatrocue-ui`

`--remote RUTA` / `--socket RUTA`, `--standalone`, `--standalone-null`, `--exit-after-ms N`, `--help`.

### 7.4 Valores operativos de referencia

| Concepto | Valor |
|---|---|
| Panic suave | 1 s de fundido |
| Parar todo (botón) | 240 frames (5 ms a 48 kHz) |
| Parada dura | 96 frames (2 ms a 48 kHz) |
| Doble Esc | < 700 ms |
| Aviso (toast) | 6 s; máximo 5 simultáneos |
| Aviso grande (HUD) | 2,5 s |
| Autoguardado | 60 s |
| Caducidad de «. N .» | 6 s |
| Aviso de vencimiento de licencia | 14 días |
| Tolerancia de reloj | 36 h |
| Plazo para canjear una clave RESP | 7 días |
| Máx. días de emergencia | 365 |
| Máx. días de licencia fija | 3650 |
| Shows recientes | 8 |
| Slices por cue | 63 |
| Memorias por cue | 500 |
| Velocidad | 0,03× – 33× |
| Fader / matriz | −120 a +12 dB (el MASTER: −60 a +6 dB) |

---

## 8. Arquitectura de Bajo Nivel y Decisiones DSP (Under the Hood)

Esta sección explica **por qué** el motor se comporta como lo hace. Cada afirmación remite al código que la sostiene y, cuando hay una cifra, a la prueba que la mide. Para el uso, ver las secciones 1 a 3; para los parámetros, §2.

### 8.1 Invariante de tiempo real: cero asignaciones, cero bloqueos

El callback de audio es el único código del programa que **no puede esperar**: si tarda más que su bloque (2,67 ms con 128 frames a 48 kHz) se oye un corte. Por eso se cumplen tres reglas, todas verificadas por pruebas:

| Regla | Cómo se garantiza | Dónde |
|---|---|---|
| **Cero asignaciones de memoria** dentro del callback | Un `GlobalAlloc` envoltorio (`RtGuardAllocator`) detecta cualquier `alloc`/`dealloc`/`realloc` en el hilo mientras corre dentro de un `RtScope`/`assert_no_alloc`. Los programas lo registran; con `--alloc-check abort` aborta con backtrace, con `count` (por defecto) lo cuenta en `rt_alloc_violations` (que las pruebas exigen en **0**, también con EQ, Carts y PFL) | `alloc_guard.rs`, `health.rs` |
| **Comandos sin destructores y de tamaño fijo** | `AudioCommand` y `AudioStatus` son `Copy`, sin `Drop`, ≤ 160 B y ≤ 32 B; lo exigen aserciones en tiempo de compilación (`const _: () = assert!(…)`). Los búferes (EQ, mezcla de auriculares, anillos) se reservan **al abrir el stream**, nunca en el callback | `rt_command.rs`, `backend_audio.rs` |
| **Sin bloqueos** | Secuenciador → audio y audio → secuenciador usan colas **SPSC lock-free** (`ringbuf`, 1024 comandos / 2048 estados). La interfaz → secuenciador usa el anillo caliente `HotControlRing` en memoria compartida (2048 ranuras de 32 B, SPSC). La telemetría de vuelta (estado del show, posiciones) se publica con un **Seqlock**: un escritor, lectores sin bloqueo que reintentan si el escritor estaba a mitad (hasta 64 intentos). Los vúmetros son máximos atómicos (`fetch_max`) que la interfaz consume con `swap(0)`. La celda `RtCell` da acceso exclusivo con un solo atómico y **devuelve `None` en vez de esperar** (reinicio del stream) | `ipc_shm.rs`, `backend_audio.rs` |

**Latencia de GO ≈ 157 µs.** Desde que la interfaz escribe GO en el anillo caliente hasta que el comando `Play` queda en la cola del audio pasan, en la prueba `the_sequencer_thread_ticks_at_1khz_and_stops_cleanly`, entre **154 y 158 µs** (seis corridas en release en la PC de desarrollo). Es una cota superior: la prueba sondea cada 100 µs. El secuenciador corre a 1 kHz (prueba: 1002 Hz) con peor retraso de 7–16 µs.

### 8.2 Curvas de fade: tabla de 33 puntos precalculada

Los fades no evalúan `exp`, `pow` ni funciones trigonométricas muestra a muestra. Cada curva (lineal, S, Bézier, paramétrica) se calcula **fuera del hilo de audio**, en el secuenciador, y se envía como una `CurveTable`: **33 puntos** (32 tramos), 132 bytes, `Copy`, en el stack (`rt_command.rs`). En el callback solo se **interpola la tabla** con un cúbico Catmull-Rom de 4 puntos (`eval`), que pasa por todos los puntos y queda acotado por sus dos vecinos: **un fade nunca rebasa su objetivo** (sin *overshoot*). Existe también la interpolación lineal de la especificación original (`eval_linear`). Las conversiones dB ↔ lineal se hacen cuando cambia un nivel, no por muestra.

### 8.3 Slices y Devamp a nivel de frame entero

- Un programa de reproducción es una lista de **segmentos `[start_frame, end_frame)` con enteros `u64`** (hasta 64 por voz): los puntos de bucle son exactos al frame, sin redondeos de segundos ni deriva acumulada. La **fase se preserva** porque el cursor salta de `end_frame` a `start_frame` de un frame al siguiente, sin remuestrear ni tocar la muestra (`SliceSegment`, `resolve_boundaries`).
- **Devamp** es una petición atómica (`AtomicU8`: `ContinueToNextSlice` o `StopAtSliceEnd`) que el callback lee **solo al llegar al último frame del slice en curso**. La pasada nunca se corta a la mitad: termina, y entonces la voz sigue al tramo siguiente o se detiene, avisando con `SliceDevamped` (y, si corresponde, dispara la cue siguiente). Es la «salida natural del slice»: sin recortar el archivo ni dejar una cola cortada (§6, punto 10).
- Los bucles `n` veces o infinitos y el bucle de todo el programa se resuelven en el mismo sitio, con una guarda que impide recorrer la lista indefinidamente si hay datos inconsistentes.

### 8.4 Remuestreador Catmull-Rom de 4 puntos (regla de Horner) y *unity bypass*

- Cuando hace falta cambiar de velocidad o de frecuencia, se interpola con un **cúbico Catmull-Rom de 4 puntos** evaluado con la **regla de Horner** (`catmull_g`): cada muestra cuesta unas pocas multiplicaciones y sumas encadenadas, ideal para FMA, sin tablas ni asignaciones.
- **Unity bypass:** si la velocidad es exactamente **1,000×** y la frecuencia del archivo **coincide** con la del dispositivo (típicamente 48 kHz), `read_source` **copia la muestra tal cual**, sin interpolar: la muestra se lee tal cual, sin interpolar (después solo se aplican los niveles), y el coste es mínimo. Cambiar la velocidad con la voz sonando (los botones 0,90×/0,95×/1,05×) activa el interpolador sin reiniciar nada.
- El mismo interpolador sirve para los fades (§8.2) y para la corrección de deriva entre placas (§8.7).

### 8.5 Suavizado de parámetros: cero *zipper noise*

Un parámetro que salta de golpe produce un escalón audible («zipper noise»). Todo lo que el operador puede mover en vivo se **desliza** hacia su valor nuevo:

| Parámetro | Suavizado |
|---|---|
| Matriz de niveles (cada celda) | Filtro de un polo por muestra, constante de tiempo de **5 ms** (`slew_coeff`); las celdas ya asentadas usan un bucle de ganancia constante vectorizable |
| Volumen MASTER y *Safe Mute* | Rampa de ≈**10 ms** (`SAFE_MUTE_RAMP_SECS`); sin coste cuando el MASTER está en 0 dB |
| **EQ** (ganancia, frecuencia, Q de cada banda) | El objetivo se desliza (≈**10 ms**) y los **coeficientes biquad RBJ** se recalculan una vez por trozo de 128 frames, no por muestra; el estado del filtro (forma directa I) se guarda en `f64`; una banda desactivada o plana se **puentea** y el EQ apagado deja la señal bit a bit idéntica |
| Arranque desde una memoria | `PlayFadeIn`: entrada lineal de **50 ms** desde silencio, para que un salto a mitad de pista no suene a clic |
| Pausa / parada | Fundidos de transporte con mínimos: pausa ≥ 5 ms (240 frames a 48 kHz), parada ≥ 2 ms (96 frames); la parada normal usa 5 ms |

### 8.6 Despacho SIMD dinámico: AVX2/FMA con *fallback* seguro

El ejecutable de Windows se compila con **CPU genérica `x86-64` (solo SSE2)**: así el código de arranque, la biblioteca estándar y la interfaz **nunca** emiten instrucciones de 256 bits antes de `main()`. (Con `x86-64-v3` el programa moría con `0xC000001D` / `-1073741795`, «instrucción ilegal», en la PC Acer del teatro.) La velocidad se recupera **en tiempo de ejecución**:

- `cpu_has_avx2_fma()` consulta CPUID una sola vez (`is_x86_feature_detected!`) y el resultado queda cacheado.
- Con AVX2 y FMA se llama a `process_voices_avx2` / `read_source_avx2` (`#[target_feature(enable = "avx2,fma")]`, FMA fusionado); si no, a `process_voices_fallback` (SSE2 / baseline del objetivo).
- Ambos caminos comparten **el mismo código fuente** (genérico en `const HW: bool`); una prueba comprueba que dan la misma salida dentro de **1e-5**.
- Medido con `qlab-bench` (64 voces con remuestreo cúbico, backend nulo, 10 s): callback máximo **0,57 ms** genérico frente a 0,60 ms con AVX2, sobre un presupuesto de 2,67 ms: sin penalización apreciable. Verificado en una VM con `-cpu Nehalem` (sin AVX).
- Además se activan FTZ/DAZ (números subnormales a cero) al inicio de cada callback.

### 8.7 PFL multitarjeta con compensación de deriva de reloj

Si los auriculares van en **otra placa**, esa placa tiene su propio cristal (decenas de ppm de diferencia) y quizá otra frecuencia. Tocar dos streams a ritmo nominal vaciaría o desbordaría el búfer entre ambos. La solución:

1. La mezcla interna lleva **2 canales extra** (la audición se enruta ahí con su propia matriz de niveles); el callback de la sala saca los primeros por su stream y envía el par extra a un **anillo SPSC** (`FrameRing`, 16 384 frames ≈ 0,34 s).
2. El stream de la segunda placa lee del anillo con un **`DriftResampler`**: un **controlador PI** mide el nivel de llenado (objetivo = 4 buffers, mínimo 512 frames) y ajusta continuamente la razón de remuestreo (tope ±1000 ppm; engancha a 0,4 Hz y se estabiliza a 0,1 Hz) sobre el interpolador Catmull-Rom (§8.4). Si las dos placas corren a frecuencias distintas (p. ej. 48 y 44,1 kHz) el mismo remuestreador las concilia.
3. El **MASTER no toca el par de auriculares**; el *Safe Mute* sí. Si la segunda placa no se puede abrir, la sala sigue sonando y la audición avisa.

Se probó con pruebas automáticas (el bus no sale por la sala, ignora el MASTER, reconstruye una señal constante a 44,1 kHz, sin asignaciones) y abriendo dos placas reales a la vez en Linux; **no se ha medido acústicamente** ni en Windows.

### 8.8 Sincronización A/V «Audio-Master»

En una cue de vídeo **el audio dicta el tiempo y la imagen lo sigue** (`video.rs`):

- El motor reproduce la pista de audio y publica el frame del archivo que está leyendo (`source_frame`); la interfaz muestra el cuadro cuyo instante corresponde a ese reloj.
- Si la interfaz va con retraso (contención gráfica, otra ventana) los **cuadros atrasados se descartan** (se cuentan en `dropped`); **nunca se frena el audio ni se acumula demora**.
- Si el reloj salta (la vuelta de un bucle, un cambio de región, un salto de memoria) el decodificador se **reinicia** en la nueva posición: tolerancia hacia atrás 0,25 s y hacia adelante 1,0 s.
- La imagen se decodifica con `ffmpeg` como subproceso (cuadros RGBA por una tubería con 4 cuadros de adelanto máximo, escalados a 960×540 como máximo): un fallo de vídeo **no afecta al sonido**.
