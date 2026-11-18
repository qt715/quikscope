//! ScreenCaptureKit loopback and Core Audio input queues (device UIDs persist
//! across enumeration order changes and application restarts).
use std::{ffi::c_void, ptr, sync::mpsc, time::{Duration, Instant}};
use anyhow::{Result, Context, anyhow, ensure};
use core_foundation::{base::TCFType, string::CFString};
use objc2_core_media::CMSampleBuffer;
use crate::media::{AudioChunk, AudioDevice, AudioSource, AudioSpec, DEFAULT_INPUT, DEFAULT_OUTPUT};

#[repr(C)]
#[derive(Clone, Copy, Default)]
struct Asbd { rate: f64, format: u32, flags: u32, packet_bytes: u32, packet_frames: u32, frame_bytes: u32, channels: u32, bits: u32, reserved: u32 }
#[repr(C)]
struct Address { selector: u32, scope: u32, element: u32 }
#[repr(C)]
struct Buffer { capacity: u32, data: *mut c_void, size: u32, user: *mut c_void, packet_capacity: u32, packets: *mut c_void, packet_count: u32 }
const fn code(s: &[u8;4]) -> u32 { u32::from_be_bytes(*s) }
#[link(name="CoreAudio", kind="framework")]
unsafe extern "C" {
    fn AudioObjectGetPropertyDataSize(id:u32, address:*const Address, qualifier_size:u32, qualifier:*const c_void, size:*mut u32)->i32;
    fn AudioObjectGetPropertyData(id:u32, address:*const Address, qualifier_size:u32, qualifier:*const c_void, size:*mut u32, data:*mut c_void)->i32;
}
#[link(name="AudioToolbox", kind="framework")]
unsafe extern "C" {
    fn AudioQueueNewInput(format:*const Asbd, callback:unsafe extern "C" fn(*mut c_void,*mut c_void,*mut Buffer,*const c_void,u32,*const c_void), user:*mut c_void, run_loop:*const c_void, mode:*const c_void, flags:u32, queue:*mut *mut c_void)->i32;
    fn AudioQueueSetProperty(queue:*mut c_void, property:u32, data:*const c_void, size:u32)->i32;
    fn AudioQueueAllocateBuffer(queue:*mut c_void, size:u32, buffer:*mut *mut Buffer)->i32;
    fn AudioQueueEnqueueBuffer(queue:*mut c_void, buffer:*mut Buffer, packets:u32, descriptions:*const c_void)->i32;
    fn AudioQueueStart(queue:*mut c_void, start:*const c_void)->i32;
    fn AudioQueueDispose(queue:*mut c_void, immediate:u8)->i32;
}
#[link(name="CoreMedia", kind="framework")]
unsafe extern "C" { fn CMAudioFormatDescriptionGetStreamBasicDescription(description:*const c_void)->*const Asbd; }
fn check(status:i32, operation:&str)->Result<()> { ensure!(status==0,"{operation}: Core Audio error {status}"); Ok(()) }
fn property<T:Copy+Default>(id:u32, selector:u32, scope:u32)->Result<T> {
    let mut data=T::default(); let mut size=std::mem::size_of::<T>() as u32;
    check(unsafe { AudioObjectGetPropertyData(id,&Address{selector,scope,element:0},0,ptr::null(),&mut size,(&mut data as *mut T).cast()) },"reading audio device property")?;
    Ok(data)
}
fn string(id:u32, selector:u32)->Result<String> {
    let pointer:usize=property(id,selector,code(b"glob"))?;
    ensure!(pointer!=0,"missing audio device name/UID");
    let value=unsafe { CFString::wrap_under_create_rule(pointer as _) };
    Ok(value.to_string())
}
fn inputs()->Result<Vec<(u32,String,String)>> {
    let address=Address{selector:code(b"dev#"),scope:code(b"glob"),element:0};
    let mut size=0;
    check(unsafe { AudioObjectGetPropertyDataSize(1,&address,0,ptr::null(),&mut size) },"listing audio devices")?;
    let mut ids=vec![0u32;size as usize/4];
    check(unsafe { AudioObjectGetPropertyData(1,&address,0,ptr::null(),&mut size,ids.as_mut_ptr().cast()) },"listing audio devices")?;
    ids.truncate(size as usize/4);
    let mut result=Vec::new();
    for id in ids {
        let address=Address{selector:code(b"stm#"),scope:code(b"inpt"),element:0}; let mut size=0;
        if unsafe { AudioObjectGetPropertyDataSize(id,&address,0,ptr::null(),&mut size) }==0 && size>0 {
            result.push((id,string(id,code(b"uid "))?,string(id,code(b"lnam"))?));
        }
    }
    Ok(result)
}
pub(super) fn devices()->Result<Vec<AudioDevice>> {
    let mut devices=Vec::new();
    if crate::platform::macos::record::system_audio_supported() { devices.push(AudioDevice{id:DEFAULT_OUTPUT.into(),name:"Desktop audio".into(),output:true}); }
    devices.push(AudioDevice{id:DEFAULT_INPUT.into(),name:"Default microphone".into(),output:false});
    devices.extend(inputs()?.into_iter().map(|(_,uid,name)|AudioDevice{id:uid,name,output:false}));
    Ok(devices)
}
pub(super) fn open(id:&str)->Result<Box<dyn AudioSource>> {
    if id==DEFAULT_OUTPUT { return crate::platform::macos::record::open_system_audio(); }
    microphone_permission()?;
    let (device, uid)=if id==DEFAULT_INPUT {
        let device:u32=property(1,code(b"dIn "),code(b"glob"))?;
        (device, string(device,code(b"uid "))?)
    } else {
        let (device, uid, _) = inputs()?.into_iter().find(|(_,uid,_)|uid==id).with_context(||format!("audio input {id:?} is unavailable"))?;
        (device,uid)
    };
    let native: Asbd = property(device,code(b"sfmt"),code(b"inpt"))?;
    ensure!(native.rate > 0. && native.rate <= u32::MAX as f64 && native.channels > 0 && native.channels <= u16::MAX as u32, "invalid microphone stream format");
    let spec=AudioSpec{rate:native.rate.round() as u32,channels:native.channels as u16};
    let (tx,rx)=mpsc::sync_channel(64);
    let context=Box::into_raw(Box::new(InputContext{tx,spec}));
    let mut queue=ptr::null_mut();
    // Keep the device's channel count: requesting stereo on a mono microphone
    // is not supported by every AudioQueue converter.
    let bytes=native.channels*4;
    let format=Asbd{rate:spec.rate as f64,format:code(b"lpcm"),flags:1|8,packet_bytes:bytes,packet_frames:1,frame_bytes:bytes,channels:native.channels,bits:32,reserved:0};
    let status=unsafe { AudioQueueNewInput(&format,input_callback,context.cast(),ptr::null(),ptr::null(),0,&mut queue) };
    if status!=0 { unsafe { drop(Box::from_raw(context)); } check(status,"opening microphone (check System Settings → Privacy & Security → Microphone)")?; }
    let source=Input{queue,context,rx,spec};
    let result=(|| {
        let uid=CFString::new(&uid); let pointer=uid.as_concrete_TypeRef();
        check(unsafe { AudioQueueSetProperty(queue,code(b"aqcd"),(&pointer as *const _).cast(),std::mem::size_of_val(&pointer) as u32) },"selecting microphone")?;
        for _ in 0..3 { let mut buffer=ptr::null_mut(); check(unsafe { AudioQueueAllocateBuffer(queue,1024*bytes,&mut buffer) },"allocating microphone buffer")?; check(unsafe { AudioQueueEnqueueBuffer(queue,buffer,0,ptr::null()) },"queueing microphone buffer")?; }
        check(unsafe { AudioQueueStart(queue,ptr::null()) },"starting microphone (check Microphone permission)")
    })();
    if let Err(error)=result { drop(source); return Err(error); }
    Ok(Box::new(source))
}
fn microphone_permission() -> Result<()> {
    use objc2_av_foundation::{AVCaptureDevice, AVAuthorizationStatus, AVMediaTypeAudio};
    unsafe {
        let status = AVCaptureDevice::authorizationStatusForMediaType(AVMediaTypeAudio);
        if status == AVAuthorizationStatus::Authorized { return Ok(()); }
        if status == AVAuthorizationStatus::NotDetermined {
            let (tx, rx) = mpsc::sync_channel(1);
            let handler = block2::RcBlock::new(move |granted: objc2::runtime::Bool| { let _ = tx.send(granted.as_bool()); });
            AVCaptureDevice::requestAccessForMediaType_completionHandler(AVMediaTypeAudio, &handler);
            ensure!(rx.recv_timeout(Duration::from_secs(120)).context("waiting for Microphone permission")?, "Microphone permission was denied");
            return Ok(());
        }
    }
    anyhow::bail!("Microphone permission is required. Enable quikscope in System Settings → Privacy & Security → Microphone, then retry.")
}
struct InputContext { tx:mpsc::SyncSender<Result<AudioChunk>>, spec:AudioSpec }
struct Input { queue:*mut c_void, context:*mut InputContext, rx:mpsc::Receiver<Result<AudioChunk>>, spec:AudioSpec }
// Queue callbacks use only the channel; disposal synchronously joins them.
unsafe impl Send for Input {}
impl Drop for Input { fn drop(&mut self) { let status=unsafe { AudioQueueDispose(self.queue,1) }; if status==0 { unsafe { drop(Box::from_raw(self.context)); } } else { log::warn!("disposing microphone queue failed ({status}); retaining callback state"); } } }
impl AudioSource for Input {
    fn spec(&self)->AudioSpec { self.spec }
    fn read(&mut self,timeout:Duration)->Result<Option<AudioChunk>> { match self.rx.recv_timeout(timeout) { Ok(chunk)=>chunk.map(Some), Err(mpsc::RecvTimeoutError::Timeout)=>Ok(None),Err(_)=>Err(anyhow!("microphone queue ended")) } }
}
unsafe extern "C" fn input_callback(user:*mut c_void,queue:*mut c_void,buffer:*mut Buffer,_time:*const c_void,_packets:u32,_descriptions:*const c_void) {
    let result=std::panic::catch_unwind(|| unsafe {
        let buffer=&*buffer;
        let context=&*user.cast::<InputContext>();
        if !buffer.data.is_null() && buffer.size%(context.spec.channels as u32*4)==0 {
            let samples=std::slice::from_raw_parts(buffer.data.cast::<f32>(),buffer.size as usize/4).to_vec();
            let duration=Duration::from_secs_f64(samples.len() as f64/(context.spec.rate as f64*context.spec.channels as f64));
            let captured=Instant::now()-duration;
            let _=context.tx.try_send(Ok(AudioChunk{samples,captured}));
        }
    });
    if result.is_err() { log::error!("microphone callback panicked"); }
    let status=unsafe { AudioQueueEnqueueBuffer(queue,buffer,0,ptr::null()) };
    if status!=0 { let _=unsafe { &*user.cast::<InputContext>() }.tx.try_send(Err(anyhow!("requeueing microphone buffer: {status}"))); }
}
pub(crate) fn sample_chunk(sample:&CMSampleBuffer)->Result<AudioChunk> {
    unsafe {
        let description=sample.format_description().context("SCK audio has no format")?;
        let format=CMAudioFormatDescriptionGetStreamBasicDescription(std::ptr::from_ref(&*description).cast()).as_ref().context("SCK audio has no PCM description")?;
        ensure!(format.format==code(b"lpcm") && format.flags&1!=0 && format.bits==32 && format.channels==2 && format.rate==48000.,"unexpected ScreenCaptureKit audio format");
        let frames=sample.num_samples();
        ensure!(frames>=0 && frames<=i32::MAX as isize && frames<=u32::MAX as isize/8, "invalid SCK audio sample count");
        let frames=frames as usize;
        let mut samples=vec![0f32;frames*2];
        #[repr(C)]
        struct PcmBuffer { channels:u32, bytes:u32, data:*mut c_void }
        // AudioBufferList has a flexible trailing array. Reserve two entries so
        // CoreMedia can copy either planar stereo or interleaved stereo safely.
        #[repr(C)]
        struct PcmList { count:u32, buffers:[PcmBuffer;2] }
        let planar=format.flags&32!=0;
        let mut list=PcmList{count:if planar {2} else {1},buffers:[
            PcmBuffer{channels:if planar {1} else {2},bytes:(frames*if planar {4} else {8}) as u32,data:samples.as_mut_ptr().cast()},
            PcmBuffer{channels:1,bytes:(frames*4) as u32,data:samples.as_mut_ptr().add(frames).cast()},
        ]};
        check(sample.copy_pcm_data_into_audio_buffer_list(0,frames as i32,std::ptr::NonNull::from(&mut list).cast()),"copying SCK audio")?;
        if planar {
            let planar=samples;
            samples=vec![0.;planar.len()];
            for i in 0..frames { samples[2*i]=planar[i]; samples[2*i+1]=planar[frames+i]; }
        }
        let captured=Instant::now()-Duration::from_secs_f64(samples.len() as f64/96000.);
        Ok(AudioChunk{samples,captured})
    }
}
