This is a test program for Mac, not the finished app. It records 10 seconds of the main
display and saves test.mp4, so we can check that capture and video encoding work on a Mac.
There is no audio, menu bar icon or hotkey yet.

It runs on Apple Silicon and Intel Macs with macOS 14 or newer (the finished app will
need macOS 15). The file is not signed, so macOS blocks it until you clear the flag below.

1. Download `mac_spike_a` below.
2. Open Terminal in the folder it landed in and run these one at a time:

       chmod +x mac_spike_a
       xattr -d com.apple.quarantine mac_spike_a

3. Run it, and move windows around or play a video while it records:

       ./mac_spike_a 10 test.mp4

4. The first time, macOS asks for Screen Recording permission or prints an error. Open
   System Settings, Privacy & Security, Screen & System Audio Recording, turn on Terminal,
   quit Terminal completely, reopen it, and run step 3 again.

Please send back everything it printed, whether test.mp4 plays, and which Mac and macOS
version you used.
