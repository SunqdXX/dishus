# Testing PC sound on Windows (10 minutes)

1. Get the test build: sign in to GitHub (any free account works), open the newest green
   "CI" run of the `windows-sound` branch under the repo's Actions tab, download
   `dishus-windows` at the bottom, unzip it.
2. Double-click `DiShus-0.31-windows-x86_64.exe`. "Windows protected your PC": More info, then Run
   anyway. Firewall: Allow. On the phone (DiShus 0.31 from Releases), scan the QR code.
3. Connect, play music on the PC, tap 🔇 so it shows 🔊. Listen for a minute.
4. Switch the PC's output once (speakers to headphones, or another device) and back.
5. Tap 🔊 off and on twice. Move the volume slider under ⚙.
6. Send back: one sentence, "clean", "glitchy" or "no sound", and the log lines that mention
   `audio` from the newest file in `%LOCALAPPDATA%\dishus\logs` (they don't contain your IP).
