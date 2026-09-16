#[cfg(not(windows))]
fn main() {
    eprintln!("display route proof requires Windows host service privileges");
    std::process::exit(1);
}

#[cfg(windows)]
fn main() -> Result<(), Box<dyn std::error::Error>> {
    use rustconsole_host_windows::gpu_encode::{GpuCapture, GpuVideoPipeline, PreparedGpuCapture};
    use rustconsole_protocol::display::{AdapterId, DisplaySelection};
    use std::time::{Duration, Instant};
    use windows::Win32::Graphics::Direct3D11::D3D11_TEXTURE2D_DESC;
    use windows::Win32::Graphics::Dxgi::IDXGIDevice;
    use windows::core::Interface;

    let host = std::env::args_os()
        .nth(1)
        .ok_or("provide the host executable path")?;
    let inventory =
        rustconsole_host_windows::display::discover_active_console(std::path::Path::new(&host))?;
    let display = inventory.resolve(&DisplaySelection::Primary)?;
    let encode_adapter = inventory
        .adapters()
        .iter()
        .find(|adapter| adapter.vendor_id == 0x10de && !adapter.software)
        .ok_or("NVENC adapter unavailable for cross-adapter proof")?;
    for adapter in [display.adapter, encode_adapter.id] {
        let prepared = PreparedGpuCapture::for_display("Default", &display.id, adapter)?;
        let dxgi = prepared.processing_device().cast::<IDXGIDevice>()?;
        let description = unsafe { dxgi.GetAdapter()?.GetDesc()? };
        let actual = AdapterId(
            u64::from(description.AdapterLuid.LowPart)
                | ((description.AdapterLuid.HighPart as u32 as u64) << 32),
        );
        if actual != adapter {
            return Err("capture opened a different processing adapter".into());
        }
        let mut capture = GpuCapture::from_prepared(prepared)?;
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            if let Some(frame) = capture.next_frame(Duration::from_millis(100), false)? {
                let mut texture = D3D11_TEXTURE2D_DESC::default();
                unsafe { frame.texture.GetDesc(&mut texture) };
                if texture.Width != display.width
                    || texture.Height != display.height
                    || frame.metadata.last_present_time == 0
                {
                    return Err("capture returned invalid dimensions or timestamp".into());
                }
                println!(
                    "same_adapter={} capture_frame_valid=true",
                    adapter == display.adapter
                );
                break;
            }
            if Instant::now() >= deadline {
                return Err("capture did not produce a frame".into());
            }
        }
    }
    let prepared = PreparedGpuCapture::for_display("Default", &display.id, encode_adapter.id)?;
    let hardware =
        rustconsole_codec_ffmpeg::HardwareDevice::from_d3d11_device(prepared.processing_device())?;
    let mut decoder = rustconsole_codec_ffmpeg::Av1D3d11Decoder::open(&hardware)?;
    let mut pipeline = GpuVideoPipeline::from_prepared(prepared, 60, 20_000_000)?;
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        if let Some(frame) = pipeline.encode_next_frame(Duration::from_millis(100))? {
            if !frame.packet.keyframe
                || decoder
                    .decode_packet(&frame.packet.data, frame.packet.presentation_timestamp)?
                    .is_none()
            {
                return Err("captured recovery frame did not decode independently".into());
            }
            println!("capture_encoder_decoder_pipeline_valid=true");
            break;
        }
        if Instant::now() >= deadline {
            return Err("pipeline did not produce a frame".into());
        }
    }
    println!("status=ok");
    Ok(())
}
