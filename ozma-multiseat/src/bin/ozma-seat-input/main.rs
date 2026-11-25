// SPDX-License-Identifier: AGPL-3.0-only
//! `ozma-seat-input` — privileged per-seat evdev grab + uinput helper.
//!
//! The supervisor (`ozma-multiseat`) starts this binary as a child and speaks
//! line-delimited JSON over stdin/stdout. The helper is the only component that
//! opens `/dev/input/event*`, calls `EVIOCGRAB`, or creates `/dev/uinput`
//! devices. It validates every path, refuses duplicate and cross-seat grabs,
//! and releases everything on `SIGTERM`/`SIGINT`, EOF, `Shutdown`, or exit.
//!
//! The protocol and state machine live in `ozma_multiseat::input_isolation`;
//! this binary only supplies the real (or explicitly requested fake) backend.

#[cfg(unix)]
mod ioctl;
#[cfg(unix)]
mod real;

use std::io::{self, BufRead, Write};
use std::sync::{Arc, Mutex};

use clap::Parser;
use ozma_multiseat::{
    decode_request, encode_response, FakeHelperBackend, HelperBackend, HelperBackendKind,
    HelperDevice, HelperError, HelperRequest, HelperResponse, InputIsolationHelper, HELPER_PROGRAM,
};

#[cfg(unix)]
use crate::real::RealBackend;

#[derive(Parser, Debug)]
#[command(
    name = "ozma-seat-input",
    about = "Privileged per-seat evdev grab and uinput helper for ozma-multiseat"
)]
struct Args {
    /// Backend: `real` opens evdev/uinput; `fake` is test-only and touches no
    /// device (used by the protocol tests).
    #[arg(long, default_value = "real", value_name = "real|fake")]
    backend: String,
}

enum Backend {
    #[cfg(unix)]
    Real(RealBackend),
    Fake(FakeHelperBackend),
}

impl HelperBackend for Backend {
    fn backend_kind(&self) -> HelperBackendKind {
        match self {
            #[cfg(unix)]
            Self::Real(backend) => backend.backend_kind(),
            Self::Fake(backend) => backend.backend_kind(),
        }
    }

    fn grab(&mut self, device: &HelperDevice) -> Result<(), HelperError> {
        match self {
            #[cfg(unix)]
            Self::Real(backend) => backend.grab(device),
            Self::Fake(backend) => backend.grab(device),
        }
    }

    fn ungrab(&mut self, path: &str) -> Result<(), HelperError> {
        match self {
            #[cfg(unix)]
            Self::Real(backend) => backend.ungrab(path),
            Self::Fake(backend) => backend.ungrab(path),
        }
    }

    fn create_uinput(
        &mut self,
        seat_id: &str,
        device: &HelperDevice,
    ) -> Result<String, HelperError> {
        match self {
            #[cfg(unix)]
            Self::Real(backend) => backend.create_uinput(seat_id, device),
            Self::Fake(backend) => backend.create_uinput(seat_id, device),
        }
    }

    fn start_relay(&mut self, device: &HelperDevice, uinput_name: &str) -> Result<(), HelperError> {
        match self {
            #[cfg(unix)]
            Self::Real(backend) => backend.start_relay(device, uinput_name),
            Self::Fake(backend) => backend.start_relay(device, uinput_name),
        }
    }

    fn stop_relay(&mut self, path: &str) -> Result<(), HelperError> {
        match self {
            #[cfg(unix)]
            Self::Real(backend) => backend.stop_relay(path),
            Self::Fake(backend) => backend.stop_relay(path),
        }
    }

    fn destroy_uinput(&mut self, name: &str) -> Result<(), HelperError> {
        match self {
            #[cfg(unix)]
            Self::Real(backend) => backend.destroy_uinput(name),
            Self::Fake(backend) => backend.destroy_uinput(name),
        }
    }
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args = Args::parse();
    let backend = match args.backend.as_str() {
        "real" => {
            #[cfg(unix)]
            {
                Backend::Real(RealBackend::new())
            }
            #[cfg(not(unix))]
            {
                return Err("the real backend is Linux-only; use --backend fake".into());
            }
        }
        "fake" => Backend::Fake(FakeHelperBackend::new()),
        other => {
            return Err(format!("unknown backend {other:?}; expected real or fake").into());
        }
    };
    eprintln!(
        "{HELPER_PROGRAM}: starting (backend={})",
        backend.backend_kind().as_str()
    );

    let state = Arc::new(Mutex::new(InputIsolationHelper::new(
        backend,
        HELPER_PROGRAM,
        std::process::id(),
    )));
    install_signal_handler(Arc::clone(&state))?;

    let stdin = io::stdin();
    let mut stdout = io::stdout();
    let mut hello_seen = false;
    let mut lines = stdin.lock().lines();
    while let Some(line) = lines.next() {
        let line = line?;
        if line.trim().is_empty() {
            continue;
        }
        let response = match decode_request(&line) {
            Ok(request) => {
                let hello = matches!(request, HelperRequest::Hello { .. });
                if !hello_seen && !hello {
                    error_response("protocol", "first request must be hello")
                } else {
                    hello_seen |= hello;
                    handle(&state, request)
                }
            }
            Err(error) => error_response("protocol", &error.to_string()),
        };
        let shutting_down = matches!(response, HelperResponse::ShuttingDown);
        writeln!(stdout, "{}", encode_response(&response)?)?;
        stdout.flush()?;
        if shutting_down {
            break;
        }
    }

    // EOF or a clean shutdown: release every seat before the process exits.
    let _ = state
        .lock()
        .expect("helper state lock")
        .handle(HelperRequest::Shutdown);
    Ok(())
}

fn handle(
    state: &Arc<Mutex<InputIsolationHelper<Backend>>>,
    request: HelperRequest,
) -> HelperResponse {
    state.lock().expect("helper state lock").handle(request)
}

fn error_response(code: &str, message: &str) -> HelperResponse {
    HelperResponse::Error {
        error: HelperError::new(
            match code {
                "protocol" => ozma_multiseat::HelperErrorCode::Protocol,
                "invalid_request" => ozma_multiseat::HelperErrorCode::InvalidRequest,
                other => {
                    // Unknown internal classes stay actionable rather than
                    // silently becoming a success.
                    eprintln!("{HELPER_PROGRAM}: internal error class {other:?}");
                    ozma_multiseat::HelperErrorCode::Backend
                }
            },
            message,
        ),
    }
}

#[cfg(unix)]
fn install_signal_handler(
    state: Arc<Mutex<InputIsolationHelper<Backend>>>,
) -> Result<(), Box<dyn std::error::Error>> {
    use nix::sys::signal::{SigSet, Signal};

    let mut set = SigSet::empty();
    set.add(Signal::SIGTERM);
    set.add(Signal::SIGINT);
    // Block in the main thread first; the signal thread inherits the mask and
    // then consumes the signal with a safe sigwait. No unsafe handler code.
    set.thread_block()?;
    std::thread::Builder::new()
        .name("ozma-seat-input-signal".to_owned())
        .spawn(move || loop {
            if set.wait().is_err() {
                continue;
            }
            if let Ok(mut helper) = state.lock() {
                let _ = helper.handle(HelperRequest::Shutdown);
            }
            std::process::exit(0);
        })?;
    Ok(())
}

#[cfg(not(unix))]
fn install_signal_handler(
    _state: Arc<Mutex<InputIsolationHelper<Backend>>>,
) -> Result<(), Box<dyn std::error::Error>> {
    Ok(())
}
