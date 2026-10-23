#!/usr/bin/env bash
# Ensambla el pendrive de instalación de TeatroCue para las PC de cabina (Windows, sin internet).
#
# USO: scripts/build_usb_installer.sh [PUNTO_DE_MONTAJE_DEL_PENDRIVE]
#   Sin argumento se busca un único pendrive montado en /run/media/$USER, /media/$USER o /Volumes y se pide confirmación.
#
# Variables opcionales:
#   TEATROCUE_USB            punto de montaje (equivale al argumento)
#   TEATROCUE_KEYDIR         dónde viven las claves del desarrollador (por defecto ~/.config/teatrocue-keys)
#   TEATROCUE_MASTER_PASSWORD contraseña maestra para crear las claves sin preguntar (mínimo 12 caracteres)
#   FFMPEG_WIN_DIR           carpeta con ffmpeg.exe (y ffprobe.exe) ya descargados: no se descarga nada
#   ASSUME_YES=1             no pedir confirmación al elegir el pendrive
#
# Resultado en el pendrive:
#   Instalar_TeatroCue.exe   master.key.enc   SHA256SUMS.txt   LEEME.txt
#   payload/{teatrocue-ui.exe, teatrocue-daemon.exe, ffmpeg.exe, ffprobe.exe, *.dll, licencias/}
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
TARGET=x86_64-pc-windows-gnu
WIN_DIR="${ROOT}/target/${TARGET}/release"
KEYDIR="${TEATROCUE_KEYDIR:-${HOME}/.config/teatrocue-keys}"
SDK_FFMPEG="${ROOT}/.sdk/ffmpeg-win64"
MINGW_BIN="/usr/x86_64-w64-mingw32/bin"
STAGE="${ROOT}/.usb-build"

die() { echo "error: $*" >&2; exit 1; }
step() { echo; echo "==> $*"; }

for tool in cargo x86_64-w64-mingw32-gcc x86_64-w64-mingw32-objdump sha256sum; do
    command -v "${tool}" >/dev/null || die "falta ${tool} (ejecuta scripts/01_setup_cachyos_toolchain.sh)"
done

# ---------------------------------------------------------------------------------------------
# 0. Destino
# ---------------------------------------------------------------------------------------------
USB="${1:-${TEATROCUE_USB:-}}"
if [ -z "${USB}" ]; then
    mapfile -t CANDIDATES < <(ls -d "/run/media/${USER}"/* "/media/${USER}"/* /Volumes/* 2>/dev/null | grep -vE '/Macintosh HD$' || true)
    case "${#CANDIDATES[@]}" in
        0) die "no se encontró ningún pendrive montado; pásalo como argumento: $0 /ruta/al/pendrive" ;;
        1) USB="${CANDIDATES[0]}" ;;
        *) printf 'Hay varios volúmenes montados; elige uno y pásalo como argumento:\n' >&2; printf '  %s\n' "${CANDIDATES[@]}" >&2; exit 1 ;;
    esac
    if [ "${ASSUME_YES:-0}" != "1" ]; then
        read -r -p "¿Usar ${USB} como pendrive de TeatroCue? Se escribirá en su raíz. [s/N] " ans
        [ "${ans}" = "s" ] || [ "${ans}" = "S" ] || die "cancelado"
    fi
fi
[ -d "${USB}" ] || die "no existe ${USB}"
USB="$(cd "${USB}" && pwd)"
case "${USB}" in
    "/"|"${HOME}"|"${ROOT}"|"${ROOT}"/*) die "${USB} no parece un pendrive; elige su punto de montaje" ;;
esac
[ -w "${USB}" ] || die "no hay permiso de escritura en ${USB}"
echo "Destino: ${USB}"

# ---------------------------------------------------------------------------------------------
# 1. Claves del desarrollador (el par se crea una sola vez y se reutiliza)
# ---------------------------------------------------------------------------------------------
step "Claves Ed25519 en ${KEYDIR}"
(cd "${ROOT}" && cargo build --release -q -p qlab-license --bin teatrocue-keygen)
KEYGEN="${ROOT}/target/release/teatrocue-keygen"
mkdir -p "${KEYDIR}"; chmod 700 "${KEYDIR}"
if [ -f "${KEYDIR}/master.key.enc" ] && [ -f "${KEYDIR}/public.key" ]; then
    echo "ya existen: se reutilizan (no se regeneran: eso invalidaría todas las licencias emitidas)"
else
    "${KEYGEN}" init --dir "${KEYDIR}"
    echo "GUARDA UNA COPIA de ${KEYDIR}/master.key.enc y recuerda la contraseña: sin ellas no podrás activar más PC."
fi
export TEATROCUE_PUBLIC_KEY_FILE="${KEYDIR}/public.key"
# Fecha de compilación: ningún reloj creíble puede ser anterior a ella (defensa contra retroceder la fecha).
export TEATROCUE_BUILD_UNIX="$(date +%s)"

# ---------------------------------------------------------------------------------------------
# 2. Compilación para Windows (clave pública incrustada y licencia exigida)
# ---------------------------------------------------------------------------------------------
step "Compilando para ${TARGET} (release, con licencia exigida)"
(cd "${ROOT}" && cargo build --release --target "${TARGET}" -p qlab-daemon -p qlab-ui -p installer-win \
    --features qlab-daemon/enforce-license,qlab-ui/enforce-license)
for exe in teatrocue-daemon.exe teatrocue-ui.exe Instalar_TeatroCue.exe; do
    [ -f "${WIN_DIR}/${exe}" ] || die "no se generó ${WIN_DIR}/${exe}"
done

# ---------------------------------------------------------------------------------------------
# 3. ffmpeg estático de Windows (vídeo sin conexión)
# ---------------------------------------------------------------------------------------------
step "ffmpeg.exe estático"
FFDIR="${FFMPEG_WIN_DIR:-}"
if [ -z "${FFDIR}" ]; then
    if [ -f "${SDK_FFMPEG}/ffmpeg.exe" ]; then
        FFDIR="${SDK_FFMPEG}"
        echo "reutilizando ${FFDIR}"
    else
        command -v curl >/dev/null || die "falta curl (o define FFMPEG_WIN_DIR con una carpeta que tenga ffmpeg.exe)"
        BASE="https://github.com/BtbN/FFmpeg-Builds/releases/download/latest"
        SUMS="$(curl -fsSL "${BASE}/checksums.sha256")"
        # Build estática LGPL (decodificar H.264/AAC no necesita la GPL); la versión más reciente publicada.
        LINE="$(printf '%s\n' "${SUMS}" | grep -E ' ffmpeg-n[0-9.]+-latest-win64-lgpl-[0-9.]+\.zip$' | sort -k2 -V | tail -1)"
        [ -n "${LINE}" ] || die "no se encontró una build lgpl de ffmpeg en ${BASE}"
        ZIP="${LINE##* }"; WANT="${LINE%% *}"
        mkdir -p "${SDK_FFMPEG}.dl"
        echo "descargando ${ZIP} (~100 MB)…"
        curl -fL --progress-bar -o "${SDK_FFMPEG}.dl/${ZIP}" "${BASE}/${ZIP}"
        GOT="$(sha256sum "${SDK_FFMPEG}.dl/${ZIP}" | cut -d' ' -f1)"
        [ "${GOT}" = "${WANT}" ] || die "la suma SHA-256 de ${ZIP} no coincide con la publicada (${GOT} ≠ ${WANT})"
        echo "SHA-256 verificada: ${GOT}"
        rm -rf "${SDK_FFMPEG}" "${SDK_FFMPEG}.x"; mkdir -p "${SDK_FFMPEG}.x"
        bsdtar -xf "${SDK_FFMPEG}.dl/${ZIP}" -C "${SDK_FFMPEG}.x"
        INNER="$(find "${SDK_FFMPEG}.x" -maxdepth 1 -mindepth 1 -type d | head -1)"
        mkdir -p "${SDK_FFMPEG}/licencias"
        cp "${INNER}/bin/ffmpeg.exe" "${INNER}/bin/ffprobe.exe" "${SDK_FFMPEG}/"
        cp "${INNER}"/LICENSE* "${INNER}"/README* "${SDK_FFMPEG}/licencias/" 2>/dev/null || true
        echo "FFmpeg (build ${ZIP}, https://github.com/BtbN/FFmpeg-Builds): LGPL v2.1+. Código fuente: https://ffmpeg.org" > "${SDK_FFMPEG}/licencias/FFMPEG-ORIGEN.txt"
        rm -rf "${SDK_FFMPEG}.x" "${SDK_FFMPEG}.dl"
        FFDIR="${SDK_FFMPEG}"
    fi
fi
[ -f "${FFDIR}/ffmpeg.exe" ] || die "${FFDIR}/ffmpeg.exe no existe"

# ---------------------------------------------------------------------------------------------
# 4. DLL de runtime que no estén enlazadas estáticamente
# ---------------------------------------------------------------------------------------------
step "Dependencias dinámicas"
rm -rf "${STAGE}"; mkdir -p "${STAGE}/payload"
cp "${WIN_DIR}/teatrocue-ui.exe" "${WIN_DIR}/teatrocue-daemon.exe" "${STAGE}/payload/"
cp "${FFDIR}/ffmpeg.exe" "${STAGE}/payload/"
[ -f "${FFDIR}/ffprobe.exe" ] && cp "${FFDIR}/ffprobe.exe" "${STAGE}/payload/" || echo "AVISO: sin ffprobe.exe (la vista previa de vídeo lo necesita)"
[ -d "${FFDIR}/licencias" ] && cp -r "${FFDIR}/licencias" "${STAGE}/payload/licencias"
cp "${WIN_DIR}/Instalar_TeatroCue.exe" "${STAGE}/"
# DLL que cada ejecutable importa y que el sistema no trae; se copian las que existan en la instalación de MinGW.
declare -A NEED=()
for exe in "${STAGE}/payload/teatrocue-ui.exe" "${STAGE}/payload/teatrocue-daemon.exe" "${STAGE}/Instalar_TeatroCue.exe"; do
    while read -r dll; do NEED["${dll,,}"]="${dll}"; done < <(x86_64-w64-mingw32-objdump -p "${exe}" | sed -n 's/^\s*DLL Name: //p')
done
for key in "${!NEED[@]}"; do
    dll="${NEED[${key}]}"
    if [ -f "${MINGW_BIN}/${dll}" ]; then
        cp "${MINGW_BIN}/${dll}" "${STAGE}/payload/"
        echo "  + ${dll} (de MinGW)"
    fi
done
echo "importaciones de Windows (ya presentes en el sistema): $(printf '%s\n' "${NEED[@]}" | while read -r d; do [ -f "${MINGW_BIN}/${d}" ] || echo -n "${d} "; done)"

# ---------------------------------------------------------------------------------------------
# 5. Clave maestra, instrucciones y sumas; copia al pendrive
# ---------------------------------------------------------------------------------------------
cp "${KEYDIR}/master.key.enc" "${STAGE}/master.key.enc"
cat > "${STAGE}/LEEME.txt" <<'TXT'
TeatroCue - instalación y renovación en la PC de cabina (Windows, sin internet)

INSTALAR
1. Conecte este pendrive a la PC del teatro.
2. Ejecute Instalar_TeatroCue.exe (doble clic).
3. Escriba la contraseña maestra del desarrollador cuando la pida.
4. Elija por cuánto tiempo debe durar la licencia (1, 3, 6 o 12 meses, sin vencimiento u otra cantidad de días)
   y confirme la fecha de vencimiento que se muestra.
5. Al terminar verá "TeatroCue instalado y activado exitosamente para el Ballet Oficial de Córdoba."
   y encontrará el acceso directo "TeatroCue" en el Escritorio. TeatroCue queda en C:\TeatroCue.

RENOVAR LA LICENCIA
Vuelva con el pendrive y ejecute Instalar_TeatroCue.exe otra vez: detecta la instalación, muestra cuánto le queda
y permite elegir una nueva duración (cuenta desde hoy). Responda "s" a "¿Renovar solo la licencia?" para no copiar
de nuevo los programas. Los shows guardados no se tocan.

LA FECHA DE LA PC
TeatroCue recuerda la fecha más alta que vio. Si la fecha de Windows se retrasa, la aplicación se bloquea y muestra
el motivo; corrija la fecha y hora. Si la fecha se adelantó por error y ya se corrigió, ejecute el instalador con la
contraseña maestra y confirme la fecha de hoy (o use: Instalar_TeatroCue.exe --reset-reloj).
TeatroCue solo comprueba la licencia al ARRANCAR: nunca corta el audio durante una función. Avisa desde 14 días antes.

La licencia queda atada a esa PC. Uso exclusivo del Ballet Oficial de Córdoba (Teatro del Libertador San Martín).
Prohibida su copia o distribución sin autorización del desarrollador.

SI LA LICENCIA VENCIO Y NO TIENE EL PENDRIVE: REACTIVACION DE EMERGENCIA
1. Al abrir TeatroCue con la licencia vencida aparece el bloque "Reactivación de Emergencia" con un código de
   solicitud (REQ-XXXX-XXXX-XXXX-XXXX-EE). Tambien esta en Ayuda (F1) > Autorización Legal y Licencia.
2. Pulse "Copiar código" y envielo al desarrollador (por ejemplo por WhatsApp).
3. El desarrollador le devuelve una clave de activación (RESP-...). Pegela en "Pegar clave de activación" y pulse
   "Activar". Si es correcta verá "Licencia extendida hasta: DD/MM/AAAA" y TeatroCue queda habilitado.
   Si hay un error se explica en pantalla y la aplicación sigue abierta: revise que la clave este completa.
Reglas: la clave sirve solo para esa PC, se canjea una vez (una clave vieja o repetida se rechaza) y hay que
usarla dentro de 7 dias desde que se genera. Una reactivacion no reemplaza la renovacion con el pendrive.

PARA EL DESARROLLADOR (en su PC, no en el pendrive)
  installer-tool sign-challenge --code REQ-XXXX-XXXX-XXXX-XXXX-EE --days 15 [--key ruta/master.key.enc]
Pide la contraseña maestra (o TEATROCUE_MASTER_PASSWORD), descifra master.key.enc solo en memoria e imprime:
  CLAVE DE ACTIVACIÓN GENERADA (Válida por N días): [RESP-...]
Máximo 365 dias. Tambien: teatrocue-daemon --license-request / --license-apply RESP-... (sin interfaz).
Al renovar con el pendrive se conserva el contador de reactivaciones, asi que las claves ya usadas no revalidan.
TXT
(cd "${STAGE}" && find . -type f ! -name SHA256SUMS.txt | sort | xargs sha256sum > SHA256SUMS.txt)

step "Copiando al pendrive: ${USB}"
mkdir -p "${USB}/payload"
cp "${STAGE}/Instalar_TeatroCue.exe" "${STAGE}/master.key.enc" "${STAGE}/LEEME.txt" "${USB}/"
rm -rf "${USB}/payload"; cp -r "${STAGE}/payload" "${USB}/payload"
cp "${STAGE}/SHA256SUMS.txt" "${USB}/SHA256SUMS.txt"
sync
(cd "${USB}" && sha256sum -c --quiet SHA256SUMS.txt) || die "la verificación de las copias en el pendrive falló"

step "Pendrive listo"
(cd "${USB}" && find . -maxdepth 2 -not -path './.*' | sort | sed 's/^\.\//  /' | head -40)
echo
echo "Las licencias se emiten en la PC del teatro al ejecutar Instalar_TeatroCue.exe (pide la contraseña maestra)."
echo "Claves del desarrollador: ${KEYDIR}"
