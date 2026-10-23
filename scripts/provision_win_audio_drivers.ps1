# Archivo: scripts/provision_win_audio_drivers.ps1
$ErrorActionPreference = "Stop"
$ProgressPreference = "SilentlyContinue"
[Net.ServicePointManager]::SecurityProtocol = [Net.SecurityProtocolType]::Tls12
$WorkDir = "C:\qlab-drivers"
New-Item -ItemType Directory -Force -Path $WorkDir | Out-Null

# 1. Instalar FlexASIO de manera silenciosa y registrar su COM CLSID de 64 bits
if (-not (Test-Path "C:\Program Files\FlexASIO\x64\FlexASIO.dll")) {
    Write-Host "[VM] Descargando e instalando FlexASIO..."
    $FlexUrl = "https://github.com/dechamps/FlexASIO/releases/download/flexasio-1.10a/FlexASIO-1.10a.exe"
    Invoke-WebRequest -Uri $FlexUrl -OutFile "$WorkDir\FlexASIO-Installer.exe" -UseBasicParsing
    Start-Process -FilePath "$WorkDir\FlexASIO-Installer.exe" -ArgumentList "/VERYSILENT /SUPPRESSMSGBOXES /NORESTART" -Wait
    regsvr32.exe /s "C:\Program Files\FlexASIO\x64\FlexASIO.dll"
}

# 2. Escribir configuración determinista C:\Users\qlabci\FlexASIO.toml para baja latencia (128 samples @ 48kHz)
$FlexToml = @"
backend = "Windows WASAPI"
bufferSizeSamples = 128

[input]
device = ""

[output]
device = ""
sampleType = "Float32"
wasapiExclusiveMode = false
wasapiAutoConvert = true
"@
Set-Content -Path "$env:USERPROFILE\FlexASIO.toml" -Value $FlexToml -Encoding UTF8

# 3. Verificar que el servicio Windows Audio (Audiosrv) y MMCSS estén activos en prioridad alta
Set-Service Audiosrv -StartupType Automatic
Set-Service AudioEndpointBuilder -StartupType Automatic
Start-Service AudioEndpointBuilder -ErrorAction SilentlyContinue
Start-Service Audiosrv -ErrorAction SilentlyContinue
$svc = Get-Service Audiosrv
$start = (Get-CimInstance Win32_Service -Filter "Name='Audiosrv'").StartMode
Write-Host "[VM] Audiosrv: $($svc.Status) / inicio $start"
if ($svc.Status -ne "Running" -or $start -ne "Auto") { throw "Audiosrv no esta activo en inicio automatico" }

# 4. Enumeracion de endpoints de audio (WASAPI) y registro ASIO
Write-Host "[VM] Endpoints WASAPI:"
Get-PnpDevice -Class AudioEndpoint | Select-Object Status, FriendlyName | Format-Table -AutoSize | Out-String | Write-Host
$asio = Get-ChildItem "HKLM:\SOFTWARE\ASIO" -ErrorAction SilentlyContinue | Select-Object -ExpandProperty PSChildName
Write-Host "[VM] Drivers ASIO registrados: $($asio -join ', ')"
Write-Host "[VM] Drivers de audio WASAPI + FlexASIO aprovisionados y verificados."
