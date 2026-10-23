@echo off
chcp 65001 >nul
title TeatroCue — Instalador Gráfico (Windows 11)

set "SCRIPT_DIR=%~dp0"
set "PS_SCRIPT=%SCRIPT_DIR%Instalador_Grafico_TeatroCue.ps1"
if not exist "%PS_SCRIPT%" (
    if exist "%SCRIPT_DIR%tools\Instalador_Grafico_TeatroCue.ps1" (
        set "PS_SCRIPT=%SCRIPT_DIR%tools\Instalador_Grafico_TeatroCue.ps1"
    )
)

if exist "%PS_SCRIPT%" (
    start "" powershell -NoProfile -ExecutionPolicy Bypass -STA -File "%PS_SCRIPT%"
    exit /b 0
)

if exist "%SCRIPT_DIR%Instalar_TeatroCue.exe" (
    start "" "%SCRIPT_DIR%Instalar_TeatroCue.exe" --gui
    exit /b 0
)

echo [!] No se encontró el instalador gráfico de TeatroCue.
pause
