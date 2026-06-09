//! `cmesh` — a thin PTY wrapper around the real `claude` that lets the wrapped
//! window answer cross-window asks "for real" (mode = live). It is transparent in
//! normal use; when the daemon forwards a question over this session's control
//! socket, cmesh injects it into the running session and reads the reply back
//! from the transcript. Anything that goes wrong falls back to pull on the daemon
//! side, so live is strictly an upgrade over pull.
//!
//! Unix only (pty + termios + unix socket). Why capture from the transcript
//! rather than scraping the TUI: the transcript is clean text and is written for
//! every turn, so we never have to parse cursor/ANSI output.

#[cfg(not(unix))]
pub fn run(_args: Vec<String>) -> anyhow::Result<()> {
    anyhow::bail!("cmesh (live mode) is only supported on Unix")
}

#[cfg(unix)]
pub use imp::run;

#[cfg(unix)]
mod imp {
    use crate::config;
    use crate::protocol::SessionFile;
    use crate::transcript;
    use portable_pty::{native_pty_system, CommandBuilder, PtySize};
    use std::io::{BufRead, BufReader, Read, Write};
    use std::os::unix::net::{UnixListener, UnixStream};
    use std::path::{Path, PathBuf};
    use std::sync::{Arc, Mutex};
    use std::time::{Duration, Instant};

    type SharedWriter = Arc<Mutex<Box<dyn Write + Send>>>;

    /// How long a live capture waits for the window's reply (kept under typical
    /// MCP client timeouts).
    const CAPTURE_TIMEOUT: Duration = Duration::from_secs(50);
    /// The turn is considered done once the transcript stops growing for this long
    /// after an assistant message has appeared.
    const QUIESCENT: Duration = Duration::from_millis(1500);

    pub fn run(claude_args: Vec<String>) -> anyhow::Result<()> {
        let live_dir = config::base_dir().join("live");
        std::fs::create_dir_all(&live_dir).ok();
        let sock_path = live_dir.join(format!("{}.sock", std::process::id()));
        let _ = std::fs::remove_file(&sock_path);
        let listener = UnixListener::bind(&sock_path)?;

        let size = term_size().unwrap_or(PtySize {
            rows: 24,
            cols: 80,
            pixel_width: 0,
            pixel_height: 0,
        });
        let pair = native_pty_system().openpty(size)?;

        // Spawn the real claude inside the pty, telling it (via env, which the hook
        // records into the session file) that it is live and where to be reached.
        let claude = std::env::var("CLAUDE_MESH_CLAUDE_BIN").unwrap_or_else(|_| "claude".into());
        let mut cmd = CommandBuilder::new(&claude);
        for a in &claude_args {
            cmd.arg(a);
        }
        cmd.env("CLAUDE_MESH_MODE", "live");
        cmd.env("CLAUDE_MESH_CTL", sock_path.to_string_lossy().to_string());
        if let Ok(cwd) = std::env::current_dir() {
            cmd.cwd(cwd);
        }
        let mut child = pair.slave.spawn_command(cmd)?;
        drop(pair.slave);

        let mut reader = pair.master.try_clone_reader()?;
        let writer: SharedWriter = Arc::new(Mutex::new(pair.master.take_writer()?));
        let master = pair.master;
        let restore = enable_raw_mode();

        // pty -> our stdout
        std::thread::spawn(move || {
            let mut stdout = std::io::stdout();
            let mut buf = [0u8; 8192];
            while let Ok(n) = reader.read(&mut buf) {
                if n == 0 || stdout.write_all(&buf[..n]).is_err() {
                    break;
                }
                let _ = stdout.flush();
            }
        });

        // our stdin -> pty
        {
            let writer = writer.clone();
            std::thread::spawn(move || {
                let mut stdin = std::io::stdin();
                let mut buf = [0u8; 4096];
                while let Ok(n) = stdin.read(&mut buf) {
                    if n == 0 {
                        break;
                    }
                    let mut w = writer.lock().unwrap();
                    if w.write_all(&buf[..n]).is_err() {
                        break;
                    }
                    let _ = w.flush();
                }
            });
        }

        // control socket: the daemon connects here to ask a live question
        {
            let writer = writer.clone();
            let sp = sock_path.clone();
            std::thread::spawn(move || serve_control(listener, writer, sp));
        }

        // foreground: keep the pty sized to the terminal, wait for claude to exit
        let mut last = (size.rows, size.cols);
        let code = loop {
            if let Ok(Some(status)) = child.try_wait() {
                break status.exit_code() as i32;
            }
            if let Some(ns) = term_size() {
                if (ns.rows, ns.cols) != last {
                    let _ = master.resize(ns);
                    last = (ns.rows, ns.cols);
                }
            }
            std::thread::sleep(Duration::from_millis(200));
        };

        if let Some(orig) = restore {
            restore_raw_mode(&orig);
        }
        let _ = std::fs::remove_file(&sock_path);
        std::process::exit(code);
    }

    fn serve_control(listener: UnixListener, writer: SharedWriter, sock_path: PathBuf) {
        for stream in listener.incoming() {
            let Ok(stream) = stream else { continue };
            let writer = writer.clone();
            let sock_path = sock_path.clone();
            std::thread::spawn(move || {
                let _ = handle_request(stream, &writer, &sock_path);
            });
        }
    }

    fn handle_request(
        mut stream: UnixStream,
        writer: &SharedWriter,
        sock_path: &Path,
    ) -> std::io::Result<()> {
        let mut line = String::new();
        BufReader::new(stream.try_clone()?).read_line(&mut line)?;
        let question = serde_json::from_str::<serde_json::Value>(line.trim())
            .ok()
            .and_then(|v| v.get("question").and_then(|q| q.as_str()).map(String::from))
            .unwrap_or_default();

        let resp = match ask_live(writer, sock_path, &question) {
            Some(answer) => serde_json::json!({ "answer": answer }),
            None => serde_json::json!({ "fallback": true }),
        };
        stream.write_all(resp.to_string().as_bytes())?;
        stream.write_all(b"\n")?;
        stream.flush()
    }

    /// Inject the question into the running session and capture the reply from the
    /// transcript. None => the daemon should fall back to pull.
    fn ask_live(writer: &SharedWriter, sock_path: &Path, question: &str) -> Option<String> {
        if question.is_empty() {
            return None;
        }
        let sf = find_my_session(sock_path)?;
        let tp = sf.transcript_path;
        if tp.is_empty() || !Path::new(&tp).exists() {
            return None;
        }
        // Don't barge into an active turn (best-effort guard against interrupting).
        if changed_within(&tp, Duration::from_secs(2)) {
            return None;
        }
        let before = file_len(&tp);
        {
            let mut w = writer.lock().ok()?;
            w.write_all(question.as_bytes()).ok()?;
            w.write_all(b"\r").ok()?; // carriage return == submit in the TUI
            w.flush().ok()?;
        }
        capture(&tp, before)
    }

    fn capture(path: &str, before: u64) -> Option<String> {
        let deadline = Instant::now() + CAPTURE_TIMEOUT;
        let mut last_len = before;
        let mut last_change = Instant::now();
        loop {
            std::thread::sleep(Duration::from_millis(300));
            let len = file_len(path);
            if len != last_len {
                last_len = len;
                last_change = Instant::now();
            }
            let ass = assistant_text(&read_from(path, before));
            if !ass.is_empty() && last_change.elapsed() > QUIESCENT {
                return Some(transcript::truncate(&ass, config::max_chars()));
            }
            if Instant::now() >= deadline {
                return (!ass.is_empty()).then(|| transcript::truncate(&ass, config::max_chars()));
            }
        }
    }

    /// Assistant text from the lines appended since injection.
    fn assistant_text(appended: &str) -> String {
        let mut parts = Vec::new();
        for line in appended.lines() {
            let Ok(v) = serde_json::from_str::<serde_json::Value>(line) else {
                continue;
            };
            if v.get("type").and_then(|x| x.as_str()) == Some("assistant") {
                let t = transcript::extract_text(v.get("message"));
                let t = t.trim();
                if !t.is_empty() {
                    parts.push(t.to_string());
                }
            }
        }
        parts.join("\n\n")
    }

    fn find_my_session(sock_path: &Path) -> Option<SessionFile> {
        let want = sock_path.to_string_lossy();
        for entry in std::fs::read_dir(config::sessions_dir()).ok()?.flatten() {
            let Ok(raw) = std::fs::read_to_string(entry.path()) else {
                continue;
            };
            let Ok(sf) = serde_json::from_str::<SessionFile>(&raw) else {
                continue;
            };
            if sf.ctl == want {
                return Some(sf);
            }
        }
        None
    }

    fn file_len(path: &str) -> u64 {
        std::fs::metadata(path).map(|m| m.len()).unwrap_or(0)
    }

    fn changed_within(path: &str, dur: Duration) -> bool {
        std::fs::metadata(path)
            .and_then(|m| m.modified())
            .ok()
            .and_then(|t| t.elapsed().ok())
            .map(|e| e < dur)
            .unwrap_or(false)
    }

    fn read_from(path: &str, offset: u64) -> String {
        use std::io::{Seek, SeekFrom};
        let Ok(mut f) = std::fs::File::open(path) else {
            return String::new();
        };
        if f.seek(SeekFrom::Start(offset)).is_err() {
            return String::new();
        }
        let mut s = String::new();
        let _ = f.read_to_string(&mut s);
        s
    }

    fn enable_raw_mode() -> Option<libc::termios> {
        use std::os::unix::io::AsRawFd;
        let fd = std::io::stdin().as_raw_fd();
        unsafe {
            let mut t: libc::termios = std::mem::zeroed();
            if libc::tcgetattr(fd, &mut t) != 0 {
                return None;
            }
            let orig = t;
            libc::cfmakeraw(&mut t);
            if libc::tcsetattr(fd, libc::TCSANOW, &t) != 0 {
                return None;
            }
            Some(orig)
        }
    }

    fn restore_raw_mode(orig: &libc::termios) {
        use std::os::unix::io::AsRawFd;
        let fd = std::io::stdin().as_raw_fd();
        unsafe {
            libc::tcsetattr(fd, libc::TCSANOW, orig);
        }
    }

    fn term_size() -> Option<PtySize> {
        use std::os::unix::io::AsRawFd;
        unsafe {
            let mut ws: libc::winsize = std::mem::zeroed();
            if libc::ioctl(std::io::stdout().as_raw_fd(), libc::TIOCGWINSZ, &mut ws) == 0
                && ws.ws_row > 0
            {
                Some(PtySize {
                    rows: ws.ws_row,
                    cols: ws.ws_col,
                    pixel_width: ws.ws_xpixel,
                    pixel_height: ws.ws_ypixel,
                })
            } else {
                None
            }
        }
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        // capture() must return the assistant reply appended after injection, once
        // the transcript goes quiescent.
        #[test]
        fn capture_reads_appended_reply() {
            let dir = std::env::temp_dir().join("claude-mesh-cmesh-tests");
            std::fs::create_dir_all(&dir).unwrap();
            let p = dir.join("t.jsonl");
            std::fs::write(&p, "").unwrap();
            let path = p.to_string_lossy().to_string();
            let before = file_len(&path);

            let p2 = path.clone();
            std::thread::spawn(move || {
                std::thread::sleep(Duration::from_millis(300));
                let mut f = std::fs::OpenOptions::new().append(true).open(&p2).unwrap();
                let line = r#"{"type":"assistant","message":{"role":"assistant","content":"LIVE_REPLY ok"}}"#;
                writeln!(f, "{line}").unwrap();
            });

            let ans = capture(&path, before).expect("captures the appended reply");
            assert!(ans.contains("LIVE_REPLY"));
        }
    }
}
