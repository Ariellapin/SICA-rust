@echo off
rem Build the workspace and launch the frontend GUI.
rem `cargo run` builds first, so this is one command. Pass any extra cargo args.

setlocal

set "CARGO_BIN=%USERPROFILE%\.cargo\bin"
set "MINGW_BIN=%LOCALAPPDATA%\Microsoft\WinGet\Packages\MartinStorsjo.LLVM-MinGW.UCRT_Microsoft.Winget.Source_8wekyb3d8bbwe\llvm-mingw-20260505-ucrt-x86_64\bin"

if not exist "%CARGO_BIN%\cargo.exe" (
    echo ERROR: cargo not found at %CARGO_BIN%. Install Rust via rustup. 1>&2
    exit /b 1
)
if not exist "%MINGW_BIN%" (
    echo ERROR: LLVM-MinGW not found at %MINGW_BIN%. 1>&2
    echo Install: winget install MartinStorsjo.LLVM-MinGW.UCRT 1>&2
    exit /b 1
)

set "PATH=%CARGO_BIN%;%MINGW_BIN%;%PATH%"

cargo.exe run -p frontend %*
exit /b %ERRORLEVEL%
