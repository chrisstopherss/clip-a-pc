//! Direct drive of an asynchronous hardware H.264 encoder MFT. Unlike the sink
//! writer, this hands back each compressed packet so it can go into a ring buffer.

use windows::core::Interface;
use windows::Win32::Media::MediaFoundation::*;
use windows::Win32::System::Variant::VARIANT;

type Res<T> = Result<T, Box<dyn std::error::Error>>;

pub struct Packet {
    pub pts_100ns: i64,
    pub key: bool,
    pub data: Vec<u8>,
}

pub struct MftEncoder {
    transform: IMFTransform,
    events: IMFMediaEventGenerator,
    provides_samples: bool,
    out_size: u32,
    need_input: u32,
    pub packets: Vec<Packet>,
    pub dropped: u64,
    pub name: String,
}

fn pack(hi: u32, lo: u32) -> u64 {
    ((hi as u64) << 32) | lo as u64
}

fn codec_set(api: &ICodecAPI, name: &str, key: &windows::core::GUID, v: VARIANT) {
    if let Err(e) = unsafe { api.SetValue(key, &v) } {
        println!("  codec setting {name} not accepted: {e}");
    }
}

impl MftEncoder {
    pub fn new(
        activate: &IMFActivate,
        name: String,
        mgr: &IMFDXGIDeviceManager,
        w: u32,
        h: u32,
        fps: u32,
        mean_bps: u32,
        max_bps: u32,
    ) -> Res<Self> {
        unsafe {
            let transform: IMFTransform = activate.ActivateObject()?;
            let attrs = transform.GetAttributes()?;
            attrs.SetUINT32(&MF_TRANSFORM_ASYNC_UNLOCK, 1)?;
            let _ = attrs.SetUINT32(&MF_LOW_LATENCY, 1);
            transform.ProcessMessage(MFT_MESSAGE_SET_D3D_MANAGER, mgr.as_raw() as usize)?;

            if let Ok(api) = transform.cast::<ICodecAPI>() {
                codec_set(
                    &api,
                    "rate control",
                    &CODECAPI_AVEncCommonRateControlMode,
                    VARIANT::from(eAVEncCommonRateControlMode_PeakConstrainedVBR.0 as u32),
                );
                codec_set(&api, "mean bitrate", &CODECAPI_AVEncCommonMeanBitRate, VARIANT::from(mean_bps));
                codec_set(&api, "max bitrate", &CODECAPI_AVEncCommonMaxBitRate, VARIANT::from(max_bps));
                codec_set(&api, "gop size", &CODECAPI_AVEncMPVGOPSize, VARIANT::from(fps));
                codec_set(&api, "b-frames", &CODECAPI_AVEncMPVDefaultBPictureCount, VARIANT::from(0u32));
                codec_set(&api, "low latency", &CODECAPI_AVLowLatencyMode, VARIANT::from(true));
            }

            let out_t = MFCreateMediaType()?;
            out_t.SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Video)?;
            out_t.SetGUID(&MF_MT_SUBTYPE, &MFVideoFormat_H264)?;
            out_t.SetUINT32(&MF_MT_AVG_BITRATE, mean_bps)?;
            out_t.SetUINT32(&MF_MT_INTERLACE_MODE, MFVideoInterlace_Progressive.0 as u32)?;
            out_t.SetUINT64(&MF_MT_FRAME_SIZE, pack(w, h))?;
            out_t.SetUINT64(&MF_MT_FRAME_RATE, pack(fps, 1))?;
            out_t.SetUINT64(&MF_MT_PIXEL_ASPECT_RATIO, pack(1, 1))?;
            out_t.SetUINT32(&MF_MT_MPEG2_PROFILE, eAVEncH264VProfile_Main.0 as u32)?;
            transform.SetOutputType(0, &out_t, 0)?;

            let in_t = MFCreateMediaType()?;
            in_t.SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Video)?;
            in_t.SetGUID(&MF_MT_SUBTYPE, &MFVideoFormat_NV12)?;
            in_t.SetUINT32(&MF_MT_INTERLACE_MODE, MFVideoInterlace_Progressive.0 as u32)?;
            in_t.SetUINT64(&MF_MT_FRAME_SIZE, pack(w, h))?;
            in_t.SetUINT64(&MF_MT_FRAME_RATE, pack(fps, 1))?;
            in_t.SetUINT64(&MF_MT_PIXEL_ASPECT_RATIO, pack(1, 1))?;
            in_t.SetUINT32(&MF_MT_VIDEO_PRIMARIES, MFVideoPrimaries_BT709.0 as u32)?;
            in_t.SetUINT32(&MF_MT_YUV_MATRIX, MFVideoTransferMatrix_BT709.0 as u32)?;
            in_t.SetUINT32(&MF_MT_VIDEO_NOMINAL_RANGE, MFNominalRange_16_235.0 as u32)?;
            transform.SetInputType(0, &in_t, 0)?;

            let info = transform.GetOutputStreamInfo(0)?;
            let provides_samples = (info.dwFlags & MFT_OUTPUT_STREAM_PROVIDES_SAMPLES.0 as u32) != 0;
            println!(
                "encoder stream info: provides_samples={provides_samples} min_out_size={}",
                info.cbSize
            );

            transform.ProcessMessage(MFT_MESSAGE_COMMAND_FLUSH, 0)?;
            transform.ProcessMessage(MFT_MESSAGE_NOTIFY_BEGIN_STREAMING, 0)?;
            transform.ProcessMessage(MFT_MESSAGE_NOTIFY_START_OF_STREAM, 0)?;

            let events: IMFMediaEventGenerator = transform.cast()?;
            Ok(Self {
                transform,
                events,
                provides_samples,
                out_size: info.cbSize.max(1 << 20),
                need_input: 0,
                packets: Vec::new(),
                dropped: 0,
                name,
            })
        }
    }

    /// Handle every pending event without blocking.
    pub fn pump(&mut self) -> Res<()> {
        loop {
            let ev = unsafe { self.events.GetEvent(MF_EVENT_FLAG_NO_WAIT) };
            let ev = match ev {
                Ok(e) => e,
                Err(e) if e.code() == MF_E_NO_EVENTS_AVAILABLE => return Ok(()),
                Err(e) => return Err(e.into()),
            };
            let t = unsafe { ev.GetType()? };
            if t == METransformNeedInput.0 as u32 {
                self.need_input += 1;
            } else if t == METransformHaveOutput.0 as u32 {
                self.take_output()?;
            }
        }
    }

    fn take_output(&mut self) -> Res<()> {
        unsafe {
            let mut buf = MFT_OUTPUT_DATA_BUFFER::default();
            if !self.provides_samples {
                let sample = MFCreateSample()?;
                sample.AddBuffer(&MFCreateMemoryBuffer(self.out_size)?)?;
                buf.pSample = std::mem::ManuallyDrop::new(Some(sample));
            }
            let mut status = 0u32;
            let mut bufs = [buf];
            let r = self.transform.ProcessOutput(0, &mut bufs, &mut status);
            let sample = std::mem::ManuallyDrop::take(&mut bufs[0].pSample);
            let _events = std::mem::ManuallyDrop::take(&mut bufs[0].pEvents);
            match r {
                Ok(()) => {}
                Err(e) if e.code() == MF_E_TRANSFORM_NEED_MORE_INPUT => return Ok(()),
                Err(e) if e.code() == MF_E_TRANSFORM_STREAM_CHANGE => {
                    if let Ok(t) = self.transform.GetOutputAvailableType(0, 0) {
                        let _ = self.transform.SetOutputType(0, &t, 0);
                    }
                    return Ok(());
                }
                Err(e) => return Err(e.into()),
            }
            let Some(sample) = sample else { return Ok(()) };
            let pts = sample.GetSampleTime().unwrap_or(0);
            let key = sample.GetUINT32(&MFSampleExtension_CleanPoint).unwrap_or(0) != 0;
            let media = sample.ConvertToContiguousBuffer()?;
            let mut ptr: *mut u8 = std::ptr::null_mut();
            let mut len = 0u32;
            media.Lock(&mut ptr, None, Some(&mut len))?;
            let data = std::slice::from_raw_parts(ptr, len as usize).to_vec();
            media.Unlock()?;
            self.packets.push(Packet { pts_100ns: pts, key, data });
        }
        Ok(())
    }

    /// Offer one frame. Returns false (and counts a drop) if the encoder is not ready.
    pub fn feed(&mut self, sample: &IMFSample) -> Res<bool> {
        self.pump()?;
        if self.need_input == 0 {
            self.dropped += 1;
            return Ok(false);
        }
        self.need_input -= 1;
        unsafe { self.transform.ProcessInput(0, sample, 0)? };
        self.pump()?;
        Ok(true)
    }

    pub fn finish(&mut self) -> Res<()> {
        unsafe {
            self.transform.ProcessMessage(MFT_MESSAGE_NOTIFY_END_OF_STREAM, 0)?;
            self.transform.ProcessMessage(MFT_MESSAGE_COMMAND_DRAIN, 0)?;
        }
        let start = std::time::Instant::now();
        while start.elapsed() < std::time::Duration::from_millis(500) {
            self.pump()?;
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        Ok(())
    }
}
