//! An MP4 file that is written while recording and finished when recording stops.
//!
//! Packets are appended to the media data as they arrive, so memory use does not
//! grow with the length of the recording beyond a small index. The header is
//! written at the end, after the media data. That layout is fine for local files,
//! but a recording that is cut short by a crash has no header and cannot be played.
//! The media data uses a 64-bit size so recordings can pass 4 GB.

use std::io::{self, Seek, SeekFrom, Write};

use crate::mp4::{build_moov, AudioParams, Chunk, VideoParams};
use crate::ring::{Clip, Entry, Track};

pub struct LiveMp4<W: Write + Seek> {
    w: W,
    video: Vec<Entry>,
    audio: Vec<Entry>,
    /// Runs of consecutive packets from the same track, with their file offsets.
    runs: Vec<Chunk>,
    run_offsets: Vec<u64>,
    mdat_start: u64,
    pos: u64,
    start_pts: Option<i64>,
}

impl<W: Write + Seek> LiveMp4<W> {
    pub fn new(mut w: W) -> io::Result<Self> {
        let mut ftyp = vec![0u8; 4];
        ftyp.extend_from_slice(b"ftyp");
        ftyp.extend_from_slice(b"isom");
        ftyp.extend_from_slice(&0x200u32.to_be_bytes());
        ftyp.extend_from_slice(b"isomiso2avc1mp41");
        let ftyp_len = ftyp.len() as u32; // the whole box, size field included
        ftyp[0..4].copy_from_slice(&ftyp_len.to_be_bytes());
        w.write_all(&ftyp)?;
        let mdat_start = ftyp.len() as u64;
        // size 1 means "the real size follows as 64 bits"; filled in when finished
        w.write_all(&1u32.to_be_bytes())?;
        w.write_all(b"mdat")?;
        w.write_all(&0u64.to_be_bytes())?;
        Ok(Self { w, video: Vec::new(), audio: Vec::new(), runs: Vec::new(), run_offsets: Vec::new(), mdat_start, pos: mdat_start + 16, start_pts: None })
    }

    /// Append one packet. The file starts at the first video keyframe; anything
    /// before that, and audio older than it, is ignored.
    pub fn add(&mut self, track: Track, pts: i64, key: bool, data: &[u8]) -> io::Result<()> {
        match track {
            Track::Video => {
                if self.start_pts.is_none() {
                    if !key {
                        return Ok(());
                    }
                    self.start_pts = Some(pts);
                }
            }
            Track::Audio => match self.start_pts {
                Some(s) if pts >= s => {}
                _ => return Ok(()),
            },
        }
        self.w.write_all(data)?;
        let is_video = track == Track::Video;
        let list = if is_video { &mut self.video } else { &mut self.audio };
        let idx = list.len();
        list.push(Entry::new(track, pts, key, data.len()));
        match self.runs.last_mut() {
            Some(c) if c.video == is_video => c.count += 1,
            _ => {
                self.runs.push(Chunk { video: is_video, first: idx, count: 1 });
                self.run_offsets.push(self.pos);
            }
        }
        self.pos += data.len() as u64;
        Ok(())
    }

    pub fn video_frames(&self) -> usize {
        self.video.len()
    }

    /// Bytes of media written so far.
    pub fn bytes(&self) -> u64 {
        self.pos - self.mdat_start - 16
    }

    /// Length of the video so far in seconds.
    pub fn seconds(&self) -> f64 {
        match (self.start_pts, self.video.last()) {
            (Some(s), Some(l)) if self.video.len() > 1 => (l.pts - s) as f64 / 1e7 * self.video.len() as f64 / (self.video.len() - 1) as f64,
            _ => 0.0,
        }
    }

    /// Write the header and close up the file.
    pub fn finish(mut self, v: &VideoParams, a: Option<&AudioParams>) -> io::Result<W> {
        let Some(start_pts) = self.start_pts else {
            return Err(io::Error::new(io::ErrorKind::InvalidInput, "nothing was recorded"));
        };
        let mdat_len = self.pos - self.mdat_start;
        let (mut vo, mut ao) = (Vec::new(), Vec::new());
        for (c, o) in self.runs.iter().zip(&self.run_offsets) {
            if c.video { vo.push(*o) } else { ao.push(*o) }
        }
        let clip = Clip { video: std::mem::take(&mut self.video), audio: std::mem::take(&mut self.audio), start_pts };
        let moov = build_moov(&clip, v, a, &self.runs, &vo, &ao)?;
        self.w.write_all(&moov)?;
        self.w.seek(SeekFrom::Start(self.mdat_start + 8))?;
        self.w.write_all(&mdat_len.to_be_bytes())?;
        self.w.seek(SeekFrom::End(0))?;
        self.w.flush()?;
        Ok(self.w)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::h264::annexb_to_avcc;
    use crate::mp4::tests::{access_units, adts_frames, have, make_media, run, tmp};
    use std::io::Cursor;
    use std::process::Command;

    fn params(sps: Vec<u8>, pps: Vec<u8>, asc: Vec<u8>) -> (VideoParams, AudioParams) {
        (
            VideoParams { width: 1280, height: 720, sps, pps },
            AudioParams { sample_rate: 48000, channels: 2, asc, bitrate: 128_000 },
        )
    }

    #[test]
    fn live_recording_decodes_cleanly_with_audio() {
        if !have("ffmpeg") || !have("ffprobe") {
            eprintln!("skipped: ffmpeg not installed");
            return;
        }
        let dir = tmp("live");
        std::fs::create_dir_all(&dir).unwrap();
        let (vf, af) = make_media(&dir);
        let aus = access_units(&std::fs::read(&vf).unwrap());
        let (aframes, asc) = adts_frames(&std::fs::read(&af).unwrap());

        let out = dir.join("live.mp4");
        let mut live = LiveMp4::new(std::io::BufWriter::new(std::fs::File::create(&out).unwrap())).unwrap();
        let (mut sps, mut pps) = (None, None);
        let (mut vi, mut ai) = (0usize, 0usize);
        // start a little way into the stream so the first packets are not keyframes
        vi += 7;
        while vi < aus.len() || ai < aframes.len() {
            let vt = vi as f64 / 30.0;
            let at = ai as f64 * 1024.0 / 48000.0;
            if vi < aus.len() && (ai >= aframes.len() || vt <= at) {
                let mut data = Vec::new();
                let c = annexb_to_avcc(&aus[vi], &mut data);
                if c.sps.is_some() { sps = c.sps; }
                if c.pps.is_some() { pps = c.pps; }
                live.add(Track::Video, (vt * 1e7) as i64, c.key, &data).unwrap();
                vi += 1;
            } else {
                live.add(Track::Audio, (at * 1e7) as i64, false, &aframes[ai]).unwrap();
                ai += 1;
            }
        }
        let frames = live.video_frames();
        assert!(frames > 100 && frames < aus.len(), "frames {frames}");
        let (v, a) = params(sps.unwrap(), pps.unwrap(), asc);
        live.finish(&v, Some(&a)).unwrap();

        let dec = run(Command::new("ffmpeg").args(["-v", "error", "-i"]).arg(&out).args(["-f", "null", "-"]));
        assert!(dec.stderr.is_empty(), "decode errors: {}", String::from_utf8_lossy(&dec.stderr));
        let st = String::from_utf8_lossy(&run(Command::new("ffprobe").args(["-v", "error", "-show_entries", "stream=codec_name", "-of", "csv=p=0"]).arg(&out)).stdout).to_string();
        assert!(st.contains("h264") && st.contains("aac"), "streams: {st}");
        let d = run(Command::new("ffprobe").args(["-v", "error", "-show_entries", "format=duration", "-of", "csv=p=0"]).arg(&out));
        let dur: f64 = String::from_utf8_lossy(&d.stdout).trim().parse().unwrap();
        // started at the keyframe at 1.0 s of a 6 s stream
        assert!((4.5..=5.3).contains(&dur), "duration {dur}");
        // header is written last
        let bytes = std::fs::read(&out).unwrap();
        let moov = bytes.windows(4).position(|w| w == b"moov").unwrap();
        let mdat = bytes.windows(4).position(|w| w == b"mdat").unwrap();
        assert!(mdat < moov);
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn nothing_is_written_until_a_keyframe_and_empty_recordings_error() {
        let mut live = LiveMp4::new(Cursor::new(Vec::new())).unwrap();
        live.add(Track::Video, 0, false, &[1, 2, 3]).unwrap();
        live.add(Track::Audio, 0, false, &[9, 9]).unwrap();
        assert_eq!(live.video_frames(), 0);
        assert_eq!(live.bytes(), 0);
        let v = VideoParams { width: 2, height: 2, sps: vec![0x67, 1, 2, 3], pps: vec![0x68] };
        assert!(live.finish(&v, None).is_err());
    }

    #[test]
    fn audio_older_than_the_first_keyframe_is_dropped() {
        let mut live = LiveMp4::new(Cursor::new(Vec::new())).unwrap();
        live.add(Track::Video, 1_000_000, true, &[1, 2, 3, 4]).unwrap();
        live.add(Track::Audio, 900_000, false, &[7; 10]).unwrap();
        live.add(Track::Audio, 1_000_000, false, &[7; 10]).unwrap();
        live.add(Track::Video, 1_333_333, false, &[5, 6]).unwrap();
        assert_eq!(live.video_frames(), 2);
        assert_eq!(live.bytes(), 4 + 10 + 2);
    }

    #[test]
    fn index_memory_per_hour_is_small() {
        // one hour: 30 fps video plus about 47 audio packets per second
        let mut live = LiveMp4::new(Cursor::new(Vec::new())).unwrap();
        let (mut v, mut a) = (0i64, 0i64);
        for i in 0..(3600 * 77) {
            if i % 77 < 30 {
                live.add(Track::Video, v * 333_333, v % 30 == 0, &[1]).unwrap();
                v += 1;
            } else {
                live.add(Track::Audio, a * 213_333 + 1_000_000, false, &[1]).unwrap();
                a += 1;
            }
        }
        let entries = live.video.len() + live.audio.len();
        let bytes = entries * std::mem::size_of::<Entry>() + live.runs.len() * std::mem::size_of::<Chunk>();
        assert!(bytes < 25 * 1024 * 1024, "index uses {} MB", bytes / 1048576);
    }
}
