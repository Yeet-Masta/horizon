@echo off
rem Builds the web version of the game into web\dist on Windows (web\README.md); web/build.sh
rem is the same for Linux and macOS.
rem
rem   web\build.bat              wasm64 (memory64), the default: nightly Rust, -Zbuild-std
rem   web\build.bat --wasm32     wasm32: stable Rust, for browsers without memory64 (Safari)
rem   web\build.bat --release    the release profile (thin LTO; slow to build) instead of `fast`
rem
rem Needs wasm-bindgen-cli at the version Cargo.lock pins
rem (cargo install wasm-bindgen-cli --version <that> --locked); wasm-opt (binaryen) is used when
rem it is on the PATH. Serve the result with: python web\serve.py
setlocal EnableExtensions

set "here=%~dp0"
set "here=%here:~0,-1%"
for %%r in ("%here%\..") do set "root=%%~fr"
set "target=wasm64-unknown-unknown"
set "profile=fast"

:args
if "%~1"=="" goto args_done
if /i "%~1"=="--wasm32" (set "target=wasm32-unknown-unknown" & shift & goto args)
if /i "%~1"=="--wasm64" (set "target=wasm64-unknown-unknown" & shift & goto args)
if /i "%~1"=="--release" (set "profile=release" & shift & goto args)
if /i "%~1"=="-h" goto usage
if /i "%~1"=="--help" goto usage
echo unknown option: %~1 1>&2
exit /b 2
:usage
echo web\build.bat [--wasm32] [--release]
echo   default: wasm64 on nightly Rust; --wasm32 for Safari; --release for thin LTO
exit /b 0
:args_done

cd /d "%root%" || exit /b 1

rem The wasm-bindgen CLI must be the version Cargo.lock pins.
set "pinned="
for /f "usebackq delims=" %%v in (`powershell -NoProfile -Command "$l = Get-Content Cargo.lock; $i = [array]::IndexOf($l, 'name = ' + [char]34 + 'wasm-bindgen' + [char]34); if ($i -ge 0) { ($l[$i + 1] -split [char]34)[1] }"`) do set "pinned=%%v"
set "have="
for /f "tokens=2" %%v in ('wasm-bindgen --version 2^>nul') do set "have=%%v"
if not "%have%"=="%pinned%" (
  echo wasm-bindgen %pinned% is needed ^(found: %have%^): 1>&2
  echo   cargo install wasm-bindgen-cli --version %pinned% --locked 1>&2
  exit /b 1
)

rem No sound device, controllers or mod runtime in a browser (web\README.md).
set "crate=-p horizon-client --bin horizon-client --no-default-features"
if "%target%"=="wasm64-unknown-unknown" (
  rem wasm64 is a tier 3 target: std is built here, with the nightly toolchain.
  rustup component add rust-src --toolchain nightly >nul || exit /b 1
  cargo +nightly build -Zbuild-std=std,panic_abort --target %target% --profile %profile% %crate% || exit /b 1
) else (
  rustup target add wasm32-unknown-unknown >nul || exit /b 1
  cargo build --target %target% --profile %profile% %crate% || exit /b 1
)

set "dist=%here%\dist"
if exist "%dist%" rmdir /s /q "%dist%"
mkdir "%dist%\pkg" "%dist%\fonts" || exit /b 1
set "out=target"
if defined CARGO_TARGET_DIR set "out=%CARGO_TARGET_DIR%"
wasm-bindgen --target web --no-typescript --out-dir "%dist%\pkg" "%out%\%target%\%profile%\horizon-client.wasm" || exit /b 1

where wasm-opt >nul 2>nul
if errorlevel 1 (
  echo wasm-opt not found: the module is not optimized further ^(install binaryen to shrink it^) 1>&2
) else (
  set "memory64="
  if "%target%"=="wasm64-unknown-unknown" set "memory64=--enable-memory64"
  call :optimize || exit /b 1
)

copy /y "%here%\index.html" "%dist%\" >nul || exit /b 1
copy /y "%here%\horizon.js" "%dist%\" >nul || exit /b 1
copy /y "%here%\apache.htaccess" "%dist%\.htaccess" >nul || exit /b 1
copy /y "website\public\favicon.svg" "%dist%\" >nul || exit /b 1
for %%f in (Geist-Regular.woff2 Geist-Medium.woff2 PixelifySans-SemiBold.woff2 OFL-Geist.txt OFL-PixelifySans.txt) do (
  copy /y "website\public\fonts\%%f" "%dist%\fonts\" >nul || exit /b 1
)
set "commit=unknown"
for /f %%c in ('git rev-parse --short HEAD 2^>nul') do set "commit=%%c"
>"%dist%\build.json" echo {"target":"%target%","profile":"%profile%","commit":"%commit%"}
for %%s in ("%dist%\pkg\horizon-client_bg.wasm") do set /a "mb=%%~zs / 1048576"
echo web\dist: %mb% MB module (%target%, %profile%)
echo Serve it with: python web\serve.py   (then open http://localhost:8080/)
exit /b 0

:optimize
wasm-opt -O2 --enable-bulk-memory --enable-nontrapping-float-to-int --enable-sign-ext --enable-mutable-globals --enable-reference-types --enable-multivalue %memory64% "%dist%\pkg\horizon-client_bg.wasm" -o "%dist%\pkg\horizon-client_bg.wasm"
exit /b %errorlevel%
