# Recovery USB Maker for Lenovo laptops (macOS & Linux)

Your Lenovo laptop won't boot, and Lenovo's **USB Recovery Creator** only runs on Windows. This app does the same
job from a Mac or a Linux PC: it signs in with your Lenovo ID, downloads the factory-recovery order Lenovo made for
your laptop, and writes a bootable recovery USB stick that reinstalls Windows exactly as it came from the factory.

> Unofficial. Not affiliated with or endorsed by Lenovo. It uses Lenovo's own recovery service with **your** Lenovo ID
> and **your** recovery order, the same way the official tool does. No Lenovo files are included in this repository.

## Download

Get the latest version from [Releases](https://github.com/hyprcat/lenovo-recovery-usb/releases):

| System | File |
|---|---|
| macOS 11 or later (Apple Silicon and Intel) | `Recovery.USB.Maker_…_universal.dmg`: open it and drag the app to Applications |
| Linux, any distribution | `…amd64.AppImage` (or `aarch64`): `chmod +x` it, then run it |
| Debian, Ubuntu, Mint… | `…amd64.deb`: `sudo apt install ./Recovery.USB.Maker_…_amd64.deb` |
| Fedora, openSUSE… | `….x86_64.rpm`: `sudo dnf install ./Recovery.USB.Maker-….x86_64.rpm` |

**First launch on macOS:** the app isn't notarized by Apple (that needs a paid developer account), so macOS says it
can't verify it. Click *Done*, open **System Settings → Privacy & Security**, scroll down and click **Open Anyway**.

## What you need

- A **recovery order** for your laptop. It's free: request one at
  [pcsupport.lenovo.com/lenovorecovery](https://pcsupport.lenovo.com/lenovorecovery) with the serial number from the
  bottom of the laptop, and wait for the confirmation email.
- A **USB stick of 32 GB or more**. It will be erased.
- About **30 GB of free disk space** for the download.
- Linux with the AppImage: `mkfs.vfat`, `sfdisk`, `wipefs`, `lsblk` and `pkexec` (packages `dosfstools`,
  `util-linux`, `pkexec`/`polkit`). Most desktops have all but `dosfstools`. The app tells you if something is missing.

## Use it

The window works like [Rufus](https://rufus.ie):

1. Click **SIGN IN...** and sign in with your Lenovo ID. The sign-in window closes by itself once you're in.
2. Pick your **Lenovo recovery order** and click **DOWNLOAD** (about 26 to 30 GB). Every file is checked against
   Lenovo's SHA-256. If it gets interrupted, click **RESUME**.
3. Plug in the stick, pick it under **Device**, click **START**, then **OK** to erase it.
   - macOS asks once whether the app may access removable volumes (allow it), and asks for your password at the end
     to write the boot sector.
   - Linux asks for your password at the start, to format the stick.

The ⓘ button shows where the downloads are kept and can delete them; the ☰ button shows the full log.

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
| *Continue with Google* won't sign you in | Google sometimes refuses sign-ins inside apps. Sign in with your Lenovo ID's email and password instead (set one with *Forgot password* if you only ever used Google). |
| "Lenovo says: … (code …)" when choosing an order | The order has probably expired or been used up. Request a new one. |
| Download stops with a link or session error | The sign-in lasts about 12 hours. Sign in again and click *Resume*. Finished files are kept. |
| "Can't build this one yet: … partition(s)" | Your order uses a stick layout this app doesn't support yet. Please [open an issue](https://github.com/hyprcat/lenovo-recovery-usb/issues) with your laptop model. |
| "not a zip archive" | Same: your order packs its files in a format this app doesn't read yet. Please open an issue. |
| macOS: "Could not open /dev/rdiskN for writing" | Allow the app under System Settings → Privacy & Security → Files and Folders → *Removable Volumes*, then click **RETRY**. |
| "VERIFY FAILED" | The stick is probably bad or fake-capacity. Try another one. |
| "stick too small" | Use a bigger stick. |
| Linux: password prompt never appears | You need a polkit agent (every desktop has one). On a bare window manager, use the command line below with `sudo`. |

## How it works

| File | What it does |
|---|---|
| `ui/index.html` | The window (a [Tauri](https://tauri.app) app: the system's own web view, no bundled browser). |
| `src-tauri/src/gui.rs` | Runs each step as a subcommand of the app's own binary and shows its progress. Catches the Lenovo ID sign-in token in a second window, as Lenovo's tool does. |
| `src-tauri/src/dds.rs` | Talks to Lenovo's recovery service (`dds.lenovo.com`): sign-in, order, download with SHA-256 checks. |
| `src-tauri/src/usb.rs` | Writes the stick: MBR + FAT32, copies and unpacks the files, verifies them, applies Lenovo's boot patch, writes the boot code and Lenovo's marker sector. |

The protocol and the stick layout mirror Lenovo's USB Recovery Creator. The two small boot files it embeds
(`Patch.zip`, `zerosector.ucm`) are not redistributed here: the app downloads Lenovo's official tool from
`tools.dds.lenovo.com` and extracts them, checked against pinned SHA-256 hashes.

Downloads, sign-in tokens and order details stay on your computer, in `~/Library/Application Support/io.github.hyprcat.recoveryusbmaker`
(macOS) or `~/.local/share/io.github.hyprcat.recoveryusbmaker` (Linux). The app shows the path at the bottom of the window.

### Command line

The app's binary does every step on its own too
(macOS: `"/Applications/Recovery USB Maker.app/Contents/MacOS/recovery-usb-maker"`):

```
recovery-usb-maker login '<URL with lenovoid.wust=...>'
recovery-usb-maker order <ORDER_ID>
recovery-usb-maker download
recovery-usb-maker disks                  # find your stick
recovery-usb-maker build disk4            # macOS (ERASES disk4)
sudo recovery-usb-maker --data ~/.local/share/io.github.hyprcat.recoveryusbmaker build sdb   # Linux (ERASES /dev/sdb)
```

### Build from source

You need [Rust](https://rustup.rs), Node.js, and on Linux the
[Tauri prerequisites](https://v2.tauri.app/start/prerequisites/#linux) (`libwebkit2gtk-4.1-dev` and friends).

```
npm ci
npm run tauri dev      # run it
npm run tauri build    # .app/.dmg on macOS, .AppImage/.deb/.rpm on Linux
cargo test --manifest-path src-tauri/Cargo.toml
```

Pushing a tag like `v0.1.0` builds every package on GitHub Actions and drafts a release.

## Status

- An earlier version of this tool built a stick on macOS that booted and fully restored a Yoga Pro 7 14IAH10. This
  app does the same steps and is tested end to end against a disk image. Reports from real sticks are welcome,
  especially on Linux.
- Only orders whose stick is a single FAT32 partition, with zip-packed archives, are supported (as far as we know,
  recent consumer models).

## License

[MIT](LICENSE)
