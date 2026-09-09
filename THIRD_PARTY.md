# Third-Party Software

Rust Console is licensed under GPL version 3 only. The `LICENSE` file contains those terms.

Rust Console uses `ffmpeg-next` 9.0.0, licensed under the WTFPL, to call native FFmpeg libraries in-process. Default features are disabled; the project enables the crate's format feature because the released codec-only configuration does not compile, and that feature also enables the codec API. Hardware device and frame operations use the low-level FFmpeg interface re-exported by that crate where no safe wrapper exists.

Rust Console uses `sdl3` 0.18.4, licensed under MIT, to create the native player window, receive native events, create the Vulkan surface, and run the Windows fullscreen stress application. The player and Windows stress application build SDL 3.4.14 statically from the source package locked by Cargo. SDL uses the zlib license.

Rust Console uses `egui` 0.33.0 under its `MIT OR Apache-2.0` license expression to define the player controls and diagnostic presentation independently from the graphics backend. Its default-font feature embeds Hack Regular, Noto Emoji Regular, Ubuntu Light, and emoji-icon-font through `epaint_default_fonts` 0.33.3. That package is distributed under `(MIT OR Apache-2.0) AND OFL-1.1 AND Ubuntu-font-1.0` and includes the corresponding font notices in its Cargo source package.

Rust Console uses `egui-ash-renderer` 0.10.0, licensed under MIT, to record egui drawing commands into the existing Vulkan render pass. Rust Console applies its HDR10 color conversion to egui mesh colors before those commands are recorded.

Rust Console uses `backon` 1.6.0, licensed under Apache-2.0, to generate bounded exponential reconnection delays. Its `fastrand` 2.5.0 dependency, licensed under `Apache-2.0 OR MIT`, supplies the random jitter. Only BackON's `std` feature is enabled; its sleeping and asynchronous runtime integrations are disabled.

Rust Console vendors `ffmpeg-sys-next` 9.0.0 under `vendor/ffmpeg-sys-next-9.0.0`, licensed under the WTFPL. Its `hwcontext_wrapper.h` excludes the VA-API wrapper when `_WIN32` is defined because the verified Windows vcpkg FFmpeg package installs `hwcontext_vaapi.h` without the Linux-only `va/va_x11.h` dependency. D3D11VA bindings remain enabled. Its Windows vcpkg link list includes `ncrypt`, `crypt32`, `mfuuid`, and `strmiids`, which are static system dependencies reported by the pinned FFmpeg 8.1.2 package but omitted by the released binding. No native FFmpeg source or behavior is changed.

The current Linux development machine provides FFmpeg 9.0.1 through the CachyOS package. Its `ffmpeg -L` output identifies that build as GPL version 3 or later. The current Windows development machine provides static FFmpeg 8.1.2#3 through the vcpkg registry at commit `9e593bb18ea69cc5095e012465dcd675a822ed0d`. The repository manifest disables default features and enables only `avcodec`, `avformat`, `nvcodec`, and `opus`; `scripts/check-windows-ffmpeg.ps1` requires the static archive to contain both `ff_av1_nvenc_encoder` and `ff_libopus_encoder`. Package metadata, copyright terms, and the installed software bill of materials remain under the vcpkg installation.

Rust Console uses libopus through FFmpeg's library API. Windows statically links libopus 1.5.2#1 from the pinned vcpkg registry. The source is https://github.com/xiph/opus at tag v1.5.2; the package applies `fix-pkgconfig-version.patch` to package metadata. Libopus uses the BSD-3-Clause license. Its installed `share/opus/copyright` file includes the copyright notice, redistribution conditions, disclaimer, and patent-license references, which must accompany applicable distributions. The current Linux development machine provides libopus 1.6.1 through its shared-library installation.

Rust Console uses VB-CABLE Standard as an external Windows prerequisite. The inspected official package is `VBCABLE_Driver_Pack45.zip`, driver version 3.3.1.7, with SHA-256 `b950e39f01af1d04ea623c8f6d8eb9b6ea5c477c637295fabf20631c85116bfb`. VB-CABLE is proprietary donationware from VB-Audio Software. It is not part of this repository. The package readme requires written authorization from VB-Audio before integration into another installation procedure, so Rust Console must not bundle or redistribute it until that permission and the applicable attribution, donation, and professional-use terms are recorded. The application detects the external prerequisite and directs users to the official product page at https://www.vb-cable.com and the official licensing and donation page at https://vb-audio.com/Services/licensing.htm. It does not contain the package, a direct package download, or a VB-CABLE installation procedure. This decision covers VB-CABLE Standard only, not the A+B or C+D packs.

Rust Console embeds the Hugeicons `reload.svg` icon in `apps/rustconsole-client/ui/icons/reload.svg`. The unmodified icon comes from https://github.com/hugeicons/hugeicons/blob/b2462ece29de25fff2cc716da4452d6ada210c7e/icons/reload.svg and is licensed under the MIT License:

```text
MIT License

Copyright (c) 2025 Hugeicons

Permission is hereby granted, free of charge, to any person obtaining a copy
of this software and associated documentation files (the "Software"), to deal
in the Software without restriction, including without limitation the rights
to use, copy, modify, merge, publish, distribute, sublicense, and/or sell
copies of the Software, and to permit persons to whom the Software is
furnished to do so, subject to the following conditions:

The above copyright notice and this permission notice shall be included in all
copies or substantial portions of the Software.

THE SOFTWARE IS PROVIDED "AS IS", WITHOUT WARRANTY OF ANY KIND, EXPRESS OR
IMPLIED, INCLUDING BUT NOT LIMITED TO THE WARRANTIES OF MERCHANTABILITY,
FITNESS FOR A PARTICULAR PURPOSE AND NONINFRINGEMENT. IN NO EVENT SHALL THE
AUTHORS OR COPYRIGHT HOLDERS BE LIABLE FOR ANY CLAIM, DAMAGES OR OTHER
LIABILITY, WHETHER IN AN ACTION OF CONTRACT, TORT OR OTHERWISE, ARISING FROM,
OUT OF OR IN CONNECTION WITH THE SOFTWARE OR THE USE OR OTHER DEALINGS IN THE
SOFTWARE.
```

These development installations are not release source offers. Before distributing a Rust Console binary, the release process must record the exact FFmpeg and external-library binaries included or required, preserve their copyright and license notices, provide the corresponding source and local changes through the same distribution channel where required, and preserve the build configuration needed to reproduce them. A release must stop if its native libraries cannot be matched to those records.
