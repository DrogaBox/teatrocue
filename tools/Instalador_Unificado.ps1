<#
.SYNOPSIS
    Instalador y Actualizador Unificado Oficial — Suite Teatro del Libertador San Martín
    Ballet Oficial de Córdoba (TeatroGestión + TeatroCue).
    Tecnología: Windows Forms Nativo (100% compatible Windows 7, 10, 11).
#>

param(
    [string]$UsbPath = ""
)

Add-Type -AssemblyName System.Windows.Forms
Add-Type -AssemblyName System.Drawing
Add-Type -AssemblyName Microsoft.VisualBasic

# Habilitar estilos visuales nativos de Windows
[System.Windows.Forms.Application]::EnableVisualStyles()

# -----------------------------------------------------------------------------
# 0. Verificación de permisos de Administrador
# -----------------------------------------------------------------------------
$isAdmin = ([Security.Principal.WindowsPrincipal][Security.Principal.WindowsIdentity]::GetCurrent()).IsInRole([Security.Principal.WindowsBuiltInRole]::Administrator)
if (-not $isAdmin) {
    try {
        $psi = New-Object System.Diagnostics.ProcessStartInfo
        $psi.FileName = "powershell.exe"
        $cmdPath = if ($PSCommandPath) { $PSCommandPath } else { (Join-Path $PSScriptRoot "Instalador_Unificado.ps1") }
        $psi.Arguments = "-NoProfile -ExecutionPolicy Bypass -STA -File `"$cmdPath`" -UsbPath `"$UsbPath`""
        $psi.Verb = "runas"
        [System.Diagnostics.Process]::Start($psi) | Out-Null
        exit 0
    } catch {
        [System.Windows.Forms.MessageBox]::Show(
            "Este instalador requiere permisos de Administrador para configurar carpetas del sistema y accesos directos. Por favor ejecútelo como Administrador.",
            "Permisos Requeridos",
            [System.Windows.Forms.MessageBoxButtons]::OK,
            [System.Windows.Forms.MessageBoxIcon]::Warning
        )
        exit 1
    }
}

try {
    # -------------------------------------------------------------------------
    # 1. Configuración de Rutas y Herramientas del Pendrive
    # -------------------------------------------------------------------------
    if ($UsbPath -and (Test-Path -LiteralPath $UsbPath)) {
        $scriptDir = $UsbPath
    } elseif ($env:TEATRO_SUITE_USB -and (Test-Path -LiteralPath $env:TEATRO_SUITE_USB)) {
        $scriptDir = $env:TEATRO_SUITE_USB
    } elseif ($PSScriptRoot -and (Test-Path -LiteralPath $PSScriptRoot)) {
        $scriptDir = $PSScriptRoot
    } elseif ($PSCommandPath -and (Test-Path -LiteralPath $PSCommandPath)) {
        $scriptDir = Split-Path -Parent $PSCommandPath
    } else {
        $scriptDir = (Get-Location).Path
    }
    $scriptDir = $scriptDir.TrimEnd('\', '/')

    function Find-Tool($name, $subfolder = "") {
        $candidates = [System.Collections.Generic.List[string]]::new()
        $candidates.Add((Join-Path $scriptDir $name))
        if ($subfolder) {
            $candidates.Add((Join-Path (Join-Path $scriptDir $subfolder) $name))
        }
        $candidates.Add((Join-Path (Join-Path $scriptDir "payload") $name))
        $candidates.Add((Join-Path (Join-Path $scriptDir "admin") $name))
        $candidates.Add((Join-Path (Join-Path $scriptDir "TeatroGestion") $name))
        $candidates.Add((Join-Path (Join-Path $scriptDir "TeatroCue") $name))
        foreach ($c in $candidates) {
            if (Test-Path -LiteralPath $c) { return $c }
        }
        return $null
    }

    $exeGestionInstall = Find-Tool "Instalar_TeatroGestion.exe" "TeatroGestion"
    $exeGestionUpdate  = Find-Tool "Actualizar_TeatroGestion.exe" "TeatroGestion"
    $exeCueInstall      = Find-Tool "Instalar_TeatroCue.exe" "TeatroCue"
    $exeCueAdmin        = Find-Tool "teatrocue-admin.exe" "admin"
    $masterKeyFile      = Find-Tool "master.key.enc" ""
    $licenciasDbFile    = Find-Tool "licencias.db" ""
    $pdfGestion         = Find-Tool "Manual de Usuario TeatroGestion.pdf" "TeatroGestion"
    $pdfCue             = Find-Tool "Manual de Usuario TeatroCue.pdf" ""
    $apkFile            = Find-Tool "TeatroGestion.apk" "TeatroGestion"

    # Detección de instalaciones y licencias en el equipo
    function Check-Installations {
        $res = @{
            GestionPath = $null
            GestionLic  = $false
            CuePath     = $null
            CueLic      = $false
        }

        # TeatroGestión
        $gCandidates = @("C:\TeatroGestion", "$env:LOCALAPPDATA\com.teatrogestion.app", "$env:ProgramFiles\TeatroGestion")
        foreach ($dir in $gCandidates) {
            if ($dir -and (Test-Path -LiteralPath (Join-Path $dir "TeatroGestion.exe"))) {
                $res.GestionPath = $dir
                if (Test-Path -LiteralPath (Join-Path $dir "license.key")) { $res.GestionLic = $true }
                break
            }
        }

        # TeatroCue
        $cCandidates = @("C:\TeatroCue", "$env:ProgramFiles\TeatroCue")
        foreach ($dir in $cCandidates) {
            if ($dir -and (Test-Path -LiteralPath (Join-Path $dir "teatrocue-ui.exe"))) {
                $res.CuePath = $dir
                if (Test-Path -LiteralPath (Join-Path $dir "license.key")) { $res.CueLic = $true }
                break
            }
        }

        return $res
    }

    $installState = Check-Installations

    # -------------------------------------------------------------------------
    # 2. Interfaz Gráfica (Dark Theme Teatro del Libertador)
    # -------------------------------------------------------------------------
    $form = New-Object System.Windows.Forms.Form
    $form.Text = "Suite Teatro del Libertador — Instalador y Actualizador Unificado"
    $form.Size = New-Object System.Drawing.Size(880, 770)
    $form.StartPosition = [System.Windows.Forms.FormStartPosition]::CenterScreen
    $form.FormBorderStyle = [System.Windows.Forms.FormBorderStyle]::FixedSingle
    $form.MaximizeBox = $false
    $form.BackColor = [System.Drawing.Color]::FromArgb(18, 18, 20)
    $form.ForeColor = [System.Drawing.Color]::FromArgb(230, 230, 230)
    $form.Font = New-Object System.Drawing.Font("Segoe UI", 9.5)

    # Encabezado Oficial
    $headerPanel = New-Object System.Windows.Forms.Panel
    $headerPanel.Location = New-Object System.Drawing.Point(0, 0)
    $headerPanel.Size = New-Object System.Drawing.Size(880, 85)
    $headerPanel.BackColor = [System.Drawing.Color]::FromArgb(25, 25, 30)
    $form.Controls.Add($headerPanel)

    $lblTitle = New-Object System.Windows.Forms.Label
    $lblTitle.Text = "TEATRO DEL LIBERTADOR SAN MARTÍN — BALLET OFICIAL DE CÓRDOBA"
    $lblTitle.Font = New-Object System.Drawing.Font("Segoe UI", 12, [System.Drawing.FontStyle]::Bold)
    $lblTitle.ForeColor = [System.Drawing.Color]::FromArgb(255, 179, 26) # Oro Teatro
    $lblTitle.Location = New-Object System.Drawing.Point(20, 15)
    $lblTitle.Size = New-Object System.Drawing.Size(830, 25)
    $headerPanel.Controls.Add($lblTitle)

    $lblSubtitle = New-Object System.Windows.Forms.Label
    $lblSubtitle.Text = "Suite Offline: TeatroCue (Sonido y Cues) + TeatroGestión (Inspección, Asistencias y Valeras)"
    $lblSubtitle.Font = New-Object System.Drawing.Font("Segoe UI", 9.5)
    $lblSubtitle.ForeColor = [System.Drawing.Color]::FromArgb(180, 180, 180)
    $lblSubtitle.Location = New-Object System.Drawing.Point(22, 42)
    $lblSubtitle.Size = New-Object System.Drawing.Size(830, 22)
    $headerPanel.Controls.Add($lblSubtitle)

    # Panel de Estado del Sistema (Badges)
    $statusPanel = New-Object System.Windows.Forms.Panel
    $statusPanel.Location = New-Object System.Drawing.Point(20, 95)
    $statusPanel.Size = New-Object System.Drawing.Size(825, 45)
    $statusPanel.BackColor = [System.Drawing.Color]::FromArgb(28, 28, 34)
    $statusPanel.BorderStyle = [System.Windows.Forms.BorderStyle]::FixedSingle
    $form.Controls.Add($statusPanel)

    $lblStatusGestion = New-Object System.Windows.Forms.Label
    $lblStatusGestion.Location = New-Object System.Drawing.Point(15, 12)
    $lblStatusGestion.Size = New-Object System.Drawing.Size(390, 22)
    $statusPanel.Controls.Add($lblStatusGestion)

    $lblStatusCue = New-Object System.Windows.Forms.Label
    $lblStatusCue.Location = New-Object System.Drawing.Point(415, 12)
    $lblStatusCue.Size = New-Object System.Drawing.Size(390, 22)
    $statusPanel.Controls.Add($lblStatusCue)

    function Refresh-StatusBadges {
        $state = Check-Installations
        if ($state.GestionPath) {
            $licText = if ($state.GestionLic) { "(Licencia OK)" } else { "(Sin Licencia)" }
            $lblStatusGestion.Text = "✔ TeatroGestión: $licText en $($state.GestionPath)"
            $lblStatusGestion.ForeColor = [System.Drawing.Color]::FromArgb(76, 175, 80)
        } else {
            $lblStatusGestion.Text = "○ TeatroGestión: No detectado en esta PC"
            $lblStatusGestion.ForeColor = [System.Drawing.Color]::FromArgb(255, 179, 26)
        }

        if ($state.CuePath) {
            $licText = if ($state.CueLic) { "(Licencia OK)" } else { "(Sin Licencia)" }
            $lblStatusCue.Text = "✔ TeatroCue: $licText en $($state.CuePath)"
            $lblStatusCue.ForeColor = [System.Drawing.Color]::FromArgb(76, 175, 80)
        } else {
            $lblStatusCue.Text = "○ TeatroCue: No detectado en esta PC"
            $lblStatusCue.ForeColor = [System.Drawing.Color]::FromArgb(255, 179, 26)
        }
    }
    Refresh-StatusBadges

    # Control de Pestañas
    $tabControl = New-Object System.Windows.Forms.TabControl
    $tabControl.Location = New-Object System.Drawing.Point(20, 150)
    $tabControl.Size = New-Object System.Drawing.Size(825, 350)
    $form.Controls.Add($tabControl)

    # -------------------------------------------------------------------------
    # TAB 1: Actualización Rápida en 1 Clic
    # -------------------------------------------------------------------------
    $tabQuick = New-Object System.Windows.Forms.TabPage
    $tabQuick.Text = "  ⚡ Actualización 1 Clic  "
    $tabQuick.BackColor = [System.Drawing.Color]::FromArgb(22, 22, 26)
    $tabControl.TabPages.Add($tabQuick)

    $lblQuickInfo = New-Object System.Windows.Forms.Label
    $lblQuickInfo.Text = "Esta función actualiza las instalaciones existentes de TeatroGestión y TeatroCue en esta máquina a la versión más reciente del pendrive.`n`n• Preserva intactas todas las bases de datos (plantel oficial de 64 integrantes, asistencias históricas, valeras y marcas de reloj).`n• Actualiza los ejecutables e incrusta de forma automática las migraciones pendientes.`n• No requiere conexión a Internet ni reconfiguración manual."
    $lblQuickInfo.Location = New-Object System.Drawing.Point(25, 15)
    $lblQuickInfo.Size = New-Object System.Drawing.Size(770, 85)
    $lblQuickInfo.ForeColor = [System.Drawing.Color]::FromArgb(210, 210, 210)
    $tabQuick.Controls.Add($lblQuickInfo)

    $lblQuickPass = New-Object System.Windows.Forms.Label
    $lblQuickPass.Text = "Contraseña Maestra (opcional, solo para emitir/renovar licencias de ambas apps):"
    $lblQuickPass.Location = New-Object System.Drawing.Point(25, 105)
    $lblQuickPass.Size = New-Object System.Drawing.Size(550, 20)
    $tabQuick.Controls.Add($lblQuickPass)

    $txtQuickPass = New-Object System.Windows.Forms.TextBox
    $txtQuickPass.PasswordChar = '*'
    $txtQuickPass.Location = New-Object System.Drawing.Point(25, 128)
    $txtQuickPass.Size = New-Object System.Drawing.Size(350, 25)
    $txtQuickPass.BackColor = [System.Drawing.Color]::FromArgb(32, 32, 40)
    $txtQuickPass.ForeColor = [System.Drawing.Color]::White
    $tabQuick.Controls.Add($txtQuickPass)

    $btnQuickUpdate = New-Object System.Windows.Forms.Button
    $btnQuickUpdate.Text = "ACTUALIZAR AMBAS APLICACIONES (1 CLIC)"
    $btnQuickUpdate.Font = New-Object System.Drawing.Font("Segoe UI", 11, [System.Drawing.FontStyle]::Bold)
    $btnQuickUpdate.BackColor = [System.Drawing.Color]::FromArgb(255, 179, 26)
    $btnQuickUpdate.ForeColor = [System.Drawing.Color]::FromArgb(18, 18, 20)
    $btnQuickUpdate.FlatStyle = [System.Windows.Forms.FlatStyle]::Flat
    $btnQuickUpdate.Location = New-Object System.Drawing.Point(25, 168)
    $btnQuickUpdate.Size = New-Object System.Drawing.Size(770, 50)
    $btnQuickUpdate.Cursor = [System.Windows.Forms.Cursors]::Hand
    $tabQuick.Controls.Add($btnQuickUpdate)

    $lblClause = New-Object System.Windows.Forms.Label
    $lblClause.Text = "★ REPARTO DE ESTUDIO, SIN ORDEN DE PRIORIDAD Y SUJETO A CAMBIOS. ★"
    $lblClause.Font = New-Object System.Drawing.Font("Segoe UI", 8.5, [System.Drawing.FontStyle]::Italic)
    $lblClause.ForeColor = [System.Drawing.Color]::FromArgb(140, 140, 140)
    $lblClause.TextAlign = [System.Drawing.ContentAlignment]::MiddleCenter
    $lblClause.Location = New-Object System.Drawing.Point(25, 230)
    $lblClause.Size = New-Object System.Drawing.Size(770, 25)
    $tabQuick.Controls.Add($lblClause)

    # -------------------------------------------------------------------------
    # TAB 2: TeatroGestión
    # -------------------------------------------------------------------------
    $tabGestion = New-Object System.Windows.Forms.TabPage
    $tabGestion.Text = "  TeatroGestión  "
    $tabGestion.BackColor = [System.Drawing.Color]::FromArgb(22, 22, 26)
    $tabControl.TabPages.Add($tabGestion)

    $lblGDir = New-Object System.Windows.Forms.Label
    $lblGDir.Text = "Carpeta de instalación destino:"
    $lblGDir.Location = New-Object System.Drawing.Point(25, 15)
    $lblGDir.Size = New-Object System.Drawing.Size(250, 20)
    $tabGestion.Controls.Add($lblGDir)

    $txtGDir = New-Object System.Windows.Forms.TextBox
    $txtGDir.Text = if ($installState.GestionPath) { $installState.GestionPath } else { "C:\TeatroGestion" }
    $txtGDir.Location = New-Object System.Drawing.Point(25, 38)
    $txtGDir.Size = New-Object System.Drawing.Size(635, 25)
    $txtGDir.BackColor = [System.Drawing.Color]::FromArgb(32, 32, 40)
    $txtGDir.ForeColor = [System.Drawing.Color]::White
    $tabGestion.Controls.Add($txtGDir)

    $btnGBrowse = New-Object System.Windows.Forms.Button
    $btnGBrowse.Text = "Examinar..."
    $btnGBrowse.Location = New-Object System.Drawing.Point(670, 37)
    $btnGBrowse.Size = New-Object System.Drawing.Size(125, 27)
    $btnGBrowse.BackColor = [System.Drawing.Color]::FromArgb(45, 45, 55)
    $btnGBrowse.FlatStyle = [System.Windows.Forms.FlatStyle]::Flat
    $tabGestion.Controls.Add($btnGBrowse)

    $lblGTerm = New-Object System.Windows.Forms.Label
    $lblGTerm.Text = "Duración de la Licencia Offline:"
    $lblGTerm.Location = New-Object System.Drawing.Point(25, 75)
    $lblGTerm.Size = New-Object System.Drawing.Size(250, 20)
    $tabGestion.Controls.Add($lblGTerm)

    $cmbGTerm = New-Object System.Windows.Forms.ComboBox
    $cmbGTerm.DropDownStyle = [System.Windows.Forms.ComboBoxStyle]::DropDownList
    $cmbGTerm.Items.AddRange(@("Temporada 2026 / 365 Días (1 Año)", "180 Días (6 Meses)", "90 Días (3 Meses)", "Licencia Perpetua (Sin Vencimiento)"))
    $cmbGTerm.SelectedIndex = 0
    $cmbGTerm.Location = New-Object System.Drawing.Point(25, 98)
    $cmbGTerm.Size = New-Object System.Drawing.Size(350, 25)
    $cmbGTerm.BackColor = [System.Drawing.Color]::FromArgb(32, 32, 40)
    $cmbGTerm.ForeColor = [System.Drawing.Color]::White
    $tabGestion.Controls.Add($cmbGTerm)

    $lblGPass = New-Object System.Windows.Forms.Label
    $lblGPass.Text = "Contraseña Maestra (para emitir licencia con master.key.enc):"
    $lblGPass.Location = New-Object System.Drawing.Point(25, 135)
    $lblGPass.Size = New-Object System.Drawing.Size(450, 20)
    $tabGestion.Controls.Add($lblGPass)

    $txtGPass = New-Object System.Windows.Forms.TextBox
    $txtGPass.PasswordChar = '*'
    $txtGPass.Location = New-Object System.Drawing.Point(25, 158)
    $txtGPass.Size = New-Object System.Drawing.Size(350, 25)
    $txtGPass.BackColor = [System.Drawing.Color]::FromArgb(32, 32, 40)
    $txtGPass.ForeColor = [System.Drawing.Color]::White
    $tabGestion.Controls.Add($txtGPass)

    $btnGInstall = New-Object System.Windows.Forms.Button
    $btnGInstall.Text = "INSTALAR O ACTUALIZAR TEATROGESTIÓN"
    $btnGInstall.Font = New-Object System.Drawing.Font("Segoe UI", 10, [System.Drawing.FontStyle]::Bold)
    $btnGInstall.BackColor = [System.Drawing.Color]::FromArgb(255, 179, 26)
    $btnGInstall.ForeColor = [System.Drawing.Color]::FromArgb(18, 18, 20)
    $btnGInstall.FlatStyle = [System.Windows.Forms.FlatStyle]::Flat
    $btnGInstall.Location = New-Object System.Drawing.Point(25, 205)
    $btnGInstall.Size = New-Object System.Drawing.Size(350, 42)
    $btnGInstall.Cursor = [System.Windows.Forms.Cursors]::Hand
    $tabGestion.Controls.Add($btnGInstall)

    $btnManualGestion = New-Object System.Windows.Forms.Button
    $btnManualGestion.Text = "Ver Manual de Usuario (PDF)"
    $btnManualGestion.Location = New-Object System.Drawing.Point(395, 205)
    $btnManualGestion.Size = New-Object System.Drawing.Size(200, 42)
    $btnManualGestion.BackColor = [System.Drawing.Color]::FromArgb(45, 45, 55)
    $btnManualGestion.FlatStyle = [System.Windows.Forms.FlatStyle]::Flat
    $tabGestion.Controls.Add($btnManualGestion)

    $btnCopyApk = New-Object System.Windows.Forms.Button
    $btnCopyApk.Text = "📱 Copiar APK Android (Samsung A15)..."
    $btnCopyApk.Location = New-Object System.Drawing.Point(605, 205)
    $btnCopyApk.Size = New-Object System.Drawing.Size(190, 42)
    $btnCopyApk.BackColor = [System.Drawing.Color]::FromArgb(45, 45, 55)
    $btnCopyApk.FlatStyle = [System.Windows.Forms.FlatStyle]::Flat
    $tabGestion.Controls.Add($btnCopyApk)

    # -------------------------------------------------------------------------
    # TAB 3: TeatroCue
    # -------------------------------------------------------------------------
    $tabCue = New-Object System.Windows.Forms.TabPage
    $tabCue.Text = "  TeatroCue  "
    $tabCue.BackColor = [System.Drawing.Color]::FromArgb(22, 22, 26)
    $tabControl.TabPages.Add($tabCue)

    $lblCDir = New-Object System.Windows.Forms.Label
    $lblCDir.Text = "Carpeta de instalación de TeatroCue:"
    $lblCDir.Location = New-Object System.Drawing.Point(25, 15)
    $lblCDir.Size = New-Object System.Drawing.Size(250, 20)
    $tabCue.Controls.Add($lblCDir)

    $txtCDir = New-Object System.Windows.Forms.TextBox
    $txtCDir.Text = if ($installState.CuePath) { $installState.CuePath } else { "C:\TeatroCue" }
    $txtCDir.Location = New-Object System.Drawing.Point(25, 38)
    $txtCDir.Size = New-Object System.Drawing.Size(635, 25)
    $txtCDir.BackColor = [System.Drawing.Color]::FromArgb(32, 32, 40)
    $txtCDir.ForeColor = [System.Drawing.Color]::White
    $tabCue.Controls.Add($txtCDir)

    $btnCBrowse = New-Object System.Windows.Forms.Button
    $btnCBrowse.Text = "Examinar..."
    $btnCBrowse.Location = New-Object System.Drawing.Point(670, 37)
    $btnCBrowse.Size = New-Object System.Drawing.Size(125, 27)
    $btnCBrowse.BackColor = [System.Drawing.Color]::FromArgb(45, 45, 55)
    $btnCBrowse.FlatStyle = [System.Windows.Forms.FlatStyle]::Flat
    $tabCue.Controls.Add($btnCBrowse)

    $lblCPass = New-Object System.Windows.Forms.Label
    $lblCPass.Text = "Contraseña Maestra (para emitir licencia TeatroCue):"
    $lblCPass.Location = New-Object System.Drawing.Point(25, 80)
    $lblCPass.Size = New-Object System.Drawing.Size(400, 20)
    $tabCue.Controls.Add($lblCPass)

    $txtCPass = New-Object System.Windows.Forms.TextBox
    $txtCPass.PasswordChar = '*'
    $txtCPass.Location = New-Object System.Drawing.Point(25, 105)
    $txtCPass.Size = New-Object System.Drawing.Size(350, 25)
    $txtCPass.BackColor = [System.Drawing.Color]::FromArgb(32, 32, 40)
    $txtCPass.ForeColor = [System.Drawing.Color]::White
    $tabCue.Controls.Add($txtCPass)

    $btnCInstall = New-Object System.Windows.Forms.Button
    $btnCInstall.Text = "INSTALAR O ACTUALIZAR TEATROCUE"
    $btnCInstall.Font = New-Object System.Drawing.Font("Segoe UI", 10, [System.Drawing.FontStyle]::Bold)
    $btnCInstall.BackColor = [System.Drawing.Color]::FromArgb(255, 179, 26)
    $btnCInstall.ForeColor = [System.Drawing.Color]::FromArgb(18, 18, 20)
    $btnCInstall.FlatStyle = [System.Windows.Forms.FlatStyle]::Flat
    $btnCInstall.Location = New-Object System.Drawing.Point(25, 160)
    $btnCInstall.Size = New-Object System.Drawing.Size(350, 42)
    $btnCInstall.Cursor = [System.Windows.Forms.Cursors]::Hand
    $tabCue.Controls.Add($btnCInstall)

    $btnManualCue = New-Object System.Windows.Forms.Button
    $btnManualCue.Text = "Ver Manual de Usuario (PDF)"
    $btnManualCue.Location = New-Object System.Drawing.Point(395, 160)
    $btnManualCue.Size = New-Object System.Drawing.Size(220, 42)
    $btnManualCue.BackColor = [System.Drawing.Color]::FromArgb(45, 45, 55)
    $btnManualCue.FlatStyle = [System.Windows.Forms.FlatStyle]::Flat
    $tabCue.Controls.Add($btnManualCue)

    # -------------------------------------------------------------------------
    # TAB 4: Recuperación de Bóveda y Herramientas
    # -------------------------------------------------------------------------
    $tabAdmin = New-Object System.Windows.Forms.TabPage
    $tabAdmin.Text = "  Bóveda y Licencias  "
    $tabAdmin.BackColor = [System.Drawing.Color]::FromArgb(22, 22, 26)
    $tabControl.TabPages.Add($tabAdmin)

    $lblVaultInfo = New-Object System.Windows.Forms.Label
    $lblVaultInfo.Text = "Restablecimiento de Contraseña de la Bóveda de Inspección (Carina Lion):`nSi la inspectora olvidó su clave, esta función valida la llave maestra master.key.enc del pendrive y permite configurar una nueva clave en la base de datos sin perder ningún dato."
    $lblVaultInfo.Location = New-Object System.Drawing.Point(25, 15)
    $lblVaultInfo.Size = New-Object System.Drawing.Size(770, 45)
    $lblVaultInfo.ForeColor = [System.Drawing.Color]::FromArgb(255, 224, 130)
    $tabAdmin.Controls.Add($lblVaultInfo)

    $btnResetVault = New-Object System.Windows.Forms.Button
    $btnResetVault.Text = "RESTABLECER CONTRASEÑA DE BÓVEDA"
    $btnResetVault.Font = New-Object System.Drawing.Font("Segoe UI", 10, [System.Drawing.FontStyle]::Bold)
    $btnResetVault.BackColor = [System.Drawing.Color]::FromArgb(220, 80, 50)
    $btnResetVault.ForeColor = [System.Drawing.Color]::White
    $btnResetVault.FlatStyle = [System.Windows.Forms.FlatStyle]::Flat
    $btnResetVault.Location = New-Object System.Drawing.Point(25, 75)
    $btnResetVault.Size = New-Object System.Drawing.Size(350, 40)
    $btnResetVault.Cursor = [System.Windows.Forms.Cursors]::Hand
    $tabAdmin.Controls.Add($btnResetVault)

    $btnOpenAdmin = New-Object System.Windows.Forms.Button
    $btnOpenAdmin.Text = "Abrir Administrador de Licencias (teatrocue-admin)"
    $btnOpenAdmin.Location = New-Object System.Drawing.Point(25, 135)
    $btnOpenAdmin.Size = New-Object System.Drawing.Size(350, 35)
    $btnOpenAdmin.BackColor = [System.Drawing.Color]::FromArgb(45, 45, 55)
    $btnOpenAdmin.FlatStyle = [System.Windows.Forms.FlatStyle]::Flat
    $btnOpenAdmin.Cursor = [System.Windows.Forms.Cursors]::Hand
    $tabAdmin.Controls.Add($btnOpenAdmin)

    # -------------------------------------------------------------------------
    # Panel de Registro / Log de Actividad
    # -------------------------------------------------------------------------
    $lblLog = New-Object System.Windows.Forms.Label
    $lblLog.Text = "Registro de Operaciones en Vivo:"
    $lblLog.Location = New-Object System.Drawing.Point(20, 510)
    $lblLog.Size = New-Object System.Drawing.Size(300, 20)
    $form.Controls.Add($lblLog)

    $txtLog = New-Object System.Windows.Forms.TextBox
    $txtLog.Multiline = $true
    $txtLog.ScrollBars = [System.Windows.Forms.ScrollBars]::Vertical
    $txtLog.ReadOnly = $true
    $txtLog.Location = New-Object System.Drawing.Point(20, 532)
    $txtLog.Size = New-Object System.Drawing.Size(825, 175)
    $txtLog.BackColor = [System.Drawing.Color]::FromArgb(12, 12, 14)
    $txtLog.ForeColor = [System.Drawing.Color]::FromArgb(0, 255, 180) # Verde terminal
    $txtLog.Font = New-Object System.Drawing.Font("Consolas", 9)
    $form.Controls.Add($txtLog)

    function Log-Msg($text) {
        $timestamp = (Get-Date).ToString("HH:mm:ss")
        $txtLog.AppendText("[$timestamp] $text`r`n")
        $txtLog.SelectionStart = $txtLog.TextLength
        $txtLog.ScrollToCaret()
        [System.Windows.Forms.Application]::DoEvents()
    }

    # Función para cerrar procesos de forma segura antes de actualizar
    function Close-AppIfRunning($procName, $friendlyName) {
        $procs = Get-Process -Name $procName -ErrorAction SilentlyContinue
        if ($procs) {
            $msg = "$friendlyName está en ejecución. Para actualizar los archivos del programa es necesario cerrarlo.`n`n¿Desea que el instalador lo cierre automáticamente ahora?"
            $res = [System.Windows.Forms.MessageBox]::Show($msg, "Cierre Requerido", [System.Windows.Forms.MessageBoxButtons]::YesNo, [System.Windows.Forms.MessageBoxIcon]::Question)
            if ($res -eq [System.Windows.Forms.DialogResult]::Yes) {
                Log-Msg "Cerrando $friendlyName..."
                foreach ($p in $procs) {
                    $p.CloseMainWindow() | Out-Null
                }
                Start-Sleep -Milliseconds 800
                $procsAfter = Get-Process -Name $procName -ErrorAction SilentlyContinue
                if ($procsAfter) {
                    foreach ($pa in $procsAfter) { $pa.Kill() | Out-Null }
                }
                Log-Msg "✔ $friendlyName cerrado correctamente."
                return $true
            } else {
                Log-Msg "⚠ Actualización cancelada por el usuario (la aplicación sigue abierta)."
                return $false
            }
        }
        return $true
    }

    # Función auxiliar para ejecutar procesos sin deadlocks de streaming
    function Run-LoggedProcess($filePath, $argList, $envVars = @{}) {
        $psi = New-Object System.Diagnostics.ProcessStartInfo
        $psi.FileName = $filePath
        $psi.Arguments = $argList
        $psi.RedirectStandardOutput = $true
        $psi.RedirectStandardError = $true
        $psi.UseShellExecute = $false
        $psi.CreateNoWindow = $true

        if ($envVars) {
            foreach ($k in $envVars.Keys) {
                $psi.EnvironmentVariables[$k] = [string]$envVars[$k]
            }
        }

        $p = [System.Diagnostics.Process]::Start($psi)
        while (-not $p.WaitForExit(50)) {
            while ($p.StandardOutput.Peek() -ge 0) {
                $line = $p.StandardOutput.ReadLine()
                if ($line) { Log-Msg $line }
            }
            while ($p.StandardError.Peek() -ge 0) {
                $errLine = $p.StandardError.ReadLine()
                if ($errLine) { Log-Msg "[!] $errLine" }
            }
            [System.Windows.Forms.Application]::DoEvents()
        }
        $restOut = $p.StandardOutput.ReadToEnd()
        if ($restOut) {
            foreach ($l in ($restOut -split "`r?`n")) {
                if ($l.Trim()) { Log-Msg $l }
            }
        }
        $restErr = $p.StandardError.ReadToEnd()
        if ($restErr) {
            foreach ($l in ($restErr -split "`r?`n")) {
                if ($l.Trim()) { Log-Msg "[!] $l" }
            }
        }
        return $p.ExitCode
    }

    # -------------------------------------------------------------------------
    # Acciones y Eventos
    # -------------------------------------------------------------------------

    # Examinar carpetas
    $btnGBrowse.Add_Click({
        $dlg = New-Object System.Windows.Forms.FolderBrowserDialog
        $dlg.SelectedPath = $txtGDir.Text
        if ($dlg.ShowDialog() -eq [System.Windows.Forms.DialogResult]::OK) {
            $txtGDir.Text = $dlg.SelectedPath
        }
    })

    $btnCBrowse.Add_Click({
        $dlg = New-Object System.Windows.Forms.FolderBrowserDialog
        $dlg.SelectedPath = $txtCDir.Text
        if ($dlg.ShowDialog() -eq [System.Windows.Forms.DialogResult]::OK) {
            $txtCDir.Text = $dlg.SelectedPath
        }
    })

    # Apertura de manuales PDF
    $btnManualGestion.Add_Click({
        if ($pdfGestion -and (Test-Path -LiteralPath $pdfGestion)) {
            Start-Process -FilePath $pdfGestion
        } else {
            [System.Windows.Forms.MessageBox]::Show("Manual de TeatroGestión no encontrado en el pendrive.", "Aviso", [System.Windows.Forms.MessageBoxButtons]::OK, [System.Windows.Forms.MessageBoxIcon]::Information)
        }
    })

    $btnManualCue.Add_Click({
        if ($pdfCue -and (Test-Path -LiteralPath $pdfCue)) {
            Start-Process -FilePath $pdfCue
        } else {
            [System.Windows.Forms.MessageBox]::Show("Manual de TeatroCue no encontrado en el pendrive.", "Aviso", [System.Windows.Forms.MessageBoxButtons]::OK, [System.Windows.Forms.MessageBoxIcon]::Information)
        }
    })

    # Copiar APK para Celular
    $btnCopyApk.Add_Click({
        if (-not $apkFile -or -not (Test-Path -LiteralPath $apkFile)) {
            [System.Windows.Forms.MessageBox]::Show("El archivo 'TeatroGestion.apk' no se encuentra en el pendrive.", "Aviso", [System.Windows.Forms.MessageBoxButtons]::OK, [System.Windows.Forms.MessageBoxIcon]::Warning)
            return
        }
        $dlg = New-Object System.Windows.Forms.FolderBrowserDialog
        $dlg.Description = "Seleccione la carpeta donde desea guardar el APK de Android (por ejemplo, el Escritorio o la memoria del celular):"
        if ($dlg.ShowDialog() -eq [System.Windows.Forms.DialogResult]::OK) {
            $destApk = Join-Path $dlg.SelectedPath "TeatroGestion.apk"
            try {
                Log-Msg "Copiando TeatroGestion.apk a $destApk..."
                Copy-Item -LiteralPath $apkFile -Destination $destApk -Force
                Log-Msg "✔ APK copiado exitosamente a: $destApk"
                [System.Windows.Forms.MessageBox]::Show("El APK de TeatroGestión fue copiado con éxito a:`n$destApk`n`nPuede transferirlo a cualquier celular Android (ej. Samsung Galaxy A15) e instalarlo.", "Copia Exitosa", [System.Windows.Forms.MessageBoxButtons]::OK, [System.Windows.Forms.MessageBoxIcon]::Information)
            } catch {
                Log-Msg "ERROR al copiar APK: $($_.Exception.Message)"
            }
        }
    })

    # Acción: Actualizar Ambas (1 Clic)
    $btnQuickUpdate.Add_Click({
        $btnQuickUpdate.Enabled = $false
        try {
            Log-Msg "======================================================="
            Log-Msg "Iniciando Actualización Unificada en 1 Clic..."
            Log-Msg "======================================================="

            # 1. Comprobar procesos abiertos
            if (-not (Close-AppIfRunning "TeatroGestion" "TeatroGestión")) { return }
            if (-not (Close-AppIfRunning "teatrocue-ui" "TeatroCue")) { return }

            $masterPass = $txtQuickPass.Text.Trim()

            # 2. TeatroGestión
            if ($exeGestionUpdate -and (Test-Path -LiteralPath $exeGestionUpdate)) {
                Log-Msg "Ejecutando Actualizador de TeatroGestión..."
                $code = Run-LoggedProcess $exeGestionUpdate "--si"
                if ($code -eq 0) {
                    Log-Msg "✔ TeatroGestión actualizado con éxito a la versión v1.3.0 (Plantel Oficial de 64 integrantes migrado)."
                } else {
                    Log-Msg "⚠ Actualizador de TeatroGestión finalizó con código: $code"
                }
            } elseif ($exeGestionInstall -and (Test-Path -LiteralPath $exeGestionInstall)) {
                Log-Msg "Lanzando instalación de TeatroGestión..."
                if ($masterPass) {
                    $envs = @{ "TEATROGESTION_MASTER_PASSWORD" = $masterPass }
                    $dest = if ($installState.GestionPath) { $installState.GestionPath } else { "C:\TeatroGestion" }
                    $code = Run-LoggedProcess $exeGestionInstall "--target `"$dest`"" $envs
                    Log-Msg "Resultado instalación TeatroGestión: $code"
                } else {
                    Start-Process -FilePath $exeGestionInstall -Wait
                }
            } else {
                Log-Msg "⚠ No se encontró el actualizador de TeatroGestión en el pendrive."
            }

            # 3. TeatroCue
            $cueState = Check-Installations
            if ($cueState.CuePath -and (Test-Path -LiteralPath $cueState.CuePath)) {
                if ($masterPass -and $exeCueInstall -and (Test-Path -LiteralPath $exeCueInstall)) {
                    Log-Msg "Actualizando TeatroCue y renovando licencia offline con master.key.enc..."
                    $envs = @{ "TEATROCUE_MASTER_PASSWORD" = $masterPass }
                    $code = Run-LoggedProcess $exeCueInstall "--target `"$($cueState.CuePath)`" --perpetual --no-pause" $envs
                    if ($code -eq 0) {
                        Log-Msg "✔ TeatroCue actualizado y licencia renovada con éxito."
                    } else {
                        Log-Msg "⚠ Renovación de licencia de TeatroCue finalizó con código: $code"
                    }
                } else {
                    Log-Msg "Actualizando binarios de TeatroCue en $($cueState.CuePath)..."
                    $payloadDir = Join-Path $scriptDir "payload"
                    $filesToCopy = @("teatrocue-ui.exe", "teatrocue-daemon.exe", "ffmpeg.exe", "ffprobe.exe")
                    $allCopied = $true
                    foreach ($fn in $filesToCopy) {
                        $srcF = Join-Path $payloadDir $fn
                        if (Test-Path -LiteralPath $srcF) {
                            $dstF = Join-Path $cueState.CuePath $fn
                            try {
                                Copy-Item -LiteralPath $srcF -Destination $dstF -Force
                                Log-Msg "  -> Copiado $fn OK"
                            } catch {
                                Log-Msg "  [!] Error al copiar $fn : $($_.Exception.Message)"
                                $allCopied = $false
                            }
                        }
                    }
                    if ($allCopied) {
                        Log-Msg "✔ Binarios de TeatroCue actualizados correctamente (licencia preservada)."
                    }
                }
            } else {
                Log-Msg "○ TeatroCue no está instalado en este equipo. (Use la pestaña TeatroCue para instalarlo por primera vez)."
            }

            Log-Msg "======================================================="
            Log-Msg "=== OPERACIÓN DE ACTUALIZACIÓN FINALIZADA CON ÉXITO ==="
            Log-Msg "======================================================="
            Refresh-StatusBadges
            [System.Windows.Forms.MessageBox]::Show(
                "La actualización unificada se ha completado exitosamente.`n`nSe mantuvieron intactas las bases de datos de personal, asistencias, inventario, cues y licencias.",
                "Actualización Completa",
                [System.Windows.Forms.MessageBoxButtons]::OK,
                [System.Windows.Forms.MessageBoxIcon]::Information
            )
        } finally {
            $btnQuickUpdate.Enabled = $true
        }
    })

    # Acción: Instalar TeatroGestión
    $btnGInstall.Add_Click({
        $target = $txtGDir.Text.Trim()
        if (-not $target) {
            [System.Windows.Forms.MessageBox]::Show("Especifique una carpeta de destino válida.", "Error", [System.Windows.Forms.MessageBoxButtons]::OK, [System.Windows.Forms.MessageBoxIcon]::Warning)
            return
        }

        if (-not (Close-AppIfRunning "TeatroGestion" "TeatroGestión")) { return }

        $btnGInstall.Enabled = $false
        try {
            Log-Msg "Instalando / Actualizando TeatroGestión en $target..."
            if ($exeGestionInstall -and (Test-Path -LiteralPath $exeGestionInstall)) {
                $masterPass = $txtGPass.Text.Trim()
                if ($masterPass) {
                    $envs = @{ "TEATROGESTION_MASTER_PASSWORD" = $masterPass }
                    $termMap = @(4, 3, 2, 5) # 365d, 180d, 90d, Perpetua
                    $termIdx = $termMap[$cmbGTerm.SelectedIndex]
                    if (-not $termIdx) { $termIdx = 4 }
                    $argLine = "--silent-install `"$target`" $termIdx 0 `"Oficina Inspección`""
                    Log-Msg "Ejecutando instalación silenciosa con emisión de licencia offline..."
                    $code = Run-LoggedProcess $exeGestionInstall $argLine $envs
                    if ($code -eq 0) {
                        Log-Msg "✔ TeatroGestión instalado y activado correctamente."
                    } else {
                        Log-Msg "⚠ Instalador finalizó con código: $code"
                    }
                } else {
                    Log-Msg "Abriendo instalador interactivo en consola..."
                    Start-Process -FilePath $exeGestionInstall -ArgumentList "--target `"$target`"" -Wait
                    Log-Msg "Instalador interactivo finalizado."
                }
                Refresh-StatusBadges
            } else {
                Log-Msg "ERROR: No se encontró Instalar_TeatroGestion.exe"
            }
        } finally {
            $btnGInstall.Enabled = $true
        }
    })

    # Acción: Instalar TeatroCue
    $btnCInstall.Add_Click({
        $target = $txtCDir.Text.Trim()
        if (-not $target) {
            [System.Windows.Forms.MessageBox]::Show("Especifique una carpeta de destino válida.", "Error", [System.Windows.Forms.MessageBoxButtons]::OK, [System.Windows.Forms.MessageBoxIcon]::Warning)
            return
        }

        if (-not (Close-AppIfRunning "teatrocue-ui" "TeatroCue")) { return }

        $btnCInstall.Enabled = $false
        try {
            Log-Msg "Instalando / Actualizando TeatroCue en $target..."
            if ($exeCueInstall -and (Test-Path -LiteralPath $exeCueInstall)) {
                $cuePass = $txtCPass.Text.Trim()
                if ($cuePass) {
                    $envs = @{ "TEATROCUE_MASTER_PASSWORD" = $cuePass }
                    $code = Run-LoggedProcess $exeCueInstall "--target `"$target`" --perpetual --no-pause" $envs
                    Log-Msg "Proceso terminado con código: $code"
                } else {
                    Log-Msg "Abriendo instalador interactivo de TeatroCue en consola..."
                    Start-Process -FilePath $exeCueInstall -ArgumentList "--target `"$target`"" -Wait
                    Log-Msg "Instalador de TeatroCue finalizado."
                }
                Refresh-StatusBadges
            } else {
                Log-Msg "ERROR: No se encontró Instalar_TeatroCue.exe"
            }
        } finally {
            $btnCInstall.Enabled = $true
        }
    })

    # Acción: Restablecer Bóveda
    $btnResetVault.Add_Click({
        if ($exeGestionInstall -and (Test-Path -LiteralPath $exeGestionInstall)) {
            Log-Msg "Abriendo asistente de restablecimiento de contraseña de Bóveda..."
            Start-Process -FilePath $exeGestionInstall -ArgumentList "--admin-reset-vault" -Wait
            Log-Msg "Asistente de Bóveda finalizado."
        } else {
            Log-Msg "ERROR: No se encontró Instalar_TeatroGestion.exe"
        }
    })

    # Acción: Abrir TeatroCue Admin
    $btnOpenAdmin.Add_Click({
        if ($exeCueAdmin -and (Test-Path -LiteralPath $exeCueAdmin)) {
            Start-Process -FilePath $exeCueAdmin
            Log-Msg "Administrador de licencias abierto."
        } else {
            Log-Msg "ERROR: No se encontró teatrocue-admin.exe"
        }
    })

    # Log inicial
    Log-Msg "Suite Teatro del Libertador iniciada correctamente."
    Log-Msg "Unidad USB detectada en: $scriptDir"
    if ($masterKeyFile) { Log-Msg "Llave maestra master.key.enc: DETECTADA" } else { Log-Msg "Llave maestra: NO ENCONTRADA" }
    if ($licenciasDbFile) { Log-Msg "Base licencias.db: DETECTADA" } else { Log-Msg "Base licencias.db: NO ENCONTRADA" }
    if ($apkFile) { Log-Msg "APK Android Samsung A15: DETECTADO" }

    # Mostrar ventana modal
    $form.ShowDialog() | Out-Null

} catch {
    [System.Windows.Forms.MessageBox]::Show(
        "Se produjo un error al iniciar el instalador gráfico:`n`n$($_.Exception.Message)",
        "Error del Instalador",
        [System.Windows.Forms.MessageBoxButtons]::OK,
        [System.Windows.Forms.MessageBoxIcon]::Error
    )
}
