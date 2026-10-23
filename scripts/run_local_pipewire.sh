#!/usr/bin/env bash
# Lanza teatrocue-daemon (PipeWire, 48 kHz) y teatrocue-ui contra él para probar con audio real.
# Al cerrar la ventana de la UI (o con Ctrl+C / SIGTERM) el daemon se apaga y se limpian socket y memoria compartida.
#
# USO: scripts/run_local_pipewire.sh [opciones] [-- args-para-la-UI]
#   --buffer N      frames de buffer: 128 (por defecto) o 256
#   --device NOMBRE dispositivo de salida (por defecto, el predeterminado de PipeWire)
#   --channels N    canales de salida (2)
#   --show ARCHIVO  show nativo (qlab-rs/v1) que se carga al arrancar
#   --demo          genera y carga un show de demostración (tono + cue con slice infinito para ver VAMP/Devamp)
#   --no-build      no ejecutar cargo build (solo verifica que los binarios existen)
#   --list-devices  lista las salidas de audio y termina
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
BIN_DIR="${ROOT}/target/release"
DAEMON="${BIN_DIR}/teatrocue-daemon"
UI="${BIN_DIR}/teatrocue-ui"
BENCH="${BIN_DIR}/qlab-bench"

SOCKET="${TEATROCUE_SOCKET:-/tmp/teatrocue.sock}"
SHM_NAME="${TEATROCUE_SHM:-teatrocue_rt_telemetry_v1}"          # valor por defecto del daemon; vive en /dev/shm
RATE=48000
BUFFER="${TEATROCUE_BUFFER:-128}"
CHANNELS="${TEATROCUE_CHANNELS:-2}"
DEVICE="${TEATROCUE_DEVICE:-}"
SHOW=""
DEMO=0
BUILD=1
LIST=0
LOG="${XDG_RUNTIME_DIR:-/tmp}/qlab-daemon.log"

die() { echo "error: $*" >&2; exit 1; }

UI_ARGS=()
while [ $# -gt 0 ]; do
    case "$1" in
        --buffer)  BUFFER="${2:?--buffer necesita un valor}"; shift 2 ;;
        --device)  DEVICE="${2:?--device necesita un valor}"; shift 2 ;;
        --channels) CHANNELS="${2:?--channels necesita un valor}"; shift 2 ;;
        --show)    SHOW="${2:?--show necesita un archivo}"; shift 2 ;;
        --demo)    DEMO=1; shift ;;
        --no-build) BUILD=0; shift ;;
        --list-devices) LIST=1; shift ;;
        -h|--help) sed -n '2,12p' "${BASH_SOURCE[0]}"; exit 0 ;;
        --) shift; UI_ARGS=("$@"); break ;;
        *) die "opción desconocida: $1 (usa --help)" ;;
    esac
done
case "${BUFFER}" in 128|256) ;; *) die "--buffer debe ser 128 o 256 (recibido ${BUFFER})" ;; esac

# 1. Compilar (incremental: no hace nada si ya está al día) o verificar los binarios release.
if [ "${BUILD}" -eq 1 ]; then
    echo "[run] cargo build --release (qlab-daemon, qlab-ui, qlab-bench)…"
    (cd "${ROOT}" && cargo build --release -p qlab-daemon -p qlab-ui -p qlab-bench)
fi
for b in "${DAEMON}" "${UI}"; do
    [ -x "${b}" ] || die "falta ${b}; ejecuta sin --no-build"
done

if [ "${LIST}" -eq 1 ]; then
    exec "${DAEMON}" --list-devices
fi

if [ "${DEMO}" -eq 1 ]; then
    [ -x "${BENCH}" ] || die "falta ${BENCH}; ejecuta sin --no-build"
    SHOW="$("${BENCH}" --write-demo-show "${XDG_RUNTIME_DIR:-/tmp}/qlab-demo")"
    echo "[run] show de demostración: ${SHOW}"
fi
[ -z "${SHOW}" ] || [ -f "${SHOW}" ] || die "no existe el show ${SHOW}"

# 2. Un solo daemon a la vez: si ya hay uno vivo no se arranca otro.
# (`pgrep -x` trunca el nombre a 15 caracteres, así que se busca en la línea de órdenes.)
RUNNING_PID="$(pgrep -f -- '(^|/)teatrocue-daemon( |$)' | grep -vx "$$" | head -1 || true)"
if [ -n "${RUNNING_PID}" ]; then
    die "ya hay un teatrocue-daemon en marcha (PID ${RUNNING_PID}); ciérralo (kill ${RUNNING_PID}) o conéctate a él con: ${UI}"
fi
# Restos de una ejecución anterior que murió sin limpiar.
rm -f "${SOCKET}" "/dev/shm/${SHM_NAME}"

DAEMON_PID=""
cleanup() {
    local code=$?
    trap - EXIT INT TERM HUP
    if [ -n "${DAEMON_PID}" ] && kill -0 "${DAEMON_PID}" 2>/dev/null; then
        echo "[run] apagando el daemon (PID ${DAEMON_PID})…"
        kill -TERM "${DAEMON_PID}" 2>/dev/null || true
        for _ in $(seq 1 50); do                      # hasta 5 s para el apagado ordenado
            kill -0 "${DAEMON_PID}" 2>/dev/null || break
            sleep 0.1
        done
        if kill -0 "${DAEMON_PID}" 2>/dev/null; then
            echo "[run] el daemon no respondió a SIGTERM; SIGKILL" >&2
            kill -KILL "${DAEMON_PID}" 2>/dev/null || true
        fi
        wait "${DAEMON_PID}" 2>/dev/null || true
    fi
    # Si el daemon no llegó a limpiar (SIGKILL, fallo), se borran el socket y el segmento compartido.
    rm -f "${SOCKET}" "/dev/shm/${SHM_NAME}"
    echo "[run] limpio: socket y memoria compartida eliminados. Registro del daemon: ${LOG}"
    exit "${code}"
}
trap cleanup EXIT
trap 'exit 130' INT
trap 'exit 143' TERM HUP

# 3. Daemon en segundo plano con el backend nativo de PipeWire.
DAEMON_ARGS=(--socket "${SOCKET}" --backend cpal --host pipewire --rate "${RATE}" --channels "${CHANNELS}" --buffer "${BUFFER}")
[ -z "${DEVICE}" ] || DAEMON_ARGS+=(--device "${DEVICE}")
[ -z "${SHOW}" ] || DAEMON_ARGS+=(--show "${SHOW}")
echo "[run] ${DAEMON} ${DAEMON_ARGS[*]}"
"${DAEMON}" "${DAEMON_ARGS[@]}" >"${LOG}" 2>&1 &
DAEMON_PID=$!

# Esperar a que el socket exista y el daemon siga vivo.
for _ in $(seq 1 100); do
    [ -S "${SOCKET}" ] && break
    kill -0 "${DAEMON_PID}" 2>/dev/null || { echo "--- registro del daemon ---" >&2; cat "${LOG}" >&2; die "el daemon terminó al arrancar"; }
    sleep 0.1
done
[ -S "${SOCKET}" ] || { cat "${LOG}" >&2; die "el daemon no creó ${SOCKET} en 10 s"; }
grep -E "audio:|aviso|IPC en" "${LOG}" | sed 's/^/[daemon] /' || true

# 4. UI en primer plano: al cerrar la ventana vuelve aquí y el trap apaga el daemon.
echo "[run] lanzando la interfaz (cierra la ventana para terminar)…"
set +e
"${UI}" --socket "${SOCKET}" "${UI_ARGS[@]}"
UI_STATUS=$?
set -e
exit "${UI_STATUS}"
