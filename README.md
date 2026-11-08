# clip-a-pc

A small background clipper for Windows. It keeps the last few seconds of your
screen in memory, and when you press Alt+C it saves them as an MP4 with your
desktop audio and microphone.

No dashboard, no account, no uploads. It sits in the tray and stays out of the way.

## Status

Early development. It records and saves clips, but it has only been tested on
one machine so far (Windows 11, NVIDIA GPU).

## Use

1. Run `clip-a-pc.exe`. On first run pick a monitor and a clip length.
2. Press Alt+C any time. You get a notification and a short sound.
3. Clips land in `Videos\clip-a-pc`.

Right-click the tray icon for settings, the clips folder, and quit.

## What it records

- 720p at 30 fps, hardware H.264 (NVIDIA, AMD or Intel)
- Desktop audio from every app, plus your default microphone, in one track
- Clip lengths of 15, 30, 45, 60, 90 or 120 seconds

Memory stays low because it never stores raw frames. Only the compressed video
sits in a fixed-size buffer, and a clip is cut straight from that buffer with no
re-encoding.

## Requirements

Windows 11 and a GPU with a hardware H.264 encoder.

## Files

Settings and the log are in `%APPDATA%\clip-a-pc`.

## License

MIT
