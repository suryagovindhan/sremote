// hw_capture.rs - Complete Rewrite
// Fix: Use MFT_ENUM_FLAG_SYNCMFT | MFT_ENUM_FLAG_SORTANDFILTER for Intel iGPU support.
// Fix: Implement BT.601 integer-based BGRA->NV12 conversion.
// Fix: Added robust MFT drain loop with Annex-B normalization.

use anyhow::{anyhow, Context, Result};
use std::sync::{
    atomic::{AtomicBool, Ordering},
    mpsc::Receiver,
    Arc,
};
use std::mem::ManuallyDrop;
use tokio::sync::mpsc::Sender;
use std::time::{Duration, Instant};

use windows::core::{ComInterface, GUID};
use windows::Win32::Graphics::Direct3D::{
    D3D_DRIVER_TYPE_UNKNOWN, D3D_FEATURE_LEVEL_11_0, D3D_FEATURE_LEVEL_11_1,
};
use windows::Win32::Graphics::Direct3D11::{
    D3D11CreateDevice, ID3D11Device, ID3D11DeviceContext, ID3D11Texture2D,
    D3D11_CPU_ACCESS_READ, D3D11_CREATE_DEVICE_BGRA_SUPPORT, D3D11_MAP_READ,
    D3D11_TEXTURE2D_DESC, D3D11_USAGE_STAGING, D3D11_SDK_VERSION, ID3D11Resource,
};
use windows::Win32::Graphics::Dxgi::Common::{DXGI_FORMAT_B8G8R8A8_UNORM, DXGI_SAMPLE_DESC};
use windows::Win32::Graphics::Dxgi::{
    CreateDXGIFactory1, IDXGIFactory1, IDXGIOutput1,
    IDXGIOutputDuplication, DXGI_ERROR_ACCESS_LOST, DXGI_ERROR_WAIT_TIMEOUT,
    DXGI_OUTDUPL_FRAME_INFO, DXGI_OUTPUT_DESC,
};
use windows::Win32::Media::MediaFoundation::{
    MFCreateMediaType, MFCreateSample, MFCreateMemoryBuffer, MFStartup, MFShutdown,
    IMFMediaType, IMFSample, IMFTransform, MFTEnumEx, MFT_CATEGORY_VIDEO_ENCODER,
    MFT_ENUM_FLAG_SYNCMFT, MFT_ENUM_FLAG_SORTANDFILTER, MFT_MESSAGE_COMMAND_FLUSH,
    MFT_MESSAGE_NOTIFY_BEGIN_STREAMING, MFT_OUTPUT_DATA_BUFFER, MFT_REGISTER_TYPE_INFO,
    MFVideoInterlace_Progressive, MF_VERSION, IMFActivate,
};
use windows::Win32::System::Com::{CoInitializeEx, CoUninitialize, COINIT_MULTITHREADED, CoTaskMemFree};

// MF GUIDs (Manually defined as requested)
const MF_MT_MAJOR_TYPE:       GUID = GUID::from_u128(0x48eba18e_f8c9_4687_bf11_0a74c9f96a46);
const MF_MT_SUBTYPE:          GUID = GUID::from_u128(0xf7e34c9a_42e8_4714_b74b_cb29d72c35e5);
const MF_MT_AVG_BITRATE:      GUID = GUID::from_u128(0x20332624_fb0d_4d9e_bd0d_cbf6786c102e);
const MF_MT_FRAME_RATE:       GUID = GUID::from_u128(0xc459a2e8_3a50_4f6a_aa72_9cb9cb8db587);
const MF_MT_FRAME_SIZE:       GUID = GUID::from_u128(0x1652c33d_e6b5_4b80_a6ea_50cb6db573ba);
const MF_MT_INTERLACE_MODE:   GUID = GUID::from_u128(0xe2724bb8_e676_4806_b4b2_a8d6efaafdf4);
const MF_MT_PIXEL_ASPECT_RATIO: GUID = GUID::from_u128(0xc6376a1e_8d0a_4027_be45_6d9a0ad39bb6);
const MF_MT_MPEG2_PROFILE:      GUID = GUID::from_u128(0xad7612f7_4e25_4833_9997_37e91d8350eb);
const MF_MT_ALL_SAMPLES_INDEPENDENT: GUID = GUID::from_u128(0x73d1072d_1870_4174_a039_693fb44c095c);
const MF_MT_MPEG2_LEVEL:        GUID = GUID::from_u128(0x96222fd2_33d6_4bb2_8d36_645169696bd0);
const MFMediaType_Video:      GUID = GUID::from_u128(0x73646976_0000_0010_8000_00aa00389b71);
const MFVideoFormat_H264:     GUID = GUID::from_u128(0x34363248_0000_0010_8000_00aa00389b71);
const MFVideoFormat_NV12:     GUID = GUID::from_u128(0x3231564e_0000_0010_8000_00aa00389b71);
const MF_LOW_LATENCY:         GUID = GUID::from_u128(0x9c27891a_ed7a_40e1_88e8_b22727a024ee);

// Error codes
const MF_E_TRANSFORM_NEED_MORE_INPUT: i32 = -2147018576; // 0xC00D36B0
const MF_E_TRANSFORM_STREAM_CHANGE:   i32 = -2147018575; // 0xC00D36B1

fn align_to_16(val: u32) -> u32 {
    (val + 15) & !15
}

pub fn get_primary_display_resolution() -> Result<(u32, u32)> {
    unsafe {
        CoInitializeEx(None, COINIT_MULTITHREADED).ok();
        let factory: IDXGIFactory1 = CreateDXGIFactory1().context("CreateDXGIFactory1")?;
        let adapter = factory.EnumAdapters1(0).context("EnumAdapters1(0)")?;
        let output = adapter.EnumOutputs(0).context("EnumOutputs(0)")?;
        let mut desc = DXGI_OUTPUT_DESC::default();
        output.GetDesc(&mut desc)?;
        let w = (desc.DesktopCoordinates.right - desc.DesktopCoordinates.left).abs() as u32;
        let h = (desc.DesktopCoordinates.bottom - desc.DesktopCoordinates.top).abs() as u32;
        CoUninitialize();
        if w > 0 && h > 0 {
            Ok((w, h))
        } else {
            Ok((1920, 1080))
        }
    }
}

pub fn run_hardware_capture(
    tx: Sender<Vec<u8>>,
    resize_rx: Receiver<(Option<u32>, Option<u32>)>,
    fps: u32,
    bitrate_kbps: u32,
    stop_flag: Arc<AtomicBool>,
) -> Result<()> {
    unsafe {
        CoInitializeEx(None, COINIT_MULTITHREADED).ok();
        MFStartup(MF_VERSION, 0).context("MFStartup")?;
    }

    let result = run_capture_loop_outer(&tx, &resize_rx, fps, bitrate_kbps, stop_flag);

    unsafe {
        MFShutdown().ok();
        CoUninitialize();
    }
    result
}

fn run_capture_loop_outer(
    tx: &Sender<Vec<u8>>,
    resize_rx: &Receiver<(Option<u32>, Option<u32>)>,
    fps: u32,
    bitrate_kbps: u32,
    stop_flag: Arc<AtomicBool>,
) -> Result<()> {
    let mut current_target_w: Option<u32> = None;
    let mut current_target_h: Option<u32> = None;
    let mut timestamp_ns: i64 = 0;
    let mut frame_count: u64 = 0;
    let mut last_timeout_log = Instant::now();
    let mut timeout_count = 0;

    'rebuild: loop {
        if stop_flag.load(Ordering::Relaxed) { break; }

        let (device, ctx, dupl, src_w, src_h) = match init_d3d11_and_dxgi() {
            Ok(res) => res,
            Err(e) => {
                tracing::error!("Capture init failed: {:#}. Retrying in 2s.", e);
                std::thread::sleep(Duration::from_secs(2));
                continue 'rebuild;
            }
        };

        let target_w = align_to_16(current_target_w.unwrap_or(src_w));
        let target_h = align_to_16(current_target_h.unwrap_or(src_h));

        let staging_tex = create_staging_texture(&device, src_w, src_h)?;
        
        let encoder = match init_mft_encoder(target_w, target_h, fps, bitrate_kbps) {
            Ok(enc) => enc,
            Err(e) => {
                tracing::error!("Encoder init failed: {:#}. Retrying...", e);
                std::thread::sleep(Duration::from_secs(2));
                continue 'rebuild;
            }
        };

        let frame_interval = Duration::from_micros(1_000_000 / fps as u64);
        let mut last_frame = Instant::now() - frame_interval;
        let mut first_frame = true;

        tracing::info!("Capture loop active at {}x{} (Source {}x{})", target_w, target_h, src_w, src_h);

        'frame: loop {
            if stop_flag.load(Ordering::Relaxed) { return Ok(()); }

            let mut pending_resize = None;
            while let Ok(resize) = resize_rx.try_recv() {
                pending_resize = Some(resize);
            }
            if let Some((w_opt, h_opt)) = pending_resize {
                let nw = align_to_16(w_opt.unwrap_or(src_w));
                let nh = align_to_16(h_opt.unwrap_or(src_h));
                if nw != target_w || nh != target_h {
                    current_target_w = w_opt;
                    current_target_h = h_opt;
                    tracing::info!("Rebuilding pipeline for new resolution: {}x{}", nw, nh);
                    unsafe { let _ = encoder.ProcessMessage(MFT_MESSAGE_COMMAND_FLUSH, 0); }
                    continue 'rebuild;
                }
            }

            let mut resource = None;
            unsafe {
                let mut info = DXGI_OUTDUPL_FRAME_INFO::default();
                match dupl.AcquireNextFrame(100, &mut info, &mut resource) {
                    Ok(_) => {
                        timeout_count = 0;
                    }
                    Err(e) => {
                        if e.code() == DXGI_ERROR_ACCESS_LOST {
                            tracing::warn!("Capture session lost (ACCESS_LOST). Rebuilding...");
                            continue 'rebuild;
                        } else if e.code() == DXGI_ERROR_WAIT_TIMEOUT {
                            timeout_count += 1;
                            if last_timeout_log.elapsed() > Duration::from_secs(10) {
                                tracing::info!("Screen static ({} timeouts)", timeout_count);
                                last_timeout_log = Instant::now();
                            }
                            continue 'frame;
                        } else {
                            tracing::warn!("AcquireNextFrame error: {}. Sleeping.", e);
                            std::thread::sleep(Duration::from_millis(100));
                            continue 'frame;
                        }
                    }
                }
            }

            if last_frame.elapsed() < frame_interval && !first_frame {
                unsafe { let _ = dupl.ReleaseFrame(); }
                continue 'frame;
            }
            last_frame = Instant::now();
            
            if first_frame {
                tracing::info!("First screen frame processed!");
                first_frame = false;
            }

            let res = resource.unwrap();
            let tex: ID3D11Texture2D = res.cast().unwrap();

            unsafe {
                ctx.CopyResource(&staging_tex, &tex);
                let _ = dupl.ReleaseFrame();

                let staging_res: ID3D11Resource = staging_tex.cast().unwrap();
                let mut mapped = windows::Win32::Graphics::Direct3D11::D3D11_MAPPED_SUBRESOURCE::default();
                if ctx.Map(Some(&staging_res), 0, D3D11_MAP_READ, 0, Some(&mut mapped)).is_ok() {
                    let pitch = mapped.RowPitch as usize;
                    let bgra_ptr = mapped.pData as *const u8;
                    let nv12 = bgra_to_nv12_scale(bgra_ptr, src_w, src_h, pitch, target_w, target_h);
                    ctx.Unmap(Some(&staging_res), 0);
                    
                    if let Err(e) = encode_and_stream_nv12(&encoder, &nv12, target_w, target_h, fps, tx, &mut timestamp_ns, &mut frame_count) {
                        tracing::error!("Encoding error: {:#}. Resetting.", e);
                        continue 'rebuild;
                    }
                }
            }
        }
    }
    Ok(())
}

fn init_d3d11_and_dxgi() -> Result<(ID3D11Device, ID3D11DeviceContext, IDXGIOutputDuplication, u32, u32)> {
    unsafe {
        let factory: IDXGIFactory1 = CreateDXGIFactory1().context("CreateDXGIFactory1")?;
        
        let mut adapter_idx = 0;
        loop {
            let adapter = match factory.EnumAdapters1(adapter_idx) {
                Ok(a) => a,
                Err(_) => break,
            };
            adapter_idx += 1;

            let mut output_idx = 0;
            loop {
                let output = match adapter.EnumOutputs(output_idx) {
                    Ok(o) => o,
                    Err(_) => break,
                };
                output_idx += 1;

                let output1: IDXGIOutput1 = match output.cast() {
                    Ok(o) => o,
                    Err(_) => continue,
                };

                let mut device = None;
                let mut ctx = None;
                // Use explicit adapter, not D3D_DRIVER_TYPE_HARDWARE with null
                if D3D11CreateDevice(
                    &adapter,
                    D3D_DRIVER_TYPE_UNKNOWN,
                    None,
                    D3D11_CREATE_DEVICE_BGRA_SUPPORT,
                    Some(&[D3D_FEATURE_LEVEL_11_1, D3D_FEATURE_LEVEL_11_0]),
                    D3D11_SDK_VERSION,
                    Some(&mut device),
                    None,
                    Some(&mut ctx),
                ).is_err() {
                    continue;
                }

                let device = device.unwrap();
                let ctx = ctx.unwrap();

                match output1.DuplicateOutput(&device) {
                    Ok(dupl) => {
                        let mut desc = DXGI_OUTPUT_DESC::default();
                        output1.GetDesc(&mut desc)?;
                        let w = (desc.DesktopCoordinates.right - desc.DesktopCoordinates.left).abs() as u32;
                        let h = (desc.DesktopCoordinates.bottom - desc.DesktopCoordinates.top).abs() as u32;
                        tracing::info!("Capture target: Adapter {}, Output {} ({}x{})", adapter_idx-1, output_idx-1, w, h);
                        return Ok((device, ctx, dupl, w, h));
                    }
                    Err(_) => continue,
                }
            }
        }
        Err(anyhow!("No duplicatable output found. Try running as Admin."))
    }
}

fn create_staging_texture(device: &ID3D11Device, w: u32, h: u32) -> Result<ID3D11Texture2D> {
    let desc = D3D11_TEXTURE2D_DESC {
        Width: w,
        Height: h,
        MipLevels: 1,
        ArraySize: 1,
        Format: DXGI_FORMAT_B8G8R8A8_UNORM,
        SampleDesc: DXGI_SAMPLE_DESC { Count: 1, Quality: 0 },
        Usage: D3D11_USAGE_STAGING,
        BindFlags: 0,
        CPUAccessFlags: D3D11_CPU_ACCESS_READ.0 as u32,
        MiscFlags: 0,
    };
    unsafe {
        let mut tex = None;
        device.CreateTexture2D(&desc, None, Some(&mut tex)).context("CreateTexture2D staging")?;
        Ok(tex.unwrap())
    }
}

fn init_mft_encoder(w: u32, h: u32, fps: u32, kbps: u32) -> Result<IMFTransform> {
    let w = align_to_16(w);
    let h = align_to_16(h);
    unsafe {
        let input_type_info  = MFT_REGISTER_TYPE_INFO { guidMajorType: MFMediaType_Video, guidSubtype: MFVideoFormat_NV12 };
        let output_type_info = MFT_REGISTER_TYPE_INFO { guidMajorType: MFMediaType_Video, guidSubtype: MFVideoFormat_H264 };

        let mut pp_mft: *mut Option<IMFActivate> = std::ptr::null_mut();
        let mut count = 0u32;

        MFTEnumEx(
            MFT_CATEGORY_VIDEO_ENCODER,
            MFT_ENUM_FLAG_SYNCMFT | MFT_ENUM_FLAG_SORTANDFILTER,
            Some(&input_type_info),
            Some(&output_type_info),
            &mut pp_mft,
            &mut count,
        ).context("MFTEnumEx")?;

        if count == 0 { return Err(anyhow!("No hardware H.264 encoder found")); }
        
        let activates = std::slice::from_raw_parts(pp_mft, count as usize);
        let activate  = activates[0].as_ref().ok_or_else(|| anyhow!("null IMFActivate"))?;
        let encoder: IMFTransform = activate.ActivateObject().context("ActivateObject")?;
        CoTaskMemFree(Some(pp_mft as *mut _));

        if let Ok(attr) = encoder.GetAttributes() { let _ = attr.SetUINT32(&MF_LOW_LATENCY, 1); }

        // Mandatory H.264 parameters
        let out_type: IMFMediaType = MFCreateMediaType()?;
        out_type.SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Video)?;
        out_type.SetGUID(&MF_MT_SUBTYPE, &MFVideoFormat_H264)?;
        out_type.SetUINT32(&MF_MT_AVG_BITRATE, kbps * 1024)?;
        out_type.SetUINT64(&MF_MT_FRAME_RATE, pack_u64(fps, 1))?;
        out_type.SetUINT64(&MF_MT_FRAME_SIZE, pack_u64(w, h))?;
        out_type.SetUINT32(&MF_MT_INTERLACE_MODE, MFVideoInterlace_Progressive.0 as u32)?;
        out_type.SetUINT64(&MF_MT_PIXEL_ASPECT_RATIO, pack_u64(1, 1))?;
        out_type.SetUINT32(&MF_MT_ALL_SAMPLES_INDEPENDENT, 1)?;
        out_type.SetUINT32(&MF_MT_MPEG2_PROFILE, 66)?; // Baseline
        out_type.SetUINT32(&MF_MT_MPEG2_LEVEL, 0x28)?; // 4.0

        // Attempt 1: Full config
        if let Err(e) = encoder.SetOutputType(0, &out_type, 0) {
            tracing::warn!("Encoder rejected 1080p config: {:#}. Trying 720p fallback...", e);
            let w720 = align_to_16(1280);
            let h720 = align_to_16(720);
            out_type.SetUINT64(&MF_MT_FRAME_SIZE, pack_u64(w720, h720))?;
            
            if let Err(e2) = encoder.SetOutputType(0, &out_type, 0) {
                 tracing::error!("Encoder rejected 720p fallback: {:#}. Fatal.", e2);
                 return Err(e2).context("SetOutputType final failure");
            }
        }

        let in_type: IMFMediaType = MFCreateMediaType()?;
        in_type.SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Video)?;
        in_type.SetGUID(&MF_MT_SUBTYPE, &MFVideoFormat_NV12)?;
        in_type.SetUINT64(&MF_MT_FRAME_RATE, pack_u64(fps, 1))?;
        in_type.SetUINT64(&MF_MT_FRAME_SIZE, out_type.GetUINT64(&MF_MT_FRAME_SIZE)?)?;
        in_type.SetUINT32(&MF_MT_INTERLACE_MODE, MFVideoInterlace_Progressive.0 as u32)?;
        in_type.SetUINT64(&MF_MT_PIXEL_ASPECT_RATIO, pack_u64(1, 1))?;
        encoder.SetInputType(0, &in_type, 0).context("SetInputType")?;

        let _ = encoder.ProcessMessage(MFT_MESSAGE_NOTIFY_BEGIN_STREAMING, 0);
        Ok(encoder)
    }
}

fn pack_u64(h: u32, l: u32) -> u64 { ((h as u64) << 32) | (l as u64) }

fn has_annexb(d: &[u8]) -> bool { 
    d.len() >= 4 && (d[0..4] == [0,0,0,1] || (d.len() >= 3 && d[0..3] == [0,0,1])) 
}

fn normalize_h264(d: &[u8]) -> Vec<u8> {
    if d.is_empty() { return vec![]; }
    if has_annexb(d) { return d.to_vec(); }
    // AVCC to Annex-B conversion
    let mut out = Vec::with_capacity(d.len() + 8);
    let mut i = 0;
    while i + 4 <= d.len() {
        let len = u32::from_be_bytes([d[i], d[i+1], d[i+2], d[i+3]]) as usize;
        i += 4;
        if len == 0 || i + len > d.len() { break; }
        out.extend_from_slice(&[0, 0, 0, 1]);
        out.extend_from_slice(&d[i..i+len]);
        i += len;
    }
    if out.is_empty() {
        out.extend_from_slice(&[0,0,0,1]);
        out.extend_from_slice(d);
    }
    out
}

fn encode_and_stream_nv12(mft: &IMFTransform, nv12: &[u8], _w: u32, _h: u32, fps: u32, tx: &Sender<Vec<u8>>, ts: &mut i64, fc: &mut u64) -> Result<()> {
    let dur = 10_000_000i64 / fps.max(1) as i64;
    unsafe {
        let sample: IMFSample = MFCreateSample()?;
        let buffer = MFCreateMemoryBuffer(nv12.len() as u32)?;
        let mut p = std::ptr::null_mut();
        if buffer.Lock(&mut p, None, None).is_ok() {
            std::ptr::copy_nonoverlapping(nv12.as_ptr(), p, nv12.len());
            buffer.SetCurrentLength(nv12.len() as u32)?;
            buffer.Unlock()?;
        }
        sample.AddBuffer(&buffer)?;
        sample.SetSampleDuration(dur)?;
        sample.SetSampleTime(*ts)?;
        *ts += dur;

        mft.ProcessInput(0, &sample, 0).context("ProcessInput")?;

        let mut out_data = [MFT_OUTPUT_DATA_BUFFER::default()];
        let s_info = mft.GetOutputStreamInfo(0)?;
        let provides_samples = (s_info.dwFlags & 0x00000100 /* MFT_OUTPUT_STREAM_PROVIDES_SAMPLES */) != 0;

        loop {
            if !provides_samples {
                let s = MFCreateSample()?;
                let b = MFCreateMemoryBuffer(s_info.cbSize.max(4*1024*1024))?;
                s.AddBuffer(&b)?;
                out_data[0].pSample = ManuallyDrop::new(Some(s));
            }

            match mft.ProcessOutput(0, &mut out_data, &mut 0) {
                Ok(_) => {
                    *fc += 1;
                    if *fc % 60 == 0 { tracing::info!("H.264 Heartbeat: {} frames encoded", fc); }
                    if let Some(s) = &*out_data[0].pSample {
                        if let Ok(b) = s.ConvertToContiguousBuffer() {
                            let mut p = std::ptr::null_mut();
                            let mut l = 0;
                            if b.Lock(&mut p, None, Some(&mut l)).is_ok() {
                                let unit = normalize_h264(std::slice::from_raw_parts(p, l as usize));
                                b.Unlock().ok();
                                let _ = tx.try_send(unit);
                            }
                        }
                    }
                    if provides_samples {
                        out_data[0].pSample = ManuallyDrop::new(None);
                    }
                }
                Err(e) if e.code().0 == MF_E_TRANSFORM_NEED_MORE_INPUT => break,
                Err(e) if e.code().0 == MF_E_TRANSFORM_STREAM_CHANGE => {
                    // Update output type if needed
                    continue;
                }
                Err(e) if e.code().0 == -2147011470 /* 0xC00D6D72 */ => break, // Backpressure
                Err(e) => return Err(e).context("ProcessOutput"),
            }
        }
        Ok(())
    }
}

fn bgra_to_nv12_scale(src: *const u8, sw: u32, sh: u32, pitch: usize, dw: u32, dh: u32) -> Vec<u8> {
    let y_sz = (dw * dh) as usize;
    let mut out = vec![0u8; y_sz + (y_sz / 2)];
    let (u_off, v_off) = (y_sz, y_sz + 1);
    
    // Ratios for sampling
    let sx = sw as f32 / dw as f32;
    let sy = sh as f32 / dh as f32;

    for y in 0..dh {
        let sy_i = (y as f32 * sy) as usize;
        for x in 0..dw {
            let sx_i = (x as f32 * sx) as usize;
            let i = (sy_i * pitch) + (sx_i * 4);
            unsafe {
                let b = *src.add(i) as i32;
                let g = *src.add(i+1) as i32;
                let r = *src.add(i+2) as i32;
                // Y = ((66*r + 129*g + 25*b + 128) >> 8) + 16
                out[y as usize * dw as usize + x as usize] = (((66*r + 129*g + 25*b + 128) >> 8) + 16) as u8;

                if y % 2 == 0 && x % 2 == 0 {
                    // Average 2x2 block for Chroma
                    let mut r_sum = r;
                    let mut g_sum = g;
                    let mut b_sum = b;
                    let mut samples = 1;

                    // Right
                    if x + 1 < dw {
                        let i_r = (sy_i * pitch) + (((x+1) as f32 * sx) as usize * 4);
                        r_sum += *src.add(i_r+2) as i32;
                        g_sum += *src.add(i_r+1) as i32;
                        b_sum += *src.add(i_r) as i32;
                        samples += 1;
                    }
                    // Bottom
                    if y + 1 < dh {
                        let sy_next = ((y+1) as f32 * sy) as usize;
                        let i_b = (sy_next * pitch) + (sx_i * 4);
                        r_sum += *src.add(i_b+2) as i32;
                        g_sum += *src.add(i_b+1) as i32;
                        b_sum += *src.add(i_b) as i32;
                        samples += 1;
                        // Bottom-Right
                        if x + 1 < dw {
                            let i_br = (sy_next * pitch) + (((x+1) as f32 * sx) as usize * 4);
                            r_sum += *src.add(i_br+2) as i32;
                            g_sum += *src.add(i_br+1) as i32;
                            b_sum += *src.add(i_br) as i32;
                            samples += 1;
                        }
                    }

                    let r_avg = r_sum / samples;
                    let g_avg = g_sum / samples;
                    let b_avg = b_sum / samples;

                    let uv_idx = (y / 2) as usize * dw as usize + (x as usize);
                    // U = ((-38*r - 74*g + 112*b + 128) >> 8) + 128
                    out[u_off + uv_idx] = (((-38*r_avg - 74*g_avg + 112*b_avg + 128) >> 8) + 128) as u8;
                    // V = ((112*r - 94*g - 18*b + 128) >> 8) + 128
                    out[v_off + uv_idx] = (((112*r_avg - 94*g_avg - 18*b_avg + 128) >> 8) + 128) as u8;
                }
            }
        }
    }
    out
}
