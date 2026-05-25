//! Integration tests that drive the real `tidocs` binary through a virtual terminal.
//!
//! Uses `portable-pty` to allocate a pseudo-terminal, writes keystrokes to it,
//! and reads back the rendered ANSI output after each interaction step.

use portable_pty::{CommandBuilder, NativePtySystem, PtySize, PtySystem};
use regex::Regex;
use std::io::{Read, Write};
use std::sync::{OnceLock, mpsc};
use std::time::Duration;

/// Return the path to the compiled `tidocs` binary.
fn tidocs_bin() -> std::path::PathBuf {
    assert_cmd::Command::cargo_bin("tidocs")
        .unwrap()
        .get_program()
        .into()
}
fn strip_styles(input: &str) -> String {
    static ANSI_REGEX: OnceLock<Regex> = OnceLock::new();
    let re = ANSI_REGEX.get_or_init(|| {
        // Matches standard CSI sequences (like colors/styles) but ignores raw control bytes
        Regex::new(r"[\x1b\x9b]\[[0-?]*[ -/]*[@-~]").unwrap()
    });

    re.replace_all(input, "").into_owned()
}
/// Continuously read from the PTY in a background thread and collect all output.
struct OutputCollector {
    rx: mpsc::Receiver<Vec<u8>>,
    handle: Option<std::thread::JoinHandle<()>>,
}

impl OutputCollector {
    fn new(mut reader: Box<dyn Read + Send>) -> Self {
        let (tx, rx) = mpsc::channel();
        let handle = std::thread::spawn(move || {
            let mut buf = [0u8; 4096];
            loop {
                match reader.read(&mut buf) {
                    Ok(0) => break, // EOF
                    Ok(n) => {
                        if tx.send(buf[..n].to_vec()).is_err() {
                            break;
                        }
                    }
                    Err(_) => break,
                }
            }
        });
        Self {
            rx,
            handle: Some(handle),
        }
    }

    /// Drain all accumulated output so far and return it as a string.
    fn drain(&self) -> String {
        let mut chunks = Vec::new();
        while let Ok(chunk) = self.rx.try_recv() {
            chunks.extend(chunk);
        }
        String::from_utf8_lossy(&chunks).to_string()
    }

    /// Wait up to `timeout` for any output, then drain everything.
    fn wait_and_drain(&self, timeout: Duration) -> String {
        loop {
            match self.rx.recv_timeout(timeout) {
                Ok(chunk) => {
                    // Got one chunk; now drain any remaining pending ones.
                    let mut all = chunk;
                    while let Ok(extra) = self.rx.try_recv() {
                        all.extend(extra);
                    }
                    return String::from_utf8_lossy(&all).to_string();
                }
                Err(mpsc::RecvTimeoutError::Timeout) => {
                    // No output within timeout; drain whatever is left.
                    return self.drain();
                }
                Err(mpsc::RecvTimeoutError::Disconnected) => return self.drain(),
            }
        }
    }
}

impl Drop for OutputCollector {
    fn drop(&mut self) {
        if let Some(handle) = self.handle.take() {
            let _ = handle.join();
        }
    }
}

/// Search for `add`, press Enter, and verify that "fn add" appears on screen.
#[test]
fn search_span_add_shows_fn_add() {
    let pty_system = NativePtySystem::default();

    let pair = pty_system
        .openpty(PtySize {
            rows: 60,
            cols: 80,
            pixel_width: 0,
            pixel_height: 0,
        })
        .expect("failed to open pty");

    // Spawn tidocs connected to the slave side of the PTY.
    let mut cmd_builder = CommandBuilder::new(tidocs_bin());
    cmd_builder.arg("./target/doc");
    let child = pair
        .slave
        .spawn_command(cmd_builder)
        .expect("failed to spawn tidocs");

    // Start collecting output from the master side.
    let reader = pair
        .master
        .try_clone_reader()
        .expect("failed to clone reader");
    let collector = OutputCollector::new(reader);

    let mut writer = pair
        .master
        .take_writer()
        .expect("failed to take master writer");

    let _master = pair.master;
    // Give the app a moment to start rendering the initial frame.
    std::thread::sleep(Duration::from_millis(300));
    // Discard initial render output.
    let _initial = collector.wait_and_drain(Duration::from_millis(100));

    // Type the search query character by character into the master side of the PTY.
    let query = "::add";
    for byte in query.bytes() {
        writer.write_all(&[byte]).expect("failed to write byte");
    }
    writer.flush().expect("failed to flush");

    // Wait for the app to process keystrokes and re-render the search results.
    std::thread::sleep(Duration::from_millis(1800));
    let _after_query = collector.wait_and_drain(Duration::from_millis(100));
    writer.write_all(b"\x1b[B").expect("failed to write byte");
    writer.flush().expect("failed to flush");
    std::thread::sleep(Duration::from_millis(1800));

    // Press Enter to open the detail view for the first match.
    writer.write_all(b"\r").expect("failed to write enter");
    writer.flush().expect("failed to flush");

    // Wait for the app to switch to Detail mode and render the document.
    std::thread::sleep(Duration::from_millis(800));

    // Collect all rendered output after pressing Enter.
    let final_output = collector.wait_and_drain(Duration::from_millis(500));

    // Clean up: terminate the child process.
    // (Drop collector first to stop the read thread before killing the child.)
    let mut child = child;
    child.kill().expect("failed to kill child");
    //child.wait().expect("failed to wait for child");
    drop(collector);
    drop(writer);

    let stripped = strip_styles(&final_output);
    //print!("{}", stripped);
    // Assert that "fn add" is present in the rendered output.
    assert!(
        stripped.contains("fn add"),
        "Expected 'fn add' in terminal output, but got:\n{}",
        stripped,
    );
}
