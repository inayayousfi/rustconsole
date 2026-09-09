#define NOMINMAX

#include <d3d11_4.h>
#include <d3d11on12.h>
#include <d3d12.h>
#include <d3dcompiler.h>
#include <dxgi1_6.h>
#include <windows.graphics.capture.interop.h>
#include <windows.graphics.directx.direct3d11.interop.h>
#include <wrl/client.h>
#include <winrt/Windows.Foundation.h>
#include <winrt/Windows.Graphics.Capture.h>
#include <winrt/Windows.Graphics.DirectX.Direct3D11.h>

#include <algorithm>
#include <array>
#include <chrono>
#include <cmath>
#include <condition_variable>
#include <cstdint>
#include <cstdio>
#include <filesystem>
#include <io.h>
#include <mutex>

using Microsoft::WRL::ComPtr;

struct __declspec(uuid("A9B3D012-3DF2-4EE3-B8D1-8695F457D3C1"))
    ProofDirect3DDxgiInterfaceAccess : IUnknown {
    virtual HRESULT STDMETHODCALLTYPE GetInterface(REFIID iid, void** object) = 0;
};

static const char* format_name(DXGI_FORMAT format) {
    switch (format) {
    case DXGI_FORMAT_B8G8R8A8_UNORM:
        return "B8G8R8A8_UNORM";
    case DXGI_FORMAT_R10G10B10A2_UNORM:
        return "R10G10B10A2_UNORM";
    case DXGI_FORMAT_R16G16B16A16_FLOAT:
        return "R16G16B16A16_FLOAT";
    case DXGI_FORMAT_P010:
        return "P010";
    default:
        return "other";
    }
}

static const char* color_space_name(DXGI_COLOR_SPACE_TYPE color_space) {
    switch (color_space) {
    case DXGI_COLOR_SPACE_RGB_FULL_G22_NONE_P709:
        return "RGB_FULL_G22_NONE_P709";
    case DXGI_COLOR_SPACE_RGB_FULL_G10_NONE_P709:
        return "RGB_FULL_G10_NONE_P709";
    case DXGI_COLOR_SPACE_RGB_FULL_G2084_NONE_P2020:
        return "RGB_FULL_G2084_NONE_P2020";
    default:
        return "other";
    }
}

static int fail(const char* stage, HRESULT result) {
    std::fprintf(stderr, "status=failed\nstage=%s\nhresult=0x%08lx\n", stage,
                 static_cast<unsigned long>(result));
    return 1;
}

static D3D12_RESOURCE_DESC resource_description(
    UINT width,
    UINT height,
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

struct CrossAdapterFp16 {
    ComPtr<ID3D12Device> intel_device12;
    ComPtr<ID3D12Device> nvidia_device12;
    ComPtr<ID3D12CommandQueue> nvidia_queue;
    ComPtr<ID3D12Resource> intel_cross;
    ComPtr<ID3D12Resource> nvidia_cross;
    ComPtr<ID3D11Texture2D> intel_cross11;
    ComPtr<ID3D12Fence> intel_fence12;
    ComPtr<ID3D12Fence> nvidia_fence12;
    ComPtr<ID3D11Fence> intel_fence11;
    ComPtr<ID3D12Resource> nvidia_local12;
    ComPtr<ID3D11Device> nvidia_device11;
    ComPtr<ID3D11DeviceContext> nvidia_context11;
    ComPtr<ID3D11On12Device> nvidia_interop;
    ComPtr<ID3D11Texture2D> nvidia_local11;
    HANDLE resource_handle = nullptr;
    HANDLE fence_handle = nullptr;

    ~CrossAdapterFp16() {
        if (fence_handle) CloseHandle(fence_handle);
        if (resource_handle) CloseHandle(resource_handle);
    }
};

static HRESULT render_scrgb_to_rgb10(
    CrossAdapterFp16* proof,
    ID3D11Texture2D* source,
    bool source_is_wrapped,
    UINT width,
    UINT height,
    ID3D11Texture2D** output) {
    static constexpr char shader_source[] = R"(
Texture2D<float4> source_texture : register(t0);
SamplerState source_sampler : register(s0);

struct VertexOutput {
    float4 position : SV_Position;
    float2 texture_coordinate : TEXCOORD0;
};

VertexOutput vertex_main(uint id : SV_VertexID) {
    VertexOutput output;
    output.texture_coordinate = float2((id << 1) & 2, id & 2);
    output.position = float4(
        output.texture_coordinate.x * 2.0 - 1.0,
        1.0 - output.texture_coordinate.y * 2.0,
        0.0, 1.0);
    return output;
}

float pq(float linear_bt2020) {
    const float m1 = 2610.0 / 16384.0;
    const float m2 = 2523.0 / 32.0;
    const float c1 = 3424.0 / 4096.0;
    const float c2 = 2413.0 / 128.0;
    const float c3 = 2392.0 / 128.0;
    float p = pow(max(linear_bt2020, 0.0) * (80.0 / 10000.0), m1);
    return pow((c1 + c2 * p) / (1.0 + c3 * p), m2);
}

float4 pixel_main(VertexOutput input) : SV_Target {
    float3 rgb709 = source_texture.Sample(source_sampler, input.texture_coordinate).rgb;
    float3 rgb2020 = mul(float3x3(
        0.6274040, 0.3292820, 0.0433136,
        0.0690970, 0.9195400, 0.0113612,
        0.0163916, 0.0880132, 0.8955950), rgb709);
    return float4(pq(rgb2020.r), pq(rgb2020.g), pq(rgb2020.b), 1.0);
}
)";
    HRESULT result;
    ComPtr<ID3DBlob> vertex_bytecode;
    ComPtr<ID3DBlob> pixel_bytecode;
    ComPtr<ID3DBlob> errors;
    if (FAILED(result = D3DCompile(
                   shader_source, sizeof(shader_source), nullptr, nullptr, nullptr,
                   "vertex_main", "vs_5_0", D3DCOMPILE_ENABLE_STRICTNESS, 0,
                   &vertex_bytecode, &errors))) return result;
    errors.Reset();
    if (FAILED(result = D3DCompile(
                   shader_source, sizeof(shader_source), nullptr, nullptr, nullptr,
                   "pixel_main", "ps_5_0", D3DCOMPILE_ENABLE_STRICTNESS, 0,
                   &pixel_bytecode, &errors))) return result;
    ComPtr<ID3D11VertexShader> vertex_shader;
    ComPtr<ID3D11PixelShader> pixel_shader;
    if (FAILED(result = proof->nvidia_device11->CreateVertexShader(
                   vertex_bytecode->GetBufferPointer(), vertex_bytecode->GetBufferSize(),
                   nullptr, &vertex_shader))) return result;
    if (FAILED(result = proof->nvidia_device11->CreatePixelShader(
                   pixel_bytecode->GetBufferPointer(), pixel_bytecode->GetBufferSize(),
                   nullptr, &pixel_shader))) return result;

    ComPtr<ID3D11ShaderResourceView> source_view;
    if (FAILED(result = proof->nvidia_device11->CreateShaderResourceView(
                   source, nullptr, &source_view))) return result;
    D3D11_TEXTURE2D_DESC output_description{};
    output_description.Width = width;
    output_description.Height = height;
    output_description.MipLevels = 1;
    output_description.ArraySize = 1;
    output_description.Format = DXGI_FORMAT_R10G10B10A2_UNORM;
    output_description.SampleDesc.Count = 1;
    output_description.Usage = D3D11_USAGE_DEFAULT;
    output_description.BindFlags = D3D11_BIND_RENDER_TARGET | D3D11_BIND_SHADER_RESOURCE;
    ComPtr<ID3D11Texture2D> rgb10;
    if (FAILED(result = proof->nvidia_device11->CreateTexture2D(
                   &output_description, nullptr, &rgb10))) return result;
    ComPtr<ID3D11RenderTargetView> output_view;
    if (FAILED(result = proof->nvidia_device11->CreateRenderTargetView(
                   rgb10.Get(), nullptr, &output_view))) return result;
    D3D11_SAMPLER_DESC sampler_description{};
    sampler_description.Filter = D3D11_FILTER_MIN_MAG_MIP_POINT;
    sampler_description.AddressU = D3D11_TEXTURE_ADDRESS_CLAMP;
    sampler_description.AddressV = D3D11_TEXTURE_ADDRESS_CLAMP;
    sampler_description.AddressW = D3D11_TEXTURE_ADDRESS_CLAMP;
    sampler_description.MaxLOD = D3D11_FLOAT32_MAX;
    ComPtr<ID3D11SamplerState> sampler;
    if (FAILED(result = proof->nvidia_device11->CreateSamplerState(
                   &sampler_description, &sampler))) return result;

    ID3D11Resource* wrapped_resources[] = {source};
    if (source_is_wrapped) proof->nvidia_interop->AcquireWrappedResources(wrapped_resources, 1);
    const D3D11_VIEWPORT viewport{
        0.0f, 0.0f, static_cast<float>(width), static_cast<float>(height), 0.0f, 1.0f};
    proof->nvidia_context11->RSSetViewports(1, &viewport);
    proof->nvidia_context11->OMSetRenderTargets(1, output_view.GetAddressOf(), nullptr);
    proof->nvidia_context11->IASetPrimitiveTopology(D3D11_PRIMITIVE_TOPOLOGY_TRIANGLELIST);
    proof->nvidia_context11->VSSetShader(vertex_shader.Get(), nullptr, 0);
    proof->nvidia_context11->PSSetShader(pixel_shader.Get(), nullptr, 0);
    proof->nvidia_context11->PSSetShaderResources(0, 1, source_view.GetAddressOf());
    proof->nvidia_context11->PSSetSamplers(0, 1, sampler.GetAddressOf());
    proof->nvidia_context11->Draw(3, 0);
    ID3D11ShaderResourceView* null_view = nullptr;
    proof->nvidia_context11->PSSetShaderResources(0, 1, &null_view);
    if (source_is_wrapped) proof->nvidia_interop->ReleaseWrappedResources(wrapped_resources, 1);
    proof->nvidia_context11->Flush();
    *output = rgb10.Detach();
    return S_OK;
}

static UINT pq_code(float linear_bt2020) {
    constexpr float m1 = 2610.0f / 16384.0f;
    constexpr float m2 = 2523.0f / 32.0f;
    constexpr float c1 = 3424.0f / 4096.0f;
    constexpr float c2 = 2413.0f / 128.0f;
    constexpr float c3 = 2392.0f / 128.0f;
    const float p = std::pow(std::max(linear_bt2020, 0.0f) * (80.0f / 10000.0f), m1);
    const float encoded = std::pow((c1 + c2 * p) / (1.0f + c3 * p), m2);
    return static_cast<UINT>(std::lround(encoded * 1023.0f));
}

static HRESULT verify_scrgb_shader_numbers(CrossAdapterFp16* proof) {
    const std::array<uint16_t, 12> pixels{
        0x0000, 0x0000, 0x0000, 0x3c00,
        0x3c00, 0x3c00, 0x3c00, 0x3c00,
        0x3c00, 0x0000, 0x0000, 0x3c00,
    };
    D3D11_TEXTURE2D_DESC source_description{};
    source_description.Width = 3;
    source_description.Height = 1;
    source_description.MipLevels = 1;
    source_description.ArraySize = 1;
    source_description.Format = DXGI_FORMAT_R16G16B16A16_FLOAT;
    source_description.SampleDesc.Count = 1;
    source_description.Usage = D3D11_USAGE_IMMUTABLE;
    source_description.BindFlags = D3D11_BIND_SHADER_RESOURCE;
    D3D11_SUBRESOURCE_DATA source_data{};
    source_data.pSysMem = pixels.data();
    source_data.SysMemPitch = static_cast<UINT>(pixels.size() * sizeof(uint16_t));
    ComPtr<ID3D11Texture2D> source;
    HRESULT result = proof->nvidia_device11->CreateTexture2D(
        &source_description, &source_data, &source);
    if (FAILED(result)) return result;
    ComPtr<ID3D11Texture2D> output;
    if (FAILED(result = render_scrgb_to_rgb10(
                   proof, source.Get(), false, 3, 1, &output))) return result;

    D3D11_TEXTURE2D_DESC staging_description{};
    output->GetDesc(&staging_description);
    staging_description.Usage = D3D11_USAGE_STAGING;
    staging_description.BindFlags = 0;
    staging_description.CPUAccessFlags = D3D11_CPU_ACCESS_READ;
    ComPtr<ID3D11Texture2D> staging;
    if (FAILED(result = proof->nvidia_device11->CreateTexture2D(
                   &staging_description, nullptr, &staging))) return result;
    proof->nvidia_context11->CopyResource(staging.Get(), output.Get());
    D3D11_MAPPED_SUBRESOURCE mapped{};
    if (FAILED(result = proof->nvidia_context11->Map(
                   staging.Get(), 0, D3D11_MAP_READ, 0, &mapped))) return result;
    const auto* packed = static_cast<const uint32_t*>(mapped.pData);
    const std::array<std::array<UINT, 3>, 3> expected{{
        {pq_code(0.0f), pq_code(0.0f), pq_code(0.0f)},
        {pq_code(1.0f), pq_code(1.0f), pq_code(1.0f)},
        {pq_code(0.6274040f), pq_code(0.0690970f), pq_code(0.0163916f)},
    }};
    bool matches = true;
    for (size_t index = 0; index < expected.size(); ++index) {
        const std::array<UINT, 3> actual{
            packed[index] & 0x3ff,
            (packed[index] >> 10) & 0x3ff,
            (packed[index] >> 20) & 0x3ff,
        };
        for (size_t channel = 0; channel < actual.size(); ++channel) {
            const int difference = static_cast<int>(actual[channel]) -
                                   static_cast<int>(expected[index][channel]);
            if (std::abs(difference) > 1) matches = false;
        }
    }
    proof->nvidia_context11->Unmap(staging.Get(), 0);
    return matches ? S_OK : E_FAIL;
}

static HRESULT copy_cross_adapter_fp16(
    IDXGIAdapter1* intel_adapter,
    ID3D11Device* intel_device11,
    ID3D11DeviceContext* intel_context11,
    IDXGIAdapter1* nvidia_adapter,
    ID3D11Texture2D* source,
    CrossAdapterFp16* proof) {
    HRESULT result;
    D3D11_TEXTURE2D_DESC source_description{};
    source->GetDesc(&source_description);
    if (source_description.Format != DXGI_FORMAT_R16G16B16A16_FLOAT) return E_INVALIDARG;

    if (FAILED(result = D3D12CreateDevice(
                   intel_adapter, D3D_FEATURE_LEVEL_11_0,
                   IID_PPV_ARGS(&proof->intel_device12)))) return result;
    if (FAILED(result = D3D12CreateDevice(
                   nvidia_adapter, D3D_FEATURE_LEVEL_11_0,
                   IID_PPV_ARGS(&proof->nvidia_device12)))) return result;
    D3D12_COMMAND_QUEUE_DESC queue_description{};
    queue_description.Type = D3D12_COMMAND_LIST_TYPE_DIRECT;
    if (FAILED(result = proof->nvidia_device12->CreateCommandQueue(
                   &queue_description, IID_PPV_ARGS(&proof->nvidia_queue)))) return result;

    D3D12_HEAP_PROPERTIES heap_properties{};
    heap_properties.Type = D3D12_HEAP_TYPE_DEFAULT;
    heap_properties.CreationNodeMask = 1;
    heap_properties.VisibleNodeMask = 1;
    auto cross_description = resource_description(
        source_description.Width, source_description.Height,
        DXGI_FORMAT_R16G16B16A16_FLOAT, D3D12_TEXTURE_LAYOUT_ROW_MAJOR,
        D3D12_RESOURCE_FLAG_ALLOW_CROSS_ADAPTER |
            D3D12_RESOURCE_FLAG_ALLOW_SIMULTANEOUS_ACCESS);
    if (FAILED(result = proof->intel_device12->CreateCommittedResource(
                   &heap_properties,
                   D3D12_HEAP_FLAG_SHARED | D3D12_HEAP_FLAG_SHARED_CROSS_ADAPTER,
                   &cross_description, D3D12_RESOURCE_STATE_COMMON, nullptr,
                   IID_PPV_ARGS(&proof->intel_cross)))) return result;
    if (FAILED(result = proof->intel_device12->CreateSharedHandle(
                   proof->intel_cross.Get(), nullptr, GENERIC_ALL, nullptr,
                   &proof->resource_handle))) return result;
    if (FAILED(result = proof->nvidia_device12->OpenSharedHandle(
                   proof->resource_handle, IID_PPV_ARGS(&proof->nvidia_cross)))) return result;
    ComPtr<ID3D11Device1> intel_device1;
    if (FAILED(result = intel_device11->QueryInterface(IID_PPV_ARGS(&intel_device1)))) return result;
    if (FAILED(result = intel_device1->OpenSharedResource1(
                   proof->resource_handle, IID_PPV_ARGS(&proof->intel_cross11)))) return result;

    if (FAILED(result = proof->intel_device12->CreateFence(
                   0, D3D12_FENCE_FLAG_SHARED | D3D12_FENCE_FLAG_SHARED_CROSS_ADAPTER,
                   IID_PPV_ARGS(&proof->intel_fence12)))) return result;
    if (FAILED(result = proof->intel_device12->CreateSharedHandle(
                   proof->intel_fence12.Get(), nullptr, GENERIC_ALL, nullptr,
                   &proof->fence_handle))) return result;
    if (FAILED(result = proof->nvidia_device12->OpenSharedHandle(
                   proof->fence_handle, IID_PPV_ARGS(&proof->nvidia_fence12)))) return result;
    ComPtr<ID3D11Device5> intel_device5;
    ComPtr<ID3D11DeviceContext4> intel_context4;
    if (FAILED(result = intel_device11->QueryInterface(IID_PPV_ARGS(&intel_device5)))) return result;
    if (FAILED(result = intel_context11->QueryInterface(IID_PPV_ARGS(&intel_context4)))) return result;
    if (FAILED(result = intel_device5->OpenSharedFence(
                   proof->fence_handle, IID_PPV_ARGS(&proof->intel_fence11)))) return result;

    auto local_description = resource_description(
        source_description.Width, source_description.Height,
        DXGI_FORMAT_R16G16B16A16_FLOAT, D3D12_TEXTURE_LAYOUT_UNKNOWN,
        D3D12_RESOURCE_FLAG_NONE);
    if (FAILED(result = proof->nvidia_device12->CreateCommittedResource(
                   &heap_properties, D3D12_HEAP_FLAG_NONE, &local_description,
                   D3D12_RESOURCE_STATE_COMMON, nullptr,
                   IID_PPV_ARGS(&proof->nvidia_local12)))) return result;

    IUnknown* queues[] = {proof->nvidia_queue.Get()};
    const D3D_FEATURE_LEVEL levels[] = {D3D_FEATURE_LEVEL_11_1, D3D_FEATURE_LEVEL_11_0};
    if (FAILED(result = D3D11On12CreateDevice(
                   proof->nvidia_device12.Get(),
                   D3D11_CREATE_DEVICE_BGRA_SUPPORT | D3D11_CREATE_DEVICE_VIDEO_SUPPORT,
                   levels, ARRAYSIZE(levels), queues, ARRAYSIZE(queues), 0,
                   &proof->nvidia_device11, &proof->nvidia_context11, nullptr))) return result;
    if (FAILED(result = proof->nvidia_device11.As(&proof->nvidia_interop))) return result;
    D3D11_RESOURCE_FLAGS bind_flags{};
    bind_flags.BindFlags = D3D11_BIND_SHADER_RESOURCE;
    if (FAILED(result = proof->nvidia_interop->CreateWrappedResource(
                   proof->nvidia_local12.Get(), &bind_flags,
                   D3D12_RESOURCE_STATE_COMMON, D3D12_RESOURCE_STATE_COMMON,
                   IID_PPV_ARGS(&proof->nvidia_local11)))) return result;

    intel_context11->CopyResource(proof->intel_cross11.Get(), source);
    if (FAILED(result = intel_context4->Signal(proof->intel_fence11.Get(), 1))) return result;
    if (FAILED(result = proof->nvidia_queue->Wait(proof->nvidia_fence12.Get(), 1))) return result;

    ComPtr<ID3D12CommandAllocator> allocator;
    ComPtr<ID3D12GraphicsCommandList> commands;
    if (FAILED(result = proof->nvidia_device12->CreateCommandAllocator(
                   D3D12_COMMAND_LIST_TYPE_DIRECT, IID_PPV_ARGS(&allocator)))) return result;
    if (FAILED(result = proof->nvidia_device12->CreateCommandList(
                   0, D3D12_COMMAND_LIST_TYPE_DIRECT, allocator.Get(), nullptr,
                   IID_PPV_ARGS(&commands)))) return result;
    auto cross_barrier = transition(
        proof->nvidia_cross.Get(), D3D12_RESOURCE_STATE_COMMON,
        D3D12_RESOURCE_STATE_COPY_SOURCE);
    auto local_barrier = transition(
        proof->nvidia_local12.Get(), D3D12_RESOURCE_STATE_COMMON,
        D3D12_RESOURCE_STATE_COPY_DEST);
    commands->ResourceBarrier(1, &cross_barrier);
    commands->ResourceBarrier(1, &local_barrier);
    commands->CopyResource(proof->nvidia_local12.Get(), proof->nvidia_cross.Get());
    cross_barrier = transition(
        proof->nvidia_cross.Get(), D3D12_RESOURCE_STATE_COPY_SOURCE,
        D3D12_RESOURCE_STATE_COMMON);
    local_barrier = transition(
        proof->nvidia_local12.Get(), D3D12_RESOURCE_STATE_COPY_DEST,
        D3D12_RESOURCE_STATE_COMMON);
    commands->ResourceBarrier(1, &cross_barrier);
    commands->ResourceBarrier(1, &local_barrier);
    if (FAILED(result = commands->Close())) return result;
    ID3D12CommandList* command_lists[] = {commands.Get()};
    proof->nvidia_queue->ExecuteCommandLists(1, command_lists);
    ComPtr<ID3D12Fence> completion;
    if (FAILED(result = proof->nvidia_device12->CreateFence(
                   0, D3D12_FENCE_FLAG_NONE, IID_PPV_ARGS(&completion)))) return result;
    if (FAILED(result = proof->nvidia_queue->Signal(completion.Get(), 1))) return result;
    HANDLE event = CreateEventW(nullptr, FALSE, FALSE, nullptr);
    if (!event) return HRESULT_FROM_WIN32(GetLastError());
    result = completion->SetEventOnCompletion(1, event);
    if (SUCCEEDED(result)) {
        const DWORD wait = WaitForSingleObject(event, 5000);
        if (wait != WAIT_OBJECT_0) {
            result = HRESULT_FROM_WIN32(wait == WAIT_TIMEOUT ? ERROR_TIMEOUT : GetLastError());
        }
    }
    CloseHandle(event);
    return result;
}

int main() {
    const auto report_path =
        std::filesystem::temp_directory_path() / L"rustconsole-color-proof.txt";
    FILE* report = nullptr;
    if (_wfreopen_s(&report, report_path.c_str(), L"w", stdout) != 0) {
        return 1;
    }
    if (_dup2(_fileno(stdout), _fileno(stderr)) != 0) return 1;
    winrt::init_apartment(winrt::apartment_type::multi_threaded);

    ComPtr<IDXGIFactory1> factory;
    HRESULT result = CreateDXGIFactory1(IID_PPV_ARGS(&factory));
    if (FAILED(result)) return fail("CreateDXGIFactory1", result);

    ComPtr<IDXGIAdapter1> selected_adapter;
    ComPtr<IDXGIOutput> selected_output;
    for (UINT adapter_index = 0;; ++adapter_index) {
        ComPtr<IDXGIAdapter1> adapter;
        result = factory->EnumAdapters1(adapter_index, &adapter);
        if (result == DXGI_ERROR_NOT_FOUND) break;
        if (FAILED(result)) return fail("EnumAdapters1", result);
        for (UINT output_index = 0;; ++output_index) {
            ComPtr<IDXGIOutput> output;
            result = adapter->EnumOutputs(output_index, &output);
            if (result == DXGI_ERROR_NOT_FOUND) break;
            if (FAILED(result)) break;
            DXGI_OUTPUT_DESC description{};
            if (SUCCEEDED(output->GetDesc(&description)) && description.AttachedToDesktop) {
                selected_adapter = adapter;
                selected_output = output;
                break;
            }
        }
        if (selected_output) break;
    }
    if (!selected_output) return fail("find attached output", DXGI_ERROR_NOT_FOUND);

    DXGI_ADAPTER_DESC1 adapter_description{};
    if (FAILED(result = selected_adapter->GetDesc1(&adapter_description))) {
        return fail("adapter GetDesc1", result);
    }
    DXGI_OUTPUT_DESC basic_output_description{};
    if (FAILED(result = selected_output->GetDesc(&basic_output_description))) {
        return fail("output GetDesc", result);
    }

    ComPtr<IDXGIOutput6> output6;
    if (FAILED(result = selected_output.As(&output6))) return fail("query IDXGIOutput6", result);
    DXGI_OUTPUT_DESC1 output_description{};
    if (FAILED(result = output6->GetDesc1(&output_description))) return fail("GetDesc1", result);
    std::printf(
        "adapter_vendor=0x%04x\nadapter_device=0x%04x\noutput=%ls\n"
        "bits_per_color=%u\ncolor_space=%s\nmin_luminance=%.6f\n"
        "max_luminance=%.6f\nmax_full_frame_luminance=%.6f\n",
        adapter_description.VendorId, adapter_description.DeviceId,
        basic_output_description.DeviceName, output_description.BitsPerColor,
        color_space_name(output_description.ColorSpace),
        output_description.MinLuminance, output_description.MaxLuminance,
        output_description.MaxFullFrameLuminance);
    std::fflush(stdout);

    ComPtr<ID3D11Device> device;
    ComPtr<ID3D11DeviceContext> context;
    if (FAILED(result = D3D11CreateDevice(
                   selected_adapter.Get(), D3D_DRIVER_TYPE_UNKNOWN, nullptr,
                   D3D11_CREATE_DEVICE_BGRA_SUPPORT | D3D11_CREATE_DEVICE_VIDEO_SUPPORT,
                   nullptr, 0, D3D11_SDK_VERSION, &device, nullptr, &context))) {
        return fail("D3D11CreateDevice", result);
    }

    ComPtr<IDXGIOutput5> output5;
    if (FAILED(result = selected_output.As(&output5))) return fail("query IDXGIOutput5", result);
    ComPtr<IDXGIOutput1> output1;
    if (FAILED(result = selected_output.As(&output1))) return fail("query IDXGIOutput1", result);
    ComPtr<IDXGIOutputDuplication> legacy_duplication;
    const HRESULT legacy_result = output1->DuplicateOutput(device.Get(), &legacy_duplication);
    std::printf("duplicate_legacy_hresult=0x%08lx\n",
                static_cast<unsigned long>(legacy_result));
    legacy_duplication.Reset();
    const std::array bgra_formats{DXGI_FORMAT_B8G8R8A8_UNORM};
    const std::array rgb10_formats{
        DXGI_FORMAT_R10G10B10A2_UNORM,
        DXGI_FORMAT_B8G8R8A8_UNORM,
    };
    const std::array fp16_formats{
        DXGI_FORMAT_R16G16B16A16_FLOAT,
        DXGI_FORMAT_R10G10B10A2_UNORM,
        DXGI_FORMAT_B8G8R8A8_UNORM,
    };
    ComPtr<IDXGIOutputDuplication> duplication;
    auto try_formats = [&](const char* name, const auto& formats) {
        ComPtr<IDXGIOutputDuplication> attempt;
        const HRESULT attempt_result = output5->DuplicateOutput1(
            device.Get(), 0, static_cast<UINT>(formats.size()), formats.data(), &attempt);
        std::printf("duplicate_%s_hresult=0x%08lx\n", name,
                    static_cast<unsigned long>(attempt_result));
        if (SUCCEEDED(attempt_result)) duplication = attempt;
        return attempt_result;
    };
    try_formats("fp16_rgb10_bgra8", fp16_formats);
    if (!duplication) try_formats("rgb10_bgra8", rgb10_formats);
    if (!duplication) try_formats("bgra8", bgra_formats);

    ComPtr<IDXGIDevice> dxgi_device;
    if (FAILED(result = device.As(&dxgi_device))) return fail("query IDXGIDevice", result);
    winrt::com_ptr<IInspectable> inspectable_device;
    if (FAILED(result = CreateDirect3D11DeviceFromDXGIDevice(
                   dxgi_device.Get(), inspectable_device.put()))) {
        return fail("CreateDirect3D11DeviceFromDXGIDevice", result);
    }
    auto direct3d_device =
        inspectable_device.as<winrt::Windows::Graphics::DirectX::Direct3D11::IDirect3DDevice>();
    auto item_interop = winrt::get_activation_factory<
        winrt::Windows::Graphics::Capture::GraphicsCaptureItem,
        IGraphicsCaptureItemInterop>();
    winrt::Windows::Graphics::Capture::GraphicsCaptureItem item{nullptr};
    if (FAILED(result = item_interop->CreateForMonitor(
                   basic_output_description.Monitor,
                   winrt::guid_of<winrt::Windows::Graphics::Capture::GraphicsCaptureItem>(),
                   winrt::put_abi(item)))) {
        return fail("CreateForMonitor", result);
    }
    auto frame_pool = winrt::Windows::Graphics::Capture::Direct3D11CaptureFramePool::CreateFreeThreaded(
        direct3d_device,
        winrt::Windows::Graphics::DirectX::DirectXPixelFormat::R16G16B16A16Float,
        2, item.Size());
    auto capture_session = frame_pool.CreateCaptureSession(item);
    std::mutex frame_mutex;
    std::condition_variable frame_ready;
    winrt::Windows::Graphics::Capture::Direct3D11CaptureFrame captured{nullptr};
    const auto token = frame_pool.FrameArrived([&](const auto& sender, const auto&) {
        auto next = sender.TryGetNextFrame();
        std::scoped_lock lock(frame_mutex);
        if (!captured) captured = std::move(next);
        frame_ready.notify_one();
    });
    capture_session.StartCapture();
    {
        std::unique_lock lock(frame_mutex);
        if (!frame_ready.wait_for(lock, std::chrono::seconds(5), [&] { return captured != nullptr; })) {
            capture_session.Close();
            frame_pool.FrameArrived(token);
            frame_pool.Close();
            return fail("wait for Windows.Graphics.Capture frame", HRESULT_FROM_WIN32(WAIT_TIMEOUT));
        }
    }
    capture_session.Close();
    frame_pool.FrameArrived(token);
    frame_pool.Close();
    auto surface_access = captured.Surface().as<ProofDirect3DDxgiInterfaceAccess>();
    ComPtr<ID3D11Texture2D> captured_texture;
    if (FAILED(result = surface_access->GetInterface(IID_PPV_ARGS(&captured_texture)))) {
        return fail("get capture texture", result);
    }
    D3D11_TEXTURE2D_DESC captured_description{};
    captured_texture->GetDesc(&captured_description);
    std::printf("windows_graphics_capture_format=%s\n",
                format_name(captured_description.Format));

    ComPtr<IDXGIAdapter1> nvidia_adapter;
    for (UINT adapter_index = 0;; ++adapter_index) {
        ComPtr<IDXGIAdapter1> adapter;
        result = factory->EnumAdapters1(adapter_index, &adapter);
        if (result == DXGI_ERROR_NOT_FOUND) break;
        if (FAILED(result)) return fail("enumerate NVIDIA adapter", result);
        DXGI_ADAPTER_DESC1 description{};
        if (SUCCEEDED(adapter->GetDesc1(&description)) && description.VendorId == 0x10de) {
            nvidia_adapter = adapter;
            break;
        }
    }
    if (!nvidia_adapter) return fail("find NVIDIA adapter", DXGI_ERROR_NOT_FOUND);
    CrossAdapterFp16 cross_adapter;
    if (FAILED(result = copy_cross_adapter_fp16(
                   selected_adapter.Get(), device.Get(), context.Get(),
                   nvidia_adapter.Get(), captured_texture.Get(), &cross_adapter))) {
        return fail("copy FP16 Intel to NVIDIA through D3D12 cross-adapter resource", result);
    }
    std::printf("cross_adapter_fp16=ok\n");
    ComPtr<ID3D11Device> nvidia_device = cross_adapter.nvidia_device11;
    ComPtr<ID3D11DeviceContext> nvidia_context = cross_adapter.nvidia_context11;
    ComPtr<ID3D11Texture2D> input;
    if (FAILED(result = render_scrgb_to_rgb10(
                   &cross_adapter, cross_adapter.nvidia_local11.Get(), true,
                   captured_description.Width, captured_description.Height, &input))) {
        return fail("render scRGB FP16 to BT.2020 PQ RGB10", result);
    }
    std::printf("scrgb_fp16_to_rgb10_shader=ok\n");
    if (FAILED(result = verify_scrgb_shader_numbers(&cross_adapter))) {
        return fail("verify scRGB to BT.2020 PQ shader numbers", result);
    }
    std::printf("scrgb_shader_numeric_tolerance=1_code_value\n");
    ComPtr<ID3D11VideoDevice> video_device;
    ComPtr<ID3D11VideoContext> video_context;
    if (FAILED(result = nvidia_device.As(&video_device))) {
        return fail("query NVIDIA ID3D11VideoDevice", result);
    }
    if (FAILED(result = nvidia_context.As(&video_context))) {
        return fail("query NVIDIA ID3D11VideoContext", result);
    }

    D3D11_VIDEO_PROCESSOR_CONTENT_DESC content{};
    content.InputFrameFormat = D3D11_VIDEO_FRAME_FORMAT_PROGRESSIVE;
    content.InputFrameRate = {60, 1};
    content.InputWidth = captured_description.Width;
    content.InputHeight = captured_description.Height;
    content.OutputFrameRate = {60, 1};
    content.OutputWidth = captured_description.Width;
    content.OutputHeight = captured_description.Height;
    content.Usage = D3D11_VIDEO_USAGE_PLAYBACK_NORMAL;
    ComPtr<ID3D11VideoProcessorEnumerator> enumerator;
    ComPtr<ID3D11VideoProcessor> processor;
    if (FAILED(result = video_device->CreateVideoProcessorEnumerator(&content, &enumerator))) {
        return fail("CreateVideoProcessorEnumerator", result);
    }
    if (FAILED(result = video_device->CreateVideoProcessor(enumerator.Get(), 0, &processor))) {
        return fail("CreateVideoProcessor", result);
    }

    auto texture_description = [&](DXGI_FORMAT format) {
        D3D11_TEXTURE2D_DESC description{};
        description.Width = content.InputWidth;
        description.Height = content.InputHeight;
        description.MipLevels = 1;
        description.ArraySize = 1;
        description.Format = format;
        description.SampleDesc.Count = 1;
        description.Usage = D3D11_USAGE_DEFAULT;
        description.BindFlags = D3D11_BIND_RENDER_TARGET | D3D11_BIND_SHADER_RESOURCE;
        return description;
    };
    ComPtr<ID3D11Texture2D> output;
    auto output_texture_description = texture_description(DXGI_FORMAT_P010);
    if (FAILED(result = nvidia_device->CreateTexture2D(
                   &output_texture_description, nullptr, &output))) {
        return fail("create P010 output", result);
    }

    D3D11_VIDEO_PROCESSOR_INPUT_VIEW_DESC input_view_description{};
    input_view_description.ViewDimension = D3D11_VPIV_DIMENSION_TEXTURE2D;
    ComPtr<ID3D11VideoProcessorInputView> input_view;
    if (FAILED(result = video_device->CreateVideoProcessorInputView(
                   input.Get(), enumerator.Get(), &input_view_description, &input_view))) {
        return fail("CreateVideoProcessorInputView", result);
    }
    D3D11_VIDEO_PROCESSOR_OUTPUT_VIEW_DESC output_view_description{};
    output_view_description.ViewDimension = D3D11_VPOV_DIMENSION_TEXTURE2D;
    ComPtr<ID3D11VideoProcessorOutputView> output_view;
    if (FAILED(result = video_device->CreateVideoProcessorOutputView(
                   output.Get(), enumerator.Get(), &output_view_description, &output_view))) {
        return fail("CreateVideoProcessorOutputView", result);
    }
    const RECT destination{0, 0, static_cast<LONG>(content.OutputWidth),
                           static_cast<LONG>(content.OutputHeight)};
    video_context->VideoProcessorSetStreamDestRect(processor.Get(), 0, TRUE, &destination);
    ComPtr<ID3D11VideoContext1> video_context1;
    if (FAILED(result = video_context.As(&video_context1))) {
        return fail("query NVIDIA ID3D11VideoContext1", result);
    }
    video_context1->VideoProcessorSetStreamColorSpace1(
        processor.Get(), 0, DXGI_COLOR_SPACE_RGB_FULL_G2084_NONE_P2020);
    video_context1->VideoProcessorSetOutputColorSpace1(
        processor.Get(), DXGI_COLOR_SPACE_YCBCR_STUDIO_G2084_LEFT_P2020);
    D3D11_VIDEO_PROCESSOR_STREAM stream{};
    stream.Enable = TRUE;
    stream.pInputSurface = input_view.Get();
    if (FAILED(result = video_context->VideoProcessorBlt(
                   processor.Get(), output_view.Get(), 0, 1, &stream))) {
        return fail("VideoProcessorBlt P010", result);
    }

    std::printf(
        "status=ok\n"
        "capture_width=%u\n"
        "capture_height=%u\n"
        "hdr_color_spaces=bt2020-pq-full-rgb-to-limited-p010\n"
        "p010_video_processor_blt=ok\n",
        captured_description.Width, captured_description.Height);
    return 0;
}
