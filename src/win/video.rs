//! Screen capture to compressed H.264 packets, entirely on the GPU until the encoder.
//!
//! Windows Graphics Capture delivers frames only when the screen changes, so a
//! 30 fps timer takes the latest frame each tick, scales it to 720p and converts
//! it to NV12 on the GPU, and hands it to the hardware encoder.

use std::mem::ManuallyDrop;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::Sender;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use windows::core::{factory, Interface};
use windows::Foundation::TypedEventHandler;
use windows::Graphics::Capture::{Direct3D11CaptureFrame, Direct3D11CaptureFramePool, GraphicsCaptureItem, GraphicsCaptureSession};
use windows::Graphics::DirectX::Direct3D11::IDirect3DDevice;
use windows::Graphics::DirectX::DirectXPixelFormat;
use windows::core::BOOL;
use windows::Win32::Foundation::{HMODULE, LPARAM, RECT};
use windows::Win32::Graphics::Direct3D::D3D_DRIVER_TYPE_HARDWARE;
use windows::Win32::Graphics::Direct3D11::*;
use windows::Win32::Graphics::Dxgi::Common::{DXGI_FORMAT_NV12, DXGI_RATIONAL, DXGI_SAMPLE_DESC};
use windows::Win32::Graphics::Dxgi::{IDXGIDevice, DXGI_ADAPTER_DESC};
use windows::Win32::Graphics::Gdi::{EnumDisplayMonitors, GetMonitorInfoW, HDC, HMONITOR, MONITORINFO, MONITORINFOEXW};
use windows::Win32::Media::MediaFoundation::*;
use windows::Win32::System::WinRT::Direct3D11::{CreateDirect3D11DeviceFromDXGIDevice, IDirect3DDxgiInterfaceAccess};
use windows::Win32::System::WinRT::Graphics::Capture::IGraphicsCaptureItemInterop;

use super::engine::{Msg, Shared};
use super::h264enc::{hardware_encoders, MftEncoder};
use super::util::qpc_now_100ns;
use crate::h264::annexb_to_avcc;
use crate::mp4::VideoParams;

type Res<T> = Result<T, Box<dyn std::error::Error>>;

pub const FPS: u32 = 30;
const FRAME_100NS: i64 = 10_000_000 / FPS as i64;
const MAX_W: u32 = 1280;
const MAX_H: u32 = 720;
const MEAN_BPS: u32 = 5_000_000;
const MAX_BPS: u32 = 7_000_000;
const POOL: usize = 5;

#[derive(Clone, Debug)]
pub struct MonitorInfo {
    pub handle: isize,
    pub width: i32,
    pub height: i32,
    pub primary: bool,
    pub device_name: String,
}

/// All attached monitors, primary first.
pub fn list_monitors() -> Vec<MonitorInfo> {
    unsafe extern "system" fn cb(h: HMONITOR, _: HDC, _: *mut RECT, data: LPARAM) -> BOOL {
        let list = &mut *(data.0 as *mut Vec<MonitorInfo>);
        let mut mi = MONITORINFOEXW::default();
        mi.monitorInfo.cbSize = std::mem::size_of::<MONITORINFOEXW>() as u32;
        if GetMonitorInfoW(h, &mut mi as *mut _ as *mut MONITORINFO).as_bool() {
            let r = mi.monitorInfo.rcMonitor;
            let name = String::from_utf16_lossy(&mi.szDevice);
            list.push(MonitorInfo {
                handle: h.0 as isize,
                width: r.right - r.left,
                height: r.bottom - r.top,
                primary: mi.monitorInfo.dwFlags & 1 != 0,
                device_name: name.trim_end_matches('\0').to_string(),
            });
        }
        BOOL(1)
    }
    let mut list: Vec<MonitorInfo> = Vec::new();
    unsafe {
        let _ = EnumDisplayMonitors(None, None, Some(cb), LPARAM(&mut list as *mut _ as isize));
    }
    list.sort_by_key(|m| !m.primary);
    list
}

pub struct Pipeline {
    latest: Arc<Mutex<Option<Direct3D11CaptureFrame>>>,
    session: GraphicsCaptureSession,
    pool: Direct3D11CaptureFramePool,
    vdev: ID3D11VideoDevice,
    vctx: ID3D11VideoContext,
    venum: ID3D11VideoProcessorEnumerator,
    vproc: ID3D11VideoProcessor,
    nv12: Vec<(ID3D11Texture2D, ID3D11VideoProcessorOutputView)>,
    enc: MftEncoder,
    _mgr: IMFDXGIDeviceManager,
    pub out_w: u32,
    pub out_h: u32,
    pub adapter: String,
    pub encoder: String,
    pub dropped: u64,
}

impl Pipeline {
    pub fn new(monitor_index: usize) -> Res<Self> {
        let monitors = list_monitors();
        let mon = monitors.get(monitor_index).or_else(|| monitors.first()).ok_or("no monitors found")?;

        let (mut device, mut ctx) = (None, None);
        unsafe {
            D3D11CreateDevice(
                None,
                D3D_DRIVER_TYPE_HARDWARE,
                HMODULE::default(),
                D3D11_CREATE_DEVICE_BGRA_SUPPORT | D3D11_CREATE_DEVICE_VIDEO_SUPPORT,
                None,
                D3D11_SDK_VERSION,
                Some(&mut device),
                None,
                Some(&mut ctx),
            )?;
        }
        let device: ID3D11Device = device.ok_or("no D3D11 device")?;
        let ctx: ID3D11DeviceContext = ctx.ok_or("no D3D11 context")?;
        let dxgi: IDXGIDevice = device.cast()?;
        let adapter = unsafe {
            let _ = device.cast::<ID3D11Multithread>()?.SetMultithreadProtected(true);
            let d: DXGI_ADAPTER_DESC = dxgi.GetAdapter()?.GetDesc()?;
            String::from_utf16_lossy(&d.Description).trim_end_matches('\0').to_string()
        };

        // capture
        let interop = factory::<GraphicsCaptureItem, IGraphicsCaptureItemInterop>()?;
        let item: GraphicsCaptureItem = unsafe { interop.CreateForMonitor(HMONITOR(mon.handle as *mut _))? };
        let size = item.Size()?;
        let (src_w, src_h) = (size.Width as u32, size.Height as u32);
        let scale = (MAX_W as f64 / src_w as f64).min(MAX_H as f64 / src_h as f64).min(1.0);
        let out_w = ((src_w as f64 * scale) as u32) & !1;
        let out_h = ((src_h as f64 * scale) as u32) & !1;

        let winrt: IDirect3DDevice = unsafe { CreateDirect3D11DeviceFromDXGIDevice(&dxgi)?.cast()? };
        let pool = Direct3D11CaptureFramePool::CreateFreeThreaded(&winrt, DirectXPixelFormat::B8G8R8A8UIntNormalized, 3, size)?;
        let session = pool.CreateCaptureSession(&item)?;
        let _ = session.SetIsCursorCaptureEnabled(true);
        let _ = session.SetIsBorderRequired(false);
        let latest: Arc<Mutex<Option<Direct3D11CaptureFrame>>> = Arc::new(Mutex::new(None));
        {
            let latest = latest.clone();
            pool.FrameArrived(&TypedEventHandler::new(move |p: windows::core::Ref<Direct3D11CaptureFramePool>, _| {
                if let Some(p) = p.as_ref() {
                    if let Ok(f) = p.TryGetNextFrame() {
                        if let Some(old) = latest.lock().unwrap().replace(f) {
                            let _ = old.Close();
                        }
                    }
                }
                Ok(())
            }))?;
        }
        session.StartCapture()?;

        // GPU scaler and color converter
        let vdev: ID3D11VideoDevice = device.cast()?;
        let vctx: ID3D11VideoContext = ctx.cast()?;
        let cd = D3D11_VIDEO_PROCESSOR_CONTENT_DESC {
            InputFrameFormat: D3D11_VIDEO_FRAME_FORMAT_PROGRESSIVE,
            InputFrameRate: DXGI_RATIONAL { Numerator: FPS, Denominator: 1 },
            InputWidth: src_w,
            InputHeight: src_h,
            OutputFrameRate: DXGI_RATIONAL { Numerator: FPS, Denominator: 1 },
            OutputWidth: out_w,
            OutputHeight: out_h,
            Usage: D3D11_VIDEO_USAGE_PLAYBACK_NORMAL,
        };
        let venum = unsafe { vdev.CreateVideoProcessorEnumerator(&cd)? };
        let vproc = unsafe { vdev.CreateVideoProcessor(&venum, 0)? };
        unsafe {
            let src = RECT { left: 0, top: 0, right: src_w as i32, bottom: src_h as i32 };
            let dst = RECT { left: 0, top: 0, right: out_w as i32, bottom: out_h as i32 };
            vctx.VideoProcessorSetStreamSourceRect(&vproc, 0, true, Some(&src));
            vctx.VideoProcessorSetStreamDestRect(&vproc, 0, true, Some(&dst));
            vctx.VideoProcessorSetOutputTargetRect(&vproc, true, Some(&dst));
            vctx.VideoProcessorSetStreamColorSpace(&vproc, 0, &D3D11_VIDEO_PROCESSOR_COLOR_SPACE { _bitfield: 0 });
            vctx.VideoProcessorSetOutputColorSpace(&vproc, &D3D11_VIDEO_PROCESSOR_COLOR_SPACE { _bitfield: (1 << 2) | (1 << 4) });
        }
        let desc = D3D11_TEXTURE2D_DESC {
            Width: out_w,
            Height: out_h,
            MipLevels: 1,
            ArraySize: 1,
            Format: DXGI_FORMAT_NV12,
            SampleDesc: DXGI_SAMPLE_DESC { Count: 1, Quality: 0 },
            Usage: D3D11_USAGE_DEFAULT,
            BindFlags: D3D11_BIND_RENDER_TARGET.0 as u32,
            CPUAccessFlags: 0,
            MiscFlags: 0,
        };
        let mut nv12 = Vec::new();
        for _ in 0..POOL {
            let mut t: Option<ID3D11Texture2D> = None;
            unsafe { device.CreateTexture2D(&desc, None, Some(&mut t))? };
            let t = t.ok_or("texture create failed")?;
            let ovd = D3D11_VIDEO_PROCESSOR_OUTPUT_VIEW_DESC { ViewDimension: D3D11_VPOV_DIMENSION_TEXTURE2D, ..Default::default() };
            let mut ov: Option<ID3D11VideoProcessorOutputView> = None;
            unsafe { vdev.CreateVideoProcessorOutputView(&t, &venum, &ovd, Some(&mut ov))? };
            nv12.push((t, ov.ok_or("output view failed")?));
        }

        // hardware encoder
        let mut token = 0u32;
        let mut mgr: Option<IMFDXGIDeviceManager> = None;
        unsafe { MFCreateDXGIDeviceManager(&mut token, &mut mgr)? };
        let mgr = mgr.ok_or("no dxgi manager")?;
        unsafe { mgr.ResetDevice(&device, token)? };
        let encoders = hardware_encoders()?;
        let (act, name) = encoders
            .iter()
            .find(|(_, n)| {
                let n = n.to_lowercase();
                let a = adapter.to_lowercase();
                (a.contains("nvidia") && n.contains("nvidia")) || (a.contains("amd") && n.contains("amd")) || (a.contains("radeon") && n.contains("amd"))
            })
            .or_else(|| encoders.first())
            .ok_or("no hardware H.264 encoder found")?;
        let enc = MftEncoder::new(act, name.clone(), &mgr, out_w, out_h, FPS, MEAN_BPS, MAX_BPS)?;

        Ok(Self { latest, session, pool, vdev, vctx, venum, vproc, nv12, enc, _mgr: mgr, out_w, out_h, adapter, encoder: name.clone(), dropped: 0 })
    }

    pub fn has_frame(&self) -> bool {
        self.latest.lock().unwrap().is_some()
    }

    /// Convert the newest captured frame and give it to the encoder.
    pub fn submit(&mut self, n: u64, pts: i64) -> Res<()> {
        let (tex, ov) = &self.nv12[n as usize % POOL];
        {
            let guard = self.latest.lock().unwrap();
            let frame = guard.as_ref().ok_or("no captured frame")?;
            let surface = frame.Surface()?;
            let access: IDirect3DDxgiInterfaceAccess = surface.cast()?;
            let src: ID3D11Texture2D = unsafe { access.GetInterface()? };
            let ivd = D3D11_VIDEO_PROCESSOR_INPUT_VIEW_DESC {
                FourCC: 0,
                ViewDimension: D3D11_VPIV_DIMENSION_TEXTURE2D,
                Anonymous: D3D11_VIDEO_PROCESSOR_INPUT_VIEW_DESC_0 { Texture2D: D3D11_TEX2D_VPIV { MipSlice: 0, ArraySlice: 0 } },
            };
            let mut iv: Option<ID3D11VideoProcessorInputView> = None;
            unsafe { self.vdev.CreateVideoProcessorInputView(&src, &self.venum, &ivd, Some(&mut iv))? };
            let mut streams = [D3D11_VIDEO_PROCESSOR_STREAM { Enable: true.into(), pInputSurface: ManuallyDrop::new(iv), ..Default::default() }];
            let r = unsafe { self.vctx.VideoProcessorBlt(&self.vproc, ov, 0, &streams) };
            unsafe { ManuallyDrop::drop(&mut streams[0].pInputSurface) };
            r?;
        }
        unsafe {
            let buf = MFCreateDXGISurfaceBuffer(&ID3D11Texture2D::IID, tex, 0, false)?;
            let len = buf.cast::<IMF2DBuffer>()?.GetContiguousLength()?;
            buf.SetCurrentLength(len)?;
            let sample = MFCreateSample()?;
            sample.AddBuffer(&buf)?;
            sample.SetSampleTime(pts)?;
            sample.SetSampleDuration(FRAME_100NS)?;
            if !self.enc.feed(&sample)? {
                self.dropped += 1;
            }
        }
        Ok(())
    }

    pub fn pump(&mut self) -> Res<Vec<super::h264enc::Packet>> {
        self.enc.pump()?;
        Ok(self.enc.take_packets())
    }

    pub fn close(self) {
        let _ = self.session.Close();
        let _ = self.pool.Close();
    }
}

/// Runs until `stop` is set. Packets go to the ring through `tx`.
pub fn run(monitor: usize, tx: Sender<Msg>, shared: Arc<Shared>, stop: Arc<AtomicBool>, frames: Arc<AtomicU64>) -> Res<()> {
    let mut pipe = Pipeline::new(monitor)?;
    *shared.video_info.lock().unwrap() = format!("{} on {} ({}x{} at {} fps)", pipe.encoder, pipe.adapter, pipe.out_w, pipe.out_h, FPS);

    let wait = Instant::now();
    while !pipe.has_frame() {
        if wait.elapsed() > Duration::from_secs(5) {
            return Err("no frame from screen capture within 5 s".into());
        }
        std::thread::sleep(Duration::from_millis(5));
    }

    let mut start = Instant::now();
    let mut start_pts = qpc_now_100ns();
    let mut n = 0u64;
    let mut sps_pps: Option<(Vec<u8>, Vec<u8>)> = None;
    while !stop.load(Ordering::Relaxed) {
        let due = start + Duration::from_nanos(n * 1_000_000_000 / FPS as u64);
        let now = Instant::now();
        if due > now {
            std::thread::sleep(due - now);
        } else if now - due > Duration::from_millis(250) {
            // fell far behind (sleep, heavy load): restart the timeline instead of bursting
            start = now;
            start_pts = qpc_now_100ns();
            n = 0;
        }
        pipe.submit(n, start_pts + n as i64 * FRAME_100NS)?;
        frames.fetch_add(1, Ordering::Relaxed);
        n += 1;

        for p in pipe.pump()? {
            let mut avcc = Vec::with_capacity(p.data.len());
            let c = annexb_to_avcc(&p.data, &mut avcc);
            if let (Some(s), Some(pp)) = (c.sps, c.pps) {
                if sps_pps.as_ref().map(|(a, b)| a != &s || b != &pp).unwrap_or(true) {
                    *shared.video.lock().unwrap() = Some(VideoParams { width: pipe.out_w, height: pipe.out_h, sps: s.clone(), pps: pp.clone() });
                    sps_pps = Some((s, pp));
                }
            }
            if sps_pps.is_some() {
                let _ = tx.send(Msg::Video { pts: p.pts_100ns, key: c.key, data: avcc });
            }
        }
    }
    shared.dropped_video.store(pipe.dropped, Ordering::Relaxed);
    pipe.close();
    Ok(())
}
