//! The app window. Each step runs as a subcommand of this binary (on Linux the USB step via pkexec);
//! its output is kept for the page, which polls `status`.
//! The Lenovo ID sign-in runs in a second window, where the lenovoid.wust token is caught on navigation
//! (what USBRecoveryCreator's WebView2 NavigationStarting handler does).
use crate::dds::{self, files_dir};
use crate::usb;
use crate::{data, El, MAC, R};
use serde::Serialize;
use serde_json::{json, Value};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{mpsc, LazyLock, Mutex};
use std::time::Duration;
use tauri::{AppHandle, Manager, RunEvent, WebviewUrl, WebviewWindowBuilder, WindowEvent};

const LOGIN: &str = "https://passport.lenovo.com/glbwebauthnv6/gateway?lenovoid.action=uilogin&lenovoid.realm=dds1-app.lenovo.com\
                     &lenovoid.lang=en_US&lenovoid.cb=https://passport.lenovo.com/glbwebauthnv6";

const SAFARI: &str = "Mozilla/5.0 (Macintosh; Intel Mac OS X 10_15_7) AppleWebKit/605.1.15 (KHTML, like Gecko) Version/18.0 Safari/605.1.15";

#[derive(Default, Clone, Serialize)]
struct Job {
    name: Option<String>,
    running: bool,
    ok: Option<bool>,
    lines: Vec<String>,
    cur: String,
}

static JOB: LazyLock<Mutex<Job>> = LazyLock::new(Mutex::default);
static CHILD: AtomicU32 = AtomicU32::new(0); // pid of the running step, stopped when the app quits

/// Terminal-ish: \r rewinds the current line (progress counters).
fn log(text: &str) {
    let mut j = JOB.lock().unwrap();
    let mut rest = text;
    while let Some(i) = rest.find(['\r', '\n']) {
        j.cur.push_str(&rest[..i]);
        if rest[i..].starts_with("\r\n") || rest[i..].starts_with('\n') {
            let line = std::mem::take(&mut j.cur);
            j.lines.push(line);
        } else {
            j.cur.clear();
        }
        rest = &rest[i + if rest[i..].starts_with("\r\n") { 2 } else { 1 }..];
    }
    j.cur.push_str(rest);
    let n = j.lines.len();
    j.lines.drain(..n.saturating_sub(400));
}

/// Runs `<this binary> --data DIR args...`, streaming its output into the log.
fn sh(args: &[&str], input: Option<String>, as_root: bool) -> R {
    let me = std::env::current_exe()?;
    let mut cmd = if as_root {
        // root can't see inside a user's AppImage mount, so hand pkexec the AppImage itself
        let mut c = Command::new("pkexec");
        c.arg(std::env::var_os("APPIMAGE").map(PathBuf::from).unwrap_or(me));
        c
    } else {
        Command::new(me)
    };
    let (mut out, w) = std::io::pipe()?;
    cmd.arg("--data").arg(data()).args(args).stdout(w.try_clone()?).stderr(w);
    cmd.stdin(if input.is_some() { Stdio::piped() } else { Stdio::null() });
    let mut child = cmd.spawn()?;
    drop(cmd); // our copies of the pipe's write end, so the read below ends when the child does
    CHILD.store(child.id(), Ordering::SeqCst);
    if let (Some(text), Some(mut stdin)) = (input, child.stdin.take()) {
        stdin.write_all(text.as_bytes())?;
    }
    let (mut buf, mut pending) = (vec![0; 65536], Vec::new());
    loop {
        let n = out.read(&mut buf)?;
        if n == 0 {
            break;
        }
        pending.extend_from_slice(&buf[..n]);
        let ok = match std::str::from_utf8(&pending) {
            Ok(s) => s.len(),
            Err(e) if e.error_len().is_none() => e.valid_up_to(), // a character split across reads
            Err(_) => pending.len(),
        };
        log(&String::from_utf8_lossy(&pending[..ok]));
        pending.drain(..ok);
    }
    let st = child.wait()?;
    CHILD.store(0, Ordering::SeqCst);
    match st.code() {
        Some(0) => Ok(()),
        Some(126 | 127) if as_root => Err("The password prompt was cancelled.".into()),
        _ => Err("".into()), // the step already printed why
    }
}

type Step = Box<dyn FnOnce() -> R + Send>;

fn start(name: &str, steps: Vec<Step>) -> Result<(), String> {
    let mut j = JOB.lock().unwrap();
    if j.running {
        return Err("Another step is still running.".into());
    }
    *j = Job { name: Some(name.into()), running: true, ..Job::default() };
    drop(j);
    std::thread::spawn(move || {
        let res = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| steps.into_iter().try_for_each(|step| step())));
        let err = match res {
            Ok(Ok(())) => None,
            Ok(Err(e)) => Some(e.to_string()),
            Err(_) => Some("Internal error.".into()),
        };
        if let Some(e) = err.as_deref().filter(|e| !e.is_empty()) {
            log(&format!("\n{e}"));
        }
        let mut j = JOB.lock().unwrap();
        if !j.cur.is_empty() {
            let line = std::mem::take(&mut j.cur);
            j.lines.push(line);
        }
        (j.ok, j.running) = (Some(err.is_none()), false);
    });
    Ok(())
}

static LOGINS: AtomicU32 = AtomicU32::new(0);

/// Opens the Lenovo ID sign-in and returns the first URL carrying lenovoid.wust=.
fn capture_wust(app: &AppHandle) -> R<String> {
    let (tx, rx) = mpsc::channel::<Option<String>>();
    let closed = tx.clone();
    let label = format!("login{}", LOGINS.fetch_add(1, Ordering::SeqCst));
    let mut win = WebviewWindowBuilder::new(app, label, WebviewUrl::External(LOGIN.parse()?)).title("Sign in to Lenovo");
    if MAC {
        win = win.user_agent(SAFARI); // WKWebView's own UA lacks "Safari", which "Continue with Google" refuses
    }
    let win = win
        .inner_size(520.0, 760.0)
        .incognito(true)
        .on_navigation(move |url| {
            if !url.as_str().contains("lenovoid.wust=") {
                return true;
            }
            let _ = tx.send(Some(url.to_string()));
            false // the token is single-use: keep the page from spending it
        })
        .build()?;
    win.on_window_event(move |e| {
        if let WindowEvent::Destroyed = e {
            let _ = closed.send(None);
        }
    });
    log("Sign in with your Lenovo ID in the window that just opened...\n");
    let got = rx.recv_timeout(Duration::from_secs(15 * 60));
    let _ = win.destroy();
    match got {
        Ok(Some(url)) => {
            log("Signed in. Asking Lenovo for your recovery orders...\n");
            Ok(url)
        }
        Ok(None) => Err("The sign-in window was closed before signing in.".into()),
        Err(_) => Err("No sign-in within 15 minutes.".into()),
    }
}

fn order_files() -> Vec<El> {
    dds::order_root().map(|dr| dr.iter("file").into_iter().cloned().collect()).unwrap_or_default()
}

fn usable_disk(disk: &str) -> bool {
    usb::list_disks().is_ok_and(|ds| ds.iter().any(|d| d.id == disk && d.problem.is_none()))
}

#[tauri::command]
async fn act(app: AppHandle, name: String, arg: Option<String>) -> Result<(), String> {
    let arg = arg.unwrap_or_default();
    match name.as_str() {
        "login" => start(
            "login",
            vec![Box::new(move || {
                let url = capture_wust(&app)?;
                sh(&["login", "-"], Some(url), false)
            })],
        ),
        "order" => {
            if arg.is_empty() || arg.len() > 64 || !arg.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-') {
                return Err("bad order id".into());
            }
            start(
                "order",
                vec![
                    Box::new(move || sh(&["order", &arg], None, false)) as Step,
                    Box::new(|| {
                        // the small layout file, checked before committing to the big download
                        let rmf: Vec<String> =
                            order_files().iter().map(|f| f.attr("name").to_string()).filter(|n| n.to_uppercase().ends_with(".RMF")).collect();
                        if rmf.is_empty() {
                            return Ok(());
                        }
                        let args: Vec<&str> = std::iter::once("download").chain(rmf.iter().map(String::as_str)).collect();
                        sh(&args, None, false)
                    }),
                ],
            )
        }
        "download" => {
            start("download", vec![Box::new(|| sh(&["download"], None, false)) as Step, Box::new(|| sh(&["assets"], None, false))])
        }
        "build" | "rawfix" => {
            if !usable_disk(&arg) {
                return Err("That disk is no longer available.".into());
            }
            let (cmd, disk) = (name.clone(), arg);
            start(
                "build",
                vec![Box::new(|| sh(&["assets"], None, false)) as Step, Box::new(move || sh(&[cmd.as_str(), &disk], None, !MAC))],
            )
        }
        "cleanup" => start(
            "cleanup",
            vec![Box::new(|| {
                std::fs::remove_dir_all(files_dir())?;
                let _ = std::fs::remove_file(data().join("USBRecoveryCreator.exe"));
                log("Deleted the downloaded files.\n");
                Ok(())
            })],
        ),
        _ => Err("unknown action".into()),
    }
}

fn which(tool: &str) -> bool {
    let path = std::env::var("PATH").unwrap_or_default() + ":/usr/sbin:/sbin";
    path.split(':').any(|d| Path::new(d).join(tool).is_file())
}

#[tauri::command]
async fn status() -> Value {
    let sess = dds::load().unwrap_or_default();
    let files: Vec<El> = order_files().into_iter().filter(|f| !f.attr("source").is_empty()).collect();
    let (mut total, mut done) = (0.0, 0.0);
    for f in &files {
        let unit = match f.attr("stype").to_uppercase().as_str() {
            "KB" => 1024.0,
            "MB" => 1048576.0,
            "GB" => 1073741824.0,
            _ => 1.0,
        };
        let size = f.attr("size").parse::<f64>().unwrap_or(0.0) * unit;
        let base = dds::basename(f.attr("name"));
        let (p, tmp) = (files_dir().join(&base), std::fs::metadata(files_dir().join(base + ".tmp")).map(|m| m.len() as f64));
        total += size;
        done += if p.exists() { size } else { tmp.map_or(0.0, |t| t.min(size)) };
    }
    let order = dds::order_root().ok().map(|dr| {
        let mut o: serde_json::Map<String, Value> = dr.attrs.iter().map(|(k, v)| (k.clone(), v.as_str().into())).collect();
        o.extend([("files".into(), files.len().into()), ("total".into(), total.into()), ("done".into(), done.into())]);
        o
    });
    let have_rmf = usb::has_rmf();
    let problem = if have_rmf { usb::layout_problem() } else { None };
    let ready = have_rmf && problem.is_none() && usb::manifest().is_ok_and(|(_, fs, _)| fs.iter().all(|f| usb::local(f).exists()));
    let tools: Vec<&str> = if MAC { vec![] } else { ["lsblk", "sfdisk", "wipefs", "mkfs.vfat", "pkexec"].into_iter().filter(|t| !which(t)).collect() };
    let _ = std::fs::create_dir_all(files_dir());
    let free = nix::sys::statvfs::statvfs(&files_dir()).map(|s| s.blocks_available() as f64 * s.fragment_size() as f64).unwrap_or(0.0);
    let job = JOB.lock().unwrap().clone();
    json!({
        "mac": MAC, "signedIn": sess.get("token").and_then(Value::as_str).is_some_and(|t| !t.is_empty()),
        "orders": sess.get("orders").cloned().unwrap_or(json!([])), "order": order, "orderId": sess.get("orderId"),
        "layoutProblem": problem, "ready": ready, "missingTools": tools, "free": free, "dataDir": data(),
        "job": { "name": job.name, "running": job.running, "ok": job.ok, "cur": job.cur,
                 "lines": job.lines[job.lines.len().saturating_sub(200)..] },
    })
}

#[tauri::command]
async fn disks() -> Vec<usb::Disk> {
    usb::list_disks().unwrap_or_default()
}

#[tauri::command]
fn open_url(url: String) -> Result<(), String> {
    if !url.starts_with("https://") {
        return Err("not a web link".into());
    }
    Command::new(if MAC { "open" } else { "xdg-open" }).arg(url).spawn().map(|_| ()).map_err(|e| e.to_string())
}

fn stop_child() {
    let pid = CHILD.load(Ordering::SeqCst);
    if pid != 0 {
        let _ = nix::sys::signal::kill(nix::unistd::Pid::from_raw(pid as i32), nix::sys::signal::Signal::SIGTERM);
    }
}

pub fn run() {
    tauri::Builder::default()
        .invoke_handler(tauri::generate_handler![status, disks, act, open_url])
        .on_window_event(|w, e| {
            if w.label() == "main" && matches!(e, WindowEvent::Destroyed) {
                w.app_handle().exit(0); // closing the app window quits, even with a sign-in window open
            }
        })
        .build(tauri::generate_context!())
        .expect("could not start the app")
        .run(|_, e| {
            if let RunEvent::Exit = e {
                stop_child();
            }
        });
}
