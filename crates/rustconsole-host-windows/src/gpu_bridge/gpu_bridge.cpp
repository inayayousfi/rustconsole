#include "gpu_bridge.h"

#define NOMINMAX

#include <d3d11_1.h>
#include <d3d11_4.h>
#include <d3d11on12.h>
#include <d3d12.h>
#include <d3dcompiler.h>
#include <dxgi1_6.h>
#include <roapi.h>
#include <windows.graphics.capture.interop.h>
#include <windows.graphics.directx.direct3d11.interop.h>
#include <wrl/client.h>
#include <winrt/Windows.Foundation.h>
#include <winrt/Windows.Graphics.Capture.h>
#include <winrt/Windows.Graphics.DirectX.Direct3D11.h>

#include <chrono>
#include <cwchar>

using Microsoft::WRL::ComPtr;

static thread_local const char* failure_stage = "not started";

struct __declspec(uuid("A9B3D012-3DF2-4EE3-B8D1-8695F457D3C1"))
    Direct3DDxgiInterfaceAccess : IUnknown {
    virtual HRESULT STDMETHODCALLTYPE GetInterface(REFIID iid, void** object) = 0;
};

enum : uint32_t {
    CAPTURE_ENGINE_WGC = 1,
    CAPTURE_ENGINE_DUPLICATION = 2,
    VIDEO_FORMAT_NV12 = 1,
    VIDEO_FORMAT_P010 = 2,
    VIDEO_COLOR_BT709 = 1,
    VIDEO_COLOR_BT2020_PQ = 2,
    RECONFIGURE_NONE = 0,
    RECONFIGURE_CAPTURE_ENGINE = 1,
    RECONFIGURE_DIMENSIONS = 2,
    RECONFIGURE_REFRESH_RATE = 3,
    RECONFIGURE_PIXEL_FORMAT = 4,
    RECONFIGURE_COLOR = 5,
};

struct RustConsoleGpuBridge {
    bool normal_desktop = false;
    bool ro_initialized = false;
    uint32_t capture_engine = 0;
    uint32_t video_format = 0;
    uint32_t video_color = 0;
    uint32_t reconfiguration_cause = RECONFIGURE_NONE;
    ComPtr<IDXGIOutput6> output;
    ComPtr<IDXGIOutputDuplication> duplication;
    winrt::Windows::Graphics::Capture::Direct3D11CaptureFramePool frame_pool{nullptr};
    winrt::Windows::Graphics::Capture::GraphicsCaptureSession capture_session{nullptr};
    ComPtr<ID3D11Device> intel_capture_device;
    ComPtr<ID3D11DeviceContext> intel_capture_context;
    ComPtr<ID3D11DeviceContext4> intel_capture_context4;
    ComPtr<ID3D11Texture2D> intel_cross_resource11;
    ComPtr<ID3D11Fence> intel_cross_fence11;
    ComPtr<ID3D12Device> intel_device12;
    ComPtr<ID3D12Resource> intel_cross_resource;
    ComPtr<ID3D12Fence> intel_cross_fence;

    ComPtr<ID3D12Device> nvidia_device12;
    ComPtr<ID3D12CommandQueue> nvidia_queue;
    ComPtr<ID3D12CommandAllocator> nvidia_allocator;
    ComPtr<ID3D12GraphicsCommandList> nvidia_commands;
    ComPtr<ID3D12Resource> nvidia_cross_resource;
    ComPtr<ID3D12Fence> nvidia_cross_fence;
    ComPtr<ID3D12Fence> nvidia_copy_fence;
    ComPtr<ID3D12Resource> nvidia_local_source12;
    ComPtr<ID3D11Device> nvidia_device11on12;
    ComPtr<ID3D11DeviceContext> nvidia_context11on12;
    ComPtr<ID3D11On12Device> nvidia_interop;
    ComPtr<ID3D11Texture2D> nvidia_local_source11;
    ComPtr<ID3D11Texture2D> nvidia_converted_rgb;
    ComPtr<ID3D11ShaderResourceView> shader_input;
    ComPtr<ID3D11RenderTargetView> shader_output;
    ComPtr<ID3D11VertexShader> vertex_shader;
    ComPtr<ID3D11PixelShader> pixel_shader;
    ComPtr<ID3D11SamplerState> sampler;
    ComPtr<ID3D11VideoDevice> nvidia_video_device;
    ComPtr<ID3D11VideoContext> nvidia_video_context;
    ComPtr<ID3D11VideoProcessorEnumerator> video_enumerator;
    ComPtr<ID3D11VideoProcessor> video_processor;
    ComPtr<ID3D11VideoProcessorInputView> video_input;

    ComPtr<ID3D11Device> encoder_device;
    ComPtr<ID3D11DeviceContext> encoder_context;
    ComPtr<ID3D11Texture2D> encoder_texture;
    ComPtr<IDXGIKeyedMutex> encoder_mutex;
    ComPtr<ID3D11Texture2D> nvidia11on12_output;
    ComPtr<IDXGIKeyedMutex> nvidia11on12_mutex;
    ComPtr<ID3D11VideoProcessorOutputView> video_output;

    HANDLE cross_handle = nullptr;
    HANDLE cross_fence_handle = nullptr;
    HANDLE output_handle = nullptr;
    HANDLE fence_event = nullptr;
    uint64_t fence_value = 0;
    uint32_t width = 0;
    uint32_t height = 0;
    uint32_t refresh_rate = 0;
    bool encoder_texture_outstanding = false;

    ~RustConsoleGpuBridge() {
        try {
            if (capture_session) capture_session.Close();
            if (frame_pool) frame_pool.Close();
        } catch (...) {
        }
        if (encoder_texture_outstanding && encoder_mutex) {
            encoder_mutex->ReleaseSync(0);
        }
        if (fence_event) CloseHandle(fence_event);
        if (output_handle) CloseHandle(output_handle);
        if (cross_fence_handle) CloseHandle(cross_fence_handle);
        if (cross_handle) CloseHandle(cross_handle);
        if (ro_initialized) RoUninitialize();
    }
};

static HRESULT primary_display_name(wchar_t name[CCHDEVICENAME]) {
    for (DWORD index = 0;; ++index) {
        DISPLAY_DEVICEW display{};
        display.cb = sizeof(display);
        if (!EnumDisplayDevicesW(nullptr, index, &display, 0)) break;
        if ((display.StateFlags & DISPLAY_DEVICE_PRIMARY_DEVICE) != 0) {
            wcsncpy_s(name, CCHDEVICENAME, display.DeviceName, _TRUNCATE);
            return S_OK;
        }
    }
    return DXGI_ERROR_NOT_FOUND;
}

static HRESULT input_desktop_is_normal(bool* normal) {
    HDESK desktop = OpenInputDesktop(0, FALSE, DESKTOP_READOBJECTS);
    if (!desktop) return HRESULT_FROM_WIN32(GetLastError());
    wchar_t name[256]{};
    DWORD bytes = 0;
    const BOOL read = GetUserObjectInformationW(
        desktop, UOI_NAME, name, sizeof(name), &bytes);
    const DWORD error = read ? ERROR_SUCCESS : GetLastError();
    CloseDesktop(desktop);
    if (!read) return HRESULT_FROM_WIN32(error);
    *normal = _wcsicmp(name, L"Default") == 0;
    return S_OK;
}

static uint32_t output_refresh_rate(const DXGI_OUTPUT_DESC& output) {
    DEVMODEW mode{};
    mode.dmSize = sizeof(mode);
    return EnumDisplaySettingsW(output.DeviceName, ENUM_CURRENT_SETTINGS, &mode)
        ? mode.dmDisplayFrequency
        : 0;
}

static HRESULT check_configuration(RustConsoleGpuBridge* bridge) {
    bool normal = false;
    HRESULT result = input_desktop_is_normal(&normal);
    if (result == HRESULT_FROM_WIN32(ERROR_ACCESS_DENIED) && bridge->normal_desktop) {
        bridge->reconfiguration_cause = RECONFIGURE_CAPTURE_ENGINE;
        return DXGI_ERROR_ACCESS_LOST;
    }
    if (FAILED(result)) return result;
    if (normal != bridge->normal_desktop) {
        bridge->reconfiguration_cause = RECONFIGURE_CAPTURE_ENGINE;
        return DXGI_ERROR_ACCESS_LOST;
    }
    DXGI_OUTPUT_DESC1 output{};
    if (FAILED(result = bridge->output->GetDesc1(&output))) return result;
    const uint32_t width = static_cast<uint32_t>(output.DesktopCoordinates.right - output.DesktopCoordinates.left);
    const uint32_t height = static_cast<uint32_t>(output.DesktopCoordinates.bottom - output.DesktopCoordinates.top);
    if (width != bridge->width || height != bridge->height) {
        bridge->reconfiguration_cause = RECONFIGURE_DIMENSIONS;
        return DXGI_ERROR_ACCESS_LOST;
    }
    DXGI_OUTPUT_DESC basic{};
    if (FAILED(result = bridge->output->GetDesc(&basic))) return result;
    if (const uint32_t rate = output_refresh_rate(basic); rate != bridge->refresh_rate) {
        bridge->reconfiguration_cause = RECONFIGURE_REFRESH_RATE;
        return DXGI_ERROR_ACCESS_LOST;
    }
    const uint32_t color = normal && output.ColorSpace == DXGI_COLOR_SPACE_RGB_FULL_G2084_NONE_P2020
        ? VIDEO_COLOR_BT2020_PQ
        : VIDEO_COLOR_BT709;
    if (color != bridge->video_color) {
        bridge->reconfiguration_cause = RECONFIGURE_COLOR;
        return DXGI_ERROR_ACCESS_LOST;
    }
    return S_OK;
}

static HRESULT select_adapters(
    IDXGIFactory1* factory,
    IDXGIAdapter1** intel,
    IDXGIOutput6** output,
    IDXGIAdapter1** nvidia) {
    wchar_t primary[CCHDEVICENAME]{};
    HRESULT result = primary_display_name(primary);
    if (FAILED(result)) return result;

    for (UINT adapter_index = 0;; ++adapter_index) {
        ComPtr<IDXGIAdapter1> adapter;
        result = factory->EnumAdapters1(adapter_index, &adapter);
        if (result == DXGI_ERROR_NOT_FOUND) break;
        if (FAILED(result)) return result;
        DXGI_ADAPTER_DESC1 adapter_description{};
        if (FAILED(result = adapter->GetDesc1(&adapter_description))) return result;
        if (adapter_description.VendorId == 0x10de && !*nvidia) {
            *nvidia = adapter.Detach();
            continue;
        }
        if (adapter_description.VendorId != 0x8086) continue;
        for (UINT output_index = 0;; ++output_index) {
            ComPtr<IDXGIOutput> candidate;
            result = adapter->EnumOutputs(output_index, &candidate);
            if (result == DXGI_ERROR_NOT_FOUND) break;
            if (FAILED(result)) return result;
            DXGI_OUTPUT_DESC description{};
            if (FAILED(result = candidate->GetDesc(&description))) return result;
            if (description.AttachedToDesktop && _wcsicmp(primary, description.DeviceName) == 0) {
                ComPtr<IDXGIOutput6> output6;
                if (FAILED(result = candidate.As(&output6))) return result;
                *intel = adapter.Detach();
                *output = output6.Detach();
                break;
            }
        }
    }
    return *intel && *output && *nvidia ? S_OK : DXGI_ERROR_NOT_FOUND;
}

static HRESULT create_queue(ID3D12Device* device, ID3D12CommandQueue** queue) {
    D3D12_COMMAND_QUEUE_DESC description{};
    description.Type = D3D12_COMMAND_LIST_TYPE_DIRECT;
    return device->CreateCommandQueue(&description, IID_PPV_ARGS(queue));
}

static HRESULT create_11on12(
    ID3D12Device* device12,
    ID3D12CommandQueue* queue,
    ID3D11Device** device11,
    ID3D11DeviceContext** context11) {
    IUnknown* queues[] = {queue};
    const D3D_FEATURE_LEVEL levels[] = {D3D_FEATURE_LEVEL_11_1, D3D_FEATURE_LEVEL_11_0};
    return D3D11On12CreateDevice(
        device12,
        D3D11_CREATE_DEVICE_BGRA_SUPPORT | D3D11_CREATE_DEVICE_VIDEO_SUPPORT,
        levels,
        ARRAYSIZE(levels),
        queues,
        ARRAYSIZE(queues),
        0,
        device11,
        context11,
        nullptr);
}

static D3D12_RESOURCE_DESC texture_description(
    uint32_t width,
    uint32_t height,
    DXGI_FORMAT format,
    D3D12_TEXTURE_LAYOUT layout,
    D3D12_RESOURCE_FLAGS flags) {
    D3D12_RESOURCE_DESC description{};
    description.Dimension = D3D12_RESOURCE_DIMENSION_TEXTURE2D;
    description.Width = width;
    description.Height = height;
    description.DepthOrArraySize = 1;
    description.MipLevels = 1;
    description.Format = format;
    description.SampleDesc.Count = 1;
    description.Layout = layout;
    description.Flags = flags;
    return description;
}

static HRESULT create_shader_conversion(RustConsoleGpuBridge* bridge) {
    static constexpr char source[] = R"(
Texture2D<float4> source_texture : register(t0);
SamplerState source_sampler : register(s0);
struct VertexOutput { float4 position : SV_Position; float2 uv : TEXCOORD0; };
VertexOutput vertex_main(uint id : SV_VertexID) {
    VertexOutput output;
    output.uv = float2((id << 1) & 2, id & 2);
    output.position = float4(output.uv.x * 2.0 - 1.0, 1.0 - output.uv.y * 2.0, 0.0, 1.0);
    return output;
}
float pq(float value) {
    const float m1 = 2610.0 / 16384.0, m2 = 2523.0 / 32.0;
    const float c1 = 3424.0 / 4096.0, c2 = 2413.0 / 128.0, c3 = 2392.0 / 128.0;
    float p = pow(max(value, 0.0) * (80.0 / 10000.0), m1);
    return pow((c1 + c2 * p) / (1.0 + c3 * p), m2);
}
float bt709(float value) {
    value = max(value, 0.0);
    return value < 0.018 ? 4.5 * value : 1.099 * pow(value, 0.45) - 0.099;
}
float4 pixel_main(VertexOutput input) : SV_Target {
    float3 rgb709 = source_texture.Sample(source_sampler, input.uv).rgb;
#ifdef HDR_OUTPUT
    float3 rgb2020 = mul(float3x3(
        0.6274040, 0.3292820, 0.0433136,
        0.0690970, 0.9195400, 0.0113612,
        0.0163916, 0.0880132, 0.8955950), rgb709);
    return float4(pq(rgb2020.r), pq(rgb2020.g), pq(rgb2020.b), 1.0);
#else
    return float4(bt709(rgb709.r), bt709(rgb709.g), bt709(rgb709.b), 1.0);
#endif
}
)";
    HRESULT result;
    ComPtr<ID3DBlob> vertex_bytecode;
    ComPtr<ID3DBlob> pixel_bytecode;
    ComPtr<ID3DBlob> errors;
    if (FAILED(result = D3DCompile(
                   source, sizeof(source), nullptr, nullptr, nullptr, "vertex_main", "vs_5_0",
                   D3DCOMPILE_ENABLE_STRICTNESS, 0, &vertex_bytecode, &errors))) return result;
    D3D_SHADER_MACRO macros[] = {
        {"HDR_OUTPUT", bridge->video_color == VIDEO_COLOR_BT2020_PQ ? "1" : nullptr},
        {nullptr, nullptr},
    };
    const D3D_SHADER_MACRO* selected_macros = bridge->video_color == VIDEO_COLOR_BT2020_PQ
        ? macros
        : nullptr;
    errors.Reset();
    if (FAILED(result = D3DCompile(
                   source, sizeof(source), nullptr, selected_macros, nullptr, "pixel_main", "ps_5_0",
                   D3DCOMPILE_ENABLE_STRICTNESS, 0, &pixel_bytecode, &errors))) return result;
    if (FAILED(result = bridge->nvidia_device11on12->CreateVertexShader(
                   vertex_bytecode->GetBufferPointer(), vertex_bytecode->GetBufferSize(), nullptr,
                   &bridge->vertex_shader))) return result;
    if (FAILED(result = bridge->nvidia_device11on12->CreatePixelShader(
                   pixel_bytecode->GetBufferPointer(), pixel_bytecode->GetBufferSize(), nullptr,
                   &bridge->pixel_shader))) return result;
    if (FAILED(result = bridge->nvidia_device11on12->CreateShaderResourceView(
                   bridge->nvidia_local_source11.Get(), nullptr, &bridge->shader_input))) return result;
    D3D11_TEXTURE2D_DESC output{};
    output.Width = bridge->width;
    output.Height = bridge->height;
    output.MipLevels = 1;
    output.ArraySize = 1;
    output.Format = bridge->video_color == VIDEO_COLOR_BT2020_PQ
        ? DXGI_FORMAT_R10G10B10A2_UNORM
        : DXGI_FORMAT_B8G8R8A8_UNORM;
    output.SampleDesc.Count = 1;
    output.Usage = D3D11_USAGE_DEFAULT;
    output.BindFlags = D3D11_BIND_RENDER_TARGET | D3D11_BIND_SHADER_RESOURCE;
    if (FAILED(result = bridge->nvidia_device11on12->CreateTexture2D(
                   &output, nullptr, &bridge->nvidia_converted_rgb))) return result;
    if (FAILED(result = bridge->nvidia_device11on12->CreateRenderTargetView(
                   bridge->nvidia_converted_rgb.Get(), nullptr, &bridge->shader_output))) return result;
    D3D11_SAMPLER_DESC sampler{};
    sampler.Filter = D3D11_FILTER_MIN_MAG_MIP_POINT;
    sampler.AddressU = sampler.AddressV = sampler.AddressW = D3D11_TEXTURE_ADDRESS_CLAMP;
    sampler.MaxLOD = D3D11_FLOAT32_MAX;
    return bridge->nvidia_device11on12->CreateSamplerState(&sampler, &bridge->sampler);
}

static HRESULT initialize_bridge(
    RustConsoleGpuBridge* bridge,
    ID3D11Device** encoder_device,
    bool normal_desktop,
    uint32_t* width,
    uint32_t* height,
    uint32_t* refresh_rate,
    uint32_t* capture_engine,
    uint32_t* video_format,
    uint32_t* video_color) {
    HRESULT result;
    if (normal_desktop) {
        failure_stage = "initialize Windows Runtime for Windows.Graphics.Capture";
        result = RoInitialize(RO_INIT_MULTITHREADED);
        if (FAILED(result)) return result;
        bridge->ro_initialized = true;
    }
    ComPtr<IDXGIFactory1> factory;
    ComPtr<IDXGIAdapter1> intel_adapter;
    ComPtr<IDXGIAdapter1> nvidia_adapter;
    ComPtr<IDXGIOutput6> output;
    const D3D_FEATURE_LEVEL levels[] = {D3D_FEATURE_LEVEL_11_1, D3D_FEATURE_LEVEL_11_0};
    failure_stage = "create DXGI factory";
    if (FAILED(result = CreateDXGIFactory1(IID_PPV_ARGS(&factory)))) return result;
    failure_stage = "select Intel display and NVIDIA encode adapters";
    if (FAILED(result = select_adapters(
                   factory.Get(), &intel_adapter, &output, &nvidia_adapter))) return result;
    bridge->normal_desktop = normal_desktop;
    bridge->output = output;
    DXGI_OUTPUT_DESC1 output_description{};
    if (FAILED(result = output->GetDesc1(&output_description))) return result;
    DXGI_OUTPUT_DESC basic_output_description{};
    if (FAILED(result = output->GetDesc(&basic_output_description))) return result;
    bridge->width = static_cast<uint32_t>(
        output_description.DesktopCoordinates.right - output_description.DesktopCoordinates.left);
    bridge->height = static_cast<uint32_t>(
        output_description.DesktopCoordinates.bottom - output_description.DesktopCoordinates.top);
    bridge->refresh_rate = output_refresh_rate(basic_output_description);
    bridge->capture_engine = normal_desktop ? CAPTURE_ENGINE_WGC : CAPTURE_ENGINE_DUPLICATION;
    bridge->video_color = normal_desktop &&
            output_description.ColorSpace == DXGI_COLOR_SPACE_RGB_FULL_G2084_NONE_P2020
        ? VIDEO_COLOR_BT2020_PQ
        : VIDEO_COLOR_BT709;
    bridge->video_format = bridge->video_color == VIDEO_COLOR_BT2020_PQ
        ? VIDEO_FORMAT_P010
        : VIDEO_FORMAT_NV12;
    failure_stage = "create D3D12 and D3D11On12 devices";
    if (FAILED(result = D3D12CreateDevice(
                   intel_adapter.Get(), D3D_FEATURE_LEVEL_11_0,
                   IID_PPV_ARGS(&bridge->intel_device12)))) return result;
    if (FAILED(result = D3D12CreateDevice(
                   nvidia_adapter.Get(), D3D_FEATURE_LEVEL_11_0,
                   IID_PPV_ARGS(&bridge->nvidia_device12)))) return result;
    if (FAILED(result = create_queue(bridge->nvidia_device12.Get(), &bridge->nvidia_queue))) return result;
    if (FAILED(result = create_11on12(
                   bridge->nvidia_device12.Get(), bridge->nvidia_queue.Get(),
                   &bridge->nvidia_device11on12, &bridge->nvidia_context11on12))) return result;
    if (FAILED(result = bridge->nvidia_device11on12.As(&bridge->nvidia_interop))) return result;
    if (FAILED(result = bridge->nvidia_device11on12.As(&bridge->nvidia_video_device))) return result;
    if (FAILED(result = bridge->nvidia_context11on12.As(&bridge->nvidia_video_context))) return result;

    failure_stage = "create native Intel D3D11 capture device";
    if (FAILED(result = D3D11CreateDevice(
                   intel_adapter.Get(), D3D_DRIVER_TYPE_UNKNOWN, nullptr,
                   D3D11_CREATE_DEVICE_BGRA_SUPPORT | D3D11_CREATE_DEVICE_VIDEO_SUPPORT,
                   levels, ARRAYSIZE(levels),
                   D3D11_SDK_VERSION, &bridge->intel_capture_device, nullptr,
                   &bridge->intel_capture_context))) return result;
    if (normal_desktop) {
        ComPtr<IDXGIDevice> dxgi_device;
        failure_stage = "query the Intel capture DXGI device";
        if (FAILED(result = bridge->intel_capture_device.As(&dxgi_device))) return result;
        winrt::com_ptr<IInspectable> inspectable_device;
        failure_stage = "create the Windows Runtime Direct3D capture device";
        if (FAILED(result = CreateDirect3D11DeviceFromDXGIDevice(
                       dxgi_device.Get(), inspectable_device.put()))) return result;
        auto direct3d_device = inspectable_device.as<
            winrt::Windows::Graphics::DirectX::Direct3D11::IDirect3DDevice>();
        failure_stage = "activate Windows.Graphics.Capture interop";
        auto interop = winrt::get_activation_factory<
            winrt::Windows::Graphics::Capture::GraphicsCaptureItem,
            IGraphicsCaptureItemInterop>();
        winrt::Windows::Graphics::Capture::GraphicsCaptureItem item{nullptr};
        failure_stage = "create the Windows.Graphics.Capture monitor item";
        if (FAILED(result = interop->CreateForMonitor(
                       basic_output_description.Monitor,
                       winrt::guid_of<winrt::Windows::Graphics::Capture::GraphicsCaptureItem>(),
                       winrt::put_abi(item)))) return result;
        failure_stage = "create the Windows.Graphics.Capture frame pool";
        bridge->frame_pool = winrt::Windows::Graphics::Capture::Direct3D11CaptureFramePool::CreateFreeThreaded(
            direct3d_device,
            winrt::Windows::Graphics::DirectX::DirectXPixelFormat::R16G16B16A16Float,
            2, item.Size());
        failure_stage = "create the Windows.Graphics.Capture session";
        bridge->capture_session = bridge->frame_pool.CreateCaptureSession(item);
    } else {
        failure_stage = "create Desktop Duplication for secure desktop";
        ComPtr<IDXGIOutput1> output1;
        if (FAILED(result = output.As(&output1))) return result;
        if (FAILED(result = output1->DuplicateOutput(
                       bridge->intel_capture_device.Get(), &bridge->duplication))) return result;
    }

    const DXGI_FORMAT source_format = normal_desktop
        ? DXGI_FORMAT_R16G16B16A16_FLOAT
        : DXGI_FORMAT_B8G8R8A8_UNORM;

    D3D12_HEAP_PROPERTIES heap_properties{};
    heap_properties.Type = D3D12_HEAP_TYPE_DEFAULT;
    heap_properties.CreationNodeMask = 1;
    heap_properties.VisibleNodeMask = 1;
    auto cross_description = texture_description(
        bridge->width,
        bridge->height,
        source_format,
        D3D12_TEXTURE_LAYOUT_ROW_MAJOR,
        D3D12_RESOURCE_FLAG_ALLOW_CROSS_ADAPTER |
            D3D12_RESOURCE_FLAG_ALLOW_SIMULTANEOUS_ACCESS);
    failure_stage = "create and share cross-adapter capture resource";
    if (FAILED(result = bridge->intel_device12->CreateCommittedResource(
                   &heap_properties,
                   D3D12_HEAP_FLAG_SHARED | D3D12_HEAP_FLAG_SHARED_CROSS_ADAPTER,
                   &cross_description,
                   D3D12_RESOURCE_STATE_COMMON,
                   nullptr,
                   IID_PPV_ARGS(&bridge->intel_cross_resource)))) return result;
    if (FAILED(result = bridge->intel_device12->CreateSharedHandle(
                   bridge->intel_cross_resource.Get(), nullptr, GENERIC_ALL,
                   nullptr, &bridge->cross_handle))) return result;
    if (FAILED(result = bridge->nvidia_device12->OpenSharedHandle(
                   bridge->cross_handle, IID_PPV_ARGS(&bridge->nvidia_cross_resource)))) return result;
    ComPtr<ID3D11Device1> intel_capture_device1;
    failure_stage = "open D3D12 cross-adapter capture resource on native Intel D3D11";
    if (FAILED(result = bridge->intel_capture_device.As(&intel_capture_device1))) return result;
    if (FAILED(result = intel_capture_device1->OpenSharedResource1(
                   bridge->cross_handle, IID_PPV_ARGS(&bridge->intel_cross_resource11)))) return result;

    failure_stage = "create and share cross-adapter fence";
    if (FAILED(result = bridge->intel_device12->CreateFence(
                   0, D3D12_FENCE_FLAG_SHARED | D3D12_FENCE_FLAG_SHARED_CROSS_ADAPTER,
                   IID_PPV_ARGS(&bridge->intel_cross_fence)))) return result;
    if (FAILED(result = bridge->intel_device12->CreateSharedHandle(
                   bridge->intel_cross_fence.Get(), nullptr, GENERIC_ALL,
                   nullptr, &bridge->cross_fence_handle))) return result;
    if (FAILED(result = bridge->nvidia_device12->OpenSharedHandle(
                   bridge->cross_fence_handle, IID_PPV_ARGS(&bridge->nvidia_cross_fence)))) return result;
    ComPtr<ID3D11Device5> intel_capture_device5;
    failure_stage = "open D3D12 cross-adapter fence on native Intel D3D11";
    if (FAILED(result = bridge->intel_capture_device.As(&intel_capture_device5))) return result;
    if (FAILED(result = bridge->intel_capture_context.As(&bridge->intel_capture_context4))) return result;
    if (FAILED(result = intel_capture_device5->OpenSharedFence(
                   bridge->cross_fence_handle, IID_PPV_ARGS(&bridge->intel_cross_fence11)))) return result;

    auto local_description = texture_description(
        bridge->width,
        bridge->height,
        source_format,
        D3D12_TEXTURE_LAYOUT_UNKNOWN,
        D3D12_RESOURCE_FLAG_NONE);
    failure_stage = "create and wrap NVIDIA-local capture resource";
    if (FAILED(result = bridge->nvidia_device12->CreateCommittedResource(
                   &heap_properties, D3D12_HEAP_FLAG_NONE, &local_description,
                   D3D12_RESOURCE_STATE_COMMON, nullptr,
                   IID_PPV_ARGS(&bridge->nvidia_local_source12)))) return result;
    D3D11_RESOURCE_FLAGS no_bind_flags{};
    if (normal_desktop) no_bind_flags.BindFlags = D3D11_BIND_SHADER_RESOURCE;
    if (FAILED(result = bridge->nvidia_interop->CreateWrappedResource(
                   bridge->nvidia_local_source12.Get(), &no_bind_flags,
                   D3D12_RESOURCE_STATE_COMMON, D3D12_RESOURCE_STATE_COMMON,
                   IID_PPV_ARGS(&bridge->nvidia_local_source11)))) return result;

    failure_stage = "create native NVIDIA D3D11 device";
    if (FAILED(result = D3D11CreateDevice(
                   nvidia_adapter.Get(), D3D_DRIVER_TYPE_UNKNOWN, nullptr,
                   D3D11_CREATE_DEVICE_BGRA_SUPPORT | D3D11_CREATE_DEVICE_VIDEO_SUPPORT,
                   levels, ARRAYSIZE(levels), D3D11_SDK_VERSION,
                   &bridge->encoder_device, nullptr, &bridge->encoder_context))) return result;
    D3D11_TEXTURE2D_DESC encoder_description{};
    encoder_description.Width = bridge->width;
    encoder_description.Height = bridge->height;
    encoder_description.MipLevels = 1;
    encoder_description.ArraySize = 1;
    encoder_description.Format = bridge->video_format == VIDEO_FORMAT_P010
        ? DXGI_FORMAT_P010
        : DXGI_FORMAT_NV12;
    encoder_description.SampleDesc.Count = 1;
    encoder_description.Usage = D3D11_USAGE_DEFAULT;
    encoder_description.BindFlags = D3D11_BIND_RENDER_TARGET | D3D11_BIND_SHADER_RESOURCE;
    encoder_description.MiscFlags =
        D3D11_RESOURCE_MISC_SHARED_NTHANDLE | D3D11_RESOURCE_MISC_SHARED_KEYEDMUTEX;
    failure_stage = "create native NVIDIA shared keyed-mutex encoder texture";
    if (FAILED(result = bridge->encoder_device->CreateTexture2D(
                   &encoder_description, nullptr, &bridge->encoder_texture))) return result;
    ComPtr<IDXGIResource1> encoder_resource;
    failure_stage = "query native NVIDIA encoder IDXGIResource1";
    if (FAILED(result = bridge->encoder_texture.As(&encoder_resource))) return result;
    failure_stage = "create native NVIDIA encoder shared handle";
    if (FAILED(result = encoder_resource->CreateSharedHandle(
                   nullptr, DXGI_SHARED_RESOURCE_READ | DXGI_SHARED_RESOURCE_WRITE,
                   nullptr, &bridge->output_handle))) return result;
    ComPtr<ID3D11Device1> nvidia_device1;
    failure_stage = "query NVIDIA D3D11On12 ID3D11Device1";
    if (FAILED(result = bridge->nvidia_device11on12.As(&nvidia_device1))) return result;
    failure_stage = "open native NVIDIA encoder texture on D3D11On12";
    if (FAILED(result = nvidia_device1->OpenSharedResource1(
                   bridge->output_handle, IID_PPV_ARGS(&bridge->nvidia11on12_output)))) return result;
    failure_stage = "query native NVIDIA encoder keyed mutex";
    if (FAILED(result = bridge->encoder_texture.As(&bridge->encoder_mutex))) return result;
    failure_stage = "query D3D11On12 encoder keyed mutex";
    if (FAILED(result = bridge->nvidia11on12_output.As(&bridge->nvidia11on12_mutex))) return result;

    if (normal_desktop) {
        failure_stage = "create scRGB conversion shader resources";
        if (FAILED(result = create_shader_conversion(bridge))) return result;
    }

    D3D11_VIDEO_PROCESSOR_CONTENT_DESC content{};
    content.InputFrameFormat = D3D11_VIDEO_FRAME_FORMAT_PROGRESSIVE;
    content.InputFrameRate = {bridge->refresh_rate, 1};
    content.InputWidth = bridge->width;
    content.InputHeight = bridge->height;
    content.OutputFrameRate = {bridge->refresh_rate, 1};
    content.OutputWidth = bridge->width;
    content.OutputHeight = bridge->height;
    content.Usage = D3D11_VIDEO_USAGE_PLAYBACK_NORMAL;
    failure_stage = "create NVIDIA video processor and views";
    if (FAILED(result = bridge->nvidia_video_device->CreateVideoProcessorEnumerator(
                   &content, &bridge->video_enumerator))) return result;
    if (FAILED(result = bridge->nvidia_video_device->CreateVideoProcessor(
                   bridge->video_enumerator.Get(), 0, &bridge->video_processor))) return result;
    const RECT destination = {
        0,
        0,
        static_cast<LONG>(bridge->width),
        static_cast<LONG>(bridge->height),
    };
    bridge->nvidia_video_context->VideoProcessorSetStreamDestRect(
        bridge->video_processor.Get(), 0, TRUE, &destination);
    ComPtr<ID3D11VideoContext1> video_context1;
    if (FAILED(result = bridge->nvidia_video_context.As(&video_context1))) return result;
    if (bridge->video_color == VIDEO_COLOR_BT2020_PQ) {
        video_context1->VideoProcessorSetStreamColorSpace1(
            bridge->video_processor.Get(), 0, DXGI_COLOR_SPACE_RGB_FULL_G2084_NONE_P2020);
        video_context1->VideoProcessorSetOutputColorSpace1(
            bridge->video_processor.Get(), DXGI_COLOR_SPACE_YCBCR_STUDIO_G2084_LEFT_P2020);
    } else {
        video_context1->VideoProcessorSetStreamColorSpace1(
            bridge->video_processor.Get(), 0, DXGI_COLOR_SPACE_RGB_FULL_G22_NONE_P709);
        video_context1->VideoProcessorSetOutputColorSpace1(
            bridge->video_processor.Get(), DXGI_COLOR_SPACE_YCBCR_STUDIO_G22_LEFT_P709);
    }
    D3D11_VIDEO_PROCESSOR_INPUT_VIEW_DESC input_description{};
    input_description.ViewDimension = D3D11_VPIV_DIMENSION_TEXTURE2D;
    if (FAILED(result = bridge->nvidia_video_device->CreateVideoProcessorInputView(
                   (normal_desktop ? bridge->nvidia_converted_rgb : bridge->nvidia_local_source11).Get(),
                   bridge->video_enumerator.Get(),
                   &input_description, &bridge->video_input))) return result;
    D3D11_VIDEO_PROCESSOR_OUTPUT_VIEW_DESC output_view_description{};
    output_view_description.ViewDimension = D3D11_VPOV_DIMENSION_TEXTURE2D;
    if (FAILED(result = bridge->nvidia_video_device->CreateVideoProcessorOutputView(
                   bridge->nvidia11on12_output.Get(), bridge->video_enumerator.Get(),
                   &output_view_description, &bridge->video_output))) return result;

    failure_stage = "create cross-adapter command infrastructure";
    if (FAILED(result = bridge->nvidia_device12->CreateCommandAllocator(
                   D3D12_COMMAND_LIST_TYPE_DIRECT, IID_PPV_ARGS(&bridge->nvidia_allocator)))) return result;
    if (FAILED(result = bridge->nvidia_device12->CreateCommandList(
                   0, D3D12_COMMAND_LIST_TYPE_DIRECT, bridge->nvidia_allocator.Get(), nullptr,
                   IID_PPV_ARGS(&bridge->nvidia_commands)))) return result;
    if (FAILED(result = bridge->nvidia_commands->Close())) return result;
    if (FAILED(result = bridge->nvidia_device12->CreateFence(
                   0, D3D12_FENCE_FLAG_NONE, IID_PPV_ARGS(&bridge->nvidia_copy_fence)))) return result;
    bridge->fence_event = CreateEventW(nullptr, FALSE, FALSE, nullptr);
    if (!bridge->fence_event) return HRESULT_FROM_WIN32(GetLastError());

    if (normal_desktop) {
        failure_stage = "start the Windows.Graphics.Capture session";
        bridge->capture_session.StartCapture();
    }

    bridge->encoder_device.CopyTo(encoder_device);
    *width = bridge->width;
    *height = bridge->height;
    *refresh_rate = bridge->refresh_rate;
    *capture_engine = bridge->capture_engine;
    *video_format = bridge->video_format;
    *video_color = bridge->video_color;
    failure_stage = nullptr;
    return S_OK;
}

static D3D12_RESOURCE_BARRIER transition(
    ID3D12Resource* resource,
    D3D12_RESOURCE_STATES before,
    D3D12_RESOURCE_STATES after) {
    D3D12_RESOURCE_BARRIER barrier{};
    barrier.Type = D3D12_RESOURCE_BARRIER_TYPE_TRANSITION;
    barrier.Transition.pResource = resource;
    barrier.Transition.StateBefore = before;
    barrier.Transition.StateAfter = after;
    barrier.Transition.Subresource = D3D12_RESOURCE_BARRIER_ALL_SUBRESOURCES;
    return barrier;
}

static HRESULT wait_for_fence(RustConsoleGpuBridge* bridge, uint64_t value) {
    if (bridge->nvidia_copy_fence->GetCompletedValue() >= value) return S_OK;
    HRESULT result = bridge->nvidia_copy_fence->SetEventOnCompletion(value, bridge->fence_event);
    if (FAILED(result)) return result;
    const DWORD wait = WaitForSingleObject(bridge->fence_event, 5000);
    return wait == WAIT_OBJECT_0 ? S_OK : HRESULT_FROM_WIN32(wait == WAIT_TIMEOUT ? ERROR_TIMEOUT : GetLastError());
}

static HRESULT capture_frame(
    RustConsoleGpuBridge* bridge,
    uint32_t timeout_millis,
    ID3D11Texture2D** encoder_texture,
    int64_t* last_present_time,
    uint32_t* accumulated_frames,
    int32_t* protected_content_masked,
    bool external_consumer) {
    if (bridge->encoder_texture_outstanding) return DXGI_ERROR_INVALID_CALL;
    bridge->reconfiguration_cause = RECONFIGURE_NONE;
    HRESULT result = check_configuration(bridge);
    if (FAILED(result)) return result;
    DXGI_OUTDUPL_FRAME_INFO frame_info{};
    ComPtr<ID3D11Texture2D> source_texture;
    bool frame_acquired = false;
    if (bridge->normal_desktop) {
        failure_stage = "acquire Windows.Graphics.Capture frame";
        winrt::Windows::Graphics::Capture::Direct3D11CaptureFrame frame{nullptr};
        const auto started = std::chrono::steady_clock::now();
        while (!frame) {
            frame = bridge->frame_pool.TryGetNextFrame();
            if (frame) break;
            result = check_configuration(bridge);
            if (FAILED(result)) return result;
            if (std::chrono::steady_clock::now() - started >=
                std::chrono::milliseconds(timeout_millis)) return DXGI_ERROR_WAIT_TIMEOUT;
            Sleep(1);
        }
        auto access = frame.Surface().as<Direct3DDxgiInterfaceAccess>();
        if (FAILED(result = access->GetInterface(IID_PPV_ARGS(&source_texture)))) return result;
        D3D11_TEXTURE2D_DESC description{};
        source_texture->GetDesc(&description);
        if (description.Width != bridge->width || description.Height != bridge->height) {
            bridge->reconfiguration_cause = RECONFIGURE_DIMENSIONS;
            return DXGI_ERROR_ACCESS_LOST;
        }
        if (description.Format != DXGI_FORMAT_R16G16B16A16_FLOAT) {
            bridge->reconfiguration_cause = RECONFIGURE_PIXEL_FORMAT;
            return DXGI_ERROR_ACCESS_LOST;
        }
        const auto relative = frame.SystemRelativeTime();
        LARGE_INTEGER frequency{};
        QueryPerformanceFrequency(&frequency);
        frame_info.LastPresentTime.QuadPart =
            relative.count() * frequency.QuadPart / 10000000;
        frame_info.AccumulatedFrames = 1;
    } else {
        ComPtr<IDXGIResource> desktop_resource;
        failure_stage = "acquire Desktop Duplication frame";
        result = bridge->duplication->AcquireNextFrame(
            timeout_millis, &frame_info, &desktop_resource);
        if (FAILED(result)) return result;
        frame_acquired = true;
        failure_stage = "query Intel Desktop Duplication texture";
        result = desktop_resource.As(&source_texture);
    }
    if (SUCCEEDED(result)) {
        failure_stage = "copy native Intel frame into D3D12 cross-adapter resource";
        bridge->intel_capture_context->CopyResource(
            bridge->intel_cross_resource11.Get(), source_texture.Get());
        result = bridge->intel_capture_context4->Signal(
            bridge->intel_cross_fence11.Get(), ++bridge->fence_value);
    }
    const uint64_t value = bridge->fence_value;
    if (frame_acquired) {
        const HRESULT released = bridge->duplication->ReleaseFrame();
        if (SUCCEEDED(result)) result = released;
    }
    if (FAILED(result)) return result;

    failure_stage = "copy cross-adapter frame into NVIDIA-local texture";
    if (FAILED(result = bridge->nvidia_queue->Wait(bridge->nvidia_cross_fence.Get(), value))) return result;
    if (FAILED(result = bridge->nvidia_allocator->Reset())) return result;
    if (FAILED(result = bridge->nvidia_commands->Reset(bridge->nvidia_allocator.Get(), nullptr))) return result;
    D3D12_RESOURCE_BARRIER nvidia_barriers[] = {
        transition(bridge->nvidia_cross_resource.Get(), D3D12_RESOURCE_STATE_COMMON, D3D12_RESOURCE_STATE_COPY_SOURCE),
        transition(bridge->nvidia_local_source12.Get(), D3D12_RESOURCE_STATE_COMMON, D3D12_RESOURCE_STATE_COPY_DEST),
    };
    bridge->nvidia_commands->ResourceBarrier(ARRAYSIZE(nvidia_barriers), nvidia_barriers);
    bridge->nvidia_commands->CopyResource(
        bridge->nvidia_local_source12.Get(), bridge->nvidia_cross_resource.Get());
    nvidia_barriers[0] = transition(
        bridge->nvidia_cross_resource.Get(), D3D12_RESOURCE_STATE_COPY_SOURCE, D3D12_RESOURCE_STATE_COMMON);
    nvidia_barriers[1] = transition(
        bridge->nvidia_local_source12.Get(), D3D12_RESOURCE_STATE_COPY_DEST, D3D12_RESOURCE_STATE_COMMON);
    bridge->nvidia_commands->ResourceBarrier(ARRAYSIZE(nvidia_barriers), nvidia_barriers);
    if (FAILED(result = bridge->nvidia_commands->Close())) return result;
    ID3D12CommandList* nvidia_lists[] = {bridge->nvidia_commands.Get()};
    bridge->nvidia_queue->ExecuteCommandLists(ARRAYSIZE(nvidia_lists), nvidia_lists);
    if (FAILED(result = bridge->nvidia_queue->Signal(bridge->nvidia_copy_fence.Get(), value))) return result;
    if (FAILED(result = wait_for_fence(bridge, value))) return result;

    failure_stage = "convert NVIDIA-local capture texture to encoder format";
    if (FAILED(result = bridge->nvidia11on12_mutex->AcquireSync(0, 5000))) return result;
    ID3D11Resource* wrapped[] = {bridge->nvidia_local_source11.Get()};
    bridge->nvidia_interop->AcquireWrappedResources(wrapped, ARRAYSIZE(wrapped));
    if (bridge->normal_desktop) {
        const D3D11_VIEWPORT viewport{
            0.0f, 0.0f, static_cast<float>(bridge->width),
            static_cast<float>(bridge->height), 0.0f, 1.0f};
        bridge->nvidia_context11on12->RSSetViewports(1, &viewport);
        bridge->nvidia_context11on12->OMSetRenderTargets(
            1, bridge->shader_output.GetAddressOf(), nullptr);
        bridge->nvidia_context11on12->IASetPrimitiveTopology(
            D3D11_PRIMITIVE_TOPOLOGY_TRIANGLELIST);
        bridge->nvidia_context11on12->VSSetShader(bridge->vertex_shader.Get(), nullptr, 0);
        bridge->nvidia_context11on12->PSSetShader(bridge->pixel_shader.Get(), nullptr, 0);
        bridge->nvidia_context11on12->PSSetShaderResources(
            0, 1, bridge->shader_input.GetAddressOf());
        bridge->nvidia_context11on12->PSSetSamplers(0, 1, bridge->sampler.GetAddressOf());
        bridge->nvidia_context11on12->Draw(3, 0);
        ID3D11ShaderResourceView* null_view = nullptr;
        bridge->nvidia_context11on12->PSSetShaderResources(0, 1, &null_view);
    }
    D3D11_VIDEO_PROCESSOR_STREAM stream{};
    stream.Enable = TRUE;
    stream.pInputSurface = bridge->video_input.Get();
    result = bridge->nvidia_video_context->VideoProcessorBlt(
        bridge->video_processor.Get(), bridge->video_output.Get(), 0, 1, &stream);
    bridge->nvidia_interop->ReleaseWrappedResources(wrapped, ARRAYSIZE(wrapped));
    bridge->nvidia_context11on12->Flush();
    const HRESULT released_nv12 = bridge->nvidia11on12_mutex->ReleaseSync(1);
    if (SUCCEEDED(result)) result = released_nv12;
    if (FAILED(result)) return result;
    if (!external_consumer) {
        failure_stage = "acquire native NVIDIA encoder texture";
        if (FAILED(result = bridge->encoder_mutex->AcquireSync(1, 5000))) return result;
        bridge->encoder_texture_outstanding = true;
        bridge->encoder_texture.CopyTo(encoder_texture);
    }
    *last_present_time = frame_info.LastPresentTime.QuadPart;
    *accumulated_frames = frame_info.AccumulatedFrames;
    *protected_content_masked = frame_info.ProtectedContentMaskedOut ? 1 : 0;
    failure_stage = nullptr;
    return S_OK;
}

extern "C" HRESULT rustconsole_gpu_bridge_create(
    RustConsoleGpuBridge** bridge,
    ID3D11Device** encoder_device,
    int32_t normal_desktop,
    uint32_t* width,
    uint32_t* height,
    uint32_t* refresh_rate,
    uint32_t* capture_engine,
    uint32_t* video_format,
    uint32_t* video_color) {
    if (!bridge || !encoder_device || !width || !height || !refresh_rate ||
        !capture_engine || !video_format || !video_color) return E_POINTER;
    *bridge = nullptr;
    *encoder_device = nullptr;
    auto value = new (std::nothrow) RustConsoleGpuBridge();
    if (!value) return E_OUTOFMEMORY;
    HRESULT result;
    try {
        result = initialize_bridge(
            value, encoder_device, normal_desktop != 0, width, height, refresh_rate,
            capture_engine, video_format, video_color);
    } catch (...) {
        result = winrt::to_hresult();
    }
    if (FAILED(result)) {
        delete value;
        return result;
    }
    *bridge = value;
    return S_OK;
}

extern "C" HRESULT rustconsole_gpu_bridge_capture(
    RustConsoleGpuBridge* bridge,
    uint32_t timeout_millis,
    ID3D11Texture2D** encoder_texture,
    int64_t* last_present_time,
    uint32_t* accumulated_frames,
    int32_t* protected_content_masked) {
    if (!bridge || !encoder_texture || !last_present_time ||
        !accumulated_frames || !protected_content_masked) return E_POINTER;
    *encoder_texture = nullptr;
    try {
        return capture_frame(
            bridge, timeout_millis, encoder_texture, last_present_time,
            accumulated_frames, protected_content_masked, false);
    } catch (...) {
        return winrt::to_hresult();
    }
}

extern "C" HRESULT rustconsole_gpu_bridge_capture_external(
    RustConsoleGpuBridge* bridge,
    uint32_t timeout_millis,
    int64_t* last_present_time,
    uint32_t* accumulated_frames,
    int32_t* protected_content_masked) {
    if (!bridge || !last_present_time || !accumulated_frames ||
        !protected_content_masked) return E_POINTER;
    try {
        return capture_frame(
            bridge, timeout_millis, nullptr, last_present_time,
            accumulated_frames, protected_content_masked, true);
    } catch (...) {
        return winrt::to_hresult();
    }
}

extern "C" uintptr_t rustconsole_gpu_bridge_output_handle(
    RustConsoleGpuBridge* bridge) {
    return bridge ? reinterpret_cast<uintptr_t>(bridge->output_handle) : 0;
}

extern "C" HRESULT rustconsole_gpu_bridge_open_external(
    RustConsoleGpuBridge** bridge,
    ID3D11Device** encoder_device,
    HANDLE shared_texture,
    uint32_t width,
    uint32_t height,
    uint32_t refresh_rate,
    uint32_t video_format,
    uint32_t video_color) {
    if (!bridge || !encoder_device || !shared_texture || !width || !height ||
        (video_format != VIDEO_FORMAT_NV12 && video_format != VIDEO_FORMAT_P010) ||
        (video_color != VIDEO_COLOR_BT709 && video_color != VIDEO_COLOR_BT2020_PQ)) {
        return E_INVALIDARG;
    }
    *bridge = nullptr;
    *encoder_device = nullptr;
    auto value = new (std::nothrow) RustConsoleGpuBridge();
    if (!value) return E_OUTOFMEMORY;
    HRESULT result = E_FAIL;
    do {
        ComPtr<IDXGIFactory1> factory;
        ComPtr<IDXGIAdapter1> intel_adapter;
        ComPtr<IDXGIAdapter1> nvidia_adapter;
        ComPtr<IDXGIOutput6> output;
        failure_stage = "create DXGI factory for external WGC texture";
        if (FAILED(result = CreateDXGIFactory1(IID_PPV_ARGS(&factory)))) break;
        failure_stage = "select NVIDIA adapter for external WGC texture";
        if (FAILED(result = select_adapters(
                       factory.Get(), &intel_adapter, &output, &nvidia_adapter))) break;
        const D3D_FEATURE_LEVEL levels[] = {
            D3D_FEATURE_LEVEL_11_1, D3D_FEATURE_LEVEL_11_0};
        failure_stage = "create NVIDIA device for external WGC texture";
        if (FAILED(result = D3D11CreateDevice(
                       nvidia_adapter.Get(), D3D_DRIVER_TYPE_UNKNOWN, nullptr,
                       D3D11_CREATE_DEVICE_BGRA_SUPPORT | D3D11_CREATE_DEVICE_VIDEO_SUPPORT,
                       levels, ARRAYSIZE(levels), D3D11_SDK_VERSION,
                       &value->encoder_device, nullptr, &value->encoder_context))) break;
        ComPtr<ID3D11Device1> device1;
        if (FAILED(result = value->encoder_device.As(&device1))) break;
        failure_stage = "open external WGC encoder texture";
        if (FAILED(result = device1->OpenSharedResource1(
                       shared_texture, IID_PPV_ARGS(&value->encoder_texture)))) break;
        if (FAILED(result = value->encoder_texture.As(&value->encoder_mutex))) break;
        D3D11_TEXTURE2D_DESC description{};
        value->encoder_texture->GetDesc(&description);
        const DXGI_FORMAT expected = video_format == VIDEO_FORMAT_P010
            ? DXGI_FORMAT_P010 : DXGI_FORMAT_NV12;
        if (description.Width != width || description.Height != height ||
            description.Format != expected) {
            failure_stage = "validate external WGC encoder texture";
            result = E_INVALIDARG;
            break;
        }
        value->normal_desktop = true;
        value->capture_engine = CAPTURE_ENGINE_WGC;
        value->width = width;
        value->height = height;
        value->refresh_rate = refresh_rate;
        value->video_format = video_format;
        value->video_color = video_color;
        value->encoder_device.CopyTo(encoder_device);
        failure_stage = nullptr;
        result = S_OK;
    } while (false);
    if (FAILED(result)) {
        delete value;
        return result;
    }
    *bridge = value;
    return S_OK;
}

extern "C" HRESULT rustconsole_gpu_bridge_acquire_external(
    RustConsoleGpuBridge* bridge,
    uint32_t timeout_millis,
    ID3D11Texture2D** encoder_texture) {
    if (!bridge || !encoder_texture) return E_POINTER;
    if (bridge->encoder_texture_outstanding) return DXGI_ERROR_INVALID_CALL;
    failure_stage = "acquire external WGC encoder texture";
    const HRESULT result = bridge->encoder_mutex->AcquireSync(1, timeout_millis);
    if (FAILED(result)) return result;
    bridge->encoder_texture_outstanding = true;
    bridge->encoder_texture.CopyTo(encoder_texture);
    failure_stage = nullptr;
    return S_OK;
}

extern "C" HRESULT rustconsole_gpu_bridge_release_encoder_texture(
    RustConsoleGpuBridge* bridge) {
    if (!bridge) return E_POINTER;
    if (!bridge->encoder_texture_outstanding) return DXGI_ERROR_INVALID_CALL;
    const HRESULT result = bridge->encoder_mutex->ReleaseSync(0);
    if (SUCCEEDED(result)) bridge->encoder_texture_outstanding = false;
    return result;
}

extern "C" void rustconsole_gpu_bridge_destroy(RustConsoleGpuBridge* bridge) {
    delete bridge;
}

extern "C" const char* rustconsole_gpu_bridge_failure_stage() {
    return failure_stage;
}

extern "C" uint32_t rustconsole_gpu_bridge_reconfiguration_cause(
    RustConsoleGpuBridge* bridge) {
    return bridge ? bridge->reconfiguration_cause : RECONFIGURE_NONE;
}
