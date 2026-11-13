//! Minimal MP4 writer for one H.264 track and an optional AAC track.
//!
//! The header (moov) is built from packet metadata alone and written first, so
//! the file starts playing before it is fully downloaded. The media data is
//! then streamed straight out of the ring with no second copy in memory.
//! Video must have no B-frames, so decode time equals presentation time.

use std::io::{self, Write};

use crate::ring::{Clip, Entry, Ring};

const VIDEO_TIMESCALE: u32 = 90_000;
const MOVIE_TIMESCALE: u32 = 1000;
const AAC_FRAME_SAMPLES: u32 = 1024;

#[derive(Clone)]
pub struct VideoParams {
    pub width: u32,
    pub height: u32,
    /// SPS NAL unit including its header byte, without a start code.
    pub sps: Vec<u8>,
    pub pps: Vec<u8>,
}

#[derive(Clone)]
pub struct AudioParams {
    pub sample_rate: u32,
    pub channels: u16,
    /// AudioSpecificConfig bytes from the AAC encoder.
    pub asc: Vec<u8>,
    pub bitrate: u32,
}

// ---- box helpers -----------------------------------------------------------

fn bx(kind: &[u8; 4], payload: &[u8]) -> Vec<u8> {
    let mut v = Vec::with_capacity(8 + payload.len());
    v.extend_from_slice(&((8 + payload.len()) as u32).to_be_bytes());
    v.extend_from_slice(kind);
    v.extend_from_slice(payload);
    v
}

fn full(kind: &[u8; 4], flags: u32, payload: &[u8]) -> Vec<u8> {
    let mut p = Vec::with_capacity(4 + payload.len());
    p.extend_from_slice(&flags.to_be_bytes()); // version 0 in the top byte
    p.extend_from_slice(payload);
    bx(kind, &p)
}

fn cat(parts: &[&[u8]]) -> Vec<u8> {
    parts.concat()
}

fn u16b(v: u16) -> [u8; 2] {
    v.to_be_bytes()
}
fn u32b(v: u32) -> [u8; 4] {
    v.to_be_bytes()
}

const MATRIX: [u32; 9] = [0x0001_0000, 0, 0, 0, 0x0001_0000, 0, 0, 0, 0x4000_0000];

fn matrix() -> Vec<u8> {
    MATRIX.iter().flat_map(|m| m.to_be_bytes()).collect()
}

// ---- per-track sample tables ----------------------------------------------

struct Samples {
    sizes: Vec<u32>,
    /// (sample count, delta) runs for stts
    deltas: Vec<(u32, u32)>,
    /// 1-based keyframe sample numbers; empty means every sample is a sync sample
    sync: Vec<u32>,
    /// total duration in the track timescale
    duration: u64,
}

fn video_samples(v: &[Entry], t0: i64) -> Samples {
    let ticks: Vec<i64> = v.iter().map(|e| (e.pts - t0).max(0) * 9 / 1000).collect();
    let mut deltas: Vec<(u32, u32)> = Vec::new();
    let mut push = |d: u32| match deltas.last_mut() {
        Some((n, last)) if *last == d => *n += 1,
        _ => deltas.push((1, d)),
    };
    let mut total = 0u64;
    let mut last_d = (VIDEO_TIMESCALE / 30).max(1);
    for i in 0..ticks.len() {
        let d = if i + 1 < ticks.len() {
            (ticks[i + 1] - ticks[i]).max(1) as u32
        } else {
            last_d
        };
        last_d = d;
        total += d as u64;
        push(d);
    }
    Samples {
        sizes: v.iter().map(|e| e.len as u32).collect(),
        deltas,
        sync: v.iter().enumerate().filter(|(_, e)| e.key).map(|(i, _)| i as u32 + 1).collect(),
        duration: total,
    }
}

fn audio_samples(a: &[Entry]) -> Samples {
    let n = a.len() as u32;
    Samples {
        sizes: a.iter().map(|e| e.len as u32).collect(),
        deltas: if n > 0 { vec![(n, AAC_FRAME_SAMPLES)] } else { vec![] },
        sync: vec![],
        duration: n as u64 * AAC_FRAME_SAMPLES as u64,
    }
}

// ---- chunk layout ----------------------------------------------------------

pub(crate) struct Chunk {
    pub(crate) video: bool,
    pub(crate) first: usize,
    pub(crate) count: usize,
}

/// Interleave: for each second of the clip, the video samples then the audio samples.
fn layout(clip: &Clip) -> Vec<Chunk> {
    let sec = |e: &Entry| ((e.pts - clip.start_pts).max(0) / 10_000_000) as usize;
    let (mut vi, mut ai) = (0usize, 0usize);
    let mut chunks = Vec::new();
    let mut w = 0usize;
    while vi < clip.video.len() || ai < clip.audio.len() {
        let vstart = vi;
        while vi < clip.video.len() && sec(&clip.video[vi]) <= w {
            vi += 1;
        }
        if vi > vstart {
            chunks.push(Chunk { video: true, first: vstart, count: vi - vstart });
        }
        let astart = ai;
        while ai < clip.audio.len() && sec(&clip.audio[ai]) <= w {
            ai += 1;
        }
        if ai > astart {
            chunks.push(Chunk { video: false, first: astart, count: ai - astart });
        }
        w += 1;
    }
    chunks
}

// ---- sample entry boxes ----------------------------------------------------

fn avc1(v: &VideoParams) -> io::Result<Vec<u8>> {
    if v.sps.len() < 4 || v.pps.is_empty() {
        return Err(io::Error::new(io::ErrorKind::InvalidInput, "missing SPS or PPS"));
    }
    let avcc = {
        let mut p = vec![1, v.sps[1], v.sps[2], v.sps[3], 0xFF, 0xE1];
        p.extend_from_slice(&u16b(v.sps.len() as u16));
        p.extend_from_slice(&v.sps);
        p.push(1);
        p.extend_from_slice(&u16b(v.pps.len() as u16));
        p.extend_from_slice(&v.pps);
        bx(b"avcC", &p)
    };
    let mut p = Vec::new();
    p.extend_from_slice(&[0; 6]);
    p.extend_from_slice(&u16b(1)); // data reference index
    p.extend_from_slice(&[0; 16]); // pre_defined and reserved
    p.extend_from_slice(&u16b(v.width as u16));
    p.extend_from_slice(&u16b(v.height as u16));
    p.extend_from_slice(&u32b(0x0048_0000));
    p.extend_from_slice(&u32b(0x0048_0000));
    p.extend_from_slice(&[0; 4]);
    p.extend_from_slice(&u16b(1)); // frame count
    p.extend_from_slice(&[0; 32]); // compressor name
    p.extend_from_slice(&u16b(0x0018));
    p.extend_from_slice(&0xFFFFu16.to_be_bytes());
    p.extend_from_slice(&avcc);
    Ok(bx(b"avc1", &p))
}

fn mp4a(a: &AudioParams) -> Vec<u8> {
    let dsi = cat(&[&[0x05, a.asc.len() as u8], &a.asc]);
    let dcd_payload = cat(&[
        &[0x40, 0x15, 0, 0, 0],
        &u32b(a.bitrate),
        &u32b(a.bitrate),
        &dsi,
    ]);
    let dcd = cat(&[&[0x04, dcd_payload.len() as u8], &dcd_payload]);
    let sl = [0x06, 0x01, 0x02];
    let es_payload = cat(&[&[0, 0, 0], &dcd, &sl]);
    let es = cat(&[&[0x03, es_payload.len() as u8], &es_payload]);
    let esds = full(b"esds", 0, &es);

    let mut p = Vec::new();
    p.extend_from_slice(&[0; 6]);
    p.extend_from_slice(&u16b(1));
    p.extend_from_slice(&[0; 8]);
    p.extend_from_slice(&u16b(a.channels));
    p.extend_from_slice(&u16b(16));
    p.extend_from_slice(&[0; 4]);
    p.extend_from_slice(&u32b(a.sample_rate << 16));
    p.extend_from_slice(&esds);
    bx(b"mp4a", &p)
}

// ---- track and movie -------------------------------------------------------

#[allow(clippy::too_many_arguments)]
fn trak(
    id: u32,
    video: bool,
    timescale: u32,
    s: &Samples,
    entry: &[u8],
    chunk_runs: &[(u32, u32)],
    chunk_offsets: &[u64],
    dims: (u32, u32),
) -> Vec<u8> {
    let movie_dur = s.duration * MOVIE_TIMESCALE as u64 / timescale as u64;
    let tkhd = full(
        b"tkhd",
        3,
        &cat(&[
            &u32b(0),
            &u32b(0),
            &u32b(id),
            &u32b(0),
            &u32b(movie_dur as u32),
            &[0; 8],
            &u16b(0),
            &u16b(0),
            &u16b(if video { 0 } else { 0x0100 }),
            &u16b(0),
            &matrix(),
            &u32b(dims.0 << 16),
            &u32b(dims.1 << 16),
        ]),
    );
    let mdhd = full(
        b"mdhd",
        0,
        &cat(&[&u32b(0), &u32b(0), &u32b(timescale), &u32b(s.duration as u32), &u16b(0x55C4), &u16b(0)]),
    );
    let name: &[u8] = if video { b"VideoHandler\0" } else { b"SoundHandler\0" };
    let hdlr = full(
        b"hdlr",
        0,
        &cat(&[&u32b(0), if video { b"vide" } else { b"soun" }, &[0; 12], name]),
    );
    let media_header = if video {
        full(b"vmhd", 1, &[0; 8])
    } else {
        full(b"smhd", 0, &[0; 4])
    };
    let dinf = bx(b"dinf", &full(b"dref", 0, &cat(&[&u32b(1), &full(b"url ", 1, &[])])));

    let stsd = full(b"stsd", 0, &cat(&[&u32b(1), entry]));
    let mut stts_p = u32b(s.deltas.len() as u32).to_vec();
    for (n, d) in &s.deltas {
        stts_p.extend_from_slice(&u32b(*n));
        stts_p.extend_from_slice(&u32b(*d));
    }
    let stts = full(b"stts", 0, &stts_p);
    let stss = if video {
        let mut p = u32b(s.sync.len() as u32).to_vec();
        for k in &s.sync {
            p.extend_from_slice(&u32b(*k));
        }
        full(b"stss", 0, &p)
    } else {
        Vec::new()
    };
    let mut stsc_p = u32b(chunk_runs.len() as u32).to_vec();
    let mut first_chunk = 1u32;
    // chunk_runs holds (chunks in run, samples per chunk)
    for (chunks, per) in chunk_runs {
        stsc_p.extend_from_slice(&u32b(first_chunk));
        stsc_p.extend_from_slice(&u32b(*per));
        stsc_p.extend_from_slice(&u32b(1));
        first_chunk += chunks;
    }
    let stsc = full(b"stsc", 0, &stsc_p);
    let mut stsz_p = cat(&[&u32b(0), &u32b(s.sizes.len() as u32)]);
    for z in &s.sizes {
        stsz_p.extend_from_slice(&u32b(*z));
    }
    let stsz = full(b"stsz", 0, &stsz_p);
    let stco = offsets_box(chunk_offsets);

    let stbl = bx(b"stbl", &cat(&[&stsd, &stts, &stss, &stsc, &stsz, &stco]));
    let minf = bx(b"minf", &cat(&[&media_header, &dinf, &stbl]));
    let mdia = bx(b"mdia", &cat(&[&mdhd, &hdlr, &minf]));
    bx(b"trak", &cat(&[&tkhd, &mdia]))
}

/// Chunk offsets as 32-bit `stco`, or 64-bit `co64` when the file is bigger than 4 GB.
fn offsets_box(offsets: &[u64]) -> Vec<u8> {
    let wide = offsets.iter().any(|&o| o > u32::MAX as u64);
    let mut p = u32b(offsets.len() as u32).to_vec();
    for &o in offsets {
        if wide {
            p.extend_from_slice(&o.to_be_bytes());
        } else {
            p.extend_from_slice(&u32b(o as u32));
        }
    }
    full(if wide { b"co64" } else { b"stco" }, 0, &p)
}

/// Run-length encode per-chunk sample counts as (chunks in run, samples per chunk).
fn chunk_runs(counts: &[usize]) -> Vec<(u32, u32)> {
    let mut runs: Vec<(u32, u32)> = Vec::new();
    for &c in counts {
        match runs.last_mut() {
            Some((n, per)) if *per == c as u32 => *n += 1,
            _ => runs.push((1, c as u32)),
        }
    }
    runs
}

pub(crate) fn build_moov(
    clip: &Clip,
    v: &VideoParams,
    a: Option<&AudioParams>,
    chunks: &[Chunk],
    video_offsets: &[u64],
    audio_offsets: &[u64],
) -> io::Result<Vec<u8>> {
    let vs = video_samples(&clip.video, clip.start_pts);
    let v_counts: Vec<usize> = chunks.iter().filter(|c| c.video).map(|c| c.count).collect();
    let vt = trak(
        1,
        true,
        VIDEO_TIMESCALE,
        &vs,
        &avc1(v)?,
        &chunk_runs(&v_counts),
        video_offsets,
        (v.width, v.height),
    );
    let mut movie_dur = vs.duration * MOVIE_TIMESCALE as u64 / VIDEO_TIMESCALE as u64;
    let mut tracks = vt;
    let mut next_id = 2;
    if let Some(a) = a {
        if !clip.audio.is_empty() {
            let asamp = audio_samples(&clip.audio);
            let a_counts: Vec<usize> = chunks.iter().filter(|c| !c.video).map(|c| c.count).collect();
            let at = trak(
                2,
                false,
                a.sample_rate,
                &asamp,
                &mp4a(a),
                &chunk_runs(&a_counts),
                audio_offsets,
                (0, 0),
            );
            movie_dur = movie_dur.max(asamp.duration * MOVIE_TIMESCALE as u64 / a.sample_rate as u64);
            tracks.extend_from_slice(&at);
            next_id = 3;
        }
    }
    let mvhd = full(
        b"mvhd",
        0,
        &cat(&[
            &u32b(0),
            &u32b(0),
            &u32b(MOVIE_TIMESCALE),
            &u32b(movie_dur as u32),
            &u32b(0x0001_0000),
            &u16b(0x0100),
            &[0; 10],
            &matrix(),
            &[0; 24],
            &u32b(next_id),
        ]),
    );
    Ok(bx(b"moov", &cat(&[&mvhd, &tracks])))
}

/// Write the clip as an MP4 to `w`. Nothing is buffered beyond the header.
pub fn write_mp4<W: Write>(
    w: &mut W,
    ring: &Ring,
    clip: &Clip,
    v: &VideoParams,
    a: Option<&AudioParams>,
) -> io::Result<()> {
    if clip.video.is_empty() {
        return Err(io::Error::new(io::ErrorKind::InvalidInput, "clip has no video"));
    }
    let video_only;
    let clip = if a.is_none() && !clip.audio.is_empty() {
        video_only = Clip { video: clip.video.clone(), audio: Vec::new(), start_pts: clip.start_pts };
        &video_only
    } else {
        clip
    };
    let chunks = layout(clip);
    let entry = |c: &Chunk, i: usize| if c.video { clip.video[c.first + i] } else { clip.audio[c.first + i] };
    let payload: u64 = chunks
        .iter()
        .map(|c| (0..c.count).map(|i| entry(c, i).len as u64).sum::<u64>())
        .sum();
    if payload > 3_500_000_000 {
        return Err(io::Error::new(io::ErrorKind::InvalidInput, "clip too large for 32 bit MP4"));
    }

    let ftyp = bx(b"ftyp", &cat(&[b"isom", &u32b(0x200), b"isom", b"iso2", b"avc1", b"mp41"]));
    let counts = (chunks.iter().filter(|c| c.video).count(), chunks.iter().filter(|c| !c.video).count());
    // The moov size does not depend on the offsets, so build it once to measure it.
    let probe = build_moov(clip, v, a, &chunks, &vec![0; counts.0], &vec![0; counts.1])?;
    let mut pos = (ftyp.len() + probe.len() + 8) as u64;
    let (mut vo, mut ao) = (Vec::new(), Vec::new());
    for c in &chunks {
        if c.video { vo.push(pos) } else { ao.push(pos) }
        pos += (0..c.count).map(|i| entry(c, i).len as u64).sum::<u64>();
    }
    let moov = build_moov(clip, v, a, &chunks, &vo, &ao)?;
    debug_assert_eq!(moov.len(), probe.len());

    w.write_all(&ftyp)?;
    w.write_all(&moov)?;
    w.write_all(&u32b((8 + payload) as u32))?;
    w.write_all(b"mdat")?;
    for c in &chunks {
        for i in 0..c.count {
            ring.write_entry(&entry(c, i), w)?;
        }
    }
    Ok(())
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::h264::{annexb_to_avcc, split_nals};
    use crate::ring::Track;
    use std::path::{Path, PathBuf};
    use std::process::Command;

    pub(crate) fn have(tool: &str) -> bool {
        Command::new(tool).arg("-version").output().map(|o| o.status.success()).unwrap_or(false)
    }

    pub(crate) fn tmp(name: &str) -> PathBuf {
        std::env::temp_dir().join(format!("clip-a-pc-test-{}-{name}", std::process::id()))
    }

    /// Split an Annex B stream into access units (one per frame, single slice).
    pub(crate) fn access_units(data: &[u8]) -> Vec<Vec<u8>> {
        let mut aus: Vec<Vec<u8>> = Vec::new();
        let mut cur: Vec<u8> = Vec::new();
        let mut has_slice = false;
        for nal in split_nals(data) {
            let t = nal[0] & 0x1f;
            let is_slice = t == 1 || t == 5;
            if has_slice && (is_slice || matches!(t, 6 | 7 | 8 | 9)) {
                aus.push(std::mem::take(&mut cur));
                has_slice = false;
            }
            cur.extend_from_slice(&[0, 0, 0, 1]);
            cur.extend_from_slice(nal);
            has_slice |= is_slice;
        }
        if has_slice {
            aus.push(cur);
        }
        aus
    }

    /// Split an ADTS stream into raw AAC frames and return the AudioSpecificConfig.
    pub(crate) fn adts_frames(data: &[u8]) -> (Vec<Vec<u8>>, Vec<u8>) {
        let (mut frames, mut i, mut asc) = (Vec::new(), 0, Vec::new());
        while i + 7 <= data.len() {
            assert!(data[i] == 0xFF && data[i + 1] & 0xF0 == 0xF0, "bad ADTS sync");
            let hdr = if data[i + 1] & 1 == 0 { 9 } else { 7 };
            let len = (((data[i + 3] & 3) as usize) << 11) | ((data[i + 4] as usize) << 3) | (data[i + 5] >> 5) as usize;
            if asc.is_empty() {
                let aot = ((data[i + 2] >> 6) & 3) + 1;
                let sfi = (data[i + 2] >> 2) & 0xF;
                let ch = ((data[i + 2] & 1) << 2) | (data[i + 3] >> 6);
                let v = ((aot as u16) << 11) | ((sfi as u16) << 7) | ((ch as u16) << 3);
                asc = v.to_be_bytes().to_vec();
            }
            frames.push(data[i + hdr..i + len].to_vec());
            i += len;
        }
        (frames, asc)
    }

    pub(crate) fn run(cmd: &mut Command) -> std::process::Output {
        let o = cmd.output().expect("failed to run tool");
        assert!(o.status.success(), "{:?}\n{}", cmd, String::from_utf8_lossy(&o.stderr));
        o
    }

    pub(crate) fn make_media(dir: &Path) -> (PathBuf, PathBuf) {
        let (vf, af) = (dir.join("v.h264"), dir.join("a.aac"));
        run(Command::new("ffmpeg").args(["-v", "error", "-y", "-f", "lavfi", "-i", "testsrc2=size=1280x720:rate=30", "-t", "6",
            "-c:v", "libx264", "-preset", "ultrafast", "-profile:v", "main", "-bf", "0", "-g", "30",
            "-x264-params", "keyint=30:min-keyint=30:scenecut=0", "-f", "h264"]).arg(&vf));
        run(Command::new("ffmpeg").args(["-v", "error", "-y", "-f", "lavfi", "-i", "sine=frequency=440:sample_rate=48000",
            "-t", "6", "-ac", "2", "-c:a", "aac", "-b:a", "128k", "-f", "adts"]).arg(&af));
        (vf, af)
    }

    #[test]
    fn ring_to_mp4_decodes_cleanly() {
        if !have("ffmpeg") || !have("ffprobe") {
            eprintln!("skipped: ffmpeg not installed");
            return;
        }
        let dir = tmp("e2e");
        std::fs::create_dir_all(&dir).unwrap();
        let (vf, af) = make_media(&dir);

        let aus = access_units(&std::fs::read(&vf).unwrap());
        let (aframes, asc) = adts_frames(&std::fs::read(&af).unwrap());
        assert!(aus.len() >= 170 && aframes.len() > 250, "{} {}", aus.len(), aframes.len());

        // Capacity is 80% of everything we will push, so the ring wraps and drops
        // the oldest packets, but still holds well over 3 s plus one keyframe interval.
        let total: usize = aus.iter().map(|a| a.len()).sum::<usize>() + aframes.iter().map(|f| f.len()).sum::<usize>();
        let mut ring = Ring::new(total * 8 / 10, 20.0);
        let (mut sps, mut pps) = (None, None);
        let (mut vi, mut ai) = (0usize, 0usize);
        // feed in time order, like the live pipeline would
        while vi < aus.len() || ai < aframes.len() {
            let vt = vi as f64 / 30.0;
            let at = ai as f64 * 1024.0 / 48000.0;
            if vi < aus.len() && (ai >= aframes.len() || vt <= at) {
                let mut out = Vec::new();
                let c = annexb_to_avcc(&aus[vi], &mut out);
                if c.sps.is_some() { sps = c.sps; }
                if c.pps.is_some() { pps = c.pps; }
                ring.push(Track::Video, (vt * 1e7) as i64, c.key, &out);
                vi += 1;
            } else {
                ring.push(Track::Audio, (at * 1e7) as i64, false, &aframes[ai]);
                ai += 1;
            }
        }
        assert!(ring.bytes_used() <= ring.capacity());
        assert!(ring.len() < aus.len() + aframes.len(), "ring should have wrapped");

        let clip = ring.select(3.0).expect("clip");
        let v = VideoParams { width: 1280, height: 720, sps: sps.unwrap(), pps: pps.unwrap() };
        let a = AudioParams { sample_rate: 48000, channels: 2, asc, bitrate: 128_000 };
        let out = dir.join("out.mp4");
        let mut f = std::fs::File::create(&out).unwrap();
        write_mp4(&mut f, &ring, &clip, &v, Some(&a)).unwrap();
        drop(f);

        // 1. decoding every frame must produce no errors
        let dec = run(Command::new("ffmpeg").args(["-v", "error", "-i"]).arg(&out).args(["-f", "null", "-"]));
        assert!(dec.stderr.is_empty(), "decode errors: {}", String::from_utf8_lossy(&dec.stderr));
        // 2. both streams present
        let st = run(Command::new("ffprobe").args(["-v", "error", "-show_entries", "stream=codec_type,codec_name,width,height", "-of", "csv=p=0"]).arg(&out));
        let st = String::from_utf8_lossy(&st.stdout).to_string();
        assert!(st.contains("h264,video") || st.contains("h264,1280,720"), "streams: {st}");
        assert!(st.contains("aac"), "streams: {st}");
        // 3. duration is the 3 s asked for, plus at most one keyframe interval
        let d = run(Command::new("ffprobe").args(["-v", "error", "-show_entries", "format=duration", "-of", "csv=p=0"]).arg(&out));
        let dur: f64 = String::from_utf8_lossy(&d.stdout).trim().parse().unwrap();
        assert!((3.0..=4.2).contains(&dur), "duration {dur}");
        // 4. fast start: moov comes before mdat
        let bytes = std::fs::read(&out).unwrap();
        let moov = bytes.windows(4).position(|w| w == b"moov").unwrap();
        let mdat = bytes.windows(4).position(|w| w == b"mdat").unwrap();
        assert!(moov < mdat);
        eprintln!("clip: {:.2} s, {} KB", dur, bytes.len() / 1024);
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn video_only_clip_works() {
        if !have("ffmpeg") || !have("ffprobe") {
            return;
        }
        let dir = tmp("vonly");
        std::fs::create_dir_all(&dir).unwrap();
        let (vf, _) = make_media(&dir);
        let aus = access_units(&std::fs::read(&vf).unwrap());
        let mut ring = Ring::new(2_000_000, 20.0);
        let (mut sps, mut pps) = (None, None);
        for (i, au) in aus.iter().enumerate() {
            let mut out = Vec::new();
            let c = annexb_to_avcc(au, &mut out);
            if c.sps.is_some() { sps = c.sps; }
            if c.pps.is_some() { pps = c.pps; }
            ring.push(Track::Video, (i as f64 / 30.0 * 1e7) as i64, c.key, &out);
        }
        let clip = ring.select(2.0).unwrap();
        let v = VideoParams { width: 1280, height: 720, sps: sps.unwrap(), pps: pps.unwrap() };
        let out = dir.join("v.mp4");
        let mut f = std::fs::File::create(&out).unwrap();
        write_mp4(&mut f, &ring, &clip, &v, None).unwrap();
        drop(f);
        let dec = run(Command::new("ffmpeg").args(["-v", "error", "-i"]).arg(&out).args(["-f", "null", "-"]));
        assert!(dec.stderr.is_empty(), "{}", String::from_utf8_lossy(&dec.stderr));
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn offsets_use_64_bit_boxes_only_when_needed() {
        let small = offsets_box(&[100, 4_000_000_000]);
        assert_eq!(&small[4..8], b"stco");
        let big = offsets_box(&[100, 5_000_000_000]);
        assert_eq!(&big[4..8], b"co64");
        // 8 header + 4 version/flags + 4 count + 2 * 8 entries
        assert_eq!(big.len(), 8 + 4 + 4 + 16);
        assert_eq!(&big[big.len() - 8..], &5_000_000_000u64.to_be_bytes());
    }
}
