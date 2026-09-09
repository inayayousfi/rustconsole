# Rust Console Repository Guide

## Product glossary

- **Client** means `apps/rustconsole-client`: the control application through which the user discovers hosts, chooses connection settings, manages credentials, and starts or supervises the player. It does not render or play the stream itself.
- **Player** or **viewer** means `apps/rustconsole-player`: the interactive stream application that connects to a host, receives and decodes media, presents video, plays audio, and sends local input back to the host.
- **Host** or **server** means `apps/rustconsole-host`: the stream-producing application that accepts player connections, captures and encodes video and audio, sends that media, and applies remote input.
- **Core crate** means a `rustconsole-*-core` library that owns application rules and orchestration without operating-system APIs.
- **Platform crate** means a library ending in `-windows` or `-linux`. It adapts core behavior to that operating system and owns calls to its native APIs.
- **Technology crate** means a library named after a replaceable implementation, such as `-ffmpeg` or `-vulkan`. It implements one capability without deciding application policy.
- **Protocol** means the versioned messages and negotiation rules exchanged between the host and player.
- **Session** means one authenticated host-player connection and its control, media, input, lifecycle, queueing, and statistics behavior.
- **Native component** means code under `native/` that has a build or runtime boundary outside the main Cargo workspace. It is not another ordinary workspace crate.

## Runtime map

The user starts the client. The client discovers and probes hosts through `rustconsole-client-core`, then launches the player as a child process. The client and player exchange launch commands and status events through the bounded process protocol owned by `rustconsole-player-core`.

The player connects directly to the host through the session transport. The host authenticates the connection, negotiates stream settings, captures and encodes video and audio, and sends the media to the player. The player assembles and decodes those packets, presents video, and plays audio. Keyboard and pointer events travel in the opposite direction from the player to the host, where the host input integration applies them.

Keep these process boundaries explicit. The client is the control application, the player is the interactive stream application, and the host is the streaming server. Do not move media rendering into the client or host capture into the player merely because the processes ship as one product.

## Repository map

### Applications

- `apps/rustconsole-client` is the client composition root. It owns the user-interface entry point, commands exposed to that interface, credential-store adaptation, and player process startup. Its current interface is the Tauri application and static web UI under `ui/`.
- `apps/rustconsole-player` is the player composition root. It owns the interactive event loop, window lifecycle, reconnect behavior, input-event translation, and selection of media and rendering implementations. It currently composes SDL, the Linux player integration, and the Vulkan renderer.
- `apps/rustconsole-host` is the host composition root. It owns process startup and command-line parsing, then selects the available host platform implementation. It currently delegates service, installation, firewall, worker, and proof behavior to `rustconsole-host-windows`.
- `apps/rustconsole-test-game` is a Windows test application with predictable graphics, audio, and raw-input behavior. It is product test support, not a reusable application layer.

Application crates are composition roots. They may connect libraries and deal with process startup or UI frameworks, but reusable rules belong in the narrowest library that owns those rules.

### Application orchestration crates

- `crates/rustconsole-client-core` owns platform-neutral host selection, address resolution, discovery composition, host probing, and player child-process supervision. It bridges the Tauri client to player-side connection behavior without depending on Tauri.
- `crates/rustconsole-player-core` owns platform-neutral player session orchestration: authenticated host probing, stream negotiation and reception, media packet assembly, input transmission, and the client-player process protocol.
- `crates/rustconsole-host-core` owns platform-neutral host session rules: session lifecycle and single-stream ownership, adaptive bitrate behavior, bounded queues, and the capture, audio, and input contracts implemented by a host platform.

Core crates define policy and orchestration. They must not call operating-system, user-interface, media-library, or graphics APIs directly.

### Neutral mechanism crates

- `crates/rustconsole-protocol` owns stable domain values, wire messages, protocol and feature versions, AV1 negotiation, and protocol encoding limits. Both ends of a network exchange must agree with this crate.
- `crates/rustconsole-session` owns shared connection mechanics: OPAQUE authentication, QUIC setup, connection binding, media and input datagrams, packet assembly, queue behavior, lifecycle primitives, and transport statistics. It does not decide host or player UI behavior.
- `crates/rustconsole-media` owns platform-neutral media timestamps, audio and video data types, and codec interfaces. It contains no codec or device implementation.
- `crates/rustconsole-discovery` owns discovery sources and candidate merging. It finds possible addresses through manual, LAN, or Tailscale information; authenticated network probing belongs to player orchestration.
- `crates/rustconsole-render` owns platform-neutral player rendering contracts and overlay statistics. It must not contain a concrete graphics API implementation.

Neutral mechanism crates should describe one capability. Do not use them as dumping grounds for application orchestration or platform convenience code.

### Technology implementation crates

- `crates/rustconsole-codec-ffmpeg` owns the FFmpeg implementation of codec and hardware-context behavior, including AV1 and Opus support. It is shared by host encoding and player decoding integrations.
- `crates/rustconsole-render-vulkan` implements the neutral player rendering contract with Vulkan. It owns presentation, color conversion, overlays, swapchain behavior, and a generic frame-import contract. It does not own operating-system-specific frame import.

Technology crates depend toward neutral contracts when one exists. A neutral crate must never depend on a technology implementation.

### Platform crates

- `crates/rustconsole-host-windows` implements the Windows host service, desktop and audio capture, GPU encoding, media workers, credentials, firewall integration, and composition of remote input. It adapts `rustconsole-host-core` contracts to Windows.
- `crates/rustconsole-input-windows` owns the safe host-side virtual-input state, HID report construction, shared-memory communication, and Windows connection to the virtual input driver.
- `crates/rustconsole-player-linux` implements Linux player decoding, DMA-BUF media handling, SDL audio output, input and secret-store integration, and the Linux stream adapter around `rustconsole-player-core`.
- `crates/rustconsole-render-vulkan-linux` imports Linux DMA-BUF decoded frames into the shared Vulkan renderer. Keep Linux external-memory and DRM format details here rather than in `rustconsole-render-vulkan`.

Platform crates may depend on neutral contracts, neutral mechanisms, and the technology implementations they adapt. Neutral crates must not depend back on platform crates.

### Other boundaries

- `native/windows-input-driver` is the Windows UMDF virtual mouse and keyboard driver. It is a separate Cargo workspace because its toolchain, build, installation, and runtime boundary differ from the main workspace. Driver implementation belongs here; safe user-mode state and communication belong in `rustconsole-input-windows`.
- `vendor/ffmpeg-sys-next-9.0.0` is the patched FFmpeg binding used through the workspace patch. Treat it as third-party source, not ordinary project code.
- `patches/` contains maintained patches applied to external projects. Preserve patch format and upstream context.
- `scripts/` contains repository policy and build checks. In particular, `scripts/check-dependency-direction.mjs` is the executable source of truth for allowed internal dependencies.
- `docs/` contains focused design or operational documents that are too detailed for this repository map.

## Naming conventions

Use `rustconsole-<role>` for a deployable application under `apps/`, where the role names the process from the product glossary, such as `client`, `player`, or `host`.

Use `rustconsole-<responsibility>` for a reusable crate under `crates/`. Name the capability it owns, such as `protocol`, `session`, `media`, or `discovery`, rather than the application that first needs it.

Add `-core` only when the crate orchestrates one application's behavior while remaining independent of operating-system and UI APIs. Add `-windows` or `-linux` when the crate adapts behavior to that operating system. Use a technology name such as `-ffmpeg` or `-vulkan` when the crate owns a replaceable implementation based on that technology. Combine suffixes when both distinctions matter, as in `rustconsole-render-vulkan-linux`.

Use `native/<component>` when a component has a separate toolchain, workspace, installation model, or binary boundary that prevents it from behaving like an ordinary workspace crate.

Within a crate, name modules after the responsibility they implement, such as `authentication`, `capture`, `audio_output`, or `video_datagram`. Avoid broad names such as `common`, `shared`, `helpers`, or `utils`; put code in the narrow owner whose reason to change matches the code.

## Choosing where code belongs

1. Start at the process where the behavior is visible: client, player, or host.
2. If the code only wires libraries to a UI, command line, event loop, or child process, keep it in the application crate.
3. If it expresses reusable rules for one application without native APIs, place it in that application's `-core` crate.
4. If multiple applications or implementations need the same data or behavior, define it in the narrow neutral crate that owns that concept: `protocol`, `session`, `media`, `discovery`, or `render`.
5. If it implements a neutral capability with a replaceable library or graphics API, place it in a technology crate that depends on the neutral contract.
6. If it calls an operating-system API or adapts a neutral or technology contract to native resources, place it in the matching platform crate.
7. If it belongs to a separately built native binary rather than its safe application-side interface, place it under `native/`.

Examples of ownership follow directly from these rules: a new network message belongs in `rustconsole-protocol`; session transport behavior belongs in `rustconsole-session`; host stream policy belongs in `rustconsole-host-core`; a rendering operation belongs in the neutral `rustconsole-render` contract; one implementation of that operation belongs in a technology crate; native frame import belongs in a platform adapter; and a UI command that only exposes existing client behavior belongs in `apps/rustconsole-client`.

Before adding an internal dependency, inspect the caller, the proposed dependency, and `scripts/check-dependency-direction.mjs`. A new edge is an architecture decision. Do not change the policy merely to make a check pass; move the responsibility to the correct owner or establish why the new direction is intended.

If work on any task reveals that neutral policy depends directly on a platform or technology implementation, report the exact dependency and its consequence to the user. Do not hide the coupling and do not expand the task to refactor it without the user's decision.

## Boundary rules

- Keep wire parsing, packet assembly, queues, and allocations bounded. Protocol changes must handle compatibility deliberately and update permanent protocol fixtures when their encoded form changes.
- Treat authentication, credentials, connection binding, input release, timeouts, queue limits, and cleanup as security or reliability boundaries. Do not weaken them as a shortcut.
- Keep native APIs in platform or native components. A build on one operating system does not prove that another operating-system implementation works.
- Change generated files through their source and regeneration path. This includes Tauri schemas under `apps/rustconsole-client/gen/schemas` and Vulkan shader binaries paired with shader source.
- Limit `vendor/` and `patches/` changes to concrete external compatibility needs. Preserve upstream licenses, provenance, and valid patch formatting.
- Keep credentials, real network discovery output, account names, machine paths, browser artifacts, screenshots, logs, and local reports out of committed source and fixtures.
- Keep `THIRD_PARTY.md` synchronized with bundled libraries, assets, patches, native link requirements, and external prerequisites. Do not bundle proprietary prerequisites without authorization and matching distribution records.
- Validate the smallest affected owner first, then the repository checks that cover every changed boundary. State which operating-system code and hardware behavior were not compiled or exercised; never present one platform's validation as cross-platform evidence.
