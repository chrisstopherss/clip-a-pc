//! The "clip saved" toast and a short chime.
//!
//! Windows can hide toasts while a game runs full screen, so every save also
//! plays a quiet sound. The chime is played by this process, which desktop
//! capture leaves out, so it never ends up in a clip.

use std::sync::OnceLock;

use windows::core::{HSTRING, PCWSTR};
use windows::Data::Xml::Dom::XmlDocument;
use windows::UI::Notifications::{ToastNotification, ToastNotificationManager};
use windows::Win32::Media::Audio::{PlaySoundW, SND_ASYNC, SND_MEMORY, SND_NODEFAULT};

use super::store::set_registry_string;

const AUMID: &str = "clip-a-pc.recorder";

/// Unpackaged apps must register an app id before Windows will show their toasts.
pub fn register() {
    let key = format!("Software\\Classes\\AppUserModelId\\{AUMID}");
    set_registry_string(&key, "DisplayName", "clip-a-pc");
}

fn escape(s: &str) -> String {
    s.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;").replace('"', "&quot;")
}

pub fn notify(title: &str, body: &str) {
    let go = || -> windows::core::Result<()> {
        let xml = XmlDocument::new()?;
        xml.LoadXml(&HSTRING::from(format!(
            "<toast><visual><binding template=\"ToastGeneric\"><text>{}</text><text>{}</text></binding></visual><audio silent=\"true\"/></toast>",
            escape(title),
            escape(body)
        )))?;
        let toast = ToastNotification::CreateToastNotification(&xml)?;
        // a new toast replaces the previous one in the notification list
        let _ = toast.SetTag(&HSTRING::from("clip"));
        ToastNotificationManager::CreateToastNotifierWithId(&HSTRING::from(AUMID))?.Show(&toast)?;
        Ok(())
    };
    if let Err(e) = go() {
        super::log::log(&format!("toast failed: {e}"));
    }
}

/// Two short rising tones as a WAV image, built once.
fn chime_wav() -> &'static Vec<u8> {
    static WAV: OnceLock<Vec<u8>> = OnceLock::new();
    WAV.get_or_init(|| {
        let rate = 22_050u32;
        let mut pcm: Vec<i16> = Vec::new();
        for (freq, ms) in [(784.0f32, 70u32), (1175.0f32, 90u32)] {
            let n = (rate * ms / 1000) as usize;
            for i in 0..n {
                let t = i as f32 / rate as f32;
                let fade = (1.0 - i as f32 / n as f32).powf(0.6) * (i as f32 / 120.0).min(1.0);
                pcm.push(((t * freq * std::f32::consts::TAU).sin() * 0.22 * fade * 32767.0) as i16);
            }
        }
        let data_len = (pcm.len() * 2) as u32;
        let mut v = Vec::new();
        v.extend_from_slice(b"RIFF");
        v.extend_from_slice(&(36 + data_len).to_le_bytes());
        v.extend_from_slice(b"WAVEfmt ");
        v.extend_from_slice(&16u32.to_le_bytes());
        v.extend_from_slice(&1u16.to_le_bytes());
        v.extend_from_slice(&1u16.to_le_bytes());
        v.extend_from_slice(&rate.to_le_bytes());
        v.extend_from_slice(&(rate * 2).to_le_bytes());
        v.extend_from_slice(&2u16.to_le_bytes());
        v.extend_from_slice(&16u16.to_le_bytes());
        v.extend_from_slice(b"data");
        v.extend_from_slice(&data_len.to_le_bytes());
        for s in pcm {
            v.extend_from_slice(&s.to_le_bytes());
        }
        v
    })
}

pub fn chime() {
    let wav = chime_wav();
    unsafe {
        let _ = PlaySoundW(PCWSTR(wav.as_ptr() as *const u16), None, SND_MEMORY | SND_ASYNC | SND_NODEFAULT);
    }
}
