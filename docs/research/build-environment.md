# Windows build environment (verified setup)

Record of a from-scratch build environment set up on a Windows x64 machine that
had no Rust, no MSVC C++ tools and no LLVM, and of the commands that produced a
working development binary.

Machine: Windows (10.0.26300), PowerShell (`pwsh`), non-elevated user with UAC
available. Git, Node.js 26.7.0, Python 3.11.9, the WebView2 Runtime and a
Windows SDK were already present; `rustc`/`cargo`/`rustup`, MSVC C++ Build Tools
and LLVM were not.

## Installed versions

| Component | Version | Location |
| --- | --- | --- |
| rustup | 1.29.2 | `%USERPROFILE%\.cargo\bin` |
| rustc | 1.97.1 (8bab26f4f 2026-07-14) | `%USERPROFILE%\.rustup\toolchains\1.97.1-x86_64-pc-windows-msvc` |
| cargo | 1.97.1 (c980f4866 2026-06-30) | same |
| clippy | 0.1.97 | same |
| rustfmt | 1.9.0-stable | same |
| Visual Studio Build Tools | 17.14.37827.9 (2022) | `C:\Program Files (x86)\Microsoft Visual Studio\2022\BuildTools` |
| MSVC compiler | 14.44.35207 | `...\BuildTools\VC\Tools\MSVC\14.44.35207\bin\Hostx64\x64\cl.exe` |
| Windows SDK | 10.0.26100.0 (also 10.0.22621.0, 10.0.17763.0) | `C:\Program Files (x86)\Windows Kits\10` |
| LLVM / libclang | 23.1.4 | `C:\Program Files\LLVM` (`bin\libclang.dll`) |

CMake is **not** required: `sherpa-onnx-sys` is built with its
`download-binaries` feature (via `shared`), so it links the pinned prebuilt
archive instead of configuring a CMake build.

## 1. rustup (no elevation)

```powershell
Invoke-WebRequest -Uri "https://win.rustup.rs/x86_64" `
  -OutFile "$env:TEMP\rustup-init.exe" -UseBasicParsing
& "$env:TEMP\rustup-init.exe" -y --default-toolchain none --profile minimal
```

`--default-toolchain none` leaves the toolchain choice to the repository. The
pinned channel in `rust-toolchain.toml` (1.97.1, components `clippy` and
`rustfmt`) is installed automatically the first time any `cargo` or `rustc`
command runs inside the repository:

```powershell
cd G:\GitHub\VocalCode
cargo --version   # syncs 1.97.1-x86_64-pc-windows-msvc and installs clippy/rustfmt
```

A shell that already existed before rustup ran must be restarted (or have
`%USERPROFILE%\.cargo\bin` prepended to `PATH`) to see `cargo`.

## 2. MSVC C++ Build Tools (elevation / UAC required)

```powershell
Invoke-WebRequest -Uri "https://aka.ms/vs/17/release/vs_BuildTools.exe" `
  -OutFile "$env:TEMP\vs_BuildTools.exe" -UseBasicParsing

Start-Process -FilePath "$env:TEMP\vs_BuildTools.exe" -Verb RunAs -Wait -PassThru `
  -ArgumentList '--quiet','--wait','--norestart','--nocache',
                '--add','Microsoft.VisualStudio.Workload.VCTools','--includeRecommended'
```

`Start-Process -Verb RunAs` raises a UAC prompt; the install must be approved.
It ran for roughly 15 minutes here. Do not pass `--log <path>` to this
bootstrapper version: it is rejected as an unknown option and the process exits
with code 87 (`ERROR_INVALID_PARAMETER`) before installing anything. The
bootstrapper's own logs land in `%TEMP%\dd_bootstrapper_*.log` and
`%TEMP%\dd_installer_*.log`.

Verification:

```powershell
& "C:\Program Files (x86)\Microsoft Visual Studio\Installer\vswhere.exe" `
  -products * -requires Microsoft.VisualStudio.Component.VC.Tools.x86.x64 `
  -property installationVersion
# 17.14.37827.9

& "C:\Program Files (x86)\Microsoft Visual Studio\Installer\vswhere.exe" `
  -products * -requires Microsoft.VisualStudio.Component.Windows11SDK.26100 `
  -property installationPath
# C:\Program Files (x86)\Microsoft Visual Studio\2022\BuildTools
```

## 3. LLVM / libclang (elevation / UAC required)

`.cargo/config.toml` sets `LIBCLANG_PATH = 'C:\Program Files\LLVM\bin'`, which is
the default location of the official installer. A `LIBCLANG_PATH` that exists
but contains no `libclang` shadows clang-sys's default search list, so the
install must be at that exact path (or `LIBCLANG_PATH` must be set explicitly).

```powershell
# https://github.com/llvm/llvm-project/releases/download/llvmorg-23.1.4/LLVM-23.1.4-win64.msi
Start-Process -FilePath "msiexec.exe" -Verb RunAs -Wait -PassThru `
  -ArgumentList '/i',"`"$env:TEMP\LLVM-win64.msi`"",'/qn','/norestart'
```

Verification:

```powershell
Test-Path "C:\Program Files\LLVM\bin\libclang.dll"   # True
```

`winget install --id LLVM.LLVM --scope machine --accept-package-agreements --accept-source-agreements`
installs the same MSI if a direct download is undesirable.

## 4. Building

No developer shell is needed. `rustc` locates the MSVC toolchain through
vswhere/registry on its own, and `cc`-based build scripts do the same, so a
plain PowerShell session with `%USERPROFILE%\.cargo\bin` on `PATH` is enough.
A `vcvars64.bat` environment still works if preferred:

```powershell
cmd /c '"C:\Program Files (x86)\Microsoft Visual Studio\2022\BuildTools\VC\Auxiliary\Build\vcvars64.bat" && pwsh'
```

Build commands (from the repository root):

```powershell
cargo build -p vocalcode-app --locked --features community           # debug
cargo build -p vocalcode-app --release --locked --features community # release
```

`community` is also the crate's default feature; passing it explicitly matches
`BUILDING.md`.

Observed durations on this machine (warm Cargo registry, dependencies already
fetched with `cargo fetch --locked`):

- debug: about 70 s after dependencies were compiled
- release: about 2 m 36 s (release profile uses `lto = "thin"`,
  `codegen-units = 1`, `strip = true`, `panic = "abort"`)

A genuinely cold first build is longer: Cargo downloads ~400 crates, and the
`sherpa-onnx-sys` build script downloads and checksum-verifies
`sherpa-onnx-v1.13.6-win-x64-shared-MD-Release-no-tts.tar.bz2` from the
`sherpa-onnx` GitHub releases before extracting it into the build's `OUT_DIR`
(the verified archive is cached there, so later builds reuse it). Network
access is required for that step; `--offline` does not make an empty machine
self-contained.

## 5. Output layout

- Debug executable: `target\debug\vocalcode-app.exe` (~32 MB)
- Release executable: `target\release\vocalcode-app.exe` (~12.5 MB, stripped)

The four runtime DLLs are copied next to the executable by the
`sherpa-onnx-sys` build script (and into `deps\` and `examples\` for tests):

- `onnxruntime.dll` (~16 MB)
- `onnxruntime_providers_shared.dll`
- `sherpa-onnx-c-api.dll` (~2.7 MB)
- `sherpa-onnx-cxx-api.dll`

The executable is not portable on its own: those DLLs must stay adjacent, and an
installed VC++ runtime may be needed on a machine without the Build Tools.

## 6. Verifying the binary

```powershell
& .\target\release\vocalcode-app.exe --build-info
```

Prints (exit code 0):

```json
{"data_directory":"VocalCode Dev","edition":"community","identity":"development","update_manifest":"https://github.com/wudaming00/vocalcode-community/releases/latest/download/latest.json","version":"1.4.1"}
```

`"identity": "development"` is what a non-packaging build must report;
`--build-info` opens no devices, hooks no keys and creates no user-data folder,
so it is the safe smoke check.

## Caveats

- **Do not capture `--build-info` output into a PowerShell variable.**
  `$o = & .\target\release\vocalcode-app.exe --build-info` closes the pipe early
  and the process panics with `failed printing to stdout: The pipe is being
  closed. (os error 232)`. Run it directly, or redirect with
  `Start-Process -RedirectStandardOutput <file>`.
- **UAC prompts.** Installing the Build Tools and LLVM both need elevation; if
  the prompts are never approved, `Start-Process -Verb RunAs` reports the
  elevation was cancelled and nothing is installed.
- **`LIBCLANG_PATH`.** Fixed at `C:\Program Files\LLVM\bin` in
  `.cargo/config.toml`. Cargo's `[env]` is `force = false`, so an exported
  `LIBCLANG_PATH` overrides it; keep the install at the default path or export
  the variable yourself.
- **Debug CRT.** `sherpa-onnx-sys` links `msvcrtd` when its own
  `debug-assertions` are on, while the downloaded DLLs are Release/MD. The
  workspace already sets `debug-assertions = false` for that package in the
  `dev` and `test` profiles, so no debug CRT dependency is introduced.
- **Development identity.** A development build keeps its data in
  `%LOCALAPPDATA%\VocalCode Dev` and its own login item and mutex, so it does
  not touch an installed VocalCode. Both hook the talk key, so quit the
  installed app before dictating with a development build.
- **Disk.** A full debug + release build of this workspace uses several GB under
  `target\`.
