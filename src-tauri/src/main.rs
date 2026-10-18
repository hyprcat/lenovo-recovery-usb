//! Recovery USB Maker: Lenovo's USB Recovery Creator for macOS and Linux.
//!
//! With no arguments it opens the app. The app runs every step as a subcommand of this same binary
//! (on Linux the USB step goes through pkexec), so everything also works from a terminal.
mod dds;
mod gui;
mod usb;

use sha2::{Digest, Sha256};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::OnceLock;

pub type R<T = ()> = Result<T, Box<dyn std::error::Error + Send + Sync>>;

pub const MAC: bool = cfg!(target_os = "macos");
const ID: &str = "io.github.hyprcat.recoveryusbmaker";
const USAGE: &str = "usage: recovery-usb-maker [--data DIR] [COMMAND]

  (no command)          open the app
  login URL|-           sign in with the URL containing lenovoid.wust=... ('-' reads it from stdin)
  order ORDER_ID        fetch the recovery order
  download [NAME...]    download the order's files, checking each SHA-256
  assets                extract the two boot files from Lenovo's own tool
  disks                 list USB disks
  build DISK            ERASE DISK and write the recovery stick (macOS: diskN; Linux: sdX, as root)
  rawfix DISK           only the final boot-sector step of build

Downloads and sign-in data live in DIR (default: the app's data folder).";

static DATA: OnceLock<PathBuf> = OnceLock::new();

pub fn data() -> &'static Path {
    DATA.get().expect("data dir set in main")
}

fn default_data() -> PathBuf {
    let home = PathBuf::from(std::env::var_os("HOME").unwrap_or_default());
    if MAC {
        return home.join("Library/Application Support").join(ID);
    }
    let xdg = std::env::var_os("XDG_DATA_HOME").map(PathBuf::from).filter(|p| p.is_absolute());
    xdg.unwrap_or_else(|| home.join(".local/share")).join(ID)
}

fn main() {
    let mut a: Vec<String> = std::env::args().skip(1).collect();
    let data = match a.first().map(String::as_str) {
        Some("--data") if a.len() > 1 => PathBuf::from(a.drain(..2).nth(1).unwrap()),
        _ => default_data(),
    };
    DATA.set(std::path::absolute(data).expect("data dir")).unwrap();
    if a.is_empty() || a[0].starts_with("-psn_") {
        return gui::run();
    }
    let a: Vec<&str> = a.iter().map(String::as_str).collect();
    let res = match a.as_slice() {
        ["login", arg] => dds::login(arg),
        ["order", id] => dds::order(id, None),
        ["order", id, force] => dds::order(id, Some(force)),
        ["download", only @ ..] => dds::download(only),
        ["assets"] => usb::ensure_assets(),
        ["disks"] => usb::list_disks().map(|d| println!("{}", serde_json::to_string_pretty(&d).unwrap())),
        ["build", disk] if usb::disk_name_ok(disk) => usb::build(disk),
        ["rawfix", disk] if usb::disk_name_ok(disk) => usb::rawfix(disk),
        _ => Err(USAGE.into()),
    };
    if let Err(e) = res {
        let _ = std::io::stdout().flush();
        eprintln!("{e}");
        std::process::exit(1);
    }
}

// ---- shared helpers ------------------------------------------------------------------------------

pub fn sha256(path: &Path) -> R<String> {
    let mut f = std::fs::File::open(path)?;
    let (mut h, mut buf) = (Sha256::new(), vec![0; 4 << 20]);
    loop {
        match f.read(&mut buf)? {
            0 => return Ok(hex(&h.finalize())),
            n => h.update(&buf[..n]),
        }
    }
}

pub fn hex(b: &[u8]) -> String {
    b.iter().map(|x| format!("{x:02x}")).collect()
}

pub fn random_hex(n: usize) -> R<String> {
    let mut b = vec![0; n];
    std::fs::File::open("/dev/urandom")?.read_exact(&mut b)?;
    Ok(hex(&b))
}

/// Copies data only (no extended attributes, so macOS doesn't litter the FAT stick with ._ files).
pub fn pump(r: &mut impl Read, w: &mut impl Write) -> std::io::Result<u64> {
    let (mut buf, mut n) = (vec![0; 4 << 20], 0);
    loop {
        match r.read(&mut buf) {
            Ok(0) => return w.flush().map(|_| n),
            Ok(k) => {
                w.write_all(&buf[..k])?;
                n += k as u64;
            }
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => {}
            Err(e) => return Err(e),
        }
    }
}

pub fn print_flush(s: &str) {
    print!("{s}");
    let _ = std::io::stdout().flush();
}

pub fn truncate(s: &str, n: usize) -> String {
    s.chars().take(n).collect()
}

/// Just enough of Python's ElementTree to read Lenovo's XML.
#[derive(Clone, Default)]
pub struct El {
    pub tag: String,
    pub attrs: Vec<(String, String)>,
    pub text: String,
    pub kids: Vec<El>,
}

impl El {
    pub fn parse(xml: &str) -> R<El> {
        Ok(Self::own(roxmltree::Document::parse(xml)?.root_element()))
    }

    fn own(n: roxmltree::Node) -> El {
        El {
            tag: n.tag_name().name().into(),
            attrs: n.attributes().map(|a| (a.name().into(), a.value().into())).collect(),
            text: n.text().unwrap_or("").into(),
            kids: n.children().filter(|c| c.is_element()).map(Self::own).collect(),
        }
    }

    pub fn get(&self, k: &str) -> Option<&str> {
        self.attrs.iter().find(|(n, _)| n == k).map(|(_, v)| v.as_str())
    }

    pub fn attr(&self, k: &str) -> &str {
        self.get(k).unwrap_or("")
    }

    pub fn findall(&self, path: &str) -> Vec<&El> {
        path.split('/').fold(vec![self], |cur, tag| cur.into_iter().flat_map(|e| e.kids.iter().filter(move |k| k.tag == tag)).collect())
    }

    pub fn find(&self, path: &str) -> Option<&El> {
        self.findall(path).into_iter().next()
    }

    pub fn iter<'a>(&'a self, tag: &str) -> Vec<&'a El> {
        let mut out = if self.tag == tag { vec![self] } else { vec![] };
        out.extend(self.kids.iter().flat_map(|k| k.iter(tag)));
        out
    }
}
