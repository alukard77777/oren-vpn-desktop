# Desktop renderer

The desktop app draws with Slint's **Skia** renderer (Slint 1.17, `renderer-skia`),
which replaced FemtoVG for its text: glyphs come from the platform rasterizer
(DirectWrite on Windows, FreeType/fontconfig on Linux), so small text is crisper and
reaches full contrast, especially at 125% scaling.

| Platform | GPU path | When it can't start |
| --- | --- | --- |
| Windows x64, x86 and arm64 | Skia on Direct3D 12: a hardware adapter with feature level 11, else Microsoft's WARP software adapter | restart with Slint's software renderer |
| Linux | Skia on OpenGL (glutin, EGL/GLX); Skia itself drops to its CPU raster surface without a GL context | restart with Slint's software renderer |

Windows selects Direct3D explicitly (`renderer_fallback::select_gpu_renderer`); without
that request Slint's Skia renderer draws on the CPU there. Direct3D 12 with the WARP
fallback removes the old OpenGL 2.0 failure (basic display adapter, Hyper-V guests,
RDP). The software-renderer fallback stays for broken drivers: a GPU start failure is
recorded in `<data root>/software-renderer` and the app restarts itself with
`MULTICORE_SOFTWARE_RENDERER=1`; either one selects the software renderer.
`SLINT_BACKEND` (for example `winit-software`) overrides the selection for diagnostics.

A renderer can also fail later, while drawing: Skia's Direct3D surface returns an error
for a failed `Present`, fence or swap-chain call (for example `Error presenting d3d swap
chain: 887a0005` once the driver removed the device), and Slint's winit backend leaves the
event loop on it. Up to 1.1.7 that closed the app without a trace. `event_loop_guard` runs
the loop until the app itself quits (`event_loop_guard::request_quit`): any other exit is
written to `logs/crash.log`, every window gets a new native window and renderer (a new
Direct3D device), and the loop runs again. After three such recoveries within ten minutes
the error is final, and one that names the GPU renderer (matched without regard to case)
restarts the app with the software renderer as above.

## Skia binaries in release builds

`skia-bindings` normally downloads prebuilt Skia from `rust-skia/skia-binaries`. The
release builds pin the archive by SHA-256 instead (`build.skiaBinaries` in each
platform's `packaging/*/versions.json`) and hand it to `skia-bindings` through
`SKIA_BINARIES_URL=file://...`; after the build they check that the linked
`out/skia/key.txt` is the pinned key.

- **Linux**: the rust-skia prebuilt archive for `x86_64-unknown-linux-gnu`
  (`gl-jpegd-jpege-pdf-textlayout-vulkan`). `scripts/package-linux-release.sh`
  downloads it into its download cache and verifies the hash; the Docker build needs
  network access once. The binary links the system `libfreetype.so.6`,
  `libfontconfig.so.1` and `libstdc++.so.6` (package dependencies list them).
- **Windows**: rust-skia publishes no prebuilt Skia for the static C runtime
  (`+crt-static`, which the release needs: clean Windows has no `vcruntime140.dll`) and
  none for 32-bit or ARM64 Windows. `scripts/build-skia-binaries.ps1 -Architecture x64|x86|arm64
  -OutputDirectory <cache> -LlvmHome <LLVM>` builds the pinned Skia source
  (`rust-skia/skia` `m150-0.98.1`) with `skia-bindings`' own offline build (clang-cl
  from LLVM 23.1.2, about 3 minutes per architecture on 12-16 cores; needs git, Python 3
  and network for Skia's third-party sources) and packs it like a download.
  `scripts/package-windows-release.ps1` needs that directory as
  `-SkiaBinariesDirectory` or `MULTICORE_SKIA_BINARIES_DIR` and refuses an archive
  whose SHA-256 differs from the pin. The Skia build is not bit-reproducible: building
  the archive again means updating `build.skiaBinaries.sha256`.
- **Windows on ARM (arm64)**: cross-compiled on an x64 host for `aarch64-pc-windows-msvc`.
  The Skia archive is built like the others (`-Architecture arm64`); clang-cl compiles
  Skia and skia-bindings' `bindings.cpp` and llvm-lib archives it, so building the archive
  needs no MSVC ARM64 compiler, only the architecture-neutral MSVC headers. Linking
  MultiCore for ARM64 (`package-windows-release.ps1 -Architecture arm64`, `cargo test
  --no-run`) needs the Visual Studio component "MSVC v143 - VS 2022 C++ ARM64 build tools"
  (`Microsoft.VisualStudio.Component.VC.Tools.ARM64`) and LLVM `clang.exe` on `PATH`:
  `ring` compiles its ARM64 code only with clang, so the packager points
  `CC_aarch64_pc_windows_msvc` at it with clang's path-remapping flags. An x64 build host
  cannot run the ARM64 tests or executables; they are built and linted there and need an
  ARM64 Windows machine to run.

Bumping Slint or `skia-safe` changes the key and the archives: rebuild/download them
and update all four pins (Linux, Windows x64, x86 and arm64).
