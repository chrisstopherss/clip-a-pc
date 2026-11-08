# clip-a-pc

A small background clipper for Windows. It keeps the last few seconds of your
screen in memory, and when you press Alt+C it saves them as an MP4 with your
game audio and microphone.

No dashboard, no account, no uploads. It sits in the tray and stays out of the way.

## Status

Early development. Nothing usable yet.

## Goals

- 720p, 30 fps, hardware H.264 encoding
- Under 200 MB of memory while buffering
- Alt+C saves a clip and shows a Windows notification
- One-time setup: pick a monitor and a clip length (15, 30, 45, 60, 90 or 120 seconds)
- Desktop audio and microphone from the default Windows devices

## Requirements

Windows 11 and a GPU with a hardware H.264 encoder.

## License

MIT
