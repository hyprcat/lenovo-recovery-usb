//! Writes a downloaded recovery order onto a USB stick.
//!
//! Mirrors USBRecoveryCreator's BurnUSBViewModel: PartitionUSB/CleanMBR -> FormatUSB -> CopyFiles ->
//! WriteValuetoSector(62, 80) -> ValidateUSBKey -> ApplyPatch.
use crate::dds::{basename, files_dir, AGENT};
use crate::{data, print_flush, pump, sha256, El, MAC, R};
use serde::Serialize;
use std::fs::{self, File};
use std::io::{self, BufReader, BufWriter, Read, Write};
use std::os::unix::fs::FileExt;
use std::path::{Component, Path, PathBuf};
use std::process::{Command, Stdio};
use std::thread::sleep;
use std::time::Duration;

const TOOL_URL: &str = "https://tools.dds.lenovo.com/win8/USBRecoveryCreator.exe";
// Embedded in USBRecoveryCreator.dll as managed resources: (file, sha256, length)
const ASSETS: [(&str, &str, usize); 2] = [
    ("Patch.zip", "c3789261c3f8d8d4a5dd8a34e63e8eb46e9776eb9ec0dd48d11281d6177edeea", 5050303),
    ("zerosector.ucm", "ace0fb7a119a776657a9b761e757cf2780e3ded807e4c3c7701fa5f0e5621ff3", 512),
];
const MARKER_OFF: u64 = (62 - 1) * 512; // readWriteSector: (sector - 1) << 9
const MARKER: &[u8; 22] = b"StateApp\x50\0\0\0\0\0ppAetatS"; // writeStatus at OFFSET2 = 16
#[cfg_attr(target_os = "macos", allow(dead_code))] // Linux only
const NOT_USB: &str = "not a USB/removable disk";
#[cfg_attr(target_os = "macos", allow(dead_code))] // Linux only
const SAFE_MOUNTS: [&str; 4] = ["/media/", "/run/media/", "/mnt/", "/Volumes/"]; // where removable sticks get auto-mounted

pub fn cypher_decode(s: &str) -> Option<String> {
    // Utilities/Cypher.cs
    let k = b"k`gybs0vampjd";
    if !s.bytes().all(|c| c.is_ascii_alphanumeric()) {
        return None;
    }
    Some(s.bytes().enumerate().map(|(i, c)| (k[c as usize % k.len()] + 2 - (i % 3) as u8) as char).collect())
}

fn read_lines(path: &Path) -> R<Vec<String>> {
    // File.ReadLines
    let text = fs::read_to_string(path)?;
    let text = text.strip_prefix('\u{feff}').unwrap_or(&text).replace("\r\n", "\n").replace('\r', "\n");
    let mut lines: Vec<String> = text.split('\n').map(String::from).collect();
    if lines.last().is_some_and(|l| l.is_empty()) {
        lines.pop();
    }
    Ok(lines)
}

fn write_lines(path: &Path, lines: &[String]) -> R {
    // File.WriteAllLines on Windows: UTF-8 without BOM, CRLF
    Ok(fs::write(path, lines.iter().map(|l| format!("{l}\r\n")).collect::<String>())?)
}

fn update_aod(mut lines: Vec<String>, module: &str) -> R<Vec<String>> {
    // BurnUSBViewModel.UpdateAOD, quirks included
    let key = "Unit(1).SW_Module()=";
    let t = lines.iter().find(|l| l.contains(key)).and_then(|l| l.split(':').nth(1)).ok_or("AOD.DAT has no Unit(1).SW_Module() count")?;
    let (t, n) = (t.to_string(), t.trim().parse::<i64>()? + 1);
    for l in lines.iter_mut().filter(|l| l.contains(key)) {
        *l = l.replace(&t, &n.to_string());
    }
    lines.push(format!("  SW_Module({n}).CRI_File={module}.CRI"));
    Ok(lines)
}

fn run(cmd: &[&str]) -> R {
    println!("$ {}", cmd.join(" "));
    let st = Command::new(cmd[0]).args(&cmd[1..]).status().map_err(|e| format!("{}: {e}", cmd[0]))?;
    if !st.success() {
        return Err(format!("{} failed ({st})", cmd.join(" ")).into());
    }
    Ok(())
}

fn output(cmd: &str, args: &[&str]) -> R<Vec<u8>> {
    let out = Command::new(cmd).args(args).stderr(Stdio::piped()).output().map_err(|e| format!("{cmd}: {e}"))?;
    if !out.status.success() {
        let why = if out.stderr.is_empty() { &out.stdout } else { &out.stderr }; // diskutil complains on stdout
        return Err(format!("{cmd} {}: {}", args.join(" "), String::from_utf8_lossy(why).trim()).into());
    }
    Ok(out.stdout)
}

// ---- Lenovo's boot files ---------------------------------------------------------------------

fn bytes<const N: usize>(d: &[u8], at: usize) -> R<[u8; N]> {
    Ok(d.get(at..at + N).ok_or("truncated bundle")?.try_into()?)
}

/// One file out of a .NET single-file bundle (format v6).
fn unbundle(d: &[u8], want: &str) -> R<Vec<u8>> {
    let sig = b"\x8b\x12\x02\xb9\x6a\x61\x20\x38\x72\x7b\x93\x02\x14\xd7\xa0\x32\x13\xf5\xb9\xe6\xef\xae\x33\x18\xee\x3b\x2d\xce\x24\xb3\x6a\xae";
    let i = d.windows(32).position(|w| w == sig).ok_or("Lenovo's tool is not the .NET build this app knows")?;
    let mut p = i64::from_le_bytes(bytes(d, i.checked_sub(8).ok_or("bad bundle")?)?) as usize;
    let rstr = |p: &mut usize| -> R<String> {
        let (mut n, mut shift) = (0usize, 0);
        loop {
            let b = *d.get(*p).ok_or("truncated bundle")?;
            *p += 1;
            n |= ((b & 0x7f) as usize) << shift;
            shift += 7;
            if b < 0x80 {
                break;
            }
        }
        let s = String::from_utf8(d.get(*p..*p + n).ok_or("truncated bundle")?.to_vec())?;
        *p += n;
        Ok(s)
    };
    let major = u32::from_le_bytes(bytes(d, p)?);
    let count = i32::from_le_bytes(bytes(d, p + 8)?);
    p += 12;
    rstr(&mut p)?;
    if major >= 2 {
        p += 40;
    }
    for _ in 0..count {
        let off = i64::from_le_bytes(bytes(d, p)?) as usize;
        let size = i64::from_le_bytes(bytes(d, p + 8)?) as usize;
        let csize = i64::from_le_bytes(bytes(d, p + 16)?) as usize;
        p += 25;
        if rstr(&mut p)? == want {
            let blob = d.get(off..off + if csize > 0 { csize } else { size }).ok_or("truncated bundle")?;
            if csize == 0 {
                return Ok(blob.to_vec());
            }
            let mut out = Vec::with_capacity(size);
            flate2::read::DeflateDecoder::new(blob).read_to_end(&mut out)?;
            return Ok(out);
        }
    }
    Err(format!("{want} not found in Lenovo's tool").into())
}

pub fn asset(name: &str) -> PathBuf {
    data().join("assets").join(name)
}

/// Patch.zip and zerosector.ucm are Lenovo's, so they are not shipped: pull them out of Lenovo's own tool.
pub fn ensure_assets() -> R {
    if ASSETS.iter().all(|(n, h, _)| sha256(&asset(n)).is_ok_and(|x| x == *h)) {
        return Ok(());
    }
    let exe = data().join("USBRecoveryCreator.exe");
    if !exe.exists() {
        fs::create_dir_all(data())?;
        println!("Downloading Lenovo USB Recovery Creator (75 MB) from {TOOL_URL}");
        let tmp = data().join("USBRecoveryCreator.exe.tmp");
        pump(&mut AGENT.get(TOOL_URL).call()?.into_reader(), &mut File::create(&tmp)?)?;
        fs::rename(tmp, &exe)?;
    }
    let dll = unbundle(&fs::read(&exe)?, "USBRecoveryCreator.dll")?;
    fs::create_dir_all(asset(""))?;
    for (name, hash, len) in ASSETS {
        // managed resources are stored as <uint32 length><bytes>
        let tag = (len as u32).to_le_bytes();
        let blob = (dll.windows(4 + len).filter(|w| w[..4] == tag).map(|w| &w[4..]))
            .find(|b| crate::hex(&<sha2::Sha256 as sha2::Digest>::digest(b)) == hash)
            .ok_or_else(|| format!("{name} not found inside Lenovo's tool (new version?) - please open an issue"))?;
        fs::write(asset(name), blob)?;
    }
    println!("Boot files extracted from Lenovo's tool into {}", asset("").display());
    Ok(())
}

// ---- disks -------------------------------------------------------------------------------------

#[derive(Serialize, Clone)]
pub struct Disk {
    pub id: String,
    pub name: String,
    pub size: u64,
    pub problem: Option<String>,
}

pub fn disk_name_ok(d: &str) -> bool {
    if MAC {
        return d.strip_prefix("disk").is_some_and(|n| !n.is_empty() && n.bytes().all(|b| b.is_ascii_digit()));
    }
    d.bytes().next().is_some_and(|b| b.is_ascii_lowercase()) && d.bytes().all(|b| b.is_ascii_lowercase() || b.is_ascii_digit())
}

#[cfg(target_os = "macos")]
fn disk_info(dev: &str) -> R<plist::Dictionary> {
    let v = plist::Value::from_reader(io::Cursor::new(output("diskutil", &["info", "-plist", dev])?))?;
    Ok(v.into_dictionary().ok_or("diskutil: unexpected output")?)
}

#[cfg(target_os = "macos")]
fn mount_point(disk: &str) -> Option<PathBuf> {
    disk_info(&format!("{disk}s1")).ok()?.get("MountPoint")?.as_string().filter(|m| !m.is_empty()).map(PathBuf::from)
}

#[cfg(target_os = "macos")]
fn mac_describe(disk: &str) -> R<Disk> {
    let i = disk_info(disk)?;
    let flag = |k| i.get(k).and_then(plist::Value::as_boolean).unwrap_or(false);
    let num = |k| i.get(k).and_then(plist::Value::as_unsigned_integer).unwrap_or(0);
    let problem = if flag("Internal") || !flag("WholeDisk") {
        Some("internal disk or not a whole disk".into())
    } else if num("DeviceBlockSize") != 512 {
        Some("not 512-byte sectors".into())
    } else {
        None
    };
    let name = i.get("MediaName").and_then(plist::Value::as_string).filter(|s| !s.is_empty()).unwrap_or("USB disk");
    Ok(Disk { id: disk.into(), name: name.into(), size: num("TotalSize"), problem })
}

#[cfg_attr(target_os = "macos", allow(dead_code))] // Linux only
fn lsblk(disk: Option<&str>) -> R<Vec<serde_json::Value>> {
    let dev = disk.map(|d| format!("/dev/{d}"));
    let mut args = vec!["-J", "-b", "-o", "NAME,TYPE,SIZE,TRAN,RM,HOTPLUG,MODEL,LOG-SEC,MOUNTPOINT"];
    args.extend(dev.as_deref());
    let v: serde_json::Value = serde_json::from_slice(&output("lsblk", &args)?)?;
    let all = v["blockdevices"].as_array().cloned().unwrap_or_default();
    Ok(all.into_iter().filter(|d| d["type"] == "disk").collect())
}

#[cfg_attr(target_os = "macos", allow(dead_code))] // Linux only
/// Linux lsblk entry -> Disk. lsblk prints booleans and numbers as JSON or as strings depending on its version.
fn describe(d: &serde_json::Value) -> Disk {
    let truthy = |v: &serde_json::Value| *v == true || *v == 1 || *v == "1";
    let num = |v: &serde_json::Value| v.as_u64().or(v.as_f64().map(|f| f as u64)).or(v.as_str().and_then(|s| s.parse().ok()));
    let kids = d["children"].as_array().into_iter().flatten();
    let mounts: Vec<&str> = std::iter::once(d).chain(kids).filter_map(|c| c["mountpoint"].as_str()).collect();
    let problem = if !(d["tran"] == "usb" || truthy(&d["rm"]) || truthy(&d["hotplug"])) {
        Some(NOT_USB.into())
    } else if num(&d["log-sec"]).unwrap_or(512) != 512 {
        Some("not 512-byte sectors".into())
    } else if mounts.iter().any(|m| !SAFE_MOUNTS.iter().any(|s| m.starts_with(s))) {
        Some(format!("in use by the system ({})", mounts.join(", ")))
    } else {
        None
    };
    let name = d["model"].as_str().map(str::trim).filter(|s| !s.is_empty()).unwrap_or("USB disk");
    Disk { id: d["name"].as_str().unwrap_or("").into(), name: name.into(), size: num(&d["size"]).unwrap_or(0), problem }
}

pub fn list_disks() -> R<Vec<Disk>> {
    #[cfg(target_os = "macos")]
    {
        let v = plist::Value::from_reader(io::Cursor::new(output("diskutil", &["list", "-plist", "external", "physical"])?))?;
        let whole = v.as_dictionary().and_then(|d| d.get("WholeDisks")).and_then(plist::Value::as_array).cloned().unwrap_or_default();
        whole.iter().filter_map(plist::Value::as_string).map(mac_describe).collect()
    }
    #[cfg(not(target_os = "macos"))]
    {
        Ok(lsblk(None)?.iter().map(describe).filter(|d| d.problem.as_deref() != Some(NOT_USB)).collect())
    }
}

fn check_disk(disk: &str) -> R<Disk> {
    #[cfg(target_os = "macos")]
    let d = mac_describe(disk)?;
    #[cfg(not(target_os = "macos"))]
    let d = describe(lsblk(Some(disk))?.first().ok_or("no such disk")?);
    match &d.problem {
        Some(p) => Err(format!("{disk} ({}): {p} - refusing.", d.name).into()),
        None => Ok(d),
    }
}

fn unmount(disk: &str) -> R {
    if !MAC {
        for e in fs::read_dir("/dev")?.flatten() {
            let n = e.file_name().to_string_lossy().into_owned();
            if n.len() > disk.len() && n.starts_with(disk) {
                let _ = Command::new("umount").arg(format!("/dev/{n}")).stderr(Stdio::null()).status();
            }
        }
        return Ok(());
    }
    for _ in 0..10 {
        // a freshly mounted (FSKit/Spotlight) volume stays busy for a few seconds
        if Command::new("diskutil").args(["unmountDisk", disk]).status()?.success() {
            return Ok(());
        }
        sleep(Duration::from_secs(3));
    }
    run(&["diskutil", "unmountDisk", "force", disk])
}

/// MBR + one FAT32 partition, mounted. Returns the mount point.
fn erase(disk: &str, label: &str) -> R<PathBuf> {
    if MAC {
        run(&["diskutil", "eraseDisk", "FAT32", label, "MBRFormat", disk])?;
        #[cfg(target_os = "macos")]
        return mount_point(disk).ok_or_else(|| "the new FAT32 volume did not mount".into());
    }
    let dev = format!("/dev/{disk}");
    let part = format!("{dev}{}", if disk.ends_with(|c: char| c.is_ascii_digit()) { "p1" } else { "1" });
    unmount(disk)?;
    run(&["wipefs", "-a", &dev])?;
    println!("$ sfdisk {dev}");
    let mut sf = Command::new("sfdisk").args(["-q", &dev]).stdin(Stdio::piped()).spawn()?;
    sf.stdin.take().unwrap().write_all(b"label: dos\nstart=2048, type=c, bootable\n")?;
    if !sf.wait()?.success() {
        return Err("sfdisk failed".into());
    }
    let _ = run(&["udevadm", "settle"]);
    for _ in 0..20 {
        if Path::new(&part).exists() {
            break;
        }
        sleep(Duration::from_millis(500));
    }
    let _ = Command::new("umount").arg(&part).stderr(Stdio::null()).status(); // desktop auto-mounters can grab it
    run(&["mkfs.vfat", "-F", "32", "-n", label, &part])?;
    let mnt = std::env::temp_dir().join(format!("lenovo-usb-{}", crate::random_hex(6)?));
    std::os::unix::fs::DirBuilderExt::mode(&mut fs::DirBuilder::new(), 0o700).create(&mnt)?;
    run(&["mount", "-t", "vfat", &part, &mnt.to_string_lossy()])?;
    Ok(mnt)
}

// ---- recovery layout ---------------------------------------------------------------------------

fn rmf_files() -> Vec<PathBuf> {
    let entries = fs::read_dir(files_dir()).into_iter().flatten().flatten();
    entries.map(|e| e.path()).filter(|p| p.to_string_lossy().ends_with(".RMF")).collect()
}

fn rmf() -> R<El> {
    match rmf_files().as_slice() {
        [one] => El::parse(&fs::read_to_string(one)?),
        many => Err(format!("expected one .RMF file in {}, found {}", files_dir().display(), many.len()).into()),
    }
}

pub fn has_rmf() -> bool {
    !rmf_files().is_empty()
}

pub fn layout_problem() -> Option<String> {
    let parts = match rmf() {
        Ok(r) => r.findall("recovery/partitions/partition").into_iter().cloned().collect::<Vec<_>>(),
        Err(e) => return Some(e.to_string()),
    };
    // ponytail: single FAT32 partition only (what known orders use); multi-partition needs the MBR rotation logic
    if parts.len() != 1 || parts[0].attr("format").to_uppercase() != "FAT32" {
        let formats: Vec<&str> = parts.iter().map(|p| p.attr("format")).collect();
        return Some(format!(
            "this order needs {} partition(s) ({}); only a single FAT32 partition is supported so far",
            parts.len(),
            formats.join(", ")
        ));
    }
    None
}

/// (partition, files, manual files)
pub fn manifest() -> R<(El, Vec<El>, Vec<El>)> {
    if let Some(p) = layout_problem() {
        return Err(format!("Unsupported recovery layout: {p}").into());
    }
    let root = rmf()?;
    let rec = root.find("recovery").ok_or("RMF has no <recovery>")?;
    let all = |path: &str| -> Vec<El> { rec.findall(path).into_iter().cloned().collect() };
    Ok((rec.find("partitions/partition").unwrap().clone(), all("files/file"), all("manualfiles/file")))
}

pub fn local(f: &El) -> PathBuf {
    files_dir().join(basename(f.attr("name")))
}

/// Windows-style '\RECOVERY', '.\BOOT\BCD' -> path on the mounted stick. Never outside it.
fn onstick(mnt: &Path, parts: &[&str]) -> R<PathBuf> {
    let mut p = mnt.to_path_buf();
    for part in parts {
        let rel = part.replace('\\', "/");
        let rel = Path::new(rel.trim_matches('/'));
        if rel.components().any(|c| !matches!(c, Component::Normal(_) | Component::CurDir)) {
            return Err(format!("unsafe path in recovery manifest: {part:?}").into());
        }
        p.push(rel);
    }
    Ok(p)
}

// ---- archives (Lenovo's tool opens them with SharpCompress; the known orders use ZipCrypto zips) ---

fn open_zip(f: &El) -> R<(zip::ZipArchive<BufReader<File>>, String)> {
    let pw = cypher_decode(f.attr("key")).ok_or("archive key is not alphanumeric")?;
    let z = zip::ZipArchive::new(BufReader::new(File::open(local(f))?))
        .map_err(|e| format!("{}: not a zip archive ({e}). This app only knows zip recovery archives so far - please open an issue.", f.attr("name")))?;
    Ok((z, pw))
}

/// Decrypts and CRC-checks every entry without writing anything. Returns the unpacked size.
fn test_zip(f: &El) -> R<u64> {
    let (mut z, pw) = open_zip(f)?;
    let mut total = 0;
    for i in 0..z.len() {
        let mut e = z.by_index_decrypt(i, pw.as_bytes())?;
        total += io::copy(&mut e, &mut io::sink()).map_err(|err| format!("{}: {err}", f.attr("name")))?;
    }
    Ok(total)
}

fn extract_zip(f: &El, mnt: &Path) -> R {
    let (mut z, pw) = open_zip(f)?;
    for i in 0..z.len() {
        let mut e = z.by_index_decrypt(i, pw.as_bytes())?;
        let dst = mnt.join(e.enclosed_name().ok_or_else(|| format!("unsafe path in archive: {:?}", e.name()))?);
        if e.is_dir() {
            fs::create_dir_all(&dst)?;
            continue;
        }
        fs::create_dir_all(dst.parent().unwrap())?;
        pump(&mut e, &mut BufWriter::new(File::create(&dst)?))?;
    }
    Ok(())
}

// ---- boot sector -------------------------------------------------------------------------------

fn patch_sectors(f: &File, boot: &[u8]) -> R {
    let mut s0 = [0u8; 512];
    f.read_exact_at(&mut s0, 0)?;
    let start = u32::from_le_bytes(s0[454..458].try_into()?) as u64;
    if s0[510..512] != [0x55, 0xAA] || !matches!(s0[450], 0x0B | 0x0C) || start * 512 < MARKER_OFF + 512 {
        return Err(format!("unexpected MBR after erase (type {:#x}, start LBA {start}) - not touching it", s0[450]).into());
    }
    s0[..440].copy_from_slice(&boot[..440]); // CleanMBR: Windows NT6 MBR bootstrap
    s0[446] = 0x80; // SetPartitions: P0 active
    s0[450] = 0x0B; // SetPartitions: FAT32 -> 0x0B
    f.write_all_at(&s0, 0)?;
    let mut sec = [0u8; 512];
    f.read_exact_at(&mut sec, MARKER_OFF)?;
    sec[16..16 + MARKER.len()].copy_from_slice(MARKER);
    f.write_all_at(&sec, MARKER_OFF)?;
    nix::unistd::fsync(f)?; // plain fsync: F_FULLFSYNC (File::sync_all on macOS) isn't meant for raw devices
    Ok(())
}

/// macOS: open a raw disk through Apple's authopen (admin password dialog) and receive the fd over a socket.
#[cfg(target_os = "macos")]
fn authopen(path: &str) -> R<File> {
    use nix::sys::socket::{recvmsg, socketpair, AddressFamily, ControlMessageOwned, MsgFlags, SockFlag, SockType};
    use std::os::fd::{AsRawFd, FromRawFd, RawFd};
    let (a, b) = socketpair(AddressFamily::Unix, SockType::Stream, None, SockFlag::empty())?;
    let o_rdwr = nix::libc::O_RDWR.to_string();
    let mut child = Command::new("/usr/libexec/authopen").args(["-stdoutpipe", "-o", &o_rdwr, path]).stdout(Stdio::from(b)).spawn()?;
    let (mut buf, mut cmsg) = ([0u8; 64], nix::cmsg_space!(RawFd));
    let mut iov = [io::IoSliceMut::new(&mut buf)];
    let fd = recvmsg::<()>(a.as_raw_fd(), &mut iov, Some(&mut cmsg), MsgFlags::empty())
        .ok()
        .and_then(|m| m.cmsgs().ok()?.find_map(|c| if let ControlMessageOwned::ScmRights(f) = c { f.first().copied() } else { None }));
    child.wait()?;
    match fd {
        Some(fd) => Ok(unsafe { File::from_raw_fd(fd) }),
        None => Err(format!(
            "Could not open {path} for writing: the password dialog was cancelled, or macOS blocked access to removable \
             volumes (System Settings > Privacy & Security > Files and Folders > Recovery USB Maker > Removable Volumes)."
        )
        .into()),
    }
}

fn open_raw(disk: &str) -> R<File> {
    let rw = |p: &str| fs::OpenOptions::new().read(true).write(true).open(p);
    if !MAC {
        return Ok(rw(&format!("/dev/{disk}"))?);
    }
    let path = format!("/dev/r{disk}");
    match rw(&path) {
        Ok(f) => Ok(f), // disk images are owned by the user who attached them
        #[cfg(target_os = "macos")]
        Err(e) if e.kind() == io::ErrorKind::PermissionDenied => authopen(&path),
        Err(e) => Err(format!("{path}: {e}").into()),
    }
}

fn need_root() -> R {
    if !MAC && !nix::unistd::geteuid().is_root() {
        return Err("On Linux this step must run as root (the app asks for your password via pkexec).".into());
    }
    Ok(())
}

pub fn rawfix(disk: &str) -> R {
    need_root()?;
    let boot = fs::read(asset("zerosector.ucm"))?;
    if boot.len() != 512 {
        return Err("zerosector.ucm is damaged: run assets again".into());
    }
    check_disk(disk)?;
    unmount(disk)?;
    patch_sectors(&open_raw(disk)?, &boot)?;
    println!("MBR boot code, active flag and StateApp marker written.");
    #[cfg(target_os = "macos")]
    {
        sleep(Duration::from_secs(5)); // macOS re-probes and remounts after the MBR write; Spotlight then re-creates its folders
        if let Some(mnt) = mount_point(disk) {
            for d in [".Spotlight-V100", ".fseventsd"] {
                let _ = fs::remove_dir_all(mnt.join(d));
            }
        }
        unmount(disk)?;
        run(&["diskutil", "eject", disk])?;
    }
    if !MAC {
        run(&["sync"])?;
    }
    println!("DONE - recovery USB is ready. You can unplug it.");
    Ok(())
}

// ---- build -------------------------------------------------------------------------------------

fn apply_patch(mnt: &Path) -> R {
    // BurnUSBViewModel.ApplyPatch
    let rec = mnt.join("RECOVERY");
    let aod = rec.join("AOD.DAT");
    let text = fs::read_to_string(&aod)?;
    let arm = text.contains("WINBARM") || text.contains("WIN10ARM");
    let (boot, module) = if arm { ("bootaa64.efi", "M9ACB186KP1") } else { ("BootX64.efi", "M9CB186KP1") };
    let mut z = zip::ZipArchive::new(File::open(asset("Patch.zip"))?)?;
    let mut put = |src: &str, dst: PathBuf| -> R {
        fs::create_dir_all(dst.parent().unwrap())?;
        pump(&mut z.by_name(&format!("Patch/{src}"))?, &mut File::create(&dst)?)?;
        Ok(())
    };
    put(&format!("EFI/Boot/{boot}"), mnt.join("EFI/boot").join(boot))?;
    put("EFI/Microsoft/Boot/SKUSiPolicy.P7b", mnt.join("EFI/Microsoft/Boot/SKUSiPolicy.P7b"))?;
    for m in [module, "M9CB211C"] {
        for ext in [".CRI", ".IMZ"] {
            put(&format!("{m}{ext}"), rec.join(format!("{m}{ext}")))?;
        }
    }
    let (mut lines, mut backed_up) = (read_lines(&aod)?, false);
    for m in [module, "M9CB211C"] {
        if !lines.iter().any(|l| l.contains(&format!("{m}.CRI"))) {
            if !backed_up {
                pump(&mut File::open(&aod)?, &mut File::create(rec.join("AOD.ORG"))?)?;
                backed_up = true;
            }
            lines = update_aod(lines, m)?;
            write_lines(&aod, &lines)?;
        }
    }
    println!("Patch applied ({boot}, {module} + M9CB211C).");
    Ok(())
}

fn remove_apple_double(dir: &Path) {
    for e in fs::read_dir(dir).into_iter().flatten().flatten() {
        let p = e.path();
        if p.is_dir() {
            remove_apple_double(&p);
        } else if e.file_name().to_string_lossy().starts_with("._") {
            let _ = fs::remove_file(p);
        }
    }
}

pub fn build(disk: &str) -> R {
    need_root()?;
    ensure_assets()?;
    let (part, files, manual) = manifest()?;
    let missing: Vec<&El> = files.iter().filter(|f| !local(f).exists()).collect();
    if let Some(m) = missing.first() {
        return Err(format!("{} files not downloaded yet (e.g. {}) - download first", missing.len(), m.attr("name")).into());
    }
    let (archives, copies): (Vec<&El>, Vec<&El>) = files.iter().partition(|f| f.attr("copy") == "0");
    let mut need = 64 << 20;
    for f in &archives {
        // test the password and every CRC before erasing anything
        println!("Testing {} ...", f.attr("name"));
        need += test_zip(f)?;
    }
    for f in &copies {
        need += fs::metadata(local(f))?.len();
    }
    let info = check_disk(disk)?;
    if info.size < need {
        return Err(format!("stick too small: {:.1} GB, need {:.1} GB", info.size as f64 / 1e9, need as f64 / 1e9).into());
    }

    let label: String = part.attr("label").replace(' ', "").to_uppercase().chars().take(11).collect(); // FormatUSB strips spaces
    println!("ERASING {disk} ({}, {:.1} GB) -> MBR + FAT32 \"{label}\"", info.name, info.size as f64 / 1e9);
    let mnt = erase(disk, &label)?;
    File::create(mnt.join(".metadata_never_index"))?;

    let dest = |f: &El| onstick(&mnt, &[Some(f.attr("copypath")).filter(|c| !c.is_empty()).unwrap_or("\\RECOVERY"), f.attr("name")]);
    for (i, f) in copies.iter().enumerate() {
        let dst = dest(f)?;
        fs::create_dir_all(dst.parent().unwrap())?;
        print_flush(&format!("\r[copy {}/{}] {:<40}", i + 1, copies.len(), f.attr("name")));
        pump(&mut File::open(local(f))?, &mut File::create(&dst)?)?;
    }
    println!();
    for f in &archives {
        println!("Extracting {} ...", f.attr("name"));
        extract_zip(f, &mnt)?;
    }
    for m in &manual {
        let d = onstick(&mnt, &[m.attr("copypath")])?;
        fs::create_dir_all(&d)?;
        let tag = d.join("os.tag");
        if let Some(ostag) = m.get("ostag").filter(|_| m.attr("name").to_lowercase().contains("aod") && !tag.exists()) {
            fs::write(&tag, ostag)?;
        }
        let text = m.find("fcontent").map_or("", |c| c.text.as_str()).replace("\r\n", "\n").replace('\n', "\r\n");
        fs::write(onstick(&d, &[m.attr("name")])?, text)?;
    }
    if !MAC {
        run(&["sync"])?; // read back from the stick, not from the page cache
    }

    let mut checks: Vec<(PathBuf, &str)> = vec![];
    for f in &copies {
        checks.push((dest(f)?, f.attr("check")));
    }
    for m in &manual {
        checks.push((onstick(&mnt, &[m.attr("copypath"), m.attr("name")])?, m.attr("check")));
    }
    for c in archives.iter().flat_map(|f| f.findall("content")).filter(|c| !c.attr("check").is_empty()) {
        checks.push((onstick(&mnt, &[c.attr("path").trim_start_matches('.')])?, c.attr("check")));
    }
    let mut bad = vec![];
    for (i, (path, want)) in checks.iter().enumerate() {
        let name = path.file_name().unwrap_or_default().to_string_lossy();
        print_flush(&format!("\r[verify {}/{}] {name:<40}", i + 1, checks.len()));
        if sha256(path).ok() != Some(want.to_lowercase()) {
            bad.push(path.strip_prefix(&mnt).unwrap_or(path).display().to_string());
        }
    }
    println!();
    if !bad.is_empty() {
        return Err(format!("VERIFY FAILED for {} files: {:?}", bad.len(), &bad[..bad.len().min(10)]).into());
    }
    apply_patch(&mnt)?;

    remove_apple_double(&mnt);
    for d in [".Spotlight-V100", ".fseventsd", ".Trashes"] {
        let _ = fs::remove_dir_all(mnt.join(d));
    }
    fs::remove_file(mnt.join(".metadata_never_index"))?;
    if MAC {
        unmount(disk)?;
    } else {
        run(&["umount", &mnt.to_string_lossy()])?;
        fs::remove_dir(&mnt)?;
    }
    println!("Files written and verified. Writing the boot sector{} ...", if MAC { " (macOS will ask for your password)" } else { "" });
    rawfix(disk)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn cypher_and_aod() {
        assert_eq!(cypher_decode("AAA").as_deref(), Some("mlk"));
        assert_eq!(cypher_decode("a-b"), None);
        let aod = ["[Unit]", "Unit(1).SW_Module()=1:107", "  SW_Module(107).CRI_File=EMRPEX2511009B.CRI"].map(String::from).to_vec();
        let aod = update_aod(update_aod(aod, "M9CB186KP1").unwrap(), "M9CB211C").unwrap();
        assert_eq!(
            aod,
            [
                "[Unit]",
                "Unit(1).SW_Module()=1:109",
                "  SW_Module(107).CRI_File=EMRPEX2511009B.CRI",
                "  SW_Module(108).CRI_File=M9CB186KP1.CRI",
                "  SW_Module(109).CRI_File=M9CB211C.CRI"
            ]
        );
        assert_eq!((MARKER.len(), MARKER_OFF), (22, 31232));
    }

    #[test]
    fn linux_disks() {
        let usb = json!({"name": "sdb", "type": "disk", "size": 64e9, "tran": "usb", "rm": true, "log-sec": 512, "model": "Cruzer ",
                         "children": [{"name": "sdb1", "mountpoint": "/media/me/STICK"}]});
        let d = describe(&usb);
        assert_eq!((d.id.as_str(), d.name.as_str(), d.size, d.problem), ("sdb", "Cruzer", 64_000_000_000, None));
        let mut busy = usb.clone();
        busy["children"] = json!([{"mountpoint": "/"}]);
        assert!(describe(&busy).problem.unwrap().contains("in use"));
        let mut nvme = usb.clone();
        (nvme["tran"], nvme["rm"], nvme["hotplug"]) = (json!("nvme"), json!("0"), json!("0"));
        assert_eq!(describe(&nvme).problem.as_deref(), Some(NOT_USB));
        assert!(!disk_name_ok("") && !disk_name_ok("../x"));
    }

    #[test]
    fn stick_paths_stay_on_the_stick() {
        let m = Path::new("/mnt/s");
        assert_eq!(onstick(m, &["\\RECOVERY", "A.CRI"]).unwrap(), Path::new("/mnt/s/RECOVERY/A.CRI"));
        assert_eq!(onstick(m, &["\\BOOT\\BCD"]).unwrap(), Path::new("/mnt/s/BOOT/BCD"));
        assert_eq!(onstick(m, &[""]).unwrap(), Path::new("/mnt/s"));
        assert!(onstick(m, &["\\..\\etc"]).is_err());
    }

    #[test]
    fn mbr_patch() {
        // a fake 1 MiB disk laid out like a fresh eraseDisk/sfdisk
        let path = std::env::temp_dir().join(format!("mbr-test-{}", std::process::id()));
        let mut img = vec![0u8; 1 << 20];
        img[450] = 0x0C;
        img[454..458].copy_from_slice(&2048u32.to_le_bytes());
        img[510..512].copy_from_slice(&[0x55, 0xAA]);
        fs::write(&path, &img).unwrap();
        let boot: Vec<u8> = (0..512).map(|i| i as u8).collect();
        patch_sectors(&fs::OpenOptions::new().read(true).write(true).open(&path).unwrap(), &boot).unwrap();
        let out = fs::read(&path).unwrap();
        fs::remove_file(&path).unwrap();
        assert!(out[..440] == boot[..440] && out[446] == 0x80 && out[450] == 0x0B && out[454..458] == img[454..458]);
        assert_eq!(&out[MARKER_OFF as usize + 16..MARKER_OFF as usize + 38], MARKER);
    }
}
