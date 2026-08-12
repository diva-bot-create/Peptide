//! session — shared plumbing for the CLI session loops (`peptide session` /
//! `peptide ssf2 session`).
//!
//! The two engines drive commands over different transports (Fraymakers = an async
//! full-duplex socket with a reader thread; SSF2 = synchronous request/response RPC),
//! so their boot/ready halves stay engine-specific — that split is honest, not
//! accidental. What they DO share is the front end of the loop: tailing the session
//! control file and dispatching each newly-appended line. That boilerplate lived twice;
//! it lives here once now.
//!
//! The GUI does not use this — it's event-loop driven over webview IPC, not a control
//! file — which is exactly why the shared surface is the control-file tail and not a
//! grand unified "session" object.

use std::io::{BufReader, Read, Seek, SeekFrom};
use std::net::TcpStream;
use std::path::Path;
use std::time::Duration;

/// Classification sink for the Fraymakers engine line stream. The byte-framing and the
/// line classification (RESDIAG breadcrumb / channel feed / ANIM telemetry / READY /
/// normal line) are shared by the CLI session loop and the GUI reader — they differ ONLY
/// in where each class is routed, which is what a sink implements. Default methods are
/// no-ops, so each sink overrides just the classes it cares about (e.g. the GUI drops ANIM
/// from the chat; the CLI drops channel feeds from the log).
pub trait FrayStreamSink {
    /// The engine reported READY (boot complete / match-ready).
    fn on_ready(&mut self) {}
    /// A `RESDIAG:` breadcrumb (the failing resource id) — for the enhanced crash log.
    fn on_resdiag(&mut self, _line: &str) {}
    /// A channel feed (`matchStatus`, `charIcon`, …) → its widget, not the transcript.
    fn on_channel(&mut self, _channel: &str, _payload: &str) {}
    /// Per-transition `ANIM:<state>` telemetry (fires on change).
    fn on_anim(&mut self, _state: &str) {}
    /// A normal engine line (includes the READY line itself, after `on_ready`).
    fn on_line(&mut self, _raw: &str) {}
    /// The socket closed (engine gone) or a read error ended the stream.
    fn on_eof(&mut self) {}
}

/// Read the Fraymakers engine socket byte-by-byte, frame `\n`-terminated lines, classify
/// each, and dispatch to `sink`. The ONE place FM stream framing + classification lives —
/// the CLI session and the GUI both drive it, differing only in their sink. Returns when
/// the stream ends (EOF / error), after calling `sink.on_eof()`.
pub fn pump_fray_stream(mut reader: BufReader<TcpStream>, sink: &mut impl FrayStreamSink) {
    let mut buf: Vec<u8> = Vec::with_capacity(256);
    let mut one = [0u8; 1];
    loop {
        match reader.read(&mut one) {
            Ok(0) => { sink.on_eof(); break; } // clean EOF
            Ok(_) => {
                if one[0] == b'\n' {
                    let line = String::from_utf8_lossy(&buf).trim_end_matches('\r').to_string();
                    buf.clear();
                    if line.contains("RESDIAG:") {
                        sink.on_resdiag(&line);
                    } else if let Some((ch, payload)) = crate::interpreter::channel_payload(&line) {
                        sink.on_channel(ch, payload);
                    } else if let Some(state) = line.strip_prefix("FRAME:") {
                        // Per-frame timing telemetry. Routed OUT of the normal line path
                        // (60/sec would swamp the log) into the frame buffer `trace` reads,
                        // mirroring how the SSF2 reader routes its own FRAME: pushes.
                        crate::debug_target::push_frame(state);
                    } else if let Some(state) = line.strip_prefix("ANIM:") {
                        sink.on_anim(state);
                    } else {
                        if line.contains("READY") { sink.on_ready(); }
                        sink.on_line(&line);
                    }
                } else {
                    buf.push(one[0]);
                }
            }
            Err(ref e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(_) => { sink.on_eof(); break; } // transient/decode error: stop mirroring
        }
    }
}

/// Tail `control`, dispatching each newly-appended, non-empty line to `on_line`.
///
/// - `tick` is polled once per cycle BEFORE reading; return `true` to stop the loop
///   (e.g. the engine stream ended). Use it for periodic liveness checks.
/// - `on_line` handles one command line; return `false` to stop the loop after it
///   (e.g. an `exit`/`quit` command). Lines are already trimmed and non-empty.
///
/// Blocks until either callback asks to stop. Tracks the read offset so only content
/// appended after the previous read is dispatched (stale queued commands aren't replayed
/// — the caller truncates the file at startup).
pub fn tail_control(
    control: &Path,
    poll: Duration,
    mut tick: impl FnMut() -> bool,
    mut on_line: impl FnMut(&str) -> bool,
) {
    let mut offset: u64 = 0;
    let mut leftover = String::new();
    loop {
        if tick() {
            break;
        }
        if let Ok(mut f) = std::fs::File::open(control) {
            if f.seek(SeekFrom::Start(offset)).is_ok() {
                let mut chunk = String::new();
                if let Ok(n) = f.read_to_string(&mut chunk) {
                    if n > 0 {
                        offset += n as u64;
                        leftover.push_str(&chunk);
                        let mut stop = false;
                        while let Some(nl) = leftover.find('\n') {
                            let raw: String = leftover.drain(..=nl).collect();
                            let raw = raw.trim();
                            if raw.is_empty() {
                                continue;
                            }
                            if !on_line(raw) {
                                stop = true;
                                break;
                            }
                        }
                        if stop {
                            break;
                        }
                    }
                }
            }
        }
        std::thread::sleep(poll);
    }
}
