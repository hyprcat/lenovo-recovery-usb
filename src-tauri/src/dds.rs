//! Lenovo's recovery service (dds.lenovo.com): sign-in, order, download.
//! The protocol mirrors USBRecoveryCreator.Common.WebServices (decompiled).
use crate::{data, print_flush, sha256, truncate, El, R};
use serde_json::{Map, Value};
use std::collections::HashSet;
use std::fs;
use std::io::{Read, Write};
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::sync::LazyLock;
use std::time::{Duration, Instant};

const API: &str = "https://dds.lenovo.com/dds/services/";
const UA: &str = "USBRecoveryCreator-mac-port";
const GIB: f64 = (1u64 << 30) as f64;

pub type Session = Map<String, Value>;

pub static AGENT: LazyLock<ureq::Agent> = LazyLock::new(|| {
    ureq::AgentBuilder::new().timeout_connect(Duration::from_secs(30)).timeout_read(Duration::from_secs(60)).build()
});

pub fn order_xml() -> PathBuf {
    data().join("order.xml")
}

pub fn files_dir() -> PathBuf {
    data().join("files")
}

pub fn basename(name: &str) -> String {
    Path::new(name).file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default()
}

pub fn load() -> R<Session> {
    let text = fs::read(data().join("session.json")).map_err(|_| "Not signed in yet: run login first.")?;
    Ok(serde_json::from_slice(&text)?)
}

fn get<'a>(s: &'a Session, k: &str) -> &'a str {
    s.get(k).and_then(Value::as_str).unwrap_or("")
}

/// Sign-in tokens and order details: readable by the user only.
pub fn write_private(path: &Path, bytes: &[u8]) -> R {
    fs::create_dir_all(data())?;
    fs::write(path, bytes)?;
    fs::set_permissions(path, fs::Permissions::from_mode(0o600))?;
    Ok(())
}

fn save(s: &Session) -> R {
    write_private(&data().join("session.json"), serde_json::to_string_pretty(s)?.as_bytes())
}

fn ddm(kind: &str, inner: &str) -> String {
    format!(
        r#"<?xml version="1.0" encoding="utf-8"?><ddm xmlns:xsi="http://www.w3.org/2001/XMLSchema-instance" xmlns:xsd="http://www.w3.org/2001/XMLSchema" type="{kind}">{inner}</ddm>"#
    )
}

fn attrs(kv: &[(&str, &str)]) -> String {
    let esc = |v: &str| v.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;").replace('"', "&quot;");
    kv.iter().filter(|(_, v)| !v.is_empty()).map(|(k, v)| format!(" {k}=\"{}\"", esc(v))).collect()
}

struct Reply {
    text: String,
    token: Option<String>,
    cookie: String,
}

fn post(service: &str, body: &str, s: &Session, bearer: &str) -> R<Reply> {
    let mut req = AGENT.post(&format!("{API}{service}")).set("Content-Type", "text/xml").set("User-Agent", UA);
    if !bearer.is_empty() {
        req = req.set("Authorization", &format!("Bearer {bearer}"));
    }
    let csrf = get(s, "csrf");
    if !csrf.is_empty() {
        req = req.set("X-CSRF-TOKEN", csrf).set("Cookie", &format!("XSRF-TOKEN={csrf}"));
    }
    match req.send_string(body) {
        Ok(r) => Ok(Reply {
            token: r.header("token").map(String::from),
            cookie: r.all("set-cookie").first().copied().unwrap_or("").into(),
            text: r.into_string()?,
        }),
        Err(ureq::Error::Status(code, r)) => {
            Err(format!("{service} -> HTTP {code}: {}", truncate(&r.into_string().unwrap_or_default(), 500)).into())
        }
        Err(e) => Err(e.into()),
    }
}

pub fn login(arg: &str) -> R {
    let mut input = arg.to_string();
    if arg == "-" {
        input.clear();
        std::io::stdin().read_to_string(&mut input)?;
    }
    let wust = match input.find("lenovoid.wust=") {
        Some(i) => input[i + 14..].split(|c: char| c == '&' || c.is_whitespace()).next().unwrap_or(""),
        None => "",
    };
    let wust = if wust.is_empty() { input.trim() } else { wust };
    let client = crate::hex(&<sha2::Sha256 as sha2::Digest>::digest(crate::random_hex(16)?)); // tool sends sha256(MachineGuid)
    let mut s = Session::new();
    s.insert("clientId".into(), client.clone().into());
    let brand = if crate::MAC { "Apple Inc. - Mac" } else { "Linux PC" };
    let body = ddm(
        "entitlementRequest",
        &format!(
            "<entitlementRequest{}><metrics{} /></entitlementRequest>",
            attrs(&[("token", wust), ("country", "US")]),
            attrs(&[("brand", brand), ("machineType", "Laptop"), ("clientId", &client)])
        ),
    );
    let r = post("entitlement", &body, &s, "")?;
    // WebServices.HTTPPOSTResult: token header + first Set-Cookie split('=')[1]
    let token = r.token.unwrap_or_default();
    s.insert("entToken".into(), token.clone().into());
    s.insert("token".into(), token.clone().into());
    let csrf = r.cookie.split('=').nth(1).map(|v| v.split(';').next().unwrap_or("")).unwrap_or("");
    s.insert("csrf".into(), csrf.into());
    let root = El::parse(&r.text).ok();
    let er = match root.as_ref().and_then(|r| r.find("entitlementResponse")) {
        Some(er) if !token.is_empty() => er,
        _ => return Err(format!("Entitlement failed: {}", truncate(&r.text, 500)).into()),
    };
    s.insert("deviceId".into(), er.attr("deviceId").into());
    s.insert("lenovoID".into(), er.attr("lenovoID").into());
    let orders = er.iter("order");
    let list = orders.iter().map(|o| Value::Object(o.attrs.iter().map(|(k, v)| (k.clone(), v.as_str().into())).collect()));
    s.insert("orders".into(), Value::Array(list.collect()));
    save(&s)?;
    println!("code={} message={}", er.attr("code"), er.attr("message"));
    for o in orders {
        let a = |k| o.attr(k);
        println!("ORDER {}  {}  SN={}  {}  {}", a("id"), a("machineType"), a("serialNumber"), a("os"), a("dateTime"));
    }
    Ok(())
}

pub fn order(id: &str, force: Option<&str>) -> R {
    let mut s = load()?;
    let body = ddm(
        "downloadRequest",
        &format!(
            "<downloadRequest{} />",
            attrs(&[("id", id), ("lenovoID", get(&s, "lenovoID")), ("deviceId", get(&s, "deviceId")), ("force", force.unwrap_or("0"))])
        ),
    );
    let r = post("download", &body, &s, get(&s, "entToken"))?;
    let root = El::parse(&r.text).ok();
    let dr = root.as_ref().and_then(|r| r.find("downloadResponse")).ok_or_else(|| format!("Unexpected response: {}", truncate(&r.text, 500)))?;
    println!("code={} message={}", dr.attr("code"), dr.attr("message"));
    if dr.attr("code") != "305" {
        if let Some(t) = r.token {
            s.insert("token".into(), t.into());
        }
    }
    s.insert("orderId".into(), id.into());
    save(&s)?;
    write_private(&order_xml(), r.text.as_bytes())?;
    for p in dr.iter("partition") {
        let a = |k| p.attr(k);
        println!("PARTITION id={} label={} size={}MB format={} primary={}", a("id"), a("label"), a("size"), a("format"), a("primary"));
    }
    println!("{} files", dr.iter("file").len());
    Ok(())
}

fn regenerate(s: &Session, f: &El) -> R<String> {
    let file = attrs(&[("name", f.attr("name")), ("source", f.attr("source")), ("copypath", f.attr("copypath"))]);
    let body = ddm(
        "regenerateUrlRequest",
        &format!("<regenerateUrlRequest{}><file{file} /></regenerateUrlRequest>", attrs(&[("id", get(s, "orderId")), ("deviceId", get(s, "deviceId"))])),
    );
    let r = post("regenerateUrl", &body, s, get(s, "token"))?;
    let root = El::parse(&r.text).ok();
    match root.as_ref().and_then(|r| r.find("regenerateUrlResponse/file")) {
        Some(nf) => Ok(nf.attr("source").into()),
        None => Err(format!("Could not refresh the download link (session expired?). Sign in again: {}", truncate(&r.text, 300)).into()),
    }
}

fn tmp_of(dst: &Path, ext: &str) -> PathBuf {
    let mut p = dst.as_os_str().to_owned();
    p.push(ext);
    p.into()
}

fn fetch(s: &Session, f: &El, dst: &Path, tag: &str) -> R {
    let (name, tmp) = (f.attr("name"), tmp_of(dst, ".tmp"));
    let mut src = f.attr("source").to_string();
    for attempt in 0..3 {
        let have = fs::metadata(&tmp).map(|m| m.len()).unwrap_or(0);
        let url = format!("{src}&SSID={}", get(s, "deviceId"));
        let r = match AGENT.get(&url).set("Range", &format!("bytes={have}-")).set("User-Agent", UA).call() {
            Ok(r) => r,
            Err(ureq::Error::Status(416, _)) => return Ok(()),
            Err(ureq::Error::Status(401 | 403, _)) if attempt < 2 => {
                src = regenerate(s, f)?;
                continue;
            }
            Err(ureq::Error::Status(code, _)) => return Err(format!("{name}: HTTP {code}").into()),
            Err(e) => {
                println!("\n  network hiccup ({e}), resuming...");
                continue;
            }
        };
        let partial = r.status() == 206;
        let len: u64 = r.header("Content-Length").and_then(|v| v.parse().ok()).unwrap_or(0);
        let (total, mut done) = if partial { (have + len, have) } else { (len, 0) };
        let mut out = fs::OpenOptions::new().create(true).write(true).append(partial).truncate(!partial).open(&tmp)?;
        let (mut body, mut buf, mut shown) = (r.into_reader(), vec![0; 1 << 20], Instant::now() - Duration::from_secs(1));
        let hiccup = loop {
            let n = match body.read(&mut buf) {
                Ok(0) => break None,
                Ok(n) => n,
                Err(e) => break Some(e),
            };
            out.write_all(&buf[..n])?;
            done += n as u64;
            if shown.elapsed() > Duration::from_millis(250) {
                shown = Instant::now();
                print_flush(&format!("\r{tag}{name} {:.1}/{:.1} GB", done as f64 / GIB, total as f64 / GIB));
            }
        };
        match hiccup {
            None => {
                println!("\r{tag}{name} {:.1}/{:.1} GB", done as f64 / GIB, total as f64 / GIB);
                return Ok(());
            }
            Some(e) => println!("\n  network hiccup ({e}), resuming..."),
        }
    }
    Err(format!("Giving up on {name}").into())
}

pub fn order_root() -> R<El> {
    let root = El::parse(&fs::read_to_string(order_xml())?)?;
    Ok(root.find("downloadResponse").ok_or("order.xml has no downloadResponse")?.clone())
}

pub fn download(only: &[&str]) -> R {
    let s = load()?;
    let dr = order_root()?;
    let dir = files_dir();
    fs::create_dir_all(&dir)?;
    let all = dr.iter("file");
    let mut todo: Vec<&El> =
        all.iter().copied().filter(|f| !f.attr("source").is_empty() && (only.is_empty() || only.contains(&f.attr("name")))).collect();
    todo.sort_by_key(|f| !f.attr("name").to_uppercase().ends_with(".RMF")); // layout file first, so it can be checked early
    let keep: HashSet<String> = all.iter().flat_map(|f| [basename(f.attr("name")), basename(f.attr("name")) + ".tmp"]).collect();
    let stale: Vec<String> =
        fs::read_dir(&dir)?.filter_map(|e| e.ok()).map(|e| e.file_name().to_string_lossy().into_owned()).filter(|n| !keep.contains(n)).collect();
    if !stale.is_empty() {
        println!("Removing {} files left over from an earlier order", stale.len());
        for n in &stale {
            fs::remove_file(dir.join(n))?;
        }
    }
    for (i, f) in todo.iter().enumerate() {
        let (tag, name) = (format!("[{}/{}] ", i + 1, todo.len()), f.attr("name"));
        let base = basename(name);
        if base.is_empty() || base == ".." {
            return Err(format!("bad file name in order: {name:?}").into());
        }
        let dst = dir.join(base);
        let want = f.attr("check").to_lowercase();
        if dst.exists() && sha256(&dst)? == want {
            println!("{tag}OK (already) {name}");
            continue;
        }
        fetch(&s, f, &dst, &tag)?;
        let tmp = tmp_of(&dst, ".tmp");
        let got = sha256(&tmp)?;
        if got != want {
            fs::rename(&tmp, tmp_of(&dst, ".bad"))?;
            return Err(format!("SHA-256 mismatch for {name} (got {got}, want {want}) - saved as .bad").into());
        }
        fs::rename(&tmp, &dst)?;
        println!("{tag}OK {name}");
    }
    println!("All files downloaded and verified in {}", dir.display());
    Ok(())
}
