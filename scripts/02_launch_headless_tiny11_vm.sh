#!/usr/bin/env bash
# Archivo: scripts/02_launch_headless_tiny11_vm.sh
set -euo pipefail

WORKSPACE_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
ISO_NAME="Tiny11-25H2-26200.8037-English-Pro-2026-08-23.iso"
TINY11_ISO="${TINY11_ISO:-${WORKSPACE_DIR}/${ISO_NAME}}"
[ -f "${TINY11_ISO}" ] || TINY11_ISO="${WORKSPACE_DIR}/qlab-rea/${ISO_NAME}"
VM_DIR="${WORKSPACE_DIR}/.vm-tiny11"
DISK_IMG="${VM_DIR}/tiny11-audio.qcow2"
SEED_ISO="${VM_DIR}/unattend-seed.iso"
OVMF_CODE="/usr/share/edk2/x64/OVMF_CODE.4m.fd"
OVMF_VARS="${VM_DIR}/OVMF_VARS.4m.fd"
SSH_PORT=2222
PID_FILE="${VM_DIR}/qemu.pid"

if [ ! -f "${TINY11_ISO}" ]; then
    echo "ERROR: No se encontró ${TINY11_ISO}" >&2
    exit 1
fi

mkdir -p "${VM_DIR}"

# 1. Crear Sink Virtual PipeWire en CachyOS si no existe (garantiza reloj de audio estable sin bloqueos)
if ! pw-cli list-objects Node | grep -q "qlab_ci_null_sink"; then
    pactl load-module module-null-sink \
        sink_name=qlab_ci_null_sink \
        sink_properties=device.description="QLab_Tiny11_CI_Sink" \
        rate=48000 channels=8 || true
fi

# 2. Preparar clave SSH dedicada para CI desatendido
SSH_KEY="${VM_DIR}/id_ed25519_ci"
if [ ! -f "${SSH_KEY}" ]; then
    ssh-keygen -t ed25519 -N "" -f "${SSH_KEY}" -C "claude-ci@cachyos"
fi
PUBKEY_CONTENT="$(cat "${SSH_KEY}.pub")"

# 3. Generar ISO semilla (Autounattend.xml + bootstrap_tiny11.ps1) si no existe
if [ ! -f "${SEED_ISO}" ]; then
    SEED_BUILD="${VM_DIR}/seed_build"
    mkdir -p "${SEED_BUILD}"
    cat << EOF > "${SEED_BUILD}/bootstrap_tiny11.ps1"
\$ErrorActionPreference = "Continue"
Start-Transcript -Path C:\\Windows\\Temp\\bootstrap.log -Force
# Tiny11 puede carecer de Features-on-Demand: primero la capacidad nativa, y si falla, Win32-OpenSSH de GitHub.
Add-WindowsCapability -Online -Name OpenSSH.Server~~~~0.0.1.0
if (-not (Get-Service sshd -ErrorAction SilentlyContinue)) {
    \$zip = "C:\\Windows\\Temp\\OpenSSH-Win64.zip"
    [Net.ServicePointManager]::SecurityProtocol = [Net.SecurityProtocolType]::Tls12
    Invoke-WebRequest -UseBasicParsing -Uri "https://github.com/PowerShell/Win32-OpenSSH/releases/download/v9.8.1.0p1-Preview/OpenSSH-Win64.zip" -OutFile \$zip
    Expand-Archive -Force \$zip -DestinationPath "C:\\Program Files"
    Rename-Item "C:\\Program Files\\OpenSSH-Win64" "C:\\Program Files\\OpenSSH" -ErrorAction SilentlyContinue
    & powershell -NoProfile -ExecutionPolicy Bypass -File "C:\\Program Files\\OpenSSH\\install-sshd.ps1"
}
Set-Service -Name sshd -StartupType 'Automatic'
Start-Service sshd
New-Item -Force -ItemType Directory -Path "C:\\ProgramData\\ssh" | Out-Null
Set-Content -Path "C:\\ProgramData\\ssh\\administrators_authorized_keys" -Value "${PUBKEY_CONTENT}" -Encoding ASCII
icacls.exe "C:\\ProgramData\\ssh\\administrators_authorized_keys" /inheritance:r /grant "Administrators:F" /grant "SYSTEM:F"
New-NetFirewallRule -Name "OpenSSH-Server-In-TCP" -DisplayName "OpenSSH Server (sshd)" -Enabled True -Direction Inbound -Protocol TCP -Action Allow -LocalPort 22 -ErrorAction SilentlyContinue
New-ItemProperty -Path "HKLM:\\SOFTWARE\\OpenSSH" -Name DefaultShell -Value "C:\\Windows\\System32\\WindowsPowerShell\\v1.0\\powershell.exe" -PropertyType String -Force | Out-Null
powercfg -duplicatescheme e9a42b02-d5df-448d-aa00-03f14749eb61
Set-Service Audiosrv -StartupType Automatic; Start-Service Audiosrv -ErrorAction SilentlyContinue
Stop-Transcript
EOF
    # Autounattend completo: particionado GPT/UEFI + imagen índice 1 + OOBE saltado + cuenta qlabci con autologon.
    cat << 'EOF' > "${SEED_BUILD}/Autounattend.xml"
<?xml version="1.0" encoding="utf-8"?>
<unattend xmlns="urn:schemas-microsoft-com:unattend" xmlns:wcm="http://schemas.microsoft.com/WMIConfig/2002/State">
  <settings pass="windowsPE">
    <component name="Microsoft-Windows-International-Core-WinPE" processorArchitecture="amd64" publicKeyToken="31bf3856ad364e35" language="neutral" versionScope="nonSxS">
      <SetupUILanguage><UILanguage>en-US</UILanguage></SetupUILanguage>
      <InputLocale>en-US</InputLocale><SystemLocale>en-US</SystemLocale><UILanguage>en-US</UILanguage><UserLocale>en-US</UserLocale>
    </component>
    <component name="Microsoft-Windows-Setup" processorArchitecture="amd64" publicKeyToken="31bf3856ad364e35" language="neutral" versionScope="nonSxS">
      <DiskConfiguration>
        <WillShowUI>OnError</WillShowUI>
        <Disk wcm:action="add">
          <DiskID>0</DiskID><WillWipeDisk>true</WillWipeDisk>
          <CreatePartitions>
            <CreatePartition wcm:action="add"><Order>1</Order><Type>EFI</Type><Size>200</Size></CreatePartition>
            <CreatePartition wcm:action="add"><Order>2</Order><Type>MSR</Type><Size>16</Size></CreatePartition>
            <CreatePartition wcm:action="add"><Order>3</Order><Type>Primary</Type><Extend>true</Extend></CreatePartition>
          </CreatePartitions>
          <ModifyPartitions>
            <ModifyPartition wcm:action="add"><Order>1</Order><PartitionID>1</PartitionID><Format>FAT32</Format><Label>System</Label></ModifyPartition>
            <ModifyPartition wcm:action="add"><Order>2</Order><PartitionID>2</PartitionID></ModifyPartition>
            <ModifyPartition wcm:action="add"><Order>3</Order><PartitionID>3</PartitionID><Format>NTFS</Format><Label>Windows</Label><Letter>C</Letter></ModifyPartition>
          </ModifyPartitions>
        </Disk>
      </DiskConfiguration>
      <ImageInstall><OSImage>
        <InstallTo><DiskID>0</DiskID><PartitionID>3</PartitionID></InstallTo>
        <InstallFrom><MetaData wcm:action="add"><Key>/IMAGE/INDEX</Key><Value>1</Value></MetaData></InstallFrom>
        <WillShowUI>OnError</WillShowUI>
      </OSImage></ImageInstall>
      <UserData><AcceptEula>true</AcceptEula><ProductKey><Key/></ProductKey></UserData>
    </component>
  </settings>
  <settings pass="oobeSystem">
    <component name="Microsoft-Windows-Shell-Setup" processorArchitecture="amd64" publicKeyToken="31bf3856ad364e35" language="neutral" versionScope="nonSxS">
      <OOBE>
        <HideEULAPage>true</HideEULAPage>
        <HideOnlineAccountScreens>true</HideOnlineAccountScreens>
        <HideWirelessSetupInOOBE>true</HideWirelessSetupInOOBE>
        <ProtectYourPC>3</ProtectYourPC>
        <SkipMachineOOBE>true</SkipMachineOOBE>
        <SkipUserOOBE>true</SkipUserOOBE>
      </OOBE>
      <UserAccounts>
        <LocalAccounts>
          <LocalAccount wcm:action="add">
            <Name>qlabci</Name><Group>Administrators</Group>
            <Password><Value>qlabci</Value><PlainText>true</PlainText></Password>
          </LocalAccount>
        </LocalAccounts>
      </UserAccounts>
      <AutoLogon>
        <Enabled>true</Enabled><LogonCount>9999</LogonCount><Username>qlabci</Username>
        <Password><Value>qlabci</Value><PlainText>true</PlainText></Password>
      </AutoLogon>
      <FirstLogonCommands>
        <SynchronousCommand wcm:action="add">
          <Order>1</Order>
          <CommandLine>powershell -NoProfile -ExecutionPolicy Bypass -Command "Get-Volume | Where-Object { $_.FileSystemLabel -eq 'UNATTEND' } | ForEach-Object { &amp; ($_.DriveLetter + ':\bootstrap_tiny11.ps1') }"</CommandLine>
        </SynchronousCommand>
      </FirstLogonCommands>
    </component>
  </settings>
</unattend>
EOF
    xorriso -as mkisofs -quiet -J -r -V "UNATTEND" -o "${SEED_ISO}" "${SEED_BUILD}"
fi

# 4. Crear imagen QCOW2 y variables UEFI si es la primera ejecución
if [ ! -f "${DISK_IMG}" ]; then
    qemu-img create -f qcow2 -o preallocation=metadata,lazy_refcounts=on "${DISK_IMG}" 32G
    cp /usr/share/edk2/x64/OVMF_VARS.4m.fd "${OVMF_VARS}"
fi

# 5. Lanzar QEMU/KVM Headless con CPU host passthrough, TSC invariante, hostfwd 127.0.0.1:2222 y Audio PipeWire
if [ -f "${PID_FILE}" ] && kill -0 "$(cat "${PID_FILE}")" 2>/dev/null; then
    echo "[INFO] VM Tiny11 ya está en ejecución (PID $(cat "${PID_FILE}"))."
    exit 0
fi

export PIPEWIRE_LATENCY="128/48000"
export PIPEWIRE_NODE="qlab_ci_null_sink"

# Windows no trae controladores virtio: disco en AHCI/SATA y NIC e1000e (ambos inbox) para que la instalación funcione.
# Se envían pulsaciones por el monitor QEMU durante el arranque para saltar "Press any key to boot from CD".
MON_SOCK="${VM_DIR}/monitor.sock"
rm -f "${MON_SOCK}"
qemu-system-x86_64 \
    -name "qlab-tiny11-audio-ci" \
    -machine q35,accel=kvm,kernel-irqchip=on,hpet=off \
    -cpu host,hv_relaxed,hv_spinlocks=0x1fff,hv_vapic,hv_time,hv_synic,hv_stimer,hv_vpindex,hv_runtime,+invtsc,migratable=no \
    -smp sockets=1,cores=4,threads=1 \
    -m 4096M \
    -drive if=pflash,format=raw,readonly=on,file="${OVMF_CODE}" \
    -drive if=pflash,format=raw,file="${OVMF_VARS}" \
    -device ich9-ahci,id=ahci \
    -drive id=sys,file="${DISK_IMG}",if=none,cache=none,aio=io_uring,discard=unmap,format=qcow2 \
    -device ide-hd,drive=sys,bus=ahci.0,bootindex=2 \
    -drive id=seed,file="${SEED_ISO}",if=none,media=cdrom,readonly=on \
    -device ide-cd,drive=seed,bus=ahci.1 \
    -drive id=win,file="${TINY11_ISO}",if=none,media=cdrom,readonly=on \
    -device ide-cd,drive=win,bus=ahci.2,bootindex=1 \
    -netdev user,id=net0,hostfwd=tcp:127.0.0.1:${SSH_PORT}-:22 \
    -device e1000e,netdev=net0 \
    -audiodev pipewire,id=snd0,out.frequency=48000,out.channels=2,out.latency=2666,in.frequency=48000,in.channels=2,in.latency=2666 \
    -device ich9-intel-hda,id=sound0,msi=on \
    -device hda-duplex,id=sound0-codec0,bus=sound0.0,cad=0,audiodev=snd0 \
    -device qemu-xhci -device usb-tablet \
    -vga std \
    -display none \
    -monitor unix:"${MON_SOCK}",server,nowait \
    -serial file:"${VM_DIR}/serial.log" \
    -daemonize \
    -pidfile "${PID_FILE}"

# Primeros ~40 s: pulsar una tecla cada 2 s para pasar el aviso del cargador UEFI del DVD.
if [ ! -s "${DISK_IMG}.installed" ]; then
    for _ in $(seq 1 20); do
        echo "sendkey ret" | socat - UNIX-CONNECT:"${MON_SOCK}" >/dev/null 2>&1 || true
        sleep 2
    done
fi

echo "[OK] VM Tiny11 Headless iniciada (PID $(cat "${PID_FILE}")). Esperando SSH en 127.0.0.1:${SSH_PORT}..."
for i in $(seq 1 "${SSH_WAIT_ITERS:-120}"); do
    if ssh -i "${SSH_KEY}" -p "${SSH_PORT}" -o StrictHostKeyChecking=no -o ConnectTimeout=2 qlabci@127.0.0.1 "echo READY" 2>/dev/null | grep -q "READY"; then
        echo "[OK] SSH conectado exitosamente a la VM Tiny11."
        exit 0
    fi
    sleep 2
done
echo "ERROR: Timeout esperando SSH en la VM Tiny11." >&2
exit 1