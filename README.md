<p align="center">
  <img src="logo.svg" alt="Rust Console logo" width="140" height="140">
</p>

<h1 align="center">Rust Console</h1>

<p align="center">
  Open-source remote desktop and game streaming.
</p>

<p align="center">
  Built for the lowest possible latency and the highest possible image quality,<br>
  so using a remote computer feels as close as possible to sitting in front of it.
</p>

<p align="center">
  <a href="https://github.com/inayayousfi/rustconsole/actions/workflows/ci.yml"><img src="https://github.com/inayayousfi/rustconsole/actions/workflows/ci.yml/badge.svg" alt="CI"></a>
  <a href="LICENSE"><img src="https://img.shields.io/badge/license-GPLv3-blue" alt="GPLv3 license"></a>
  <img src="https://img.shields.io/badge/language-Rust-orange" alt="Built in Rust">
  <img src="https://img.shields.io/badge/status-in_development-b00020" alt="In development">
</p>

<p align="center">
  <a href="#overview">Overview</a> ·
  <a href="#features">Features</a> ·
  <a href="#platform-support">Platform support</a> ·
  <a href="#contributing">Contributing</a>
</p>

---

## Overview

Rust Console lets you view and control another computer over a network. Play its games, work with its applications, and use its desktop from another machine, with video, sound, and keyboard and mouse control.

The goal is to make remote use feel like local use. That means keeping the delay between an action and its result as low as possible, preserving image detail, and delivering smooth motion.

Rust Console is written primarily in Rust and released as free, open-source software. Its broader ambition is multiplatform remote desktop and game streaming. The current implementation connects a Windows host to a Linux client.

## Features

| Feature | What it provides |
| --- | --- |
| Low-latency streaming | Video and input handling designed to keep interaction responsive and avoid a growing backlog of old frames. |
| High-quality AV1 video | Hardware-accelerated encoding, decoding, and rendering for desktop and game streaming. |
| High-refresh-rate playback | An initial performance target of 1440p at 120 FPS. |
| Full desktop control | Access to applications, games, and the rest of the remote desktop. |
| Keyboard and mouse input | Desktop pointer control and relative mouse capture for games. |
| System audio | Stereo sound streamed alongside the video. |
| Adaptive bitrate | Streaming bitrate adjusts to network conditions within your chosen maximum. |
| Encrypted connections | Password-authenticated sessions over encrypted transport. |
| Host discovery | Local-network discovery, optional Tailscale discovery, and manual addresses. |
| Native playback | A dedicated player with fullscreen controls and stream statistics. |

## Gaming, work, and everyday use

For gaming, Rust Console focuses on responsive controls, clear video, and smooth playback. Games run on the host computer, and you play them from the client.

For work, you can use software and files on the remote computer while keeping your local operating system and workspace. The full desktop remains available, so you can move between applications within the same connection.

On Windows, remote access also covers the login screen, lock screen, and administrative prompts. The host runs as a service rather than requiring an open desktop application.

## Platform support

The host is the computer being controlled. The client is the computer you use to connect to it; the client side includes the desktop app and its native stream player.

These matrices describe the current implementation, not a promise of universal hardware compatibility.

### Host

| Platform | Desktop capture | AV1 hardware encoding | System audio | Keyboard and mouse | Protected screens |
| --- | :---: | :---: | :---: | :---: | :---: |
| Windows | ✅ | ✅ NVIDIA | ✅¹ | ✅ | ✅ |
| Linux | ❌ | ❌ | ❌ | ❌ | ❌ |
| macOS | ❌ | ❌ | ❌ | ❌ | ❌ |

¹ Windows system audio currently requires the external VB-CABLE Standard software.

### Client

| Platform | AV1 hardware decoding | Native rendering | Audio playback | Keyboard and mouse | Host discovery |
| --- | :---: | :---: | :---: | :---: | :---: |
| Linux | ✅ VA-API | ✅ Vulkan | ✅ | ✅ | ✅ |
| Windows | ❌ | ❌ | ❌ | ❌ | ❌ |
| macOS | ❌ | ❌ | ❌ | ❌ | ❌ |

✅ Implemented. ❌ Not implemented.

Additional platform support is part of the project's ambition. The Windows-to-Linux path is the starting point.

## Project status

Rust Console is under active development. 

The initial performance target is 1440p at 120 FPS. Broader hardware and platform support remain work to be done.

Connections require a reachable host, either directly or through a network such as Tailscale. Rust Console does not currently provide its own relay or automatic NAT traversal.

Rust Console uses its own protocol and is not compatible with Sunshine, Moonlight, or NVIDIA GameStream.

## Contributing

Bug reports, feature proposals, and code contributions are welcome. Platform support, streaming quality, responsiveness, and the user experience are all areas where contributions can help.

The source is also available to study, adapt, and use as inspiration for other projects under its license.

## License

Rust Console is licensed under the [GNU General Public License, version 3 only](LICENSE).

See [THIRD_PARTY.md](THIRD_PARTY.md) for third-party software licenses and external prerequisites.
