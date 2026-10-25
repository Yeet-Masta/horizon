@echo off
rem Builds the web version of the game into web\dist on Windows (web\README.md); web/build.sh
rem is the same for Linux and macOS.
rem
rem   web\build.bat              wasm64 (memory64) with threads, the default
rem   web\build.bat --wasm32     wasm32: for browsers without memory64 (Safari)
rem   web\build.bat --no-threads one thread: no shared memory, so no COOP/COEP headers needed
rem   web\build.bat --fast       the quicker-to-build `fast` profile (no LTO) instead of release
rem
rem Every build uses WebAssembly SIMD (web\simd.toml), which every browser with WebGPU has.
rem Threads and wasm64 build std here, with nightly Rust (rustup adds rust-src). Needs the
rem wasm-bindgen CLI from the commit Cargo.lock pins (the error says the command); wasm-opt
rem (binaryen) is used when it is on the PATH. Serve the result with: python web\serve.py
setlocal EnableExtensions

set "here=%~dp0"
set "here=%here:~0,-1%"
for %%r in ("%here%\..") do set "root=%%~fr"
set "target=wasm64-unknown-unknown"
set "profile=release"
set "threads=1"

:args
if "%~1"=="" goto args_done
if /i "%~1"=="--wasm32" (set "target=wasm32-unknown-unknown" & shift & goto args)
if /i "%~1"=="--wasm64" (set "target=wasm64-unknown-unknown" & shift & goto args)
if /i "%~1"=="--release" (set "profile=release" & shift & goto args)
if /i "%~1"=="--fast" (set "profile=fast" & shift & goto args)
if /i "%~1"=="--no-threads" (set "threads=0" & shift & goto args)
if /i "%~1"=="--threads" (set "threads=1" & shift & goto args)
if /i "%~1"=="-h" goto usage
if /i "%~1"=="--help" goto usage
echo unknown option: %~1 1>&2
exit /b 2
:usage
echo web\build.bat [--wasm32] [--no-threads] [--fast]
echo   default: wasm64 with threads on nightly Rust, release profile; --wasm32 for Safari;
echo   --no-threads for servers without COOP/COEP headers; --fast to build quicker, without LTO
exit /b 0
:args_done

cd /d "%root%" || exit /b 1

rem The wasm-bindgen CLI must match Cargo.lock: wasm-bindgen comes from its main branch for now
rem (Cargo.toml's [patch.crates-io]), so the CLI is built from the same commit.
set "pinned="
set "rev="
for /f "usebackq delims=" %%v in (`powershell -NoProfile -Command "$l = Get-Content Cargo.lock; $i = [array]::IndexOf($l, 'name = ' + [char]34 + 'wasm-bindgen' + [char]34); if ($i -ge 0) { ($l[$i + 1] -split [char]34)[1] }"`) do set "pinned=%%v"
for /f "usebackq delims=" %%v in (`powershell -NoProfile -Command "$l = Get-Content Cargo.lock; $i = [array]::IndexOf($l, 'name = ' + [char]34 + 'wasm-bindgen' + [char]34); if ($i -ge 0 -and $l[$i + 2] -match '#([0-9a-f]+)') { $Matches[1] }"`) do set "rev=%%v"
set "install=cargo install wasm-bindgen-cli --version %pinned% --locked"
if defined rev set "install=cargo install wasm-bindgen-cli --git https://github.com/wasm-bindgen/wasm-bindgen --rev %rev% --locked --force"
set "have="
for /f "tokens=2" %%v in ('wasm-bindgen --version 2^>nul') do set "have=%%v"
if not "%have%"=="%pinned%" (
  echo wasm-bindgen %pinned% %rev% is needed ^(found: %have%^): 1>&2
  echo   %install% 1>&2
  exit /b 1
)

rem No sound device, controllers or mod runtime in a browser (web\README.md).
set "crate=-p horizon-client --bin horizon-client --no-default-features --config web/simd.toml"
rem wasm64 is a tier 3 target, and threads need std built with atomics: std is built here, with
rem the nightly toolchain. web\threads.toml adds shared memory and atomics.
set "nightly=%threads%"
if "%target%"=="wasm64-unknown-unknown" set "nightly=1"
set "config="
if "%threads%"=="1" set "config=--config web/threads.toml"
if "%nightly%"=="1" (
  rustup component add rust-src --toolchain nightly >nul || exit /b 1
  cargo +nightly build -Zbuild-std=std,panic_abort %config% --target %target% --profile %profile% %crate% || exit /b 1
) else (
  rustup target add wasm32-unknown-unknown >nul || exit /b 1
  cargo build --target %target% --profile %profile% %crate% || exit /b 1
)

set "dist=%here%\dist"
if exist "%dist%" rmdir /s /q "%dist%"
mkdir "%dist%\pkg" "%dist%\fonts" || exit /b 1
set "out=target"
if defined CARGO_TARGET_DIR set "out=%CARGO_TARGET_DIR%"
wasm-bindgen --target web --no-typescript --out-dir "%dist%\pkg" "%out%\%target%\%profile%\horizon-client.wasm"
if errorlevel 1 (
  echo If wasm-bindgen says the module's bindgen format differs, install the pinned one: 1>&2
  echo   %install% 1>&2
  exit /b 1
)

where wasm-opt >nul 2>nul
if errorlevel 1 (
  echo wasm-opt not found: the module is not optimized further ^(install binaryen to shrink it^) 1>&2
) else (
  set "memory64="
  if "%target%"=="wasm64-unknown-unknown" set "memory64=--enable-memory64"
  set "atomics="
  if "%threads%"=="1" set "atomics=--enable-threads"
  call :optimize || exit /b 1
)

copy /y "%here%\index.html" "%dist%\" >nul || exit /b 1
copy /y "%here%\horizon.js" "%dist%\" >nul || exit /b 1
copy /y "%here%\worker.js" "%dist%\" >nul || exit /b 1
copy /y "%here%\apache.htaccess" "%dist%\.htaccess" >nul || exit /b 1
copy /y "website\public\favicon.svg" "%dist%\" >nul || exit /b 1
for %%f in (Geist-Regular.woff2 Geist-Medium.woff2 PixelifySans-SemiBold.woff2 OFL-Geist.txt OFL-PixelifySans.txt) do (
  copy /y "website\public\fonts\%%f" "%dist%\fonts\" >nul || exit /b 1
)
set "commit=unknown"
for /f %%c in ('git rev-parse --short HEAD 2^>nul') do set "commit=%%c"
set "threads_json=false"
if "%threads%"=="1" set "threads_json=true"
>"%dist%\build.json" echo {"target":"%target%","profile":"%profile%","threads":%threads_json%,"simd":true,"commit":"%commit%"}
for %%s in ("%dist%\pkg\horizon-client_bg.wasm") do set /a "mb=%%~zs / 1048576"
echo web\dist: %mb% MB module (%target%, %profile%, threads %threads_json%)
echo Serve it with: python web\serve.py   (then open http://localhost:8080/)
exit /b 0

:optimize
wasm-opt -O2 --enable-bulk-memory --enable-nontrapping-float-to-int --enable-sign-ext --enable-mutable-globals --enable-reference-types --enable-multivalue --enable-simd %memory64% %atomics% "%dist%\pkg\horizon-client_bg.wasm" -o "%dist%\pkg\horizon-client_bg.wasm"
exit /b %errorlevel%
