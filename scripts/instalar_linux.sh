#!/usr/bin/env bash
# Instala TeatroCue (interfaz y daemon) y TeatroCue Admin para el usuario actual en Linux: programas, iconos, menú de aplicaciones y
# accesos directos en el Escritorio. Uso: scripts/instalar_linux.sh   (compila antes con: cargo build --release -p qlab-ui -p qlab-daemon -p teatrocue-admin)
set -euo pipefail
cd "$(dirname "$0")/.."
OPT="$HOME/.local/opt/teatrocue"
APPS="$HOME/.local/share/applications"
ICONS="$HOME/.local/share/icons/hicolor/256x256/apps"
DESK="$(xdg-user-dir DESKTOP 2>/dev/null || echo "$HOME/Desktop")"
for b in teatrocue-ui teatrocue-daemon teatrocue-admin; do [ -x "target/release/$b" ] || { echo "falta target/release/$b: compile con cargo build --release -p qlab-ui -p qlab-daemon -p teatrocue-admin"; exit 1; }; done
mkdir -p "$OPT" "$APPS" "$ICONS" "$DESK"
# Se copia a un temporal y se renombra: así también se actualiza un programa que esté abierto (sin «Text file busy»).
for b in teatrocue-ui teatrocue-daemon teatrocue-admin; do
  install -m 755 "target/release/$b" "$OPT/$b.nuevo" && mv -f "$OPT/$b.nuevo" "$OPT/$b"
done
install -m 644 assets/icons/teatrocue.png "$ICONS/teatrocue.png"
install -m 644 assets/icons/teatrocue-admin.png "$ICONS/teatrocue-admin.png"
mk() { # archivo nombre comentario exe icono
  cat > "$1" <<DESKTOP
[Desktop Entry]
Type=Application
Version=1.0
Name=$2
Comment=$3
Exec=$OPT/$4
Icon=$5
Terminal=false
Categories=AudioVideo;Audio;
StartupWMClass=$2
DESKTOP
  chmod +x "$1"
}
mk "$APPS/teatrocue.desktop" "TeatroCue" "Reproductor de cues de audio para teatro y danza" teatrocue-ui teatrocue
mk "$APPS/teatrocue-admin.desktop" "TeatroCue Admin" "Administración de licencias de TeatroCue" teatrocue-admin teatrocue-admin
for f in teatrocue teatrocue-admin; do
  cp "$APPS/$f.desktop" "$DESK/$f.desktop"
  chmod +x "$DESK/$f.desktop"
  gio set "$DESK/$f.desktop" metadata::trusted true 2>/dev/null || true
done
update-desktop-database "$APPS" 2>/dev/null || true
gtk-update-icon-cache -q "$HOME/.local/share/icons/hicolor" 2>/dev/null || true
echo "Listo. Accesos directos en $DESK y en el menú de aplicaciones."
