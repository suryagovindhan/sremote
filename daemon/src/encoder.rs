// encoder.rs — Enterprise Modular Encoder (Double-Header Sabotage Resolved)

use anyhow::{anyhow, Context, Result};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::sync::mpsc::{Receiver, RecvTimeoutError, TryRecvError};
use std::time::{Duration, Instant};
use tokio::sync::mpsc::Sender;

use windows::core::{ComInterface, Interface, GUID, PCSTR};
use windows::Win32::Graphics::Direct3D::{
    D3D_DRIVER_TYPE_HARDWARE, D3D_DRIVER_TYPE_WARP, D3D_FEATURE_LEVEL_11_0,
    D3D_FEATURE_LEVEL_11_1,
    Fxc::{D3DCompile, D3DCOMPILE_ENABLE_STRICTNESS},
    ID3DBlob, D3D_PRIMITIVE_TOPOLOGY_TRIANGLESTRIP, D3D11_SRV_DIMENSION_TEXTURE2D,
};
use windows::Win32::Graphics::Direct3D11::*;
use windows::Win32::Graphics::Dxgi::Common::*;
use windows::Win32::Media::MediaFoundation::*;
use windows::Win32::System::Com::*;
use windows::Win32::System::Variant::*;
use windows::Win32::Foundation::{E_NOTIMPL, VARIANT_BOOL};

use crate::capture::{CaptureEvent, ResizeState};

#[derive(Debug)]
pub struct EncodedFrame {
    pub data: Vec<u8>,
    pub duration: Duration,
    pub captured_at: Instant,
}

const VS_SRC: &str = r#"struct VSOut { float4 pos : SV_POSITION; float2 uv : TEXCOORD0; }; VSOut vs_main(uint id : SV_VertexID) { float2 pos[4] = { float2(-1, -1), float2(-1, 1), float2(1, -1), float2(1, 1) }; float2 uv[4]  = { float2(0, 1), float2(0, 0), float2(1, 1), float2(1, 0) }; VSOut o; o.pos = float4(pos[id], 0, 1); o.uv = uv[id]; return o; }"#;
const PS_Y_SRC: &str = r#"Texture2D tex0 : register(t0); SamplerState samp0 : register(s0); float4 ps_y(float4 pos : SV_POSITION, float2 uv : TEXCOORD0) : SV_Target { float3 rgb = tex0.Sample(samp0, uv).rgb; float y = 0.257 * rgb.r + 0.504 * rgb.g + 0.098 * rgb.b + 0.0625; return float4(y, 0, 0, 1); }"#;
const PS_UV_SRC: &str = r#"Texture2D tex0 : register(t0); SamplerState samp0 : register(s0); float4 ps_uv(float4 pos : SV_POSITION, float2 uv : TEXCOORD0) : SV_Target { float3 rgb = tex0.Sample(samp0, uv).rgb; float u = -0.148 * rgb.r - 0.291 * rgb.g + 0.439 * rgb.b + 0.5; float v =  0.439 * rgb.r - 0.368 * rgb.g - 0.071 * rgb.b + 0.5; return float4(u, v, 0, 1); }"#;

const MF_MT_MAJOR_TYPE: GUID = GUID::from_u128(0x48eba18e_f8c9_4687_bf11_0a74c9f96a46);
const MF_MT_SUBTYPE: GUID = GUID::from_u128(0xf7e34c9a_42e8_4714_b74b_cb29d72c35e5);
const MF_MT_AVG_BITRATE: GUID = GUID::from_u128(0x20332624_fb0d_4d9e_bd0d_cbf6786c102e);
const MF_MT_FRAME_RATE: GUID = GUID::from_u128(0xc459a2e8_3d2c_4e44_b132_fee5156c7bb0);
const MF_MT_FRAME_SIZE: GUID = GUID::from_u128(0x1652c33d_d6b2_4012_b834_72030849a37d);
const MF_MT_INTERLACE_MODE: GUID = GUID::from_u128(0xe2724bb8_e676_4806_b4b2_a8d6efb44ccd);
const MF_MT_PIXEL_ASPECT_RATIO: GUID = GUID::from_u128(0xc6376a1e_8d0a_4027_be45_6d9a0ad39bb6);
const MF_MEDIA_TYPE_VIDEO: GUID = GUID::from_u128(0x73646976_0000_0010_8000_00aa00389b71);
const MF_VIDEO_FORMAT_H264: GUID = GUID::from_u128(0x34363248_0000_0010_8000_00aa00389b71);
const MF_VIDEO_FORMAT_NV12: GUID = GUID::from_u128(0x3231564e_0000_0010_8000_00aa00389b71);
const MF_LOW_LATENCY: GUID = GUID::from_u128(0x9c27891a_ed7a_40e1_88e8_b22727a024ee);
const MF_MT_MPEG2_PROFILE: GUID = GUID::from_u128(0x9622e67a_19e4_42e1_bc77_6f29971d5072);
const MF_MT_MPEG2_LEVEL: GUID = GUID::from_u128(0x9622e67b_19e4_42e1_bc77_6f29971d5072);
const MF_MT_MPEG_SEQUENCE_HEADER: GUID = GUID::from_u128(0x11038703_10d5_4d3c_8283_939d96221b25);
const MFSampleExtension_CleanPoint: GUID = GUID::from_u128(0x9cdf01d8_a0f0_43ba_b077_eaa06cbddada);

const CODECAPI_AVENC_COMMON_RATE_CONTROL_MODE: GUID = GUID::from_u128(0x1c0608e9_370c_4710_8a58_cb6181c42423);
const CODECAPI_AVENC_COMMON_MEAN_BIT_RATE: GUID = GUID::from_u128(0xf7222374_2144_4815_b550_a37f8e12ee52);
const CODECAPI_AVEncMPVGOPSize: GUID = GUID::from_u128(0x95f31b26_95a4_41aa_9303_246a7fc6eef1);
const CODECAPI_AVEncMPVDefaultBPictureCount: GUID = GUID::from_u128(0x8d390aac_dc5c_4200_b57f_dd9f4fa1d112);
const CODECAPI_AVEncH264CABACEnable: GUID = GUID::from_u128(0xee6cad62_d305_4248_a5e9_e1b143ce7a54);
const CODECAPI_AVEncMPVProfile: GUID = GUID::from_u128(0x580213d7_d542_451f_9b87_f647fdca6541);
const CODECAPI_AVEncVideoForceKeyFrame: GUID = GUID::from_u128(0x6d6cbb60_70d9_4d64_9e4c_5b0f6c4f4d1f);

const E_AVENC_H264_VPROFILE_BASE: u32 = 66; 
const E_AVENC_H264_VLEVEL4_0: u32 = 40; 

#[derive(Debug, Clone, Copy, Default)]
pub struct EncoderConfig {
    pub width: u32,
    pub height: u32,
    pub fps: u32,
    pub bitrate_kbps: u32,
}

struct Nv12Cache {
    tex: Option<ID3D11Texture2D>,
    width: u32,
    height: u32,
}

struct StagingBgraCache {
    tex: Option<ID3D11Texture2D>,
    width: u32,
    height: u32,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum EncoderInputMode {
    DxgiSurface,
    SystemMemory,
}

fn cap_size_preserving_aspect(width: u32, height: u32, max_width: u32, max_height: u32) -> (u32, u32) {
    if width <= max_width && height <= max_height {
        return (width, height);
    }

    let scale_w = max_width as f32 / width as f32;
    let scale_h = max_height as f32 / height as f32;
    let scale = scale_w.min(scale_h);
    let mut out_w = ((width as f32) * scale).round() as u32;
    let mut out_h = ((height as f32) * scale).round() as u32;
    out_w -= out_w % 2;
    out_h -= out_h % 2;
    (out_w.max(2), out_h.max(2))
}

fn get_or_create_nv12(
    cache: &mut Nv12Cache,
    device: &ID3D11Device,
    width: u32,
    height: u32,
) -> Result<ID3D11Texture2D> {
    if cache.tex.is_none() || cache.width != width || cache.height != height {
        let desc = D3D11_TEXTURE2D_DESC {
            Width: width,
            Height: height,
            MipLevels: 1,
            ArraySize: 1,
            Format: DXGI_FORMAT_NV12,
            SampleDesc: DXGI_SAMPLE_DESC { Count: 1, Quality: 0 },
            Usage: D3D11_USAGE_DEFAULT,
            BindFlags: (D3D11_BIND_SHADER_RESOURCE.0
                | D3D11_BIND_RENDER_TARGET.0
                | D3D11_BIND_VIDEO_ENCODER.0) as u32,
            CPUAccessFlags: 0,
            MiscFlags: 0,
        };

        let mut tex = None;
        unsafe { device.CreateTexture2D(&desc, None, Some(&mut tex))?; }
        cache.tex = Some(tex.unwrap());
        cache.width = width;
        cache.height = height;
    }
    Ok(cache.tex.as_ref().unwrap().clone())
}

fn compile_shader(src: &str, entry: &str, target: &str) -> Result<ID3DBlob> {
    unsafe {
        let mut blob = None;
        if D3DCompile(src.as_ptr() as _, src.len(), PCSTR::null(), None, None, PCSTR(entry.as_ptr()), PCSTR(target.as_ptr()), D3DCOMPILE_ENABLE_STRICTNESS, 0, &mut blob, None).is_err() {
            return Err(anyhow!("Shader compilation failed"));
        }
        Ok(blob.unwrap())
    }
}

fn ensure_annexb(data: &[u8]) -> Vec<u8> {
    if data.windows(4).any(|w| w == [0, 0, 0, 1]) || data.windows(3).any(|w| w == [0, 0, 1]) {
        return data.to_vec();
    }
    let mut out = Vec::with_capacity(data.len() + 64);
    let mut i = 0;
    while i + 4 <= data.len() {
        let len = u32::from_be_bytes([data[i], data[i+1], data[i+2], data[i+3]]) as usize;
        i += 4;
        if i + len > data.len() { break; }
        out.extend_from_slice(&[0,0,0,1]);
        out.extend_from_slice(&data[i..i+len]);
        i += len;
    }
    out
}

fn extract_sps_pps_annexb(data: &[u8]) -> (Vec<u8>, Vec<u8>) {
    let mut sps = Vec::new();
    let mut pps = Vec::new();
    let mut i = 0;
    while i + 4 <= data.len() {
        if data[i..].starts_with(&[0, 0, 0, 1]) || data[i..].starts_with(&[0, 0, 1]) {
            let offset = if data[i..].starts_with(&[0, 0, 0, 1]) { 4 } else { 3 };
            let start = i + offset;
            let mut end = data.len();
            for j in start..data.len().saturating_sub(3) {
                if data[j..].starts_with(&[0, 0, 0, 1]) || data[j..].starts_with(&[0, 0, 1]) {
                    end = j;
                    break;
                }
            }
            if start < end {
                let nal_type = data[start] & 0x1F;
                if nal_type == 7 { sps = data[start..end].to_vec(); }
                else if nal_type == 8 { pps = data[start..end].to_vec(); }
            }
            i = end;
        } else {
            i += 1;
        }
    }
    (sps, pps)
}

fn spoof_sps_to_baseline(data: &mut [u8], width: u32, fps: u32) {
    let level_idc: u8 = if width >= 3840 || fps > 60 { 0x33 } else if width > 1920 || fps >= 60 { 0x2A } else { 0x28 };
    let mut i = 0;
    while i + 3 <= data.len() {
        let is_4 = data[i..].starts_with(&[0,0,0,1]);
        let is_3 = data[i..].starts_with(&[0,0,1]);
        if is_4 || is_3 {
            let offset = if is_4 { 4 } else { 3 };
            if i + offset + 3 < data.len() && (data[i+offset] & 0x1F) == 7 { 
                data[i+offset+1] = 0x42; 
                data[i+offset+2] = 0xE0; 
                data[i+offset+3] = level_idc; 
                break;
            }
        }
        i += 1;
    }
}

fn contains_idr(data: &[u8]) -> bool {
    for i in 0..data.len().saturating_sub(4) {
        if data[i..].starts_with(&[0, 0, 0, 1]) {
            if (data[i + 4] & 0x1F) == 5 { return true; }
        } else if data[i..].starts_with(&[0, 0, 1]) {
            if (data[i + 3] & 0x1F) == 5 { return true; }
        }
    }
    false
}

// FIX: The Bitstream Surgeon. Cuts off the GPU's broken metadata so it doesn't override our spoofed headers.
fn strip_sps_pps(data: &[u8]) -> &[u8] {
    let mut i = 0;
    while i + 3 <= data.len() {
        let is_4 = data[i..].starts_with(&[0, 0, 0, 1]);
        let is_3 = data[i..].starts_with(&[0, 0, 1]);
        if is_4 || is_3 {
            let offset = if is_4 { 4 } else { 3 };
            if i + offset < data.len() {
                let nal_type = data[i + offset] & 0x1F;
                // Once we hit the actual image slice (NAL 1 = P-Frame, NAL 5 = IDR), we slice the rest of the frame.
                if nal_type == 1 || nal_type == 5 {
                    return &data[i..];
                }
            }
            i += offset;
        } else {
            i += 1;
        }
    }
    data // Fallback
}

struct Converter {
    device: ID3D11Device,
    ctx: ID3D11DeviceContext,
    vs: ID3D11VertexShader,
    ps_y: ID3D11PixelShader,
    ps_uv: ID3D11PixelShader,
    sampler: ID3D11SamplerState,
}

impl Converter {
    pub fn new(device: &ID3D11Device) -> Result<Self> {
        let ctx = unsafe { device.GetImmediateContext()? };
        let vs_blob = compile_shader(VS_SRC, "vs_main\0", "vs_5_0\0")?;
        let ps_y_blob = compile_shader(PS_Y_SRC, "ps_y\0", "ps_5_0\0")?;
        let ps_uv_blob = compile_shader(PS_UV_SRC, "ps_uv\0", "ps_5_0\0")?;

        let mut vs = None; unsafe { device.CreateVertexShader(std::slice::from_raw_parts(vs_blob.GetBufferPointer() as *const u8, vs_blob.GetBufferSize()), None, Some(&mut vs))?; }
        let mut ps_y = None; unsafe { device.CreatePixelShader(std::slice::from_raw_parts(ps_y_blob.GetBufferPointer() as *const u8, ps_y_blob.GetBufferSize()), None, Some(&mut ps_y))?; }
        let mut ps_uv = None; unsafe { device.CreatePixelShader(std::slice::from_raw_parts(ps_uv_blob.GetBufferPointer() as *const u8, ps_uv_blob.GetBufferSize()), None, Some(&mut ps_uv))?; }

        let sampler_desc = D3D11_SAMPLER_DESC {
            Filter: D3D11_FILTER_MIN_MAG_MIP_LINEAR,
            AddressU: D3D11_TEXTURE_ADDRESS_CLAMP,
            AddressV: D3D11_TEXTURE_ADDRESS_CLAMP,
            AddressW: D3D11_TEXTURE_ADDRESS_CLAMP,
            ComparisonFunc: D3D11_COMPARISON_NEVER,
            MinLOD: 0.0,
            MaxLOD: f32::MAX,
            ..Default::default()
        };
        let mut sampler = None; unsafe { device.CreateSamplerState(&sampler_desc, Some(&mut sampler))?; }

        Ok(Self { device: device.clone(), ctx, vs: vs.unwrap(), ps_y: ps_y.unwrap(), ps_uv: ps_uv.unwrap(), sampler: sampler.unwrap() })
    }

    pub fn convert(&self, bgra: &ID3D11Texture2D, nv12: &ID3D11Texture2D) -> Result<()> {
        let mut src_desc = D3D11_TEXTURE2D_DESC::default();
        let mut dst_desc = D3D11_TEXTURE2D_DESC::default();
        unsafe {
            bgra.GetDesc(&mut src_desc);
            nv12.GetDesc(&mut dst_desc);
        }

        let srv_desc = D3D11_SHADER_RESOURCE_VIEW_DESC {
            Format: src_desc.Format,
            ViewDimension: D3D11_SRV_DIMENSION_TEXTURE2D,
            Anonymous: D3D11_SHADER_RESOURCE_VIEW_DESC_0 { Texture2D: D3D11_TEX2D_SRV { MipLevels: 1, MostDetailedMip: 0 } },
        };
        let mut bgra_srv = None; unsafe { self.device.CreateShaderResourceView(bgra, Some(&srv_desc), Some(&mut bgra_srv))?; }
        let bgra_srv = bgra_srv.unwrap();

        let y_rtv_desc = D3D11_RENDER_TARGET_VIEW_DESC {
            Format: DXGI_FORMAT_R8_UNORM,
            ViewDimension: D3D11_RTV_DIMENSION_TEXTURE2D,
            Anonymous: D3D11_RENDER_TARGET_VIEW_DESC_0 { Texture2D: D3D11_TEX2D_RTV { MipSlice: 0 } },
        };
        let mut y_rtv = None; unsafe { self.device.CreateRenderTargetView(nv12, Some(&y_rtv_desc), Some(&mut y_rtv))?; }
        let y_rtv = y_rtv.unwrap();

        let uv_rtv_desc = D3D11_RENDER_TARGET_VIEW_DESC {
            Format: DXGI_FORMAT_R8G8_UNORM,
            ViewDimension: D3D11_RTV_DIMENSION_TEXTURE2D,
            Anonymous: D3D11_RENDER_TARGET_VIEW_DESC_0 { Texture2D: D3D11_TEX2D_RTV { MipSlice: 0 } },
        };
        let mut uv_rtv = None; unsafe { self.device.CreateRenderTargetView(nv12, Some(&uv_rtv_desc), Some(&mut uv_rtv))?; }
        let uv_rtv = uv_rtv.unwrap();

        unsafe {
            let ctx = &self.ctx;
            ctx.IASetPrimitiveTopology(D3D_PRIMITIVE_TOPOLOGY_TRIANGLESTRIP);
            ctx.VSSetShader(&self.vs, None);
            ctx.PSSetSamplers(0, Some(&[Some(self.sampler.clone())]));
            ctx.PSSetShaderResources(0, Some(&[Some(bgra_srv.clone())]));

            ctx.OMSetRenderTargets(Some(&[Some(y_rtv)]), None);
            ctx.PSSetShader(&self.ps_y, None);
            ctx.RSSetViewports(Some(&[D3D11_VIEWPORT { Width: dst_desc.Width as f32, Height: dst_desc.Height as f32, MinDepth: 0.0, MaxDepth: 1.0, TopLeftX: 0.0, TopLeftY: 0.0 }]));
            ctx.Draw(4, 0);

            ctx.OMSetRenderTargets(Some(&[Some(uv_rtv)]), None);
            ctx.PSSetShader(&self.ps_uv, None);
            ctx.RSSetViewports(Some(&[D3D11_VIEWPORT { Width: (dst_desc.Width / 2) as f32, Height: (dst_desc.Height / 2) as f32, MinDepth: 0.0, MaxDepth: 1.0, TopLeftX: 0.0, TopLeftY: 0.0 }]));
            ctx.Draw(4, 0);

            ctx.OMSetRenderTargets(None, None);
            ctx.PSSetShaderResources(0, Some(&[None]));
        }
        Ok(())
    }
}

fn create_processing_device() -> Result<ID3D11Device> {
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
                if let Some(ctx) = ctx {
                    if let Ok(mt) = ctx.cast::<ID3D11Multithread>() {
                        mt.SetMultithreadProtected(true);
                    }
                }
                return Ok(device.unwrap());
            }
        }
    }

    Err(anyhow!("Failed to create D3D11 processing device"))
}

fn reinitialize_pipeline(
    device: &ID3D11Device,
    source_width: u32,
    source_height: u32,
    config: &mut EncoderConfig,
    resize_state: &ResizeState,
    converter: &mut Option<Converter>,
    encoder: &mut Option<Encoder>,
    cache: &mut Nv12Cache,
    locked_size: &mut Option<(u32, u32)>,
) -> Result<()> {
    let (mut target_width, mut target_height) = resize_state.target_size(source_width, source_height);
    if let Some((locked_w, locked_h)) = *locked_size {
        target_width = locked_w;
        target_height = locked_h;
    }
    if converter.is_some()
        && encoder.is_some()
        && config.width == target_width
        && config.height == target_height
    {
        return Ok(());
    }

    let new_converter = Converter::new(device).context("Converter init")?;
    let new_encoder = Encoder::new(device, target_width, target_height, config.fps, config.bitrate_kbps)
        .with_context(|| {
            format!(
                "Encoder init failed for source {}x{} -> {}x{}",
                source_width, source_height, target_width, target_height
            )
        })?;

    config.width = new_encoder.coded_width;
    config.height = new_encoder.coded_height;
    *locked_size = Some((config.width, config.height));
    cache.tex = None;
    cache.width = 0;
    cache.height = 0;
    *converter = Some(new_converter);
    *encoder = Some(new_encoder);
    tracing::info!(
        "Initializing Modular Pipeline (source {}x{}, encode {}x{})...",
        source_width,
        source_height,
        config.width,
        config.height
    );
    Ok(())
}

fn get_or_create_staging_bgra(
    cache: &mut StagingBgraCache,
    device: &ID3D11Device,
    width: u32,
    height: u32,
) -> Result<ID3D11Texture2D> {
    if cache.tex.is_none() || cache.width != width || cache.height != height {
        let desc = D3D11_TEXTURE2D_DESC {
            Width: width,
            Height: height,
            MipLevels: 1,
            ArraySize: 1,
            Format: DXGI_FORMAT_B8G8R8A8_UNORM,
            SampleDesc: DXGI_SAMPLE_DESC { Count: 1, Quality: 0 },
            Usage: D3D11_USAGE_STAGING,
            BindFlags: 0,
            CPUAccessFlags: D3D11_CPU_ACCESS_READ.0 as u32,
            MiscFlags: 0,
        };
        let mut tex = None;
        unsafe { device.CreateTexture2D(&desc, None, Some(&mut tex))?; }
        cache.tex = Some(tex.unwrap());
        cache.width = width;
        cache.height = height;
    }
    Ok(cache.tex.as_ref().unwrap().clone())
}

fn read_bgra_texture_to_vec(
    device: &ID3D11Device,
    texture: &ID3D11Texture2D,
    width: u32,
    height: u32,
    cache: &mut StagingBgraCache,
) -> Result<Vec<u8>> {
    let staging = get_or_create_staging_bgra(cache, device, width, height)?;
    let ctx = unsafe { device.GetImmediateContext()? };
    unsafe {
        let dst: ID3D11Resource = staging.cast()?;
        let src: ID3D11Resource = texture.cast()?;
        ctx.CopyResource(&dst, &src);
    }

    let mut mapped = D3D11_MAPPED_SUBRESOURCE::default();
    unsafe {
        ctx.Map(&staging, 0, D3D11_MAP_READ, 0, Some(&mut mapped))?;
    }

    let src_pitch = mapped.RowPitch as usize;
    let dst_pitch = (width * 4) as usize;
    let mut out = vec![0u8; dst_pitch * height as usize];
    unsafe {
        let src_ptr = mapped.pData as *const u8;
        for y in 0..height as usize {
            let src_row = std::slice::from_raw_parts(src_ptr.add(y * src_pitch), dst_pitch);
            let dst_row = &mut out[y * dst_pitch..(y + 1) * dst_pitch];
            dst_row.copy_from_slice(src_row);
        }
        ctx.Unmap(&staging, 0);
    }

    Ok(out)
}

fn bgra_to_nv12(src: &[u8], width: u32, height: u32) -> Vec<u8> {
    let w = width as usize;
    let h = height as usize;
    let mut out = vec![0u8; w * h + w * h / 2];
    let (y_plane, uv_plane) = out.split_at_mut(w * h);

    for y in 0..h {
        for x in 0..w {
            let i = (y * w + x) * 4;
            let b = src[i] as f32;
            let g = src[i + 1] as f32;
            let r = src[i + 2] as f32;
            let yv = (0.257 * r + 0.504 * g + 0.098 * b + 16.0).round().clamp(0.0, 255.0) as u8;
            y_plane[y * w + x] = yv;
        }
    }

    for y in (0..h).step_by(2) {
        for x in (0..w).step_by(2) {
            let mut u_sum = 0.0f32;
            let mut v_sum = 0.0f32;
            for dy in 0..2 {
                for dx in 0..2 {
                    let px = x + dx;
                    let py = y + dy;
                    let i = (py * w + px) * 4;
                    let b = src[i] as f32;
                    let g = src[i + 1] as f32;
                    let r = src[i + 2] as f32;
                    u_sum += -0.148 * r - 0.291 * g + 0.439 * b + 128.0;
                    v_sum +=  0.439 * r - 0.368 * g - 0.071 * b + 128.0;
                }
            }
            let uv_index = (y / 2) * w + x;
            uv_plane[uv_index] = (u_sum / 4.0).round().clamp(0.0, 255.0) as u8;
            uv_plane[uv_index + 1] = (v_sum / 4.0).round().clamp(0.0, 255.0) as u8;
        }
    }

    out
}

fn resize_bgra_nearest(src: &[u8], src_w: u32, src_h: u32, dst_w: u32, dst_h: u32) -> Vec<u8> {
    if src_w == dst_w && src_h == dst_h {
        return src.to_vec();
    }

    let mut dst = vec![0u8; (dst_w * dst_h * 4) as usize];
    for y in 0..dst_h {
        let src_y = (y * src_h / dst_h) as usize;
        for x in 0..dst_w {
            let src_x = (x * src_w / dst_w) as usize;
            let src_idx = (src_y * src_w as usize + src_x) * 4;
            let dst_idx = (y as usize * dst_w as usize + x as usize) * 4;
            dst[dst_idx..dst_idx + 4].copy_from_slice(&src[src_idx..src_idx + 4]);
        }
    }
    dst
}

pub struct Encoder {
    encoder: IMFTransform,
    sps_pps: Vec<u8>,
    input_stream_id: u32,
    output_stream_id: u32,
    input_mode: EncoderInputMode,
    coded_width: u32,
    coded_height: u32,
    event_generator: Option<IMFMediaEventGenerator>,
    is_async: bool,
    async_need_input: bool,
    async_have_output: bool,
    rewrite_h264_headers: bool,
}

impl Encoder {
    pub fn new(device: &ID3D11Device, width: u32, height: u32, fps: u32, bitrate_kbps: u32) -> Result<Self> {
        let mut errors = Vec::new();
        match Self::try_create_with_flags(
            device,
            width,
            height,
            fps,
            bitrate_kbps,
            MFT_ENUM_FLAG_HARDWARE.0 | MFT_ENUM_FLAG_SORTANDFILTER.0,
        ) {
            Ok(encoder) => {
                tracing::info!("Using hardware H264 encoder MFT [build HW-GATE-5]");
                return Ok(encoder);
            }
            Err(err) => errors.push(format!("hardware: {:#}", err)),
        }

        let (sw_width, sw_height) = cap_size_preserving_aspect(width, height, 1280, 720);
        match Self::try_create_with_flags(
            device,
            sw_width,
            sw_height,
            fps,
            bitrate_kbps.min(2500),
            MFT_ENUM_FLAG_SYNCMFT.0 | MFT_ENUM_FLAG_SORTANDFILTER.0,
        ) {
            Ok(encoder) => {
                tracing::info!("Using sync-software H264 encoder MFT at {}x{}", sw_width, sw_height);
                return Ok(encoder);
            }
            Err(err) => errors.push(format!("sync-software: {:#}", err)),
        }

        Err(anyhow!(
            "No usable H264 encoder MFT found. Attempts: {}",
            errors.join(" | ")
        ))
    }

    fn try_create_with_flags(
        device: &ID3D11Device,
        width: u32,
        height: u32,
        fps: u32,
        bitrate_kbps: u32,
        flags: i32,
    ) -> Result<Self> {
        unsafe {
            let mut reset_token = 0;
            let mut device_manager: Option<IMFDXGIDeviceManager> = None;
            MFCreateDXGIDeviceManager(&mut reset_token, &mut device_manager).context("MFCreateDXGIDeviceManager")?;
            let device_manager = device_manager.unwrap();
            device_manager.ResetDevice(device, reset_token).context("ResetDevice")?;
            let manager_ptr = device_manager.as_raw() as usize;

            let in_info = MFT_REGISTER_TYPE_INFO { guidMajorType: MF_MEDIA_TYPE_VIDEO, guidSubtype: MF_VIDEO_FORMAT_NV12 };
            let out_info = MFT_REGISTER_TYPE_INFO { guidMajorType: MF_MEDIA_TYPE_VIDEO, guidSubtype: MF_VIDEO_FORMAT_H264 };

            let mut pp_mft: *mut Option<IMFActivate> = std::ptr::null_mut();
            let mut count = 0u32;
            MFTEnumEx(
                MFT_CATEGORY_VIDEO_ENCODER,
                MFT_ENUM_FLAG(flags),
                Some(&in_info),
                Some(&out_info),
                &mut pp_mft,
                &mut count,
            )
            .context("MFTEnumEx")?;
            if count == 0 {
                return Err(anyhow!("No encoder found for flags 0x{:X}", flags));
            }

            let activates = std::slice::from_raw_parts_mut(pp_mft, count as usize);
            let mut errors = Vec::new();

            for (idx, slot) in activates.iter_mut().enumerate() {
                let Some(activator) = slot.take() else { continue };
                let _ = activator.SetUINT32(&MF_TRANSFORM_ASYNC_UNLOCK, 1);

                let result: Result<Self> = (|| {
                    let encoder: IMFTransform = activator
                        .ActivateObject()
                        .context("ActivateObject(IMFTransform)")?;

                    let event_generator = encoder.cast::<IMFMediaEventGenerator>().ok();
                    let mut is_async = false;
                    if let Ok(attr) = encoder.GetAttributes().context("GetAttributes(IMFTransform)") {
                        let _ = attr.SetUINT32(&MF_LOW_LATENCY, 1);
                        let _ = attr.SetUINT32(&MF_TRANSFORM_ASYNC_UNLOCK, 1);
                        is_async = attr.GetUINT32(&MF_TRANSFORM_ASYNC).unwrap_or(0) != 0;
                        tracing::info!("Encoder candidate {} async={}", idx, is_async);
                        if attr.GetUINT32(&MF_TRANSFORM_ASYNC_UNLOCK).unwrap_or(0) != 0 {
                            tracing::info!("Encoder candidate {} unlocked successfully", idx);
                        }
                    }

                    let input_mode = match encoder.ProcessMessage(MFT_MESSAGE_SET_D3D_MANAGER, manager_ptr) {
                        Ok(()) => EncoderInputMode::DxgiSurface,
                        Err(e) if e.code().0 == 0x80004001u32 as i32 => EncoderInputMode::SystemMemory,
                        Err(e) => return Err(e).context("MFT_MESSAGE_SET_D3D_MANAGER"),
                    };

                    let mut input_count = 0u32;
                    let mut output_count = 0u32;
                    let (input_stream_id, output_stream_id) = match encoder.GetStreamCount(&mut input_count, &mut output_count) {
                        Ok(()) if input_count > 0 && output_count > 0 => {
                            let mut input_ids = vec![0u32; input_count as usize];
                            let mut output_ids = vec![0u32; output_count as usize];
                            match encoder.GetStreamIDs(&mut input_ids, &mut output_ids) {
                                Ok(()) => (input_ids[0], output_ids[0]),
                                Err(e) if e.code() == E_NOTIMPL => (0, 0),
                                Err(e) => return Err(e.into()),
                            }
                        }
                        _ => (0, 0),
                    };
                    tracing::info!(
                        "Encoder candidate {} stream ids in={} out={}",
                        idx,
                        input_stream_id,
                        output_stream_id
                    );

                    if let Ok(codec_api) = encoder.cast::<windows::Win32::Media::MediaFoundation::ICodecAPI>() {
                        let rate_control_mode = if input_mode == EncoderInputMode::DxgiSurface { 3 } else { 2 };
                        let mut v_mode = VARIANT::default(); (*v_mode.Anonymous.Anonymous).vt = VT_UI4; (*v_mode.Anonymous.Anonymous).Anonymous.ulVal = rate_control_mode;
                        let mut v_rate = VARIANT::default(); (*v_rate.Anonymous.Anonymous).vt = VT_UI4; (*v_rate.Anonymous.Anonymous).Anonymous.ulVal = bitrate_kbps * 1000;
                        let mut v_gop = VARIANT::default(); (*v_gop.Anonymous.Anonymous).vt = VT_UI4; (*v_gop.Anonymous.Anonymous).Anonymous.ulVal = (fps / 2).max(1);
                        let _ = codec_api.SetValue(&CODECAPI_AVENC_COMMON_RATE_CONTROL_MODE, &v_mode);
                        let _ = codec_api.SetValue(&CODECAPI_AVENC_COMMON_MEAN_BIT_RATE, &v_rate);
                        let _ = codec_api.SetValue(&CODECAPI_AVEncMPVGOPSize, &v_gop);

                        let mut v_bframes = VARIANT::default(); (*v_bframes.Anonymous.Anonymous).vt = VT_UI4; (*v_bframes.Anonymous.Anonymous).Anonymous.ulVal = 0;
                        let _ = codec_api.SetValue(&CODECAPI_AVEncMPVDefaultBPictureCount, &v_bframes);

                        let mut v_cabac = VARIANT::default(); (*v_cabac.Anonymous.Anonymous).vt = VT_BOOL; (*v_cabac.Anonymous.Anonymous).Anonymous.boolVal = VARIANT_BOOL(0);
                        let _ = codec_api.SetValue(&CODECAPI_AVEncH264CABACEnable, &v_cabac);

                        let mut v_profile = VARIANT::default(); (*v_profile.Anonymous.Anonymous).vt = VT_UI4; (*v_profile.Anonymous.Anonymous).Anonymous.ulVal = E_AVENC_H264_VPROFILE_BASE;
                        let _ = codec_api.SetValue(&CODECAPI_AVEncMPVProfile, &v_profile);
                    }

                    let mut out_type = None;
                    for i in 0..100 {
                        if let Ok(t) = encoder.GetOutputAvailableType(output_stream_id, i) {
                            if t.GetGUID(&MF_MT_SUBTYPE)? == MF_VIDEO_FORMAT_H264 { out_type = Some(t); break; }
                        } else { break; }
                    }
                    let out_type = out_type.ok_or(anyhow!("No H264 output type"))?;
                    out_type.SetUINT32(&MF_MT_AVG_BITRATE, bitrate_kbps * 1000)?;
                    out_type.SetUINT64(&MF_MT_FRAME_RATE, pack_u64(fps, 1))?;
                    out_type.SetUINT64(&MF_MT_FRAME_SIZE, pack_u64(width, height))?;
                    out_type.SetUINT32(&MF_MT_INTERLACE_MODE, 2)?;
                    out_type.SetUINT32(&MF_MT_MPEG2_PROFILE, E_AVENC_H264_VPROFILE_BASE)?;
                    out_type.SetUINT32(&MF_MT_MPEG2_LEVEL, E_AVENC_H264_VLEVEL4_0)?;
                    encoder
                        .SetOutputType(output_stream_id, &out_type, 0)
                        .context("SetOutputType(H264)")?;

                    let mut in_type = None;
                    for i in 0..100 {
                        if let Ok(t) = encoder.GetInputAvailableType(input_stream_id, i) {
                            if t.GetGUID(&MF_MT_SUBTYPE)? == MF_VIDEO_FORMAT_NV12 { in_type = Some(t); break; }
                        } else { break; }
                    }
                    let in_type = in_type.ok_or(anyhow!("No NV12 input type"))?;
                    in_type.SetUINT64(&MF_MT_FRAME_SIZE, pack_u64(width, height))?;
                    in_type.SetUINT64(&MF_MT_FRAME_RATE, pack_u64(fps, 1))?;
                    encoder
                        .SetInputType(input_stream_id, &in_type, 0)
                        .context("SetInputType(NV12)")?;

                    if input_mode == EncoderInputMode::DxgiSurface {
                        encoder
                            .ProcessMessage(MFT_MESSAGE_COMMAND_FLUSH, 0)
                            .context("MFT_MESSAGE_COMMAND_FLUSH")?;
                    }
                    let _ = encoder.ProcessMessage(MFT_MESSAGE_NOTIFY_BEGIN_STREAMING, 0);
                    let _ = encoder.ProcessMessage(MFT_MESSAGE_NOTIFY_START_OF_STREAM, 0);

                    let mut sps_pps = Vec::new();
                    let mut ptr = std::ptr::null_mut(); let mut len = 0;
                    if out_type.GetAllocatedBlob(&MF_MT_MPEG_SEQUENCE_HEADER, &mut ptr, &mut len).is_ok() {
                        let mut header = ensure_annexb(std::slice::from_raw_parts(ptr, len as usize));
                        spoof_sps_to_baseline(&mut header, width, fps);
                        sps_pps = header;
                        CoTaskMemFree(Some(ptr as *mut _));
                    }

                    Ok(Self {
                        encoder,
                        sps_pps,
                        input_stream_id,
                        output_stream_id,
                        input_mode,
                        coded_width: width,
                        coded_height: height,
                        event_generator,
                        is_async,
                        async_need_input: true,
                        async_have_output: false,
                        rewrite_h264_headers: input_mode != EncoderInputMode::DxgiSurface,
                    })
                })();

                match result {
                    Ok(encoder) => {
                        CoTaskMemFree(Some(pp_mft as *mut _));
                        return Ok(encoder);
                    }
                    Err(err) => errors.push(format!("candidate {}: {:#}", idx, err)),
                }
            }

            CoTaskMemFree(Some(pp_mft as *mut _));
            Err(anyhow!("No encoder candidate succeeded: {}", errors.join(" | ")))
        }
    }

    pub fn encode(&mut self, texture: &ID3D11Texture2D, ts: i64, dur: i64, width: u32, fps: u32, force_idr: bool) -> Result<Vec<Vec<u8>>> {
        unsafe {
            // FIX: Ensure an IDR frame is requested periodically to recover from WebRTC packet drops
            if ts == 0 || force_idr {
                if let Ok(codec_api) = self.encoder.cast::<ICodecAPI>() {
                    let mut v = VARIANT::default(); (*v.Anonymous.Anonymous).vt = VT_UI4; (*v.Anonymous.Anonymous).Anonymous.ulVal = 1;
                    let _ = codec_api.SetValue(&CODECAPI_AVEncVideoForceKeyFrame, &v);
                }
            }

            let buffer = MFCreateDXGISurfaceBuffer(&ID3D11Texture2D::IID, texture, 0, false)?;
            let sample = MFCreateSample()?;
            sample.AddBuffer(&buffer)?;
            sample.SetSampleTime(ts).ok();
            sample.SetSampleDuration(dur).ok();

            if self.is_async {
                let mut frames = Vec::new();
                if self.has_output_ready()? {
                    frames = self.drain_output(width, 0, fps)?;
                }
                let _ = self.process_input_sample(&sample)?;
                return Ok(frames);
            }

            let _ = self.process_input_sample(&sample)?;
            self.drain_output(width, 0, fps)
        }
    }

    pub fn encode_system_memory(
        &mut self,
        nv12: &[u8],
        ts: i64,
        dur: i64,
        width: u32,
        height: u32,
        fps: u32,
        force_idr: bool,
    ) -> Result<Vec<Vec<u8>>> {
        unsafe {
            if ts == 0 || force_idr {
                if let Ok(codec_api) = self.encoder.cast::<ICodecAPI>() {
                    let mut v = VARIANT::default(); (*v.Anonymous.Anonymous).vt = VT_UI4; (*v.Anonymous.Anonymous).Anonymous.ulVal = 1;
                    let _ = codec_api.SetValue(&CODECAPI_AVEncVideoForceKeyFrame, &v);
                }
            }

            let buffer = MFCreateMemoryBuffer(nv12.len() as u32)?;
            let mut ptr = std::ptr::null_mut();
            buffer.Lock(&mut ptr, None, None)?;
            std::ptr::copy_nonoverlapping(nv12.as_ptr(), ptr as *mut u8, nv12.len());
            buffer.Unlock()?;
            buffer.SetCurrentLength(nv12.len() as u32)?;

            let sample = MFCreateSample()?;
            sample.AddBuffer(&buffer)?;
            sample.SetSampleTime(ts).ok();
            sample.SetSampleDuration(dur).ok();

            if self.is_async {
                let mut frames = Vec::new();
                if self.has_output_ready()? {
                    frames = self.drain_output(width, height, fps)?;
                }
                let _ = self.process_input_sample(&sample)?;
                return Ok(frames);
            }

            let _ = self.process_input_sample(&sample)?;
            self.drain_output(width, height, fps)
        }
    }

    fn process_input_sample(&mut self, sample: &IMFSample) -> Result<bool> {
        unsafe {
            if self.is_async && !self.can_accept_input()? {
                return Ok(false);
            }
            match self.encoder.ProcessInput(self.input_stream_id, sample, 0) {
                Ok(()) => {
                    if self.is_async {
                        self.async_need_input = false;
                    }
                    Ok(true)
                }
                Err(e) => {
                    let code = e.code().0 as u32;
                    if code == 0xC00D36B5 {
                        if self.is_async {
                            self.async_have_output = true;
                            self.async_need_input = false;
                        }
                        Ok(false)
                    } else if code == 0xC00D36B4 {
                        Ok(false)
                    } else {
                        tracing::warn!("ProcessInput err: {:X}", code);
                        Ok(false)
                    }
                }
            }
        }
    }

    fn pump_async_events(&mut self) -> Result<()> {
        let Some(generator) = &self.event_generator else {
            return Ok(());
        };

        unsafe {
            loop {
                match generator.GetEvent(MF_EVENT_FLAG_NO_WAIT) {
                    Ok(event) => {
                        let event_type = event.GetType()?;
                        if event_type == METransformNeedInput.0 as u32 {
                            self.async_need_input = true;
                            tracing::debug!("async event: need input");
                        } else if event_type == METransformHaveOutput.0 as u32 {
                            self.async_have_output = true;
                            tracing::debug!("async event: have output");
                        } else {
                            tracing::debug!("async event: type={}", event_type);
                        }
                    }
                    Err(e) if e.code() == MF_E_NO_EVENTS_AVAILABLE => return Ok(()),
                    Err(e) => return Err(e.into()),
                }
            }
        }
    }

    fn can_accept_input(&mut self) -> Result<bool> {
        if self.is_async {
            self.pump_async_events()?;
            if self.async_need_input {
                return Ok(true);
            }

            unsafe {
                match self.encoder.GetInputStatus(self.input_stream_id) {
                    Ok(status) => return Ok((status & MFT_INPUT_STATUS_ACCEPT_DATA.0 as u32) != 0),
                    Err(_) => {}
                }
            }

            return Ok(false);
        }

        unsafe {
            match self.encoder.GetInputStatus(self.input_stream_id) {
                Ok(status) => return Ok((status & MFT_INPUT_STATUS_ACCEPT_DATA.0 as u32) != 0),
                Err(e) if e.code().0 as u32 == 0xC00D36B5 && self.input_stream_id != 0 => {
                    tracing::warn!(
                        "GetInputStatus rejected stream {} with MF_E_INVALIDSTREAMNUMBER; retrying stream 0",
                        self.input_stream_id
                    );
                    self.input_stream_id = 0;
                    self.output_stream_id = 0;
                    return Ok((self.encoder.GetInputStatus(0)? & MFT_INPUT_STATUS_ACCEPT_DATA.0 as u32) != 0);
                }
                Err(_) => {}
            }
        }

        self.async_ready_for_input()
    }

    fn async_ready_for_input(&mut self) -> Result<bool> {
        self.pump_async_events()?;
        Ok(self.async_need_input || !self.async_have_output)
    }

    fn async_has_output(&mut self) -> Result<bool> {
        self.pump_async_events()?;
        Ok(self.async_have_output)
    }

    fn has_output_ready(&mut self) -> Result<bool> {
        if self.is_async {
            self.pump_async_events()?;
            if self.async_have_output {
                return Ok(true);
            }

            unsafe {
                match self.encoder.GetOutputStatus() {
                    Ok(status) => return Ok((status & MFT_OUTPUT_STATUS_SAMPLE_READY.0 as u32) != 0),
                    Err(_) => {}
                }
            }

            return Ok(false);
        }

        unsafe {
            match self.encoder.GetOutputStatus() {
                Ok(status) => return Ok((status & MFT_OUTPUT_STATUS_SAMPLE_READY.0 as u32) != 0),
                Err(_) => {}
            }
        }

        self.async_has_output()
    }

    fn drain_output(&mut self, width: u32, _height: u32, fps: u32) -> Result<Vec<Vec<u8>>> {
        unsafe {
            let mut encoded_frames = Vec::new();
            if self.is_async && !self.has_output_ready()? {
                return Ok(encoded_frames);
            }
            let info = match self.encoder.GetOutputStreamInfo(self.output_stream_id) {
                Ok(info) => info,
                Err(e) if e.code().0 as u32 == 0xC00D36B5 && self.output_stream_id != 0 => {
                    tracing::warn!(
                        "GetOutputStreamInfo rejected stream {} with MF_E_INVALIDSTREAMNUMBER; retrying stream 0",
                        self.output_stream_id
                    );
                    self.output_stream_id = 0;
                    self.encoder.GetOutputStreamInfo(0)?
                }
                Err(e) => return Err(e.into()),
            };
            let provides = (info.dwFlags & MFT_OUTPUT_STREAM_PROVIDES_SAMPLES.0 as u32) != 0;
            tracing::debug!(
                "drain_output: async={} have_output={} provides={} flags=0x{:X} cbSize={}",
                self.is_async,
                self.async_have_output,
                provides,
                info.dwFlags,
                info.cbSize
            );

            loop {
                let mut out_data = [MFT_OUTPUT_DATA_BUFFER {
                    dwStreamID: self.output_stream_id,
                    ..Default::default()
                }];
                if !provides {
                    let s = MFCreateSample()?;
                    let b = MFCreateMemoryBuffer(info.cbSize.max(1024 * 1024))?;
                    s.AddBuffer(&b)?;
                    out_data[0].pSample = std::mem::ManuallyDrop::new(Some(s));
                }

                let mut status = 0;
                match self.encoder.ProcessOutput(0, &mut out_data, &mut status) {
                    Ok(_) => {
                        if self.is_async {
                            self.async_have_output = false;
                        }
                        if let Some(out_sample) = &*out_data[0].pSample {
                            let buffer = out_sample.ConvertToContiguousBuffer()?;
                            let mut ptr = std::ptr::null_mut(); let mut len = 0;
                            buffer.Lock(&mut ptr, None, Some(&mut len))?;

                            let raw_slice = std::slice::from_raw_parts(ptr, len as usize);
                            let data = ensure_annexb(raw_slice);
                            buffer.Unlock()?;

                            let is_keyframe = out_sample.GetUINT32(&MFSampleExtension_CleanPoint).unwrap_or(0) != 0;
                            let is_real_idr = contains_idr(&data);
                            let mut final_frame = Vec::new();

                            if !self.rewrite_h264_headers {
                                final_frame.extend_from_slice(&data);
                            } else if is_keyframe || is_real_idr {
                                if self.sps_pps.is_empty() {
                                    let (sps, pps) = extract_sps_pps_annexb(&data);
                                    if !sps.is_empty() && !pps.is_empty() {
                                        self.sps_pps.extend_from_slice(&[0, 0, 0, 1]);
                                        self.sps_pps.extend_from_slice(&sps);
                                        self.sps_pps.extend_from_slice(&[0, 0, 0, 1]);
                                        self.sps_pps.extend_from_slice(&pps);
                                        spoof_sps_to_baseline(&mut self.sps_pps, width, fps);
                                    }
                                }

                                if !self.sps_pps.is_empty() {
                                    final_frame.extend_from_slice(&self.sps_pps);
                                    final_frame.extend_from_slice(strip_sps_pps(&data));
                                } else {
                                    final_frame.extend_from_slice(&data);
                                }
                            } else {
                                final_frame.extend_from_slice(&data);
                            }

                            encoded_frames.push(final_frame);
                        }

                        if self.is_async {
                            if !self.has_output_ready()? {
                                break;
                            }
                        }
                    }
                    Err(e) => {
                        let code = e.code().0 as u32;

                        if code == 0xC00D36B2 || code == 0xC00D6D72 {
                            if self.is_async {
                                self.async_have_output = false;
                                self.async_need_input = true;
                            }
                            break;
                        }

                        if code == 0x8000FFFF && self.is_async {
                            self.async_have_output = false;
                            break;
                        }

                        if code == 0x8000FFFF {
                            tracing::error!(
                                "ProcessOutput E_UNEXPECTED: async_need_input={} async_have_output={} stream={} provides={}",
                                self.async_need_input,
                                self.async_have_output,
                                self.output_stream_id,
                                provides
                            );
                        }

                        if code == 0xC00D36B1 {
                            if let Ok(t) = self.encoder.GetOutputAvailableType(self.output_stream_id, 0) {
                                let _ = self.encoder.SetOutputType(self.output_stream_id, &t, 0);

                                let mut ptr = std::ptr::null_mut(); let mut len = 0;
                                if t.GetAllocatedBlob(&MF_MT_MPEG_SEQUENCE_HEADER, &mut ptr, &mut len).is_ok() {
                                    let mut header = ensure_annexb(std::slice::from_raw_parts(ptr, len as usize));
                                    spoof_sps_to_baseline(&mut header, width, fps);
                                    self.sps_pps = header;
                                    CoTaskMemFree(Some(ptr as *mut _));
                                }
                            }
                            continue;
                        }

                        tracing::error!("ProcessOutput unhandled error: {:X}", code);
                        break;
                    }
                }
            }

            Ok(encoded_frames)
        }
    }

    pub fn drain_pending(&mut self, width: u32, height: u32, fps: u32) -> Result<Vec<Vec<u8>>> {
        self.drain_output(width, height, fps)
    }
}

pub fn run_encoder_loop(
    frame_rx: Receiver<CaptureEvent>,
    nal_tx: Sender<EncodedFrame>,
    mut config: EncoderConfig,
    stop_flag: Arc<AtomicBool>,
    resize_state: Arc<ResizeState>,
) -> Result<()> {
    unsafe {
        CoInitializeEx(None, COINIT_MULTITHREADED).ok();
        MFStartup(MF_VERSION, 0).context("MFStartup")?;
    }

    let dur = 10_000_000i64 / config.fps.max(1) as i64;
    let nominal_frame_duration = Duration::from_millis((1000 / config.fps.max(1) as u64).max(1));
    let max_frame_age = Duration::from_millis(80);
    
    let mut converter: Option<Converter> = None;
    let mut encoder: Option<Encoder> = None;
    let mut cache = Nv12Cache { tex: None, width: 0, height: 0 };
    let mut software_device: Option<ID3D11Device> = None;
    let mut staging_bgra = StagingBgraCache { tex: None, width: 0, height: 0 };
    let mut next_init_retry = Instant::now();
    let mut locked_size: Option<(u32, u32)> = None;
    let mut next_timestamp = 0i64;
    let poll_timeout = Duration::from_millis(5);

    let mut frame_count = 0;

    loop {
        if stop_flag.load(Ordering::Relaxed) { break; }

        let first_event = match frame_rx.recv_timeout(poll_timeout) {
            Ok(event) => event,
            Err(RecvTimeoutError::Timeout) => {
                if let Some(enc) = encoder.as_mut() {
                    if let Ok(frames) = enc.drain_pending(config.width, config.height, config.fps) {
                        for bitstream in frames {
                            let _ = nal_tx.try_send(EncodedFrame {
                                data: bitstream,
                                duration: nominal_frame_duration,
                                captured_at: Instant::now(),
                            });
                            next_timestamp += dur;
                        }
                    }
                }
                continue;
            }
            Err(RecvTimeoutError::Disconnected) => break,
        };

        let mut latest_device: Option<(ID3D11Device, u32, u32)> = None;
        let mut latest_frame: Option<CaptureEvent> = None;

        let mut classify_event = |event: CaptureEvent| {
            match event {
                CaptureEvent::NewDevice(device, w, h) => latest_device = Some((device, w, h)),
                frame => latest_frame = Some(frame),
            }
        };

        classify_event(first_event);

        loop {
            match frame_rx.try_recv() {
                Ok(event) => classify_event(event),
                Err(TryRecvError::Empty) => break,
                Err(TryRecvError::Disconnected) => break,
            }
        }

        if let Some((device, w, h)) = latest_device {
            if w != 0 && h != 0 {
                match reinitialize_pipeline(
                    &device,
                    w,
                    h,
                    &mut config,
                    resize_state.as_ref(),
                    &mut converter,
                    &mut encoder,
                    &mut cache,
                    &mut locked_size,
                ) {
                    Ok(()) => next_init_retry = Instant::now(),
                    Err(err) => {
                        tracing::error!("{:#}", err);
                        converter = None;
                        encoder = None;
                        next_init_retry = Instant::now() + Duration::from_secs(2);
                    }
                }
            }
        }

        let Some(event) = latest_frame else {
            continue;
        };

        match event {
            CaptureEvent::HardwareFrame { texture, width, height, captured_at } => {
                frame_count += 1;
                // Command the GPU to fire a fresh IDR every second
                let force_idr = frame_count % config.fps.max(1) == 0; 

                if captured_at.elapsed() > max_frame_age {
                    continue;
                }

                if (converter.is_none() || encoder.is_none()) && Instant::now() < next_init_retry {
                    continue;
                }

                if let Ok(device) = unsafe { texture.GetDevice() } {
                    match reinitialize_pipeline(
                        &device,
                        width,
                        height,
                        &mut config,
                        resize_state.as_ref(),
                        &mut converter,
                        &mut encoder,
                        &mut cache,
                        &mut locked_size,
                    ) {
                        Ok(()) => next_init_retry = Instant::now(),
                        Err(err) => {
                            tracing::error!("{:#}", err);
                            converter = None;
                            encoder = None;
                            next_init_retry = Instant::now() + Duration::from_secs(2);
                            continue;
                        }
                    }
                }

                let (conv, enc) = match (&converter, &mut encoder) {
                    (Some(c), Some(e)) => (c, e),
                    _ => continue,
                };

                let device = match unsafe { texture.GetDevice() } {
                    Ok(device) => device,
                    Err(_) => continue,
                };

                let ts = next_timestamp;

                match enc.input_mode {
                    EncoderInputMode::DxgiSurface => {
                        if let Ok(nv12) = get_or_create_nv12(&mut cache, &device, config.width, config.height) {
                            if conv.convert(&texture, &nv12).is_ok() {
                                if let Ok(frames) = enc.encode(&nv12, ts, dur, config.width, config.fps, force_idr) {
                                    for bitstream in frames {
                                        let _ = nal_tx.try_send(EncodedFrame {
                                            data: bitstream,
                                            duration: nominal_frame_duration,
                                            captured_at,
                                        });
                                        next_timestamp += dur;
                                    }
                                }
                            }
                        }
                    }
                    EncoderInputMode::SystemMemory => {
                        if let Ok(bgra) = read_bgra_texture_to_vec(&device, &texture, width, height, &mut staging_bgra) {
                            let bgra = resize_bgra_nearest(&bgra, width, height, config.width, config.height);
                            let nv12 = bgra_to_nv12(&bgra, config.width, config.height);
                            if let Ok(frames) = enc.encode_system_memory(
                                &nv12,
                                ts,
                                dur,
                                config.width,
                                config.height,
                                config.fps,
                                force_idr,
                            ) {
                                for bitstream in frames {
                                    let _ = nal_tx.try_send(EncodedFrame {
                                        data: bitstream,
                                        duration: nominal_frame_duration,
                                        captured_at,
                                    });
                                    next_timestamp += dur;
                                }
                            }
                        }
                    }
                }
            }
            CaptureEvent::SoftwareFrame { data, width, height, captured_at } => {
                frame_count += 1;
                let force_idr = frame_count % config.fps.max(1) == 0;

                if captured_at.elapsed() > max_frame_age {
                    continue;
                }

                if (converter.is_none() || encoder.is_none()) && Instant::now() < next_init_retry {
                    continue;
                }

                if software_device.is_none() {
                    software_device = create_processing_device().ok();
                }

                let Some(device) = software_device.as_ref() else {
                    continue;
                };

                match reinitialize_pipeline(
                    device,
                    width,
                    height,
                    &mut config,
                    resize_state.as_ref(),
                    &mut converter,
                    &mut encoder,
                    &mut cache,
                    &mut locked_size,
                ) {
                    Ok(()) => next_init_retry = Instant::now(),
                    Err(err) => {
                        tracing::error!("{:#}", err);
                        converter = None;
                        encoder = None;
                        next_init_retry = Instant::now() + Duration::from_secs(2);
                        continue;
                    }
                }

                let (conv, enc) = match (&converter, &mut encoder) {
                    (Some(c), Some(e)) => (c, e),
                    _ => continue,
                };

                let ts = next_timestamp;

                let mut bgra_tex = None;
                let bgra_desc = D3D11_TEXTURE2D_DESC {
                    Width: width,
                    Height: height,
                    MipLevels: 1,
                    ArraySize: 1,
                    Format: DXGI_FORMAT_B8G8R8A8_UNORM,
                    SampleDesc: DXGI_SAMPLE_DESC { Count: 1, Quality: 0 },
                    Usage: D3D11_USAGE_DEFAULT,
                    BindFlags: (D3D11_BIND_RENDER_TARGET.0 | D3D11_BIND_SHADER_RESOURCE.0) as u32,
                    CPUAccessFlags: 0,
                    MiscFlags: 0,
                };

                let init = D3D11_SUBRESOURCE_DATA {
                    pSysMem: data.as_ptr() as *const _,
                    SysMemPitch: width * 4,
                    SysMemSlicePitch: (width * height * 4),
                };

                let texture = unsafe {
                    if device.CreateTexture2D(&bgra_desc, Some(&init), Some(&mut bgra_tex)).is_err() {
                        continue;
                    }
                    bgra_tex.unwrap()
                };

                match enc.input_mode {
                    EncoderInputMode::DxgiSurface => {
                        if let Ok(nv12) = get_or_create_nv12(&mut cache, device, config.width, config.height) {
                            if conv.convert(&texture, &nv12).is_ok() {
                                if let Ok(frames) = enc.encode(&nv12, ts, dur, config.width, config.fps, force_idr) {
                                    for bitstream in frames {
                                        let _ = nal_tx.try_send(EncodedFrame {
                                            data: bitstream,
                                            duration: nominal_frame_duration,
                                            captured_at,
                                        });
                                        next_timestamp += dur;
                                    }
                                }
                            }
                        }
                    }
                    EncoderInputMode::SystemMemory => {
                        let bgra = resize_bgra_nearest(&data, width, height, config.width, config.height);
                        let nv12 = bgra_to_nv12(&bgra, config.width, config.height);
                        if let Ok(frames) = enc.encode_system_memory(
                            &nv12,
                            ts,
                            dur,
                            config.width,
                            config.height,
                            config.fps,
                            force_idr,
                        ) {
                            for bitstream in frames {
                                let _ = nal_tx.try_send(EncodedFrame {
                                    data: bitstream,
                                    duration: nominal_frame_duration,
                                    captured_at,
                                });
                                next_timestamp += dur;
                            }
                        }
                    }
                }
            }
            CaptureEvent::NewDevice(_, _, _) => {}
        }
    }

    unsafe { MFShutdown().ok(); CoUninitialize(); }
    Ok(())
}

fn pack_u64(hi: u32, lo: u32) -> u64 { ((hi as u64) << 32) | (lo as u64) }
