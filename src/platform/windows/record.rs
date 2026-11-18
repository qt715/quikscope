//! DDA stays in FFmpeg: its source owns duplication/device recovery and returns
//! reference-counted D3D11 frames, avoiding a second staging readback per frame.
use std::{sync::mpsc, thread, time::{Duration, Instant}};
use anyhow::{Context, Result, bail};
use ffmpeg_next as ffmpeg;
use windows::{core::w, Win32::{Foundation::{HWND, LPARAM, WPARAM}, UI::WindowsAndMessaging::*}};
use crate::{geom::IRect, media::{Frame, VideoSource}, platform::{RecordTarget, ScreenRecorder}};

pub struct WindowsRecorder;
struct Source { graph: ffmpeg::filter::Graph, size: (u32,u32), period: Duration, next: Instant }
impl ScreenRecorder for WindowsRecorder {
    fn open(&self, target: &RecordTarget, fps: u32, cursor: bool) -> Result<Box<dyn VideoSource>> {
        anyhow::ensure!(fps > 0, "recording frame rate must be positive");
        crate::media::init();
        let monitor = super::desktop::monitors()?.into_iter().find(|m| m.name == target.output).context("recording monitor disconnected")?;
        let r = target.region;
        let m = monitor.physical;
        anyhow::ensure!(r.w > 0 && r.h > 0 && r.x >= m.x && r.y >= m.y && r.x as i64+r.w as i64 <= m.x as i64+m.w as i64 && r.y as i64+r.h as i64 <= m.y as i64+m.h as i64, "recording region lies outside monitor");
        // EnumDisplayMonitors ordering is unrelated to DXGI output_idx. Match
        // the GDI device name against DXGI's actual adapter/output enumeration.
        let (adapter, index) = output_index(&monitor.name)?;
        let mut graph = ffmpeg::filter::Graph::new();
        graph.add(&ffmpeg::filter::find("buffersink").context("FFmpeg buffersink unavailable")?, "out", "")?;
        let options = std::ffi::CString::new(format!("output_idx={index}:offset_x={}:offset_y={}:video_size={}x{}:framerate={fps}:draw_mouse={}",r.x-m.x,r.y-m.y,r.w,r.h,u8::from(cursor)))?;
        unsafe {
            use ffmpeg::ffi::*;
            let filter = avfilter_get_by_name(c"ddagrab".as_ptr());
            anyhow::ensure!(!filter.is_null(), "FFmpeg ddagrab source unavailable");
            let source = avfilter_graph_alloc_filter(graph.as_mut_ptr(),filter,c"desktop".as_ptr());
            anyhow::ensure!(!source.is_null(), "allocating DDA filter failed");
            let device_name = std::ffi::CString::new(adapter.to_string())?;
            let mut device = std::ptr::null_mut();
            let result = av_hwdevice_ctx_create(&mut device,AVHWDeviceType::AV_HWDEVICE_TYPE_D3D11VA,device_name.as_ptr(),std::ptr::null_mut(),0);
            anyhow::ensure!(result >= 0, "opening recording D3D11 device: {}",ffmpeg::Error::from(result));
            (*source).hw_device_ctx = device;
            let result = avfilter_init_str(source,options.as_ptr());
            anyhow::ensure!(result >= 0, "initializing DDA capture: {}",ffmpeg::Error::from(result));
            let sink = graph.get("out").context("missing capture sink")?.as_mut_ptr();
            let result = avfilter_link(source,0,sink,0);
            anyhow::ensure!(result >= 0, "linking DDA capture: {}",ffmpeg::Error::from(result));
        }
        graph.validate().context("opening FFmpeg ddagrab (requires a shared build with ddagrab/D3D11 enabled)")?;
        Ok(Box::new(Source { graph, size:(r.w as u32,r.h as u32), period:Duration::from_secs_f64(1.0/fps as f64), next:Instant::now() }))
    }
    fn outline(&self, region: IRect) -> Result<Box<dyn Send>> { Ok(Box::new(Outline::open(region)?)) }
}
fn output_index(name: &str) -> Result<(u32,u32)> {
    use windows::Win32::Graphics::Dxgi::{CreateDXGIFactory1, IDXGIFactory1};
    let factory: IDXGIFactory1 = unsafe { CreateDXGIFactory1()? };
    let mut adapter_index = 0;
    while let Ok(adapter) = unsafe { factory.EnumAdapters1(adapter_index) } {
        let mut index = 0;
        while let Ok(output) = unsafe { adapter.EnumOutputs(index) } {
            let desc = unsafe { output.GetDesc()? };
            let len = desc.DeviceName.iter().position(|c| *c == 0).unwrap_or(desc.DeviceName.len());
            if String::from_utf16_lossy(&desc.DeviceName[..len]) == name { return Ok((adapter_index,index)); }
            index += 1;
        }
        adapter_index += 1;
    }
    bail!("recording monitor {name} has no DXGI output")
}
impl VideoSource for Source {
    fn size(&self) -> (u32,u32) { self.size }
    fn next_frame(&mut self, timeout: Duration) -> Result<Option<Frame>> {
        let now = Instant::now();
        let wait = self.next.saturating_duration_since(now);
        if wait > timeout { thread::sleep(timeout); return Ok(None); }
        thread::sleep(wait);
        let mut frame = ffmpeg::frame::Video::empty();
        match self.graph.get("out").context("missing capture sink")?.sink().frame(&mut frame) {
            Ok(()) => { let captured=Instant::now(); self.next=captured+self.period; Ok(Some(Frame::from_av(frame,captured))) },
            Err(ffmpeg::Error::Other { errno: 11 }) => Ok(None),
            Err(error) => Err(error.into()),
        }
    }
}
struct Outline { hwnd: usize, thread: Option<thread::JoinHandle<()>> }
impl Outline {
    fn open(r: IRect) -> Result<Self> {
        let (ready, receive) = mpsc::sync_channel(1);
        let thread = thread::Builder::new().name("quikscope-outline".into()).spawn(move || {
            let _dpi = super::desktop::PhysicalDpi::enter();
            let mut windows = Vec::new();
            let result: Result<()> = (|| { for (x,y,w,h) in [(r.x-2,r.y-2,r.w+4,2),(r.x-2,r.y+r.h,r.w+4,2),(r.x-2,r.y,2,r.h),(r.x+r.w,r.y,2,r.h)] {
                let hwnd = unsafe { CreateWindowExW(WS_EX_LAYERED|WS_EX_TRANSPARENT|WS_EX_TOOLWINDOW|WS_EX_NOACTIVATE, w!("STATIC"), w!("quikscope recording outline"), WS_POPUP|WINDOW_STYLE(6), x,y,w,h,None,None,None,None)? };
                windows.push(hwnd);
                unsafe { SetLayeredWindowAttributes(hwnd, windows::Win32::Foundation::COLORREF(0), 220, LWA_ALPHA)?; SetWindowDisplayAffinity(hwnd,WDA_EXCLUDEFROMCAPTURE)?; SetWindowPos(hwnd,HWND_TOPMOST,x,y,w,h,SWP_NOACTIVATE|SWP_SHOWWINDOW)?; }
            } Ok(()) })();
            if let Err(e)=result { let _=ready.send(Err(format!("{e:#}"))); } else {
                let _=ready.send(Ok(windows[0].0 as usize));
                let mut message=MSG::default();
                while unsafe { GetMessageW(&mut message,None,0,0) }.0 > 0 {
                    if message.message == WM_APP+20 { break; }
                    unsafe { let _=TranslateMessage(&message); DispatchMessageW(&message); }
                }
            }
            for hwnd in windows { unsafe { let _=DestroyWindow(hwnd); } }
        })?;
        match receive.recv()? { Ok(hwnd)=>Ok(Self{hwnd,thread:Some(thread)}),Err(e)=>{let _=thread.join();bail!(e)} }
    }
}
impl Drop for Outline { fn drop(&mut self) { unsafe { let _=PostMessageW(HWND(self.hwnd as _),WM_APP+20,WPARAM(0),LPARAM(0)); } if let Some(t)=self.thread.take(){let _=t.join();} } }

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    #[ignore = "requires an unlocked native Windows desktop and DDA"]
    fn native_recording_frames_and_outline() -> Result<()> {
        let monitor=super::super::desktop::monitors()?.remove(0);
        let region=IRect::new(monitor.physical.x+32,monitor.physical.y+32,320,240);
        let outline=Outline::open(region)?;
        unsafe {
            let mut affinity=0;
            GetWindowDisplayAffinity(HWND(outline.hwnd as _),&mut affinity)?;
            assert_eq!(affinity,WDA_EXCLUDEFROMCAPTURE.0);
            assert!(IsWindowVisible(HWND(outline.hwnd as _)).as_bool());
        }
        let mut source=WindowsRecorder.open(&RecordTarget { output:monitor.name,output_rect:monitor.logical,region },30,false)?;
        assert_eq!(source.size(),(320,240));
        let frame=source.next_frame(Duration::from_secs(2))?.context("no DDA frame")?;
        assert_eq!((frame.width(),frame.height()),(320,240));
        let mut cpu=ffmpeg::frame::Video::empty();
        let result=unsafe { ffmpeg::ffi::av_hwframe_transfer_data(cpu.as_mut_ptr(),frame.av().as_ptr(),0) };
        anyhow::ensure!(result>=0,"D3D11 download failed: {result}");
        assert!(cpu.data(0).chunks_exact(4).any(|p| p[0]!=0 || p[1]!=0 || p[2]!=0),"DDA returned black pixels");
        drop(outline);
        Ok(())
    }
}
