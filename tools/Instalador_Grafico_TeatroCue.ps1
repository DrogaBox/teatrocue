Add-Type -AssemblyName PresentationFramework, PresentationCore, WindowsBase
Add-Type -AssemblyName System.Windows.Forms   # FolderBrowserDialog y DoEvents (sin esto «Examinar» falla)
Add-Type -AssemblyName Microsoft.VisualBasic   # InputBox de «Días personalizados»

$scriptDir = Split-Path -Parent $MyInvocation.MyCommand.Definition
$installerExe = Join-Path $scriptDir "Instalar_TeatroCue.exe"
if (-not (Test-Path $installerExe)) {
    $alt = Join-Path $scriptDir "TeatroCue\Instalar_TeatroCue.exe"
    if (Test-Path $alt) { $installerExe = $alt }
}

[xml]$xaml = @"
<Window xmlns="http://schemas.microsoft.com/winfx/2006/xaml/presentation"
        xmlns:x="http://schemas.microsoft.com/winfx/2006/xaml"
        Title="TeatroCue — Instalador Oficial Windows 11" 
        Height="680" Width="820" 
        WindowStartupLocation="CenterScreen" 
        ResizeMode="CanMinimize"
        Background="#121214" Foreground="#E0E0E0" FontFamily="Segoe UI">
    <Window.Resources>
        <Style TargetType="TextBlock">
            <Setter Property="Foreground" Value="#CCCCCC"/>
            <Setter Property="FontSize" Value="13"/>
        </Style>
        <Style TargetType="TextBox">
            <Setter Property="Background" Value="#1C1C22"/>
            <Setter Property="Foreground" Value="#FFFFFF"/>
            <Setter Property="BorderBrush" Value="#383842"/>
            <Setter Property="BorderThickness" Value="1"/>
            <Setter Property="Padding" Value="8,5"/>
            <Setter Property="FontSize" Value="13"/>
        </Style>
        <Style TargetType="PasswordBox">
            <Setter Property="Background" Value="#1C1C22"/>
            <Setter Property="Foreground" Value="#FFFFFF"/>
            <Setter Property="BorderBrush" Value="#383842"/>
            <Setter Property="BorderThickness" Value="1"/>
            <Setter Property="Padding" Value="8,5"/>
            <Setter Property="FontSize" Value="13"/>
        </Style>
        <Style TargetType="ComboBox">
            <Setter Property="Background" Value="#1C1C22"/>
            <Setter Property="Foreground" Value="#121214"/>
            <Setter Property="Padding" Value="6,4"/>
            <Setter Property="FontSize" Value="13"/>
        </Style>
        <Style TargetType="Button">
            <Setter Property="Background" Value="#FFB31A"/>
            <Setter Property="Foreground" Value="#121214"/>
            <Setter Property="FontWeight" Value="SemiBold"/>
            <Setter Property="BorderThickness" Value="0"/>
            <Setter Property="Padding" Value="16,8"/>
            <Setter Property="Cursor" Value="Hand"/>
        </Style>
    </Window.Resources>

    <Grid Margin="20">
        <Grid.RowDefinitions>
            <RowDefinition Height="Auto"/>
            <RowDefinition Height="*"/>
            <RowDefinition Height="Auto"/>
            <RowDefinition Height="140"/>
        </Grid.RowDefinitions>

        <!-- Header -->
        <Border Grid.Row="0" Background="#18181E" BorderBrush="#2D2D36" BorderThickness="1" CornerRadius="8" Padding="18,12" Margin="0,0,0,15">
            <Grid>
                <Grid.ColumnDefinitions>
                    <ColumnDefinition Width="*"/>
                    <ColumnDefinition Width="Auto"/>
                </Grid.ColumnDefinitions>
                <StackPanel Grid.Column="0">
                    <TextBlock Text="TEATROCUE — SISTEMA DE AUDIO Y CUES" FontSize="20" FontWeight="Bold" Foreground="#FFB31A"/>
                    <TextBlock Text="Ballet Oficial de Córdoba — Teatro del Libertador San Martín" FontSize="12" Foreground="#8E8E9F" Margin="0,2,0,4"/>
                    <TextBlock Text="★ REPARTO DE ESTUDIO, SIN ORDEN DE PRIORIDAD Y SUJETO A CAMBIOS. ★" FontSize="11" Foreground="#FFB31A" FontStyle="Italic"/>
                </StackPanel>
                <Border Grid.Column="1" Background="#262630" CornerRadius="4" Padding="10,4" VerticalAlignment="Center">
                    <TextBlock Text="AUDIO REPRODUCER PRO" FontSize="11" FontWeight="Bold" Foreground="#00E676"/>
                </Border>
            </Grid>
        </Border>

        <!-- Body Form -->
        <Border Grid.Row="1" Background="#18181E" BorderBrush="#2D2D36" BorderThickness="1" CornerRadius="8" Padding="18" Margin="0,0,0,15">
            <ScrollViewer VerticalScrollBarVisibility="Auto">
                <StackPanel>
                    <!-- Carpeta Destino -->
                    <TextBlock Text="Carpeta de Instalación:" FontWeight="SemiBold" Margin="0,0,0,4"/>
                    <Grid Margin="0,0,0,12">
                        <Grid.ColumnDefinitions>
                            <ColumnDefinition Width="*"/>
                            <ColumnDefinition Width="Auto"/>
                        </Grid.ColumnDefinitions>
                        <TextBox x:Name="TxtTargetDir" Grid.Column="0" Text="C:\TeatroCue"/>
                        <Button x:Name="BtnBrowseTarget" Grid.Column="1" Content="Examinar..." Margin="8,0,0,0" Background="#2C2C38" Foreground="#FFFFFF"/>
                    </Grid>

                    <Grid Margin="0,0,0,12">
                        <Grid.ColumnDefinitions>
                            <ColumnDefinition Width="*"/>
                            <ColumnDefinition Width="*"/>
                        </Grid.ColumnDefinitions>
                        <!-- Vigencia -->
                        <StackPanel Grid.Column="0" Margin="0,0,8,0">
                            <TextBlock Text="Vigencia de la Licencia:" FontWeight="SemiBold" Margin="0,0,0,4"/>
                            <ComboBox x:Name="CmbLicenseTerm" SelectedIndex="0">
                                <ComboBoxItem Content="1 Año (365 días) — Recomendado" Tag="365"/>
                                <ComboBoxItem Content="1 Mes (30 días)" Tag="30"/>
                                <ComboBoxItem Content="3 Meses (90 días)" Tag="90"/>
                                <ComboBoxItem Content="6 Meses (180 días)" Tag="180"/>
                                <ComboBoxItem Content="Perpetua (Sin vencimiento)" Tag="perpetual"/>
                                <ComboBoxItem Content="Días personalizados…" Tag="custom"/>
                            </ComboBox>
                        </StackPanel>
                        <!-- Puesto -->
                        <StackPanel Grid.Column="1" Margin="8,0,0,0">
                            <TextBlock Text="Puesto / Ubicación de esta PC:" FontWeight="SemiBold" Margin="0,0,0,4"/>
                            <TextBox x:Name="TxtPuesto" Text="Cabina Principal"/>
                        </StackPanel>
                    </Grid>

                    <!-- Contraseña Maestra -->
                    <TextBlock Text="Contraseña Maestra del Desarrollador (master.key.enc):" FontWeight="SemiBold" Margin="0,0,0,4"/>
                    <PasswordBox x:Name="TxtMasterPass" Margin="0,0,0,14"/>

                    <!-- Opciones Adicionales -->
                    <Border Background="#141418" BorderBrush="#2C2C38" BorderThickness="1" CornerRadius="6" Padding="12" Margin="0,0,0,16">
                        <StackPanel>
                            <TextBlock Text="Modo de Instalación:" FontWeight="Bold" Foreground="#FFFFFF" Margin="0,0,0,8"/>
                            <RadioButton x:Name="RadFullInstall" Content="Instalación Completa (Programas + Runtime + Licencia)" IsChecked="True" Foreground="#E0E0E0" Margin="0,0,0,6"/>
                            <RadioButton x:Name="RadLicenseOnly" Content="Renovar Solo Licencia (Conserva programas existentes)" Foreground="#E0E0E0" Margin="0,0,0,8"/>
                            <CheckBox x:Name="ChkResetClock" Content="Resetear Reloj Antifraude (Forzar resincronización de marcas)" Foreground="#B0B0C0"/>
                        </StackPanel>
                    </Border>

                    <!-- Botón de Acción -->
                    <Button x:Name="BtnInstall" Content="INSTALAR Y ACTIVAR TEATROCUE" Height="42" FontSize="14" FontWeight="Bold"/>
                </StackPanel>
            </ScrollViewer>
        </Border>

        <!-- Progress and Status -->
        <Grid Grid.Row="2" Margin="0,0,0,8">
            <Grid.ColumnDefinitions>
                <ColumnDefinition Width="*"/>
                <ColumnDefinition Width="Auto"/>
            </Grid.ColumnDefinitions>
            <ProgressBar x:Name="PrgProgress" Grid.Column="0" Height="8" Background="#18181E" Foreground="#FFB31A" BorderThickness="0" Margin="0,0,15,0"/>
            <CheckBox x:Name="ChkLaunchAfter" Grid.Column="1" Content="Iniciar TeatroCue al terminar" IsChecked="True" Foreground="#CCCCCC" VerticalAlignment="Center"/>
        </Grid>

        <!-- Log Output -->
        <Border Grid.Row="3" Background="#0A0A0C" BorderBrush="#25252E" BorderThickness="1" CornerRadius="6" Padding="6">
            <TextBox x:Name="TxtLog" Background="Transparent" Foreground="#A5D6A7" BorderThickness="0" 
                     FontFamily="Consolas" FontSize="11" IsReadOnly="True" 
                     VerticalScrollBarVisibility="Auto" TextWrapping="Wrap"/>
        </Border>
    </Grid>
</Window>
"@

$reader = (New-Object System.Xml.XmlNodeReader $xaml)
$window = [System.Windows.Markup.XamlReader]::Load($reader)

# Elementos
$txtTargetDir = $window.FindName("TxtTargetDir")
$btnBrowseTarget = $window.FindName("BtnBrowseTarget")
$cmbLicenseTerm = $window.FindName("CmbLicenseTerm")
$txtPuesto = $window.FindName("TxtPuesto")
$txtMasterPass = $window.FindName("TxtMasterPass")
$radFullInstall = $window.FindName("RadFullInstall")
$radLicenseOnly = $window.FindName("RadLicenseOnly")
$chkResetClock = $window.FindName("ChkResetClock")
$btnInstall = $window.FindName("BtnInstall")
$prgProgress = $window.FindName("PrgProgress")
$chkLaunchAfter = $window.FindName("ChkLaunchAfter")
$txtLog = $window.FindName("TxtLog")

function Log-Message([string]$msg) {
    $timestamp = (Get-Date).ToString("HH:mm:ss")
    $txtLog.AppendText("[$timestamp] $msg`r`n")
    $txtLog.ScrollToEnd()
    [System.Windows.Forms.Application]::DoEvents()
}

Log-Message "Instalador de TeatroCue iniciado."
Log-Message "Ubicación del ejecutable nativo: $installerExe"

# Examinar Carpeta Destino
$btnBrowseTarget.Add_Click({
    $dlg = New-Object System.Windows.Forms.FolderBrowserDialog
    $dlg.SelectedPath = $txtTargetDir.Text
    if ($dlg.ShowDialog() -eq [System.Windows.Forms.DialogResult]::OK) {
        $txtTargetDir.Text = $dlg.SelectedPath
    }
})

# Botón Instalar
$btnInstall.Add_Click({
    $btnInstall.IsEnabled = $false
    $prgProgress.IsIndeterminate = $true

    # Sin barra final: «C:\TeatroCue\» entre comillas escapa la comilla de cierre y rompe el argumento.
    $target = $txtTargetDir.Text.Trim().TrimEnd('\')
    $puesto = $txtPuesto.Text.Trim()
    $masterPass = $txtMasterPass.Password
    $termTag = $cmbLicenseTerm.SelectedItem.Tag

    $argsList = New-Object System.Collections.Generic.List[string]
    $argsList.Add("--target")
    $argsList.Add("`"$target`"")
    $argsList.Add("--puesto")
    $argsList.Add("`"$puesto`"")
    $argsList.Add("--no-pause")

    if ($termTag -eq "perpetual") {
        $argsList.Add("--perpetual")
    } elseif ($termTag -eq "custom") {
        $daysInput = [Microsoft.VisualBasic.Interaction]::InputBox("Ingrese la cantidad de días de vigencia (1 a 3650):", "Días de Licencia", "365")
        if (-not $daysInput) {
            $btnInstall.IsEnabled = $true
            $prgProgress.IsIndeterminate = $false
            return
        }
        $argsList.Add("--days")
        $argsList.Add($daysInput)
    } else {
        $argsList.Add("--days")
        $argsList.Add($termTag)
    }

    if ($radLicenseOnly.IsChecked) {
        $argsList.Add("--solo-licencia")
    }

    if ($chkResetClock.IsChecked) {
        $argsList.Add("--reset-reloj")
    }

    Log-Message "Iniciando instalación en: $target"
    Log-Message "Argumentos: $($argsList -join ' ')"

    $pinfo = New-Object System.Diagnostics.ProcessStartInfo
    $pinfo.FileName = $installerExe
    $pinfo.Arguments = ($argsList -join ' ')
    $pinfo.RedirectStandardOutput = $true
    $pinfo.RedirectStandardError = $true
    $pinfo.UseShellExecute = $false
    $pinfo.CreateNoWindow = $true

    if ($masterPass -ne "") {
        $pinfo.EnvironmentVariables["TEATROCUE_MASTER_PASSWORD"] = $masterPass
    }

    $p = New-Object System.Diagnostics.Process
    $p.StartInfo = $pinfo

    try {
        $p.Start() | Out-Null
        $stdout = $p.StandardOutput.ReadToEnd()
        $stderr = $p.StandardError.ReadToEnd()
        $p.WaitForExit()

        if ($stdout) {
            foreach ($line in ($stdout -split "`r?`n")) {
                if ($line.Trim()) { Log-Message $line }
            }
        }
        if ($stderr) {
            foreach ($line in ($stderr -split "`r?`n")) {
                if ($line.Trim()) { Log-Message "[!] $line" }
            }
        }

        if ($p.ExitCode -eq 0) {
            $prgProgress.IsIndeterminate = $false
            $prgProgress.Value = 100
            Log-Message "¡INSTALACIÓN COMPLETADA EXITOSAMENTE!"
            [System.Windows.MessageBox]::Show("TeatroCue ha sido instalado y activado exitosamente.`n`nUbicación: $target", "Instalación Exitosa", [System.Windows.MessageBoxButton]::OK, [System.Windows.MessageBoxImage]::Information)

            if ($chkLaunchAfter.IsChecked) {
                $exePath = Join-Path $target "teatrocue-ui.exe"
                if (Test-Path $exePath) {
                    Log-Message "Lanzando TeatroCue..."
                    Start-Process $exePath -WorkingDirectory $target
                }
            }
        } else {
            Log-Message "La instalación finalizó con código de error $($p.ExitCode)."
            [System.Windows.MessageBox]::Show("Ocurrió un error durante la instalación. Verifique que la contraseña maestra sea correcta.", "Error de Instalación", [System.Windows.MessageBoxButton]::OK, [System.Windows.MessageBoxImage]::Error)
        }
    } catch {
        Log-Message "Excepción al ejecutar instalador: $_"
    } finally {
        $btnInstall.IsEnabled = $true
        $prgProgress.IsIndeterminate = $false
    }
})

$window.ShowDialog() | Out-Null
