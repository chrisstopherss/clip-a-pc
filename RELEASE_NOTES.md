Download `clip-a-pc.exe` below and run it. It needs no install and no unzipping.

What is new in 0.2.0:

- Alt+X pops up a menu so you can choose how many seconds to save. The choices go up
  to the Clip length in settings, because that is how much the buffer holds.
- Alt+V starts a recording and Alt+V again stops it. The recording goes straight to
  disk, so it is not limited by the Clip length and does not use more memory the
  longer it runs. The tray icon changes while it records and each action has its
  own sound.
- Alt+C still saves a clip, including while a recording is running.

A recording is only playable once it has been stopped, so a crash during a long
recording loses that file. A recording stops by itself after two hours.

The app updates itself and checks the download against the published SHA-256
before installing. You can turn that off in settings.

Windows 11 and a GPU with a hardware H.264 encoder are required.
