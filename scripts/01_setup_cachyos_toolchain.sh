#!/usr/bin/env bash
# Fase 1 — Toolchain del anfitrión CachyOS (idempotente). Ejecutar desde cualquier ruta.
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
SDK_DIR="${ROOT}/.sdk"
ASIO_DIR="${SDK_DIR}/asiosdk"

echo "[1/4] Paquetes (pacman)..."
# jack2 y pipewire-jack son excluyentes; se usa jack2 (ya instalado) solo para cabeceras/libjack.
sudo pacman -Syu --needed --noconfirm \
    base-devel clang llvm lld cmake ninja pkgconf rustup \
    mingw-w64-gcc mingw-w64-crt mingw-w64-headers mingw-w64-winpthreads \
    pipewire libpipewire pipewire-alsa pipewire-pulse wireplumber jack2 \
    alsa-lib realtime-privileges rtkit \
    qemu-full libvirt virt-install edk2-ovmf swtpm xorriso dnsmasq \
    openssh sshpass jq curl unzip

echo "[2/4] Rust: toolchain y targets..."
rustup default stable
rustup target add x86_64-unknown-linux-gnu x86_64-pc-windows-gnu

echo "[3/4] Steinberg ASIO SDK -> ${ASIO_DIR}"
# Su licencia es propietaria: no redistribuir ni subir .sdk/ al repositorio.
if [ ! -f "${ASIO_DIR}/common/asio.h" ]; then
    TMP="$(mktemp -d)"
    trap 'rm -rf "${TMP}"' EXIT
    curl -fL "https://www.steinberg.net/asiosdk" -o "${TMP}/asiosdk.zip"
    unzip -q "${TMP}/asiosdk.zip" -d "${TMP}/x"
    mkdir -p "${SDK_DIR}"
    rm -rf "${ASIO_DIR}"
    SRC="$(find "${TMP}/x" -mindepth 1 -maxdepth 1 -type d -iname 'asiosdk*' | head -n1)"
    [ -n "${SRC}" ] || { echo "ERROR: el zip no contiene un directorio asiosdk*" >&2; exit 1; }
    mv "${SRC}" "${ASIO_DIR}"
fi
[ -f "${ASIO_DIR}/common/asio.h" ] || { echo "ERROR: ASIO SDK incompleto (falta common/asio.h)" >&2; exit 1; }

echo "[4/4] Grupos y servicios..."
sudo usermod -aG realtime,kvm,libvirt "${USER}"
sudo systemctl enable --now rtkit-daemon.service libvirtd.service

echo "OK. Cierra sesión y vuelve a entrar para aplicar los grupos (realtime/kvm/libvirt)."
