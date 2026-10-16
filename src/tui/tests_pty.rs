//! The real window under a real pseudo-console (ConPTY on Windows, a pty elsewhere): the loop, the alternate screen, the single
//! write per frame, keys reaching the screen, and a quiet screen staying quiet. Ignored by default because it starts the core
//! (`agentdesk daemon`) on a scratch board and needs a built `agentdesk` next to the test binary:
//!
//! `cargo build && cargo test --bin agentdesk tui::tests_pty -- --ignored --nocapture`
//!
//! It runs on a folder of its own (`AGENTDESK_DATA`, `HOME`) and kills what it starts.

use portable_pty::{native_pty_system, CommandBuilder, PtySize};
use std::io::{Read, Write};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

fn exe() -> std::path::PathBuf {
    // AGENTDESK_TEST_EXE: measure another build (the one before a change) with the same script
    if let Some(p) = std::env::var_os("AGENTDESK_TEST_EXE").filter(|p| !p.is_empty()) {
        return p.into();
    }
    let me = std::env::current_exe().unwrap();
    me.parent().unwrap().parent().unwrap().join(if cfg!(windows) { "agentdesk.exe" } else { "agentdesk" })
}

fn wait_for(out: &Arc<Mutex<Vec<u8>>>, needle: &str, from: usize, within: Duration) -> Option<Duration> {
    let t0 = Instant::now();
    while t0.elapsed() < within {
        if String::from_utf8_lossy(&out.lock().unwrap()[from..]).contains(needle) {
            return Some(t0.elapsed());
        }
        std::thread::sleep(Duration::from_millis(2));
    }
    None
}

/// Waits for anything new on the console. ratatui writes only the cells that changed, so the new screen's words do not arrive in a
/// row and cannot be searched for: the screens' text is what `ui-snapshot` and the parity checks verify.
fn wait_for_output(out: &Arc<Mutex<Vec<u8>>>, from: usize, within: Duration) -> Option<Duration> {
    let t0 = Instant::now();
    while t0.elapsed() < within {
        if out.lock().unwrap().len() > from {
            return Some(t0.elapsed());
        }
        std::thread::sleep(Duration::from_millis(2));
    }
    None
}

#[test]
#[ignore]
fn the_real_window_draws_takes_keys_stays_quiet_and_quits() {
    let dir = tempfile::tempdir().unwrap();
    let (data, home) = (dir.path().join("data"), dir.path().join("home"));
    std::fs::create_dir_all(&data).unwrap();
    std::fs::create_dir_all(&home).unwrap();
    // a machine with the Slack bridge up: its heartbeat names a live pid (this test's), which the window checks
    let beat = serde_json::json!({"ts": chrono::Utc::now().to_rfc3339(), "pid": std::process::id(), "poll_s": 5});
    std::fs::write(data.join("slack_bridge.state"), beat.to_string()).unwrap();
    let envs = [
        ("AGENTDESK_DATA", data.to_string_lossy().to_string()),
        ("HOME", home.to_string_lossy().to_string()),
        ("USERPROFILE", home.to_string_lossy().to_string()),
        ("AGENTDESK_VAULT", dir.path().join("vault").to_string_lossy().to_string()),
        ("AGENTDESK_NO_TOAST", "1".into()),
        ("AGENTDESK_NO_UPDATE_CHECK", "1".into()),
        ("AGENTDESK_NO_BACKUP", "1".into()),
        ("AGENTDESK_NO_MODEL_DOWNLOAD", "1".into()),
    ];
    // the core, on the scratch board, so the window finds it and does not start one of its own
    let mut core = std::process::Command::new(exe());
    core.arg("daemon").stdin(std::process::Stdio::null()).stdout(std::process::Stdio::null()).stderr(std::process::Stdio::null());
    for (k, v) in &envs {
        core.env(k, v);
    }
    let core = Reap(vec![core.spawn().expect("start the core")]);
    std::thread::sleep(Duration::from_secs(3));

    let pty = native_pty_system().openpty(PtySize { rows: 30, cols: 100, pixel_width: 0, pixel_height: 0 }).unwrap();
    let mut cmd = CommandBuilder::new(exe());
    for (k, v) in &envs {
        cmd.env(k, v);
    }
    let child = ReapPty(pty.slave.spawn_command(cmd).unwrap());
    let mut child = child;
    drop(pty.slave);
    let out = Arc::new(Mutex::new(Vec::<u8>::new()));
    let reads = Arc::new(Mutex::new((0usize, Instant::now())));
    let writer: Arc<Mutex<Box<dyn Write + Send>>> = Arc::new(Mutex::new(pty.master.take_writer().unwrap()));
    {
        let (out, reads, writer) = (out.clone(), reads.clone(), writer.clone());
        let mut r = pty.master.try_clone_reader().unwrap();
        std::thread::spawn(move || {
            let mut b = [0u8; 65536];
            while let Ok(n) = r.read(&mut b) {
                if n == 0 {
                    break;
                }
                // ConPTY asks where the cursor is (ESC[6n) and sends nothing more until it is told
                if b[..n].windows(4).any(|w| w == b"\x1b[6n") {
                    let _ = writer.lock().unwrap().write_all(b"\x1b[1;1R");
                }
                out.lock().unwrap().extend_from_slice(&b[..n]);
                {
                    let mut g = reads.lock().unwrap();
                    g.0 += 1;
                    g.1 = Instant::now();
                }
            }
        });
    }
    struct Keys(Arc<Mutex<Box<dyn Write + Send>>>);
    impl Write for Keys {
        fn write(&mut self, b: &[u8]) -> std::io::Result<usize> {
            self.0.lock().unwrap().write(b)
        }
        fn flush(&mut self) -> std::io::Result<()> {
            self.0.lock().unwrap().flush()
        }
    }
    let mut w = Keys(writer);

    let first = wait_for(&out, "Main menu", 0, Duration::from_secs(20)).unwrap_or_else(|| {
        let got = out.lock().unwrap().clone();
        panic!("the main menu never appeared; {} bytes arrived: {:?}", got.len(), String::from_utf8_lossy(&got[..got.len().min(600)]));
    });
    println!("first frame after {first:?}");
    // ConPTY answers its own start-up queries; give the screen a moment to settle, then watch it stay quiet
    std::thread::sleep(Duration::from_millis(800));
    let settled = out.lock().unwrap().len();
    std::thread::sleep(Duration::from_millis(2500));
    let quiet = out.lock().unwrap().len() - settled;
    println!("bytes written by a screen nobody touched, in 2.5 s: {quiet}");
    assert!(quiet < 600, "an idle window wrote {quiet} bytes in 2.5 s");

    // a key reaches the screen
    let mark = out.lock().unwrap().len();
    let t0 = Instant::now();
    w.write_all(b"q").unwrap();
    w.flush().unwrap();
    let took = wait_for_output(&out, mark, Duration::from_secs(5)).expect("the screen never changed after a key");
    println!("q: the screen changed after: {took:?} (from the write: {:?})", t0.elapsed());
    assert!(took < Duration::from_millis(1500), "a key took {took:?} to show");

    // a burst of keys, then one that changes the screen: how long until the window gets to it (a window that draws slowly and takes
    // one event per frame works through the burst first)
    // (each of these keys changes the screen: Questions, Discussion, Wiki, Work, and finally the main menu)
    let t0 = Instant::now();
    w.write_all(b"qdwjqdwjqdwjm").unwrap();
    w.flush().unwrap();
    // the burst is done when the console has been quiet for a while; its last byte is when the window finished
    while reads.lock().unwrap().1 < t0 + Duration::from_millis(1) || reads.lock().unwrap().1.elapsed() < Duration::from_millis(1500) {
        std::thread::sleep(Duration::from_millis(50));
        assert!(t0.elapsed() < Duration::from_secs(120), "the window never settled");
    }
    let settled_at = reads.lock().unwrap().1.duration_since(t0);
    println!("thirteen keys sent at once: the last frame reached the console after {settled_at:?}");
    assert!(settled_at < Duration::from_secs(2), "the window took {settled_at:?} to get through thirteen keys");

    // hang up: the window ends by itself
    w.write_all(b"g").unwrap();
    let t0 = Instant::now();
    let mut code = None;
    while t0.elapsed() < Duration::from_secs(10) && code.is_none() {
        code = child.0.try_wait().unwrap();
        std::thread::sleep(Duration::from_millis(20));
    }
    println!("frames came in {} reads, {} bytes in all", reads.lock().unwrap().0, out.lock().unwrap().len());
    drop(core);
    assert!(code.is_some(), "the window did not quit after G");
}

/// Kills what the test started, however it ends.
struct Reap(Vec<std::process::Child>);
impl Drop for Reap {
    fn drop(&mut self) {
        for c in &mut self.0 {
            let _ = c.kill();
            let _ = c.wait();
        }
    }
}

struct ReapPty(Box<dyn portable_pty::Child + Send + Sync>);
impl Drop for ReapPty {
    fn drop(&mut self) {
        let _ = self.0.kill();
    }
}
