# Sushi

Sushi is a community fork of DexNom's VIctory runtime, based on Marvisak's VIctory for Windows x64. It runs an Xbox Series executable from your own game installation and translates its GPU work for Direct3D 12. It builds on the Radeon runtime tested at `e061c69032b84ec6e2ec17340469cd689500dafd` and adds fixes for static sampler selection, wave64 lane masks and integer render targets. Game files, executables, saves and generated shader caches stay on your own computer.

## Run it on your PC

1. Use 64-bit Windows and a current Direct3D 12 driver. The game uses mesh shaders and Shader Model 6.6. The Radeon gameplay results and known issues are listed below.
2. Install Rust for Windows and Visual Studio Build Tools. In the Visual Studio installer, select **Desktop development with C++**, including the x64 MSVC toolset and a Windows 10 or 11 SDK. [Rust installer](https://rustup.rs/) · [Microsoft C++ Build Tools](https://visualstudio.microsoft.com/visual-cpp-build-tools/)
3. Download this repository using **Code → Download ZIP**, extract the ZIP to a writable folder, and double-click **Setup.cmd**. Select the Series X game executable inside your own extracted game installation. Keep its accompanying files together. Setup downloads the pinned build dependencies, verifies the BCPack format tables, and compiles the three Windows programs locally. [Download source ZIP](https://github.com/TheFrenchSushii/VIctory/archive/refs/heads/sushi.zip)
4. Open **Sushi** from the Windows Start menu. Use a controller for the game's first-time setup. The launcher and your local data are in `%LOCALAPPDATA%\Sushi`. Saves are in `state`, shader caches in `cache`, and per-run diagnostics in `logs`. The file `logs\latest.txt` points to the latest game log.

To update, close the runtime, extract the new source ZIP, and run **Setup.cmd** again. If updating an older custom installation, move the old installation directory to `%LOCALAPPDATA%\Sushi` first (only if the destination does not already exist), preserving `state`, `cache`, `logs` and `game-path.txt`. It reuses the saved game path and the same save directory. To select a different installation, pass its executable path to Setup from PowerShell:

```powershell
.\Setup.cmd -GamePath "D:\My Games\Series X\game_scarlett_master.exe"
```

To prepare the shader cache and then automatically start the game, open **Sushi (Compile shaders and play)** from the Start menu, or run `victory-warm.exe` from the installed runtime directory. The launcher checks the saved game executable, essential GTA VI archives, and separate cache, save and log directories before preloading. It displays the current phase, actual per-file candidate progress and running totals in the console. After a successful compiler exit and nonempty final summary, it starts the game automatically under the same session lock. On subsequent launches, the warmer first checks its local `cache/warmup-complete-v2.txt` marker against the installed runtime binary, game file inventory (names, sizes and modification times), runtime environment settings and previously recorded shader cache entries. If these checks pass, it prints the previous totals, including refusals, and starts the game using the cached translations. New entries added during gameplay are allowed. Old caches without a marker need one successful warmup to establish this record, reusing compatible translations. No existing cache entries are deleted by this check. The marker is not a full content checksum of every archive or cache entry, and the runtime/driver still validates individual shader and pipeline data at use time. If preloading fails, it does not start the game. Unsupported shaders may still be reported even after a successful preload; this is not a guarantee of correct rendering.

Optional launcher modes, from PowerShell in the installation directory:

```powershell
.\victory-warm.exe --check-paths  # validate paths without compiling or playing
.\victory-warm.exe --warm-only    # verify or warm the cache without starting the game
.\victory-warm.exe --force-warm   # force a full warmup pass, then play (reuse compatible cache entries)
.\victory-play.exe               # start the game directly using the existing cache
```

If the saved game path is incorrect, run Setup.cmd again with the correct `-GamePath`. The warmup and game retain separate logs (`logs\latest-warm.txt` and `logs\latest.txt`). Graphics pipelines also depend on draw state encountered during play. If you see a missing `VCRUNTIME140.dll` error, install Microsoft's x64 redistributable. [Microsoft Visual C++ Redistributable](https://learn.microsoft.com/en-us/cpp/windows/latest-supported-vc-redist)

For a manual build, run `.\Initialize-FormatTables.ps1`, then `cargo build --locked --release -p victory --features diagnostics --bins`. The release binaries are in `target\release`. Place all three together outside the checkout and pass the game executable path to `victory-play.exe`. Custom data and cache directories are documented in the configuration example. [Configuration example](victory.example.toml)

## Experimental performance profile

Run the installed launcher from PowerShell to test a lower reported display mode and parallel CPU preparation, without deleting the cache:

```powershell
& "$env:LOCALAPPDATA\Sushi\victory-play.exe" --performance
```

The profile sets `VICTORY_DISPLAY_MODE=1280x720`, `VICTORY_FILL_WORKERS=4` and `VICTORY_PARALLEL_PRELUDES=1` unless explicitly overridden in the environment. These are existing runtime options, not a forced internal rendering resolution; the title may ignore the display mode. Compare the same scene with and without the profile. The default launcher remains unchanged. `victory-warm.exe --performance` also accepts the profile.

Sushi now excludes unsupported mesh pipelines from the pipeline worker queue and early-start list when the host capability query rejects them. The final D3D12 capability guard is retained. This reduces pointless work but does **not** emulate mesh shaders or restore missing geometry on GPUs without mesh-shader support. The incomplete pipeline creation methods and other rejected host descriptors/pipelines require separate implementation and validation. No FPS improvement has been measured on a GTX 1080 Ti for these changes.

## Graphics compatibility diagnostics

Empty buffer descriptors are now rejected before allocating a backing buffer, and UAV rebasing checks for integer overflow. The texture uploader rechecks its video-memory budget in batches of 32 candidates, rather than relying on a single sample for an entire upload pass. These changes do not make an invalid descriptor valid or guarantee that more textures will fit.

For a diagnostic run with the Windows Direct3D debug layer installed:

```powershell
$env:VICTORY_D3D12_DEBUG = '1'
& "$env:LOCALAPPDATA\Sushi\victory-play.exe" --performance
Remove-Item Env:VICTORY_D3D12_DEBUG -ErrorAction SilentlyContinue
```

Install the Windows optional **Graphics Tools** feature if the runtime reports that the debug layer is unavailable. Pipeline rejection logs now include shader identifiers and the relevant pipeline/root-signature context. Debug runs are not suitable for measuring FPS. Known unresolved issues include typed 12-byte SRVs whose shared backing is not aligned to an element, unimplemented console pipeline creation methods, and mesh shader emulation on GPUs without native support. Do not bypass the capability/descriptor checks to hide these errors.

## Try experimental ray tracing on Radeon

After running **Setup.cmd**, open **Sushi (Experimental RT)** from the Windows Start menu. This starts the game with the acceleration-structure builder, ray traversal and lighting resolve enabled. The shortcut uses your existing saves. Open **Sushi** for the usual renderer.

To start the same RT configuration from PowerShell:

```powershell
& "$env:LOCALAPPDATA\Sushi\victory-play.exe" --rt
```

Chapter 1 City produced in-range ray and triangle intersections on an RX 7900 XTX. The game builds and traverses its acceleration structures through translated Direct3D 12 compute shaders. Frame times can still reach several seconds. Interior lighting, shadows and performance vary by scene. [Ray-tracing implementation](docs/fork-ray-tracing.md) · [City testing](BENCHMARK.md)

## Changes in this snapshot

- Pixel shaders that read neighboring pixels in a 2×2 group now preserve the values at triangle edges. On an RX 7900 XTX, this reduced incorrect edge pixels from 32 to zero in each of two adjoining-triangle tests. Cube-map sampling also selects the correct mip level for explicit gradients and honors the shader's minimum mip level.
- The ray-tracing memory directory can grow beyond 1,023 backing buffers. Each recorded draw or dispatch uses only the views available when it was recorded. Older command lists keep their original views.
- Streamed textures can upload when only their middle pages are resident. The runtime merges overlapping memory notifications and rescans tracked textures if the notification queue fills. Previously used depth-only pipelines now warm ahead of speculative material pipelines to reduce missing shadows during loading.
- Some dock objects still appear black in the City save. The same distant dock fixtures appeared dark with the ray-structure builder disabled. The effect of these changes on those objects and on gameplay FPS is still being tested.
- GPU readback identifies changed pages and combines adjacent copy ranges. This targets CPU/GPU synchronization and transfer overhead.
- Ray-structure builder dispatches now honor the game's thread counts and mask excess lanes in the final group. Read-only traversal no longer declares the entire guest memory directory writable when the shader sends its output to another buffer or texture.
- Texture handling searches for contiguous descriptor slots and admits already-resident textures under memory pressure. Earlier changes address publication order, placed-texture ownership and supported render-XOR layouts.
- Shaders can choose between two static samplers across a uniform branch. This recovered 40 shader variants from the tested archive. Another fix recovered one voxel visibility shader by recognizing a saved and restored wave64 lane mask. Mixed render targets retain their individual write masks when an integer target bypasses blending.
- The runtime preserves the game's vertex output interpolation and fixes output signature ordering. All 2,191 archive vertex shaders passed the Microsoft DXIL validator, including 18 water shaders. New textures and retries share the upload budget, and a previously refused buffer view is checked again when its backing allocation changes.
- The runtime resolves guest addresses from ray-tracing descriptors, preserves raw buffer offsets and prepares the memory each command list reads. Texture uploads can proceed while Windows remaps unrelated memory. In-game ray-traced lighting is still experimental.
- Five additional shadow compute shaders compile and create pipelines on the RX 7900 XTX. The translated ray-node regression also exercises three reflections through the guest-page directory. Changes confined to the standalone execution tests now preserve cached shader translations.
- Shader translation preserves the guest wave width for lane permutations and handles heap-indexed texture-size queries. Save handling flushes copied blobs before publication and can recover a complete checkpoint.

The source ZIP builds with the Windows setup script. The test report covers shader validation, a Radeon pipeline check, gameplay frame times and the remaining interior lighting issue. [Test and benchmark report](BENCHMARK.md)

### Testing CPU submission workers

For a local experiment, open PowerShell and run the following after setup. The process uses four CPU workers for eligible upload batches and can record independent transition command lists on the shared worker pool. The final GPU queue, fence signals and resource transitions retain the game's submission order. Compare the same save and camera after the initial streaming period. The default launcher uses the baseline settings.

```powershell
$env:VICTORY_FILL_WORKERS = '4'
$env:VICTORY_PARALLEL_PRELUDES = '1'
& "$env:LOCALAPPDATA\Sushi\victory-play.exe"
```

## Credits and licensing

- DexNom developed the upstream runtime used by Sushi. [DexNom/VIctory](https://github.com/DexNom/VIctory)

- Marvisak and the VIctory contributors created the original runtime. [Marvisak/VIctory](https://github.com/Marvisak/VIctory)
- ZSlayerHQ and the contributors to the victory-world line provided the rendering baseline used here, including the Radeon work from triippywhale and asdaasdsaddf. [victory-world-runtime](https://github.com/ZSlayerHQ/victory-world-runtime) · [Integrated revision](https://github.com/ZSlayerHQ/victory-world-runtime/commit/99f8834aa16b98d7f3943b8de262a8d4857bda7b)
- The XCL contributors published an independent Xbox Series compatibility layer. Their work informed our investigation of shader translation, CPU/GPU memory synchronization and performance. [xcl-dev/xcl](https://github.com/xcl-dev/xcl)

The upstream README preserves the original credits and historical test results. [Upstream README](README.upstream.md)

VIctory source is licensed under GPL-3.0-or-later. The upstream project identifies the required BCPack format tables as separately supplied data outside that license. This source archive omits that file. The setup script verifies a local or upstream copy before use. [License](LICENSE) · [Upstream asset policy and table notice](docs/asset-policy.md)
