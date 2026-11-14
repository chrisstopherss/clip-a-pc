This is a test build for Mac. It has not been run on a Mac by its author, so please expect
rough edges and tell us exactly what happens. It needs macOS 15 or newer and runs on Apple
Silicon and Intel. Nothing here is signed, so macOS blocks it until you allow it as below.

There are three downloads. Try them in this order, because each one checks a piece of the
next:

## 1. mac_spike_a (screen capture and video encoding)

Records 10 seconds of the main display and saves test.mp4. Open Terminal in the folder it
downloaded to and run these one at a time:

    chmod +x mac_spike_a
    xattr -d com.apple.quarantine mac_spike_a
    ./mac_spike_a 10 test.mp4

The first time, macOS asks for Screen Recording permission or prints an error. Open System
Settings, Privacy & Security, Screen & System Audio Recording, turn on Terminal, quit
Terminal completely, reopen it and run the last command again.

## 2. mac_spike_b (desktop audio and microphone)

Listens for 15 seconds, prints a live level for desktop audio and for the microphone, and
saves mix.aac. Play some sound and talk while it runs:

    chmod +x mac_spike_b
    xattr -d com.apple.quarantine mac_spike_b
    ./mac_spike_b 15

If the microphone level stays at 0.00, allow Terminal under Privacy & Security, Microphone.

## 3. clip-a-pc-mac.zip (the app)

Unzip it, drag clip-a-pc.app to Applications, then run this once to clear the download flag:

    xattr -dr com.apple.quarantine /Applications/clip-a-pc.app

Open it. A small record icon appears in the menu bar and a settings window asks which display
to record, how long a clip is, and the quality. Then:

- Control+Option+C saves a clip of the last few seconds
- Control+Option+X pops up a menu to pick how many seconds to save
- Control+Option+V starts a recording, and again stops it

Clips and recordings go to the Movies folder, in clip-a-pc. macOS asks for Screen Recording
and Microphone permission the first time. If it does not record, choose Open log in the menu
bar menu.

## What to send back

For each step: everything it printed or showed, whether the saved file plays and sounds right,
and which Mac and macOS version you used. For the app, also the contents of its log.
