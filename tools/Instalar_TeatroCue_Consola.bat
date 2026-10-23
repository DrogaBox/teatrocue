@echo off
chcp 65001 >nul
title TeatroCue — Instalador Consola (Ballet Oficial de Córdoba)

set "SCRIPT_DIR=%~dp0"
set "EXE=%SCRIPT_DIR%Instalar_TeatroCue.exe"

if exist "%EXE%" (
    "%EXE%" %*
    exit /b %ERRORLEVEL%
)

echo [!] No se encontró Instalar_TeatroCue.exe
pause
