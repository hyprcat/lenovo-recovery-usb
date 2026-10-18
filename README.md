# Recovery USB Maker for Lenovo laptops (macOS & Linux)

Your Lenovo laptop won't boot, and Lenovo's **USB Recovery Creator** only runs on Windows. This app does the same
job from a Mac or a Linux PC: it signs in with your Lenovo ID, downloads the factory-recovery order Lenovo made for
your laptop, and writes a bootable recovery USB stick that reinstalls Windows exactly as it came from the factory.

> Unofficial. Not affiliated with or endorsed by Lenovo. It uses Lenovo's own recovery service with **your** Lenovo ID
> and **your** recovery order, the same way the official tool does. No Lenovo files are included in this repository.

## What you need

- A **recovery order** for your laptop. It's free: request one at
  [pcsupport.lenovo.com/lenovorecovery](https://pcsupport.lenovo.com/lenovorecovery) with the serial number from the
  bottom of the laptop, and wait for the confirmation email.
- A **USB stick of 32 GB or more**. It will be erased.
- About **30 GB of free disk space** for the download.
- **Google Chrome, Chromium, Microsoft Edge or Brave.** The app opens in it and uses it for the Lenovo sign-in.
- **Python 3** and **7-Zip**:
  - macOS: Python 3 comes with the Xcode Command Line Tools (macOS offers to install them the first time you run
    `python3`). Install 7-Zip with `brew install sevenzip`.
  - Linux: `sudo apt install python3 7zip dosfstools` (older Ubuntu/Debian: `p7zip-full` instead of `7zip`),
    `sudo dnf install python3 7zip dosfstools`, or `sudo pacman -S python 7zip dosfstools`.

## Use it

1. Download this repository (green **Code** button → *Download ZIP*, or `git clone`).
2. Start the app:
   - macOS: double-click **Recovery USB Maker.command** (the first time, right-click it → *Open*), or run
     `python3 app.py` in **Terminal**.
   - Linux: run `python3 app.py`.
3. Follow the four steps in the window:
   1. **Sign in** with your Lenovo ID. A Lenovo sign-in window opens, and closes by itself once you're signed in.
   2. **Choose your order.**
   3. **Download** about 26 to 30 GB. Every file is checked against Lenovo's SHA-256. If it gets interrupted, click
      *Resume*.
   4. **Plug in the stick, pick it, and click *Erase stick and create recovery USB*.** On Linux you'll be asked for
      your password at the start (to format the stick); on macOS, at the end (to write the boot sector).

Closing the window quits the app.

## Restore the laptop

1. Plug the laptop into power and insert the stick.
2. Turn it on and tap **F12** repeatedly, or press the **Novo** pinhole button and choose *Boot Menu*.
3. Choose the USB stick. Secure Boot can stay on.
4. Follow the prompts. It **erases the laptop's internal drive** and reinstalls Windows. Leave the stick in until it
   tells you to remove it.

The stick can be reused. Label it, don't format it, and it will work again next time.

## Troubleshooting

| Problem | Fix |
|---|---|
| "This Lenovo ID has no recovery orders" | Request one at Lenovo's recovery site, wait for the email, then sign in again. |
| "Lenovo says: … (code …)" when choosing an order | The order has probably expired or been used up. Request a new one. |
| Download stops with a link or session error | The sign-in lasts about 12 hours. Sign in again and click *Resume*. Finished files are kept. |
| "Can't build this one yet: … partition(s)" | Your order uses a stick layout this app doesn't support yet. Please open an issue with your laptop model. |
| macOS: "Could not open /dev/rdiskN" | Allow the app you started this from (usually Terminal) under System Settings → Privacy & Security → Files and Folders → *Removable Volumes*, then click *Retry the last step*. Starting it from an editor's terminal, tmux or zellij can fail the same way. Use Terminal.app. |
| "VERIFY FAILED" | The stick is probably bad or fake-capacity. Try another one. |
| "stick too small" | Use a bigger stick. |
| Linux: password prompt never appears | You need a polkit agent (every desktop has one). On a bare window manager, run the CLI below with `sudo`. |

## How it works

| File | What it does |
|---|---|
| `app.py` + `index.html` | The app: a local page on 127.0.0.1 in a Chrome app window, driven over `--remote-debugging-pipe`. |
| `ddrs.py` | Talks to Lenovo's recovery service (`dds.lenovo.com`): sign-in, order, download with SHA-256 checks. |
| `build_usb.py` | Writes the stick: MBR + FAT32, copies and extracts the files, verifies them, applies Lenovo's boot patch, writes the boot code and Lenovo's marker sector. |

The protocol and the stick layout mirror Lenovo's USB Recovery Creator. The two small boot files it embeds
(`Patch.zip`, `zerosector.ucm`) are not redistributed here: the app downloads Lenovo's official tool from
`tools.dds.lenovo.com` and extracts them, checked against pinned SHA-256 hashes.

Downloads, sign-in tokens and order details stay in `work/` on your computer. Delete the folder when you're done.

### Command line

```
python3 build_usb.py selftest              # checks the tricky bits, touches no disk
python3 ddrs.py login '<URL with lenovoid.wust=...>'
python3 ddrs.py order <ORDER_ID>
python3 ddrs.py download
python3 build_usb.py disks                 # find your stick
python3 build_usb.py build disk4           # macOS (ERASES disk4)
sudo python3 build_usb.py build sdb        # Linux (ERASES /dev/sdb)
```

## Status

- Tested end to end on macOS with a Yoga Pro 7 14IAH10 (stick built, booted, factory restore succeeded).
- Linux support is new and hasn't been tested on real hardware yet. Reports are welcome.
- Only orders whose stick is a single FAT32 partition are supported (as far as we know, recent consumer models).
