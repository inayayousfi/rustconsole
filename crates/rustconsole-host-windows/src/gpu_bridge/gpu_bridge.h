#pragma once

#include <d3d11.h>
#include <stdint.h>

struct RustConsoleGpuBridge;

extern "C" HRESULT rustconsole_gpu_bridge_create(
    RustConsoleGpuBridge** bridge,
    ID3D11Device** encoder_device,
    int32_t normal_desktop,
    uint32_t* width,
    uint32_t* height,
    uint32_t* refresh_rate,
    uint32_t* capture_engine,
    uint32_t* video_format,
    uint32_t* video_color);

extern "C" HRESULT rustconsole_gpu_bridge_capture(
    RustConsoleGpuBridge* bridge,
    uint32_t timeout_millis,
    ID3D11Texture2D** encoder_texture,
    int64_t* last_present_time,
    uint32_t* accumulated_frames,
    int32_t* protected_content_masked);

extern "C" HRESULT rustconsole_gpu_bridge_capture_external(
    RustConsoleGpuBridge* bridge,
    uint32_t timeout_millis,
    int64_t* last_present_time,
    uint32_t* accumulated_frames,
    int32_t* protected_content_masked);

extern "C" uintptr_t rustconsole_gpu_bridge_output_handle(
    RustConsoleGpuBridge* bridge);

extern "C" HRESULT rustconsole_gpu_bridge_open_external(
    RustConsoleGpuBridge** bridge,
    ID3D11Device** encoder_device,
    HANDLE shared_texture,
    uint32_t width,
    uint32_t height,
    uint32_t refresh_rate,
    uint32_t video_format,
    uint32_t video_color);

extern "C" HRESULT rustconsole_gpu_bridge_acquire_external(
    RustConsoleGpuBridge* bridge,
    uint32_t timeout_millis,
    ID3D11Texture2D** encoder_texture);

extern "C" HRESULT rustconsole_gpu_bridge_release_encoder_texture(
    RustConsoleGpuBridge* bridge);

extern "C" void rustconsole_gpu_bridge_destroy(RustConsoleGpuBridge* bridge);

extern "C" const char* rustconsole_gpu_bridge_failure_stage();

extern "C" uint32_t rustconsole_gpu_bridge_reconfiguration_cause(
    RustConsoleGpuBridge* bridge);
