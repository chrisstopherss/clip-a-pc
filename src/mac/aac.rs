//! AAC-LC encoding with Apple's AudioConverter.

use std::ffi::c_void;
use std::ptr::{null_mut, NonNull};

use objc2_audio_toolbox::{
    kAudioConverterEncodeBitRate, AudioConverterDispose, AudioConverterFillComplexBuffer, AudioConverterNew, AudioConverterRef,
    AudioConverterSetProperty,
};
use objc2_core_audio_types::{
    kAudioFormatFlagIsFloat, kAudioFormatLinearPCM, kAudioFormatMPEG4AAC, AudioBuffer, AudioBufferList, AudioStreamBasicDescription,
    AudioStreamPacketDescription,
};

use crate::mixer::{FRAME, RATE};

/// AudioSpecificConfig for AAC-LC, 48 kHz, stereo. Fixed because that is exactly what we ask the encoder for.
pub const ASC: [u8; 2] = [0x11, 0x90];
const FLAG_PACKED: u32 = 1 << 3;
const NO_MORE_INPUT: i32 = 1;

pub struct Aac {
    conv: AudioConverterRef,
    queue: Vec<f32>,
}

struct Feed {
    ptr: *const f32,
    frames_left: usize,
}

unsafe extern "C-unwind" fn input_proc(
    _conv: AudioConverterRef,
    n: NonNull<u32>,
    data: NonNull<AudioBufferList>,
    _desc: *mut *mut AudioStreamPacketDescription,
    user: *mut c_void,
) -> i32 {
    let feed = &mut *(user as *mut Feed);
    let want = *n.as_ptr() as usize;
    let take = want.min(feed.frames_left);
    if take == 0 {
        *n.as_ptr() = 0;
        return NO_MORE_INPUT;
    }
    let abl = data.as_ptr();
    (*abl).mNumberBuffers = 1;
    (*abl).mBuffers[0] = AudioBuffer { mNumberChannels: 2, mDataByteSize: (take * 8) as u32, mData: feed.ptr as *mut c_void };
    feed.ptr = feed.ptr.add(take * 2);
    feed.frames_left -= take;
    *n.as_ptr() = take as u32;
    0
}

impl Aac {
    pub fn new(bitrate: u32) -> Result<Self, String> {
        let mut src = AudioStreamBasicDescription {
            mSampleRate: RATE as f64,
            mFormatID: kAudioFormatLinearPCM,
            mFormatFlags: kAudioFormatFlagIsFloat | FLAG_PACKED,
            mBytesPerPacket: 8,
            mFramesPerPacket: 1,
            mBytesPerFrame: 8,
            mChannelsPerFrame: 2,
            mBitsPerChannel: 32,
            mReserved: 0,
        };
        let mut dst = AudioStreamBasicDescription {
            mSampleRate: RATE as f64,
            mFormatID: kAudioFormatMPEG4AAC,
            mFormatFlags: 0,
            mBytesPerPacket: 0,
            mFramesPerPacket: FRAME as u32,
            mBytesPerFrame: 0,
            mChannelsPerFrame: 2,
            mBitsPerChannel: 0,
            mReserved: 0,
        };
        let mut conv: AudioConverterRef = null_mut();
        let st = unsafe { AudioConverterNew(NonNull::from(&mut src), NonNull::from(&mut dst), NonNull::from(&mut conv)) };
        if st != 0 || conv.is_null() {
            return Err(format!("could not create the AAC encoder (status {st})"));
        }
        let mut br = bitrate;
        let st = unsafe { AudioConverterSetProperty(conv, kAudioConverterEncodeBitRate, 4, NonNull::from(&mut br).cast()) };
        if st != 0 {
            // not fatal: the encoder keeps its default rate
            crate::platform::log(&format!("AAC bitrate not accepted (status {st})"));
        }
        Ok(Self { conv, queue: Vec::new() })
    }

    /// Add one FRAME of interleaved 48 kHz stereo. Returns the AAC packets that are ready.
    pub fn encode(&mut self, pcm: &[f32]) -> Result<Vec<Vec<u8>>, String> {
        self.queue.extend_from_slice(pcm);
        let mut out = Vec::new();
        loop {
            let avail = self.queue.len() / 2;
            if avail == 0 {
                break;
            }
            let mut feed = Feed { ptr: self.queue.as_ptr(), frames_left: avail };
            let mut bytes = [0u8; 8192];
            let mut abl = AudioBufferList {
                mNumberBuffers: 1,
                mBuffers: [AudioBuffer { mNumberChannels: 2, mDataByteSize: bytes.len() as u32, mData: bytes.as_mut_ptr() as *mut c_void }],
            };
            let mut packets = 1u32;
            let mut desc = AudioStreamPacketDescription { mStartOffset: 0, mVariableFramesInPacket: 0, mDataByteSize: 0 };
            let st = unsafe {
                AudioConverterFillComplexBuffer(
                    self.conv,
                    Some(input_proc),
                    &mut feed as *mut Feed as *mut c_void,
                    NonNull::from(&mut packets),
                    NonNull::from(&mut abl),
                    &mut desc,
                )
            };
            if st != 0 && st != NO_MORE_INPUT {
                return Err(format!("AAC encoding failed (status {st})"));
            }
            let used = avail - feed.frames_left;
            self.queue.drain(..used * 2);
            if packets > 0 {
                let n = abl.mBuffers[0].mDataByteSize as usize;
                out.push(bytes[..n.min(bytes.len())].to_vec());
            }
            if packets == 0 || used == 0 {
                break;
            }
        }
        Ok(out)
    }
}

impl Drop for Aac {
    fn drop(&mut self) {
        unsafe {
            AudioConverterDispose(self.conv);
        }
    }
}
