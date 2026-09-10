#define NOMINMAX

#include "gpu_bridge.h"

#include <windows.h>
#include <shellapi.h>

#include <array>
#include <cstdint>
#include <cwchar>

#pragma pack(push, 1)
struct HelperHello {
    uint32_t magic;
    uint32_t version;
    std::array<uint8_t, 16> token;
    uint64_t shared_handle;
    uint32_t width;
    uint32_t height;
    uint32_t refresh_rate;
    uint32_t capture_engine;
    uint32_t video_format;
    uint32_t video_color;
    int32_t result;
    char failure_stage[192];
};

struct HelperFrame {
    uint32_t magic;
    int32_t result;
    int64_t last_present_time;
    uint32_t accumulated_frames;
    int32_t protected_content_masked;
    uint32_t reconfiguration_cause;
    uint64_t capture_acquisition_micros;
    uint64_t cross_adapter_copy_micros;
    uint64_t color_conversion_micros;
    char failure_stage[192];
};
#pragma pack(pop)

static constexpr uint32_t HELLO_MAGIC = 0x48474352;
static constexpr uint32_t FRAME_MAGIC = 0x46474352;

static bool decode_token(const wchar_t* text, std::array<uint8_t, 16>* token) {
    if (!text || wcslen(text) != 32) return false;
    for (size_t index = 0; index < token->size(); ++index) {
        wchar_t pair[3] = {text[index * 2], text[index * 2 + 1], 0};
        wchar_t* end = nullptr;
        const unsigned long value = wcstoul(pair, &end, 16);
        if (!end || *end != 0 || value > 0xff) return false;
        (*token)[index] = static_cast<uint8_t>(value);
    }
    return true;
}

static void copy_stage(char destination[192]) {
    const char* stage = rustconsole_gpu_bridge_failure_stage();
    if (stage) strncpy_s(destination, 192, stage, _TRUNCATE);
}

static bool write_all(HANDLE pipe, const void* value, DWORD size) {
    const auto* cursor = static_cast<const uint8_t*>(value);
    while (size != 0) {
        DWORD written = 0;
        if (!WriteFile(pipe, cursor, size, &written, nullptr) || written == 0) return false;
        cursor += written;
        size -= written;
    }
    return true;
}

static bool read_all(HANDLE pipe, void* value, DWORD size) {
    auto* cursor = static_cast<uint8_t*>(value);
    while (size != 0) {
        DWORD read = 0;
        if (!ReadFile(pipe, cursor, size, &read, nullptr) || read == 0) return false;
        cursor += read;
        size -= read;
    }
    return true;
}

int WINAPI wWinMain(HINSTANCE, HINSTANCE, PWSTR, int) {
    int argument_count = 0;
    wchar_t** arguments = CommandLineToArgvW(GetCommandLineW(), &argument_count);
    if (!arguments || argument_count != 3) return 2;
    std::array<uint8_t, 16> token{};
    if (!decode_token(arguments[2], &token)) {
        LocalFree(arguments);
        return 2;
    }
    const HANDLE pipe = CreateFileW(
        arguments[1], GENERIC_READ | GENERIC_WRITE, 0, nullptr, OPEN_EXISTING, 0, nullptr);
    LocalFree(arguments);
    if (pipe == INVALID_HANDLE_VALUE) return 3;

    RustConsoleGpuBridge* bridge = nullptr;
    ID3D11Device* device = nullptr;
    HelperHello hello{};
    hello.magic = HELLO_MAGIC;
    hello.version = 1;
    hello.token = token;
    hello.result = rustconsole_gpu_bridge_create(
        &bridge, &device, TRUE, &hello.width, &hello.height,
        &hello.refresh_rate, &hello.capture_engine, &hello.video_format,
        &hello.video_color);
    if (device) device->Release();
    hello.shared_handle = rustconsole_gpu_bridge_output_handle(bridge);
    if (FAILED(hello.result)) copy_stage(hello.failure_stage);
    if (!write_all(pipe, &hello, sizeof(hello)) || FAILED(hello.result)) {
        rustconsole_gpu_bridge_destroy(bridge);
        CloseHandle(pipe);
        return 4;
    }

    uint8_t diagnostics = 0;
    if (!read_all(pipe, &diagnostics, sizeof(diagnostics)) || diagnostics > 1) {
        rustconsole_gpu_bridge_destroy(bridge);
        CloseHandle(pipe);
        return 5;
    }
    for (;;) {
        HelperFrame frame{};
        frame.magic = FRAME_MAGIC;
        frame.result = rustconsole_gpu_bridge_capture_external(
            bridge, 250, &frame.last_present_time, &frame.accumulated_frames,
            &frame.protected_content_masked,
            diagnostics ? &frame.capture_acquisition_micros : nullptr,
            diagnostics ? &frame.cross_adapter_copy_micros : nullptr,
            diagnostics ? &frame.color_conversion_micros : nullptr);
        if (frame.result == DXGI_ERROR_WAIT_TIMEOUT) continue;
        if (FAILED(frame.result)) {
            frame.reconfiguration_cause = rustconsole_gpu_bridge_reconfiguration_cause(bridge);
            copy_stage(frame.failure_stage);
        }
        if (!write_all(pipe, &frame, sizeof(frame)) || FAILED(frame.result)) break;
    }
    rustconsole_gpu_bridge_destroy(bridge);
    CloseHandle(pipe);
    return 0;
}
