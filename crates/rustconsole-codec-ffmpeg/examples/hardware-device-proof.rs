#[cfg(not(windows))]
use rustconsole_codec_ffmpeg::Av1VaApiDecoder;
#[cfg(windows)]
use rustconsole_codec_ffmpeg::{
    Av1ColorDescription, Av1EncoderConfiguration, Av1FrameFormat, Av1NvencEncoder,
};
use rustconsole_codec_ffmpeg::{HardwareDevice, HardwareDeviceType, library_version};
#[cfg(windows)]
use windows::Win32::Foundation::HMODULE;
#[cfg(windows)]
use windows::Win32::Graphics::Direct3D::{
    D3D_DRIVER_TYPE_UNKNOWN, D3D_FEATURE_LEVEL_11_0, D3D_FEATURE_LEVEL_11_1,
};
#[cfg(windows)]
use windows::Win32::Graphics::Direct3D11::{
    D3D11_CREATE_DEVICE_VIDEO_SUPPORT, D3D11_SDK_VERSION, D3D11_TEXTURE2D_DESC,
    D3D11_USAGE_DEFAULT, D3D11CreateDevice, ID3D11Device, ID3D11Texture2D,
};
#[cfg(windows)]
use windows::Win32::Graphics::Dxgi::Common::{DXGI_FORMAT_P010, DXGI_SAMPLE_DESC};
#[cfg(windows)]
use windows::Win32::Graphics::Dxgi::{CreateDXGIFactory1, DXGI_ERROR_NOT_FOUND, IDXGIFactory1};

fn main() -> Result<(), Box<dyn std::error::Error>> {
    #[cfg(target_os = "linux")]
    let (device_type, device_name) = (HardwareDeviceType::VaApi, Some("/dev/dri/renderD128"));
    #[cfg(windows)]
    let (device_type, device_name) = (HardwareDeviceType::D3d11Va, None);
    #[cfg(not(any(target_os = "linux", windows)))]
    return Err("the hardware-device proof supports only Linux and Windows".into());

    let device = HardwareDevice::open(device_type, device_name)?;
    println!("ffmpeg_version={}", library_version().to_string_lossy());
    println!("hardware_device={}", device.device_type());

    #[cfg(not(windows))]
    {
        let mut decoder = Av1VaApiDecoder::open(&device)?;
        println!("codec=av1");
        println!("codec_operation=decoder_open");
        if let Ok(path) = std::env::var("RUSTCONSOLE_AV1_PROOF_PACKET") {
            let packet = std::fs::read(path)?;
            let frame = decoder.decode_one_packet(&packet)?;
            let mapped = frame.map_dma_buf()?;
            println!("decoded_width={}", mapped.width());
            println!("decoded_height={}", mapped.height());
            println!("color_description={:?}", frame.color_description());
            println!("drm_layer_count={}", mapped.layers().len());
            for (index, layer) in mapped.layers().iter().enumerate() {
                println!("drm_layer_{index}_format={:#010x}", layer.format);
                println!("drm_layer_{index}_planes={}", layer.planes.len());
            }
        }
    }

    #[cfg(windows)]
    {
        let d3d11_device = nvidia_d3d11_device()?;
        let device = HardwareDevice::from_d3d11_device(&d3d11_device)?;
        let texture = p010_texture(&d3d11_device, 2560, 1440)?;
        let mut encoder = Av1NvencEncoder::open(
            &device,
            Av1EncoderConfiguration {
                width: 2560,
                height: 1440,
                frames_per_second: 120,
                bitrate_bits_per_second: 20_000_000,
                frame_format: Av1FrameFormat::Yuv420Ten,
                color_description: Av1ColorDescription::Bt2020PqLimited,
            },
        )?;
        let first_packet = (0..4)
            .find_map(|timestamp| {
                encoder
                    .encode_d3d11_texture(&texture, timestamp)
                    .transpose()
            })
            .transpose()?
            .ok_or("AV1 NVENC returned no packet before reconfiguration")?;
        encoder.set_bitrate(8_000_000)?;
        let second_packet = (4..8)
            .find_map(|timestamp| {
                encoder
                    .encode_d3d11_texture(&texture, timestamp)
                    .transpose()
            })
            .transpose()?
            .ok_or("AV1 NVENC returned no packet after reconfiguration")?;
        std::fs::write(
            std::env::temp_dir().join("rustconsole-av1-10bit.bin"),
            &first_packet.data,
        )?;
        println!("codec=av1_nvenc");
        println!("codec_operation=encode_and_reconfigure");
        println!("mode=2560x1440@120-yuv420-10bit");
        println!("initial_packet_bytes={}", first_packet.data.len());
        println!("reconfigured_packet_bytes={}", second_packet.data.len());
        println!("initial_bitrate_bits_per_second=20000000");
        println!("reconfigured_bitrate_bits_per_second=8000000");
        println!("vbv_frame_budgets=4");
        println!("encoder_recreated=false");
    }
    println!("status=ok");
    Ok(())
}

#[cfg(windows)]
fn nvidia_d3d11_device() -> Result<ID3D11Device, Box<dyn std::error::Error>> {
    let factory: IDXGIFactory1 = unsafe { CreateDXGIFactory1()? };
    for index in 0.. {
        let adapter = match unsafe { factory.EnumAdapters1(index) } {
            Ok(adapter) => adapter,
            Err(error) if error.code() == DXGI_ERROR_NOT_FOUND => break,
            Err(error) => return Err(error.into()),
        };
        if unsafe { adapter.GetDesc1()? }.VendorId != 0x10de {
            continue;
        }
        let mut device = None;
        let feature_levels = [D3D_FEATURE_LEVEL_11_1, D3D_FEATURE_LEVEL_11_0];
        unsafe {
            D3D11CreateDevice(
                &adapter,
                D3D_DRIVER_TYPE_UNKNOWN,
                HMODULE::default(),
                D3D11_CREATE_DEVICE_VIDEO_SUPPORT,
                Some(&feature_levels),
                D3D11_SDK_VERSION,
                Some(&mut device),
                None,
                None,
            )?;
        }
        return device.ok_or_else(|| "D3D11 returned no NVIDIA device".into());
    }
    Err("no NVIDIA adapter was found".into())
}

#[cfg(windows)]
fn p010_texture(
    device: &ID3D11Device,
    width: u32,
    height: u32,
) -> Result<ID3D11Texture2D, Box<dyn std::error::Error>> {
    let description = D3D11_TEXTURE2D_DESC {
        Width: width,
        Height: height,
        MipLevels: 1,
        ArraySize: 1,
        Format: DXGI_FORMAT_P010,
        SampleDesc: DXGI_SAMPLE_DESC {
            Count: 1,
            Quality: 0,
        },
        Usage: D3D11_USAGE_DEFAULT,
        ..Default::default()
    };
    let mut texture = None;
    unsafe { device.CreateTexture2D(&description, None, Some(&mut texture))? };
    texture.ok_or_else(|| "D3D11 returned no P010 texture".into())
}
