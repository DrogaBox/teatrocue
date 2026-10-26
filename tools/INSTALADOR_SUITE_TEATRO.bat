@echo off
setlocal EnableDelayedExpansion
chcp 65001 >nul
title Suite Teatro del Libertador San Martín — Instalador y Actualizador

:: 0. Asegurar directorio de trabajo en la raíz del script (USB pendrive)
set "CURRENT_DIR=%~dp0"
if "%CURRENT_DIR:~-1%"=="\" set "CURRENT_DIR=%CURRENT_DIR:~0,-1%"
cd /d "%CURRENT_DIR%"
set "SCRIPT_DIR=%CURRENT_DIR%\"
set "TEATRO_SUITE_USB=%CURRENT_DIR%"

:: Seleccionar el ejecutable de PowerShell (priorizando 64-bit nativo)
set "PS_EXE=powershell.exe"
if exist "%SystemRoot%\System32\WindowsPowerShell\v1.0\powershell.exe" (
    set "PS_EXE=%SystemRoot%\System32\WindowsPowerShell\v1.0\powershell.exe"
)

:: 1. Verificación y elevación automática a Administrador (UAC)
net session >nul 2>&1
if %errorlevel% neq 0 (
    echo.
    echo =======================================================================
    echo   SUITE TEATRO DEL LIBERTADOR SAN MARTÍN — BALLET OFICIAL DE CÓRDOBA
    echo   TeatroCue (Sonido) + TeatroGestión (Inspección y Valeras)
    echo =======================================================================
    echo.
    echo [i] Solicitando elevación de permisos de Administrador...
    "%PS_EXE%" -NoProfile -ExecutionPolicy Bypass -Command "Start-Process cmd.exe -WorkingDirectory '%CURRENT_DIR%' -ArgumentList '/c cd /d """%CURRENT_DIR%""" && """%~f0"""' -Verb RunAs" 2>nul || (
        echo [!] No se pudo solicitar elevación automática.
        echo     Haga clic derecho en 'INSTALADOR_SUITE_TEATRO.bat' y seleccione 'Ejecutar como administrador'.
        pause
    )
    exit /b 0
)

:: 2. Ubicar script PowerShell Unificado
set "PS_UNIFICADO=%SCRIPT_DIR%Instalador_Unificado.ps1"
if not exist "%PS_UNIFICADO%" (
    if exist "%SCRIPT_DIR%scripts\Instalador_Unificado.ps1" (
        set "PS_UNIFICADO=%SCRIPT_DIR%scripts\Instalador_Unificado.ps1"
    )
)

:: 3. Lanzar Instalador Gráfico Unificado (Windows Forms - Compatible Win 7/10/11)
if exist "%PS_UNIFICADO%" (
    :: Intento 1: Ejecución directa por archivo pasando la ruta del pendrive
    "%PS_EXE%" -NoProfile -ExecutionPolicy Bypass -STA -File "%PS_UNIFICADO%" -UsbPath "%CURRENT_DIR%"
    if !errorlevel! equ 0 exit /b 0

    :: Intento 2: Ejecución en memoria para sortear restricciones estrictas de directivas de grupo
    "%PS_EXE%" -NoProfile -ExecutionPolicy Bypass -STA -Command "[Console]::OutputEncoding=[System.Text.Encoding]::UTF8; $env:TEATRO_SUITE_USB='%CURRENT_DIR%'; & ([ScriptBlock]::Create((Get-Content -Raw -Encoding UTF8 '%PS_UNIFICADO%'))) -UsbPath '%CURRENT_DIR%'"
    if !errorlevel! equ 0 exit /b 0

    echo.
    echo [!] La interfaz gráfica finalizó o fue cerrada. Continuando al menú de consola...
)

:: 4. Fallback de Consola interactivo en CMD
:MENU_CONSOLA
cls
cd /d "%CURRENT_DIR%"
echo ===============================================================================
echo   SUITE TEATRO DEL LIBERTADOR SAN MARTÍN — BALLET OFICIAL DE CÓRDOBA
echo   Gestión de Instalaciones, Actualizaciones y Licencias Offline
echo ===============================================================================
echo.
echo   [1] Actualizar AMBAS aplicaciones en 1 Clic (TeatroGestión + TeatroCue)
echo   [2] Instalar / Actualizar TeatroGestión (v1.3.0 - Plantel Oficial 64 miembros)
echo   [3] Instalar / Actualizar TeatroCue (v0.1.0 - Motor Audio 1 kHz)
echo   [4] Renovar Licencias Offline (master.key.enc)
echo   [5] Restablecer Contraseña de Bóveda de Inspección (Carina Lion)
echo   [6] Ver Manual de Usuario de TeatroGestión (PDF)
echo   [7] Ver Manual de Usuario de TeatroCue (PDF)
echo   [8] Copiar APK de TeatroGestión para Celular (Samsung Galaxy A15)
echo   [9] Abrir Carpeta de la Unidad USB en el Explorador
echo   [0] Salir
echo.
set /p "OPC=Seleccione una opción [0-9]: "

if "%OPC%"=="1" goto ACTUALIZAR_AMBAS
if "%OPC%"=="2" goto GESTION
if "%OPC%"=="3" goto CUE
if "%OPC%"=="4" goto LICENCIAS
if "%OPC%"=="5" goto BOVEDA
if "%OPC%"=="6" goto MANUAL_GESTION
if "%OPC%"=="7" goto MANUAL_CUE
if "%OPC%"=="8" goto COPIAR_APK
if "%OPC%"=="9" goto EXPLORER
if "%OPC%"=="0" exit /b 0
goto MENU_CONSOLA

:ACTUALIZAR_AMBAS
echo.
echo [*] Actualizando TeatroGestión a la última versión...
if exist "%SCRIPT_DIR%Actualizar_TeatroGestion.exe" (
    "%SCRIPT_DIR%Actualizar_TeatroGestion.exe" --si
) else if exist "%SCRIPT_DIR%TeatroGestion\Actualizar_TeatroGestion.exe" (
    "%SCRIPT_DIR%TeatroGestion\Actualizar_TeatroGestion.exe" --si
) else (
    echo [!] No se encontró Actualizar_TeatroGestion.exe
)

echo.
echo [*] Actualizando TeatroCue...
if exist "%SCRIPT_DIR%Instalar_TeatroCue.exe" (
    "%SCRIPT_DIR%Instalar_TeatroCue.exe" --perpetual --no-pause
) else if exist "%SCRIPT_DIR%TeatroCue\Instalar_TeatroCue.exe" (
    "%SCRIPT_DIR%TeatroCue\Instalar_TeatroCue.exe" --perpetual --no-pause
) else (
    echo [!] No se encontró Instalar_TeatroCue.exe
)
echo.
echo [OK] Proceso de actualización finalizado.
pause
goto MENU_CONSOLA

:GESTION
if exist "%SCRIPT_DIR%Instalar_TeatroGestion.exe" (
    "%SCRIPT_DIR%Instalar_TeatroGestion.exe"
) else if exist "%SCRIPT_DIR%TeatroGestion\Instalar_TeatroGestion.exe" (
    "%SCRIPT_DIR%TeatroGestion\Instalar_TeatroGestion.exe"
) else (
    echo [!] Instalar_TeatroGestion.exe no encontrado.
    pause
)
goto MENU_CONSOLA

:CUE
if exist "%SCRIPT_DIR%Instalar_TeatroCue.exe" (
    "%SCRIPT_DIR%Instalar_TeatroCue.exe"
) else if exist "%SCRIPT_DIR%TeatroCue\Instalar_TeatroCue.exe" (
    "%SCRIPT_DIR%TeatroCue\Instalar_TeatroCue.exe"
) else (
    echo [!] Instalar_TeatroCue.exe no encontrado.
    pause
)
goto MENU_CONSOLA

:LICENCIAS
if exist "%SCRIPT_DIR%admin\teatrocue-admin.exe" (
    "%SCRIPT_DIR%admin\teatrocue-admin.exe"
) else (
    echo [!] Herramienta administrativa no encontrada en admin\teatrocue-admin.exe
    pause
)
goto MENU_CONSOLA

:BOVEDA
if exist "%SCRIPT_DIR%Instalar_TeatroGestion.exe" (
    "%SCRIPT_DIR%Instalar_TeatroGestion.exe" --admin-reset-vault
) else if exist "%SCRIPT_DIR%TeatroGestion\Instalar_TeatroGestion.exe" (
    "%SCRIPT_DIR%TeatroGestion\Instalar_TeatroGestion.exe" --admin-reset-vault
) else (
    echo [!] Instalar_TeatroGestion.exe no encontrado.
    pause
)
goto MENU_CONSOLA

:MANUAL_GESTION
if exist "%SCRIPT_DIR%Manual de Usuario TeatroGestion.pdf" (
    start "" "%SCRIPT_DIR%Manual de Usuario TeatroGestion.pdf"
) else (
    echo [!] Archivo 'Manual de Usuario TeatroGestion.pdf' no encontrado.
    pause
)
goto MENU_CONSOLA

:MANUAL_CUE
if exist "%SCRIPT_DIR%Manual de Usuario TeatroCue.pdf" (
    start "" "%SCRIPT_DIR%Manual de Usuario TeatroCue.pdf"
) else (
    echo [!] Archivo 'Manual de Usuario TeatroCue.pdf' no encontrado.
    pause
)
goto MENU_CONSOLA

:COPIAR_APK
echo.
set "APK_SRC=%SCRIPT_DIR%TeatroGestion\TeatroGestion.apk"
if not exist "%APK_SRC%" set "APK_SRC=%SCRIPT_DIR%TeatroGestion.apk"
if exist "%APK_SRC%" (
    echo [*] Copiando TeatroGestion.apk al Escritorio...
    if defined USERPROFILE (
        copy /y "%APK_SRC%" "%USERPROFILE%\Desktop\TeatroGestion.apk" >nul
        echo [OK] Copiado a: %USERPROFILE%\Desktop\TeatroGestion.apk
        echo     Conecte el celular Samsung Galaxy A15 por USB e instálelo.
    ) else (
        echo [!] Variable USERPROFILE no disponible.
    )
) else (
    echo [!] Archivo TeatroGestion.apk no encontrado en el pendrive.
)
pause
goto MENU_CONSOLA

:EXPLORER
start "" explorer.exe "%CURRENT_DIR%"
goto MENU_CONSOLA
