// capture.rs — Universal Capture (DXGI Hardware + GDI Software Fallback)

use anyhow::{anyhow, Context, Result};
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::Arc;
use std::sync::mpsc::TrySendError;
use std::time::{Duration, Instant};

use windows::core::ComInterface;
use windows::Win32::Graphics::Direct3D::{
    D3D_DRIVER_TYPE_HARDWARE, D3D_DRIVER_TYPE_UNKNOWN, D3D_DRIVER_TYPE_WARP,
    D3D_FEATURE_LEVEL_11_0, D3D_FEATURE_LEVEL_11_1,
};
use windows::Win32::Graphics::Direct3D11::*;
use windows::Win32::Graphics::Dxgi::Common::{DXGI_FORMAT_B8G8R8A8_UNORM, DXGI_SAMPLE_DESC};
use windows::Win32::Graphics::Dxgi::*;
use windows::Win32::System::Com::{CoInitializeEx, CoUninitialize, COINIT_MULTITHREADED};
use windows::Win32::Graphics::Gdi::*;

#[derive(Debug, Default)]
pub struct ResizeState {
    width: AtomicU32,
    height: AtomicU32,
}

impl ResizeState {
    pub fn new(width: Option<u32>, height: Option<u32>) -> Self {
        let state = Self::default();
        state.set_target(width, height);
        state
    }

    pub fn set_target(&self, width: Option<u32>, height: Option<u32>) {
        self.width.store(normalize_requested_dimension(width), Ordering::SeqCst);
        self.height.store(normalize_requested_dimension(height), Ordering::SeqCst);
    }

    pub fn target_size(&self, source_w: u32, source_h: u32) -> (u32, u32) {
        resolve_target_size(
            source_w,
            source_h,
            stored_dimension(self.width.load(Ordering::SeqCst)),
            stored_dimension(self.height.load(Ordering::SeqCst)),
        )
    }
}

pub enum CaptureEvent {
    // Path A: Hardware (GPU Zero-Copy)
    NewDevice(ID3D11Device, u32, u32),
    HardwareFrame {
        texture: ID3D11Texture2D,
        width:   u32,
        height:  u32,
        captured_at: Instant,
    },
    // Path B: Software (CPU Fallback for VMs/Servers)
    #[allow(dead_code)]
    SoftwareFrame {
        data:   Vec<u8>,
        width:  u32,
        height: u32,
        captured_at: Instant,
    }
}

pub fn run_capture_loop(
    frame_tx:  std::sync::mpsc::SyncSender<CaptureEvent>,
    stop_flag: Arc<AtomicBool>,
    fps:       u32,
    resize_state: Arc<ResizeState>,
) -> Result<()> {
    unsafe {
        CoInitializeEx(None, COINIT_MULTITHREADED).ok();
    }

    let frame_interval = Duration::from_micros(1_000_000 / fps.max(1) as u64);
    let mut last_frame = Instant::now() - frame_interval;

    'outer: loop {
        if stop_flag.load(Ordering::Relaxed) { break; }

        // --- PATH A: DXGI Hardware Capture ---
        if let Ok((device, ctx, dupl, width, height)) = init_dxgi_duplication() {
            if frame_tx.send(CaptureEvent::NewDevice(device.clone(), width, height)).is_err() {
                break 'outer;
            }

            let mut tex_pool = Vec::with_capacity(3);
            for _ in 0..3 {
                if let Ok(t) = create_gpu_texture(&device, width, height) {
                    tex_pool.push(t);
                }
            }
            if tex_pool.is_empty() {
                tracing::error!("DXGI capture texture pool init failed. Falling back to GDI Capture.");
                continue 'outer;
            }
            let mut pool_idx = 0;
            let mut last_sent_texture: Option<ID3D11Texture2D> = None;
            let mut last_duplicate_sent = Instant::now() - Duration::from_millis(100);

            tracing::info!("DXGI Hardware Capture Active: {}x{} @ {} fps", width, height, fps);
            
            'frame: loop {
                if stop_flag.load(Ordering::Relaxed) { break 'outer; }

                let mut resource = None;
                let mut info = DXGI_OUTDUPL_FRAME_INFO::default();
                unsafe {
                    match dupl.AcquireNextFrame(5, &mut info, &mut resource) {
                        Ok(_) => {
                            if last_frame.elapsed() >= frame_interval {
                                if let Some(res) = resource {
                                    let dxgi_tex: ID3D11Texture2D = res.cast().unwrap();
                                    let target_res: ID3D11Resource = tex_pool[pool_idx].cast().unwrap();
                                    let dxgi_res: ID3D11Resource = dxgi_tex.cast().unwrap();
                                    ctx.CopyResource(&target_res, &dxgi_res);

                                    let current_texture = tex_pool[pool_idx].clone();
                                    match frame_tx.try_send(CaptureEvent::HardwareFrame {
                                        texture: current_texture.clone(),
                                        width,
                                        height,
                                        captured_at: Instant::now(),
                                    }) {
                                        Ok(()) | Err(TrySendError::Full(_)) => {}
                                        Err(TrySendError::Disconnected(_)) => break 'outer,
                                    }
                                    last_sent_texture = Some(current_texture);
                                    last_duplicate_sent = Instant::now();
                                    pool_idx = (pool_idx + 1) % tex_pool.len();
                                }
                                last_frame = Instant::now();
                            }
                            let _ = dupl.ReleaseFrame();
                        }
                        Err(e) if e.code() == DXGI_ERROR_ACCESS_LOST => break 'frame, // Drop to outer loop to rebuild
                        Err(e) if e.code() == DXGI_ERROR_WAIT_TIMEOUT => {
                            if last_frame.elapsed() >= frame_interval
                                && last_duplicate_sent.elapsed() >= Duration::from_millis(90)
                            {
                                if let Some(texture) = &last_sent_texture {
                                    match frame_tx.try_send(CaptureEvent::HardwareFrame {
                                        texture: texture.clone(),
                                        width,
                                        height,
                                        captured_at: Instant::now(),
                                    }) {
                                        Ok(()) | Err(TrySendError::Full(_)) => {}
                                        Err(TrySendError::Disconnected(_)) => break 'outer,
                                    }
                                    last_frame = Instant::now();
                                    last_duplicate_sent = Instant::now();
                                }
                            }
                            continue 'frame
                        },
                        Err(e) => {
                            tracing::error!("DXGI Capture error: {}. Continuing.", e);
                            std::thread::sleep(Duration::from_millis(100));
                            continue 'frame;
                        }
                    }
                }
            }
        }

        // --- PATH B: GDI Software Capture (Fallback for VMs/Servers) ---
        tracing::warn!("Hardware capture unavailable or lost. Falling back to GDI Capture.");
        let (real_w, real_h) = get_primary_display_resolution().unwrap_or((1920, 1080));
        let (device, ctx) = match create_processing_device() {
            Ok(v) => v,
            Err(e) => {
                tracing::error!("GDI fallback device init failed: {}. Retrying hardware capture.", e);
                std::thread::sleep(Duration::from_millis(500));
                continue 'outer;
            }
        };
        let mut tex_pool: Vec<ID3D11Texture2D> = Vec::new();
        let mut pool_idx = 0usize;
        let mut active_target = (0, 0);

        while !stop_flag.load(Ordering::Relaxed) {
            if last_frame.elapsed() >= frame_interval {
                let target = resize_state.target_size(real_w, real_h);
                if target != active_target {
                    active_target = target;
                    tex_pool.clear();
                    for _ in 0..3 {
                        tex_pool.push(create_gpu_texture(&device, target.0, target.1)?);
                    }
                    if frame_tx.send(CaptureEvent::NewDevice(device.clone(), target.0, target.1)).is_err() {
                        break 'outer;
                    }
                    tracing::info!("GDI Capture Active: {}x{} -> {}x{} @ {} fps", real_w, real_h, target.0, target.1, fps);
                }

                match capture_screen_gdi(real_w, real_h) {
                    Ok(raw_data) => {
                        let sent_data = if active_target != (real_w, real_h) {
                            downscale_bgra(&raw_data, real_w, real_h, active_target.0, active_target.1)
                        } else {
                            raw_data
                        };

                        upload_bgra_texture(&ctx, &tex_pool[pool_idx], &sent_data, active_target.0)?;

                        match frame_tx.try_send(CaptureEvent::HardwareFrame {
                            texture: tex_pool[pool_idx].clone(),
                            width: active_target.0,
                            height: active_target.1,
                            captured_at: Instant::now(),
                        }) {
                            Ok(()) | Err(TrySendError::Full(_)) => {}
                            Err(TrySendError::Disconnected(_)) => break 'outer,
                        }
                        pool_idx = (pool_idx + 1) % tex_pool.len();
                    }
                    Err(e) => {
                        tracing::error!("GDI capture failed: {}. Retrying PATH A.", e);
                        std::thread::sleep(Duration::from_millis(500));
                        continue 'outer;
                    }
                }
                last_frame = Instant::now();
            }
            std::thread::sleep(Duration::from_millis(5));
        }
    }

    unsafe { CoUninitialize(); }
    Ok(())
}

fn capture_screen_gdi(width: u32, height: u32) -> Result<Vec<u8>> {
    unsafe {
        let h_screen = GetDC(None);
        let h_dc = CreateCompatibleDC(h_screen);
        let h_bitmap = CreateCompatibleBitmap(h_screen, width as i32, height as i32);
        SelectObject(h_dc, h_bitmap);
        
        BitBlt(h_dc, 0, 0, width as i32, height as i32, h_screen, 0, 0, SRCCOPY).context("GDI BitBlt failed")?;

        let mut bmi = BITMAPINFO {
            bmiHeader: BITMAPINFOHEADER {
                biSize: std::mem::size_of::<BITMAPINFOHEADER>() as u32,
                biWidth: width as i32,
                biHeight: -(height as i32), // Top-down
                biPlanes: 1,
                biBitCount: 32,
                biCompression: BI_RGB.0,
                ..Default::default()
            },
            ..Default::default()
        };

        let mut buffer = vec![0u8; (width * height * 4) as usize];
        GetDIBits(h_dc, h_bitmap, 0, height, Some(buffer.as_mut_ptr() as *mut _), &mut bmi, DIB_RGB_COLORS);

        DeleteObject(h_bitmap);
        DeleteDC(h_dc);
        ReleaseDC(None, h_screen);
        
        Ok(buffer)
    }
}

fn downscale_bgra(src: &[u8], src_w: u32, src_h: u32, dst_w: u32, dst_h: u32) -> Vec<u8> {
    let mut dst = vec![0u8; (dst_w * dst_h * 4) as usize];
    for y in 0..dst_h {
        let src_y = (y * src_h / dst_h) as usize;
        for x in 0..dst_w {
            let src_x = (x * src_w / dst_w) as usize;
            let src_idx = (src_y * src_w as usize + src_x) * 4;
            let dst_idx = (y as usize * dst_w as usize + x as usize) * 4;
            dst[dst_idx..dst_idx+4].copy_from_slice(&src[src_idx..src_idx+4]);
        }
    }
    dst
}

fn normalize_requested_dimension(value: Option<u32>) -> u32 {
    value.map(normalize_dimension).unwrap_or(0)
}

fn stored_dimension(value: u32) -> Option<u32> {
    (value > 0).then_some(value)
}

fn normalize_dimension(value: u32) -> u32 {
    let even = value.max(2);
    even - (even % 2)
}

pub fn resolve_target_size(
    source_w: u32,
    source_h: u32,
    requested_w: Option<u32>,
    requested_h: Option<u32>,
) -> (u32, u32) {
    let mut target_w = requested_w.unwrap_or(source_w);
    let mut target_h = requested_h.unwrap_or(source_h);

    match (requested_w, requested_h) {
        (Some(w), None) if source_w > 0 => {
            target_w = w;
            target_h = ((source_h as u64 * w as u64 + source_w as u64 / 2) / source_w as u64) as u32;
        }
        (None, Some(h)) if source_h > 0 => {
            target_h = h;
            target_w = ((source_w as u64 * h as u64 + source_h as u64 / 2) / source_h as u64) as u32;
        }
        (None, None) if target_w > 1920 => {
            let scale = 1920.0 / target_w as f32;
            target_w = 1920;
            target_h = (target_h as f32 * scale).round() as u32;
        }
        _ => {}
    }

    target_w = normalize_dimension(target_w);
    target_h = normalize_dimension(target_h);
    (target_w.max(2), target_h.max(2))
}

fn init_dxgi_duplication() -> Result<(ID3D11Device, ID3D11DeviceContext, IDXGIOutputDuplication, u32, u32)> {
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
                
                let creation_flags = D3D11_CREATE_DEVICE_BGRA_SUPPORT.0 as u32 
                                   | D3D11_CREATE_DEVICE_VIDEO_SUPPORT.0 as u32;

                if D3D11CreateDevice(
                    &adapter,
                    D3D_DRIVER_TYPE_UNKNOWN,
                    None,
                    D3D11_CREATE_DEVICE_FLAG(creation_flags),
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

                // Enable Multithread safety for shared Device Managers (needed by MF)
                if let Ok(mt) = ctx.cast::<ID3D11Multithread>() {
                    mt.SetMultithreadProtected(true);
                }

                if let Ok(dupl) = output1.DuplicateOutput(&device) {
                    let mut desc = DXGI_OUTPUT_DESC::default();
                    output.GetDesc(&mut desc).ok();
                    let w = (desc.DesktopCoordinates.right - desc.DesktopCoordinates.left).unsigned_abs();
                    let h = (desc.DesktopCoordinates.bottom - desc.DesktopCoordinates.top).unsigned_abs();
                    
                    return Ok((device, ctx, dupl, w, h));
                }
            }
        }
        Err(anyhow!("No display found for DXGI duplication"))
    }
}

fn create_gpu_texture(device: &ID3D11Device, w: u32, h: u32) -> Result<ID3D11Texture2D> {
    let desc = D3D11_TEXTURE2D_DESC {
        Width:      w,
        Height:     h,
        MipLevels:  1,
        ArraySize:  1,
        Format:     DXGI_FORMAT_B8G8R8A8_UNORM,
        SampleDesc: DXGI_SAMPLE_DESC { Count: 1, Quality: 0 },
        Usage:      D3D11_USAGE_DEFAULT,
        BindFlags:  (D3D11_BIND_RENDER_TARGET.0 | D3D11_BIND_SHADER_RESOURCE.0) as u32,
        CPUAccessFlags: 0,
        MiscFlags:  0,
    };
    unsafe {
        let mut tex = None;
        device.CreateTexture2D(&desc, None, Some(&mut tex)).context("CreateTexture2D GPU pool")?;
        Ok(tex.unwrap())
    }
}

fn create_processing_device() -> Result<(ID3D11Device, ID3D11DeviceContext)> {
    let creation_flags =
        D3D11_CREATE_DEVICE_BGRA_SUPPORT.0 as u32 | D3D11_CREATE_DEVICE_VIDEO_SUPPORT.0 as u32;

    for driver_type in [D3D_DRIVER_TYPE_HARDWARE, D3D_DRIVER_TYPE_WARP] {
        let mut device = None;
        let mut ctx = None;
        unsafe {
            if D3D11CreateDevice(
                None,
                driver_type,
                None,
                D3D11_CREATE_DEVICE_FLAG(creation_flags),
                Some(&[D3D_FEATURE_LEVEL_11_1, D3D_FEATURE_LEVEL_11_0]),
                D3D11_SDK_VERSION,
                Some(&mut device),
                None,
                Some(&mut ctx),
            )
            .is_ok()
            {
                let device = device.unwrap();
                let ctx = ctx.unwrap();
                if let Ok(mt) = ctx.cast::<ID3D11Multithread>() {
                    mt.SetMultithreadProtected(true);
                }
                return Ok((device, ctx));
            }
        }
    }

    Err(anyhow!("Failed to create D3D11 processing device"))
}

fn upload_bgra_texture(
    ctx: &ID3D11DeviceContext,
    texture: &ID3D11Texture2D,
    data: &[u8],
    width: u32,
) -> Result<()> {
    let row_pitch = (width * 4) as usize;
    let expected = row_pitch * texture_height(texture)? as usize;
    if data.len() != expected {
        return Err(anyhow!("BGRA upload buffer size mismatch"));
    }

    unsafe {
        let resource: ID3D11Resource = texture.cast()?;
        ctx.UpdateSubresource(
            &resource,
            0,
            None,
            data.as_ptr() as *const _,
            row_pitch as u32,
            0,
        );
    }

    Ok(())
}

fn texture_height(texture: &ID3D11Texture2D) -> Result<u32> {
    let mut desc = D3D11_TEXTURE2D_DESC::default();
    unsafe { texture.GetDesc(&mut desc) };
    Ok(desc.Height)
}

pub fn get_primary_display_resolution() -> Result<(u32, u32)> {
    unsafe {
        CoInitializeEx(None, COINIT_MULTITHREADED).ok();
        let factory: IDXGIFactory1 = CreateDXGIFactory1().context("CreateDXGIFactory1")?;

        let mut adapter_idx = 0u32;
        loop {
            let adapter = match factory.EnumAdapters1(adapter_idx) {
                Ok(a)  => a,
                Err(_) => break,
            };
            adapter_idx += 1;

            let mut output_idx = 0u32;
            loop {
                let output = match adapter.EnumOutputs(output_idx) {
                    Ok(o)  => o,
                    Err(_) => break,
                };
                output_idx += 1;

                if output.cast::<IDXGIOutput1>().is_err() { continue; }

                let mut desc = DXGI_OUTPUT_DESC::default();
                output.GetDesc(&mut desc).ok();
                let w = (desc.DesktopCoordinates.right  - desc.DesktopCoordinates.left).unsigned_abs();
                let h = (desc.DesktopCoordinates.bottom - desc.DesktopCoordinates.top).unsigned_abs();
                if w > 0 && h > 0 {
                    return Ok((w, h));
                }
            }
        }
        Err(anyhow!("No capture-capable display found"))
    }
}
