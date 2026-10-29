# TeatroCue — guía rápida para el sonidista

Todo esto también está en el manual del programa (tecla **F1**).

## En función
| Qué | Cómo |
|---|---|
| Disparar la cue en espera | **Espacio** o el botón GO |
| Parar todo (suave / duro) | **Esc** / **Esc Esc** (dos veces seguidas) |
| Volumen de toda la sala | Deslizador **MASTER** (barra superior). Doble clic = 0 dB |
| Pausar / reanudar todo | Botones «Pausar todo» / «Reanudar todo» |
| Mover el playhead sin disparar | Flechas ↑ ↓ |
| Pausar / reanudar | **P**: el cabezal queda quieto en la onda y al reanudar sigue desde ese mismo frame |
| Marcar una memoria | **M** (o Enter) |

## Memorias (como las «Ubicaciones de memoria» de Pro Tools)
1. **Marcar**: con la cue sonando pulse **M** (o **Enter**, también el del teclado numérico). Se captura ese instante y aparece el cuadro **«Nombre de la memoria:»** ya enfocado: escriba el nombre y **Enter** (vacío = «Memoria N»), **Esc** cancela. También sirve con la cue parada: haga clic en la onda y pulse M.
2. En la **regla de la onda** queda un **rombo ámbar** con su nombre; un clic en el rombo salta a ese punto, y **sosteniéndolo y arrastrando** se mueve: una línea roja cruza toda la onda (para alinear con una entrada musical) y un rótulo muestra el nombre y el tiempo exacto; al soltar se guarda en ese frame. También se puede **escribir el tiempo** (clic sobre él: `01:14.5`, `1:14` o `74.5`).
3. **Ver → Memorias…** abre un **panel lateral** (izquierda o derecha, botón «A la derecha / A la izquierda») que no tapa la tabla de cues: lista numerada `#1  00:12.345  1er levantada` con la cue y la nota. El campo **«Buscar memoria…»** filtra por nombre, nota o cue. Cada fila tiene **Renombrar** (cambia el nombre en línea: Enter guarda, Esc cancela) y **Quitar** (elimina la memoria; Deshacer la devuelve). Las filas se pueden **reordenar arrastrándolas** a otra posición de su misma cue (una línea ámbar marca dónde caerán); el orden queda guardado con el show.
4. **Un clic en una fila**:
   - cue parada: queda lista, el playhead sobre ella, y **Espacio** la dispara desde ese punto (también si la cue no estaba abierta: se selecciona y el punto queda puesto);
   - cue sonando: salta a ese punto y **sigue sonando** desde ahí.
5. «Desde el inicio» (en la lista de memorias de la cue) quita el punto de arranque.

## Ecualizador (EQ) por cue
Pestaña **EQ** de la cue de audio: active «Ecualizador activo» y arrastre los 4 puntos de la curva (izquierda/derecha = frecuencia, arriba/abajo = ganancia). La **rueda** sobre un punto cambia la Q y el **doble clic** activa o desactiva esa banda. La tabla de abajo permite poner frecuencia, ganancia, Q y tipo de filtro con el teclado. Suena en vivo y se guarda con el show.

## Archivos largos (obras completas)
La forma de onda se calcula en segundo plano, leyendo el archivo de corrido sin cargarlo en memoria, y siempre se reduce a 4096 columnas (cada una conserva el pico más alto y el más bajo): una obra de 1, 2 o 3 horas se dibuja entera. Mientras se calcula, la interfaz sigue respondiendo y la cue se puede disparar normalmente.

**Archivos dañados.** Si una parte del audio no se puede leer (archivo copiado o recuperado con errores), TeatroCue avisa «Audio dañado en …», marca esa zona en rojo sobre la onda y la deja en silencio (nunca suena ruido). Debajo de la onda aparece **Crear copia reparada**: escribe junto al original una copia que llega hasta poco antes del daño, sin recodificar (misma imagen y mismo sonido), y luego **Usar la copia en esta cue** la asigna. El original no se toca.

**Nombre automático.** Al soltar un archivo o elegirlo con «Examinar…», la cue toma el **título** de sus etiquetas (con el artista si lo trae, p. ej. «Entrada de Basilio — Ludwig Minkus») o, si no tiene, el nombre del archivo limpio («04 - Variacion_de_Kitri.wav» → «Variacion de Kitri»). Un nombre escrito a mano no se cambia.

## Ensayo y función en vivo
- **Pre-roll de memorias**: selector **Off | 2s | 4s** en el panel de Memorias y en Time & Loops. Al saltar a una memoria arranca esos segundos antes del marcador (o desde 00:00.00 si no hay tanto), entra con un micro-fade de 50 ms y avisa «Pre-roll: -2.0s -> Memoria [nombre]». **Shift+clic** en una memoria aplica 2 s a ese salto.
- **Velocidades de ensayo** (Time & Loops → Velocidad): 0.90× · 0.95× · 1.00× · 1.05×, en caliente y sin cortar el sonido. Doble clic en cualquiera, o en el número, vuelve a 1.000×.
- **Teclado numérico**: **. N .** (o **. N Enter**) salta a la memoria número N; aparece «Memoria #N». Esc cancela la secuencia y sigue siendo PANIC.
- **Bloqueo de función (Ctrl+L)**: candado en la barra de herramientas y borde ámbar. Bloquea soltar archivos, editar, borrar, duplicar, deshacer, nuevo/abrir y marcar memorias. Siguen vivos: Espacio (GO), Esc (Panic), selección, MASTER, saltos de memoria, Devamp, pads de Carts y Proyección.
- **Retomar al parar (ensayos)**: botón «Retomar al parar» (o Opciones). Encendido, parar con Esc o «Parar todo» deja la música lista en ese punto y el próximo GO la retoma desde ahí, con el pre-roll del panel de Memorias. Sirve una vez (al terminar esa música vuelve a su inicio), no se guarda en el show y se borra al activar el Bloqueo de función; con el Bloqueo puesto, parar no deja ningún punto.
- **Atajos de teclado**: Opciones > Preferencias y atajos de teclado. Clic en la tecla de una orden y pulsar la nueva (Esc cancela); hasta dos teclas por orden; no deja repetir teclas ni usar los números y el punto (son del salto «. N .»). «Restaurar» vuelve a la de fábrica. Se guardan en `preferencias.json` junto a `audio.json`.
- **Silenciar Windows**: botón «Silenciar Windows» / «Windows en silencio» (encendido de fábrica). Silencia la sesión «Sonidos del sistema» del mezclador de Windows en cada salida mientras TeatroCue está abierto; el show no pasa por ahí. Al cerrar se devuelve (también tras un cierre de golpe, al volver a abrir).
- **Importar audio** (Archivo > Importar audio: convertir / copiar de CD…): *Convertir archivos* pasa audio (o el audio de un video) a WAV 16/24 bits, FLAC, MP3 u OGG, con frecuencia, mono/estéreo e «igualar volumen» opcionales; no toca los originales ni pisa archivos; puede agregar el resultado al show como cues. *Copiar de un CD* (solo Windows) lee las pistas del disco, se marcan y nombran, y se copian directo a archivo; un sector ilegible queda en silencio y se avisa. Pendiente de probar con un CD real.
- **Zoom de la onda** (Time & Loops): botones − / + / Todo / Región / Seguir; Ctrl + rueda acerca donde está el puntero, Mayús + rueda desplaza; con zoom aparece una tira de navegación con el archivo entero. Al acercar se calcula el detalle de ese tramo.
- **EQ en vivo**: el EQ se oye mientras se arrastra un punto; la pestaña dice si el cambio ya está aplicado. El PFL también usa el EQ de la cue.
- **Tiempo en la onda**: al hacer clic en la onda de Time & Loops, el marcador ámbar lleva un rombo con su tiempo exacto; el cursor de reproducción muestra el suyo. El rombo ámbar se puede arrastrar y su tiempo lo sigue.
- **Novedades**: la primera vez que se abre una versión nueva aparece el cuadro «Novedades de esta versión» con los cambios; se cierra con «Entendido» o Esc y se vuelve a ver en Ayuda > Novedades de esta versión.
- **Nombres largos**: en STANDBY, en la lista de cues y en la cabecera del Inspector, un nombre que no entra no se parte ni se sale del recuadro: **se desplaza de lado a lado** unas vueltas cuando aparece (por ejemplo al pasar el playhead) y luego queda quieto con «…». Pasando el ratón por encima vuelve a moverse mientras esté encima. La columna Destino (la ruta del archivo) solo se mueve con el ratón encima.
- **Reloj gigante** (a la derecha de STANDBY): tiempo restante de la pista que suena (−mm:ss.d). Ámbar parpadeante bajo 10 s y rojo bajo 5 s.

## Botonera de Carts (efectos rápidos)
**Ver → Carts** o **F3**. 16 pads: clic derecho en uno para elegir el archivo (.wav/.mp3/.ogg), nombre, color, volumen, atajo (1-9, 0, F5-F12) y modo (*Disparar y reiniciar*, *Alternar Play/Stop*, *Pulsar para reproducir*). Suenan al instante sin mover la cue en espera. Se guardan dentro del show.

## Auriculares (PFL / audición)
En **Ver → Configuración de audio…**, campo **Auriculares (PFL)**, hay tres tipos de opción:
- **Sin salida de auriculares.**
- **Canales 3-4 (5-6…) de la placa principal**, solo si esa interfaz tiene 4 o más canales.
- **Otras salidas del sistema** (el jack de la notebook, «Realtek High Definition Audio», una placa USB aparte…): los auriculares suenan por **su propia placa**, aunque la sala sea una interfaz estéreo. Las dos placas tienen relojes distintos y el programa adapta el audio solo (remuestreo y corrección de deriva); si la placa de auriculares corre a otra frecuencia, también.

Después, el botón **PFL** del Inspector escucha la cue —y el **PFL** de cada fila de Memorias escucha desde ese punto— solo en los auriculares: no sale por la sala y el volumen MASTER no lo toca. Si la placa de auriculares no se puede abrir (desconectada, ocupada), la sala sigue funcionando y la audición avisa.

## Autoguardado y exportación
- Cada minuto, si hay cambios sin guardar, se escribe `~nombre.autosave` junto al show. Si el programa se cierra de golpe, al abrir ofrece **recuperar los cambios**. Se borra solo al cerrar bien.
- **Archivo → Exportar Guion Técnico…** crea `cue_sheet.html` (tabla imprimible en blanco y negro con cabecera del Ballet).

## Editar
- **Ctrl+Z** deshacer, **Ctrl+Mayús+Z** rehacer. **Ctrl+D** duplica la cue.
- **+ Audio / Fade / Video / Grupo / Control / OSC Cue** en la barra de herramientas.
- Cue **Control**: Stop (con fundido), Pause, Resume, Devamp, Go to (repetir una sección), Load, Arm, Disarm.
- Cue **Memo/Espera**: no suena, dura el tiempo que indique; con «Auto-continue» encadena la siguiente.
- Cue **OSC**: manda un mensaje a otro equipo (luces, otra PC).
- **Archivo → Abrir reciente**, **Empaquetar show** (copia el show con todos sus audios).

## Audio
**Ver → Configuración de audio…**: interfaz de sonido, canales, buffer y frecuencia. Aplicar reinicia el motor (se corta lo que suena) y recarga el show abierto.

## Control remoto por OSC (opcional, apagado por defecto)
Se activa en la misma ventana de audio (puerto 53000). Desde un celular o tablet (TouchOSC, etc.):
`/go` `/panic` `/stop` `/pause` `/resume` `/next` `/prev` `/master <dB>` y por cue
`/cue/<n>/start|stop|pause|resume|devamp|load|arm|disarm`.
Úselo solo en una red de confianza: quien conozca el puerto puede disparar la función.

## Si algo falla
- Un programa que se cierra solo deja el motivo en `C:\ProgramData\TeatroCue\crash.log`.
- Para actualizar el programa: **cierre TeatroCue** y ejecute `Instalar_TeatroCue.exe` del pendrive (si pregunta por la licencia, responda **n**).
- Si la licencia venció y no hay pendrive: «Reactivación de Emergencia» en la pantalla de bloqueo (copie el código y mándelo al desarrollador).

## Administración de licencias (solo desarrollador)
`teatrocue-admin` detecta el pendrive con `admin/licencias.db` (Linux: unidades bajo `/run/media` y `/media`; Windows: letras `D:` a `Z:`). Arriba a la derecha muestra «Pendrive no detectado» o «Pendrive detectado [ruta]» con el botón **Sincronizar ahora**, y al conectarlo sincroniza solo: fusiona en los dos sentidos las licencias y equipos (sin duplicar; las notas de ambos lados se conservan) y avisa «Sincronización completada: +N equipos nuevos, +M licencias importadas del pendrive».

