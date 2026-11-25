// SPDX-License-Identifier: AGPL-3.0-only
//!
//! Opt-in live smoke for two real Xorg seats with dummy video and libinput.
//!
//! Nothing runs unless `OZMA_MULTISEAT_REAL_XORG=1`. The test:
//!
//! 1. creates throwaway uinput keyboard/mouse sources in this process;
//! 2. starts the real `ozma-seat-input --backend real` helper and grabs those
//!    sources, producing four `ozma-seat-...` uinput devices;
//! 3. starts two real `/usr/lib/Xorg` servers on `:90` and `:91` with the
//!    `dummy` video driver, the libinput input driver, per-display
//!    `Xauthority` cookies, `-nolisten tcp`, `-novtswitch`, and
//!    `-sharevts`;
//! 4. uses the real `ProcessSeatInputTargeter` (`xinput`) to leave only seat
//!    A's devices enabled on `:90` and only seat B's on `:91`;
//! 5. injects a key through each source and proves `xev -root` sees seat A's
//!    key only on `:90` and seat B's key only on `:91`.
//!
//! It never opens a physical evdev node, never touches the operator's X
//! session, and never configures the host GPU. `sudo -n` is used only to run
//! the throwaway Xorg instances; the test fails early if passwordless sudo or
//! the dummy driver is unavailable.
//!
//! ```bash
//! OZMA_MULTISEAT_REAL_XORG=1 \
//!   cargo test -p ozma-multiseat --test live_xorg_seat_smoke \
//!   -- --ignored --nocapture
//! ```

#![cfg(unix)]

use std::fs::{self, File};
use std::io::{BufRead, BufReader, Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, ChildStdout, Command, Stdio};
use std::time::{Duration, Instant};

use evdev::uinput::{VirtualDevice, VirtualDeviceBuilder};
use evdev::{AttributeSet, BusType, EventType, InputEvent, InputId, Key, RelativeAxisType};
use ozma_multiseat::{
    decode_response, encode_request, parse_xinput_list, probe_x11_display, HelperDevice,
    HelperRequest, HelperResponse, InputDeviceKind, ProcessSeatInputTargeter,
    SeatInputTargetRequest, SeatInputTargeter,
};

const DISPLAY_A: u32 = 90;
const DISPLAY_B: u32 = 91;

fn enabled() -> bool {
    matches!(
        std::env::var("OZMA_MULTISEAT_REAL_XORG").as_deref(),
        Ok("1") | Ok("true") | Ok("yes")
    )
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

fn cookie_bytes() -> Vec<u8> {
    let mut bytes = [0u8; 16];
    File::open("/dev/urandom")
        .and_then(|mut file| file.read_exact(&mut bytes))
        .expect("read /dev/urandom");
    bytes.to_vec()
}

fn find_event(name: &str) -> PathBuf {
    for _ in 0..200 {
        if let Ok(entries) = fs::read_dir("/sys/class/input") {
            for entry in entries.flatten() {
                let file_name = entry.file_name();
                let file_name = file_name.to_string_lossy();
                if !file_name.starts_with("event") {
                    continue;
                }
                if let Ok(actual) = fs::read_to_string(entry.path().join("device/name")) {
                    if actual.trim() == name {
                        return PathBuf::from("/dev/input").join(file_name.as_ref());
                    }
                }
            }
        }
        std::thread::sleep(Duration::from_millis(25));
    }
    panic!("no /dev/input event node named {name}");
}

/// New uinput nodes are root-only until udev runs. The real helper opens them
/// with the unprivileged test user, so wait for the uaccess ACL instead of
/// racing udev or assuming group membership.
fn wait_readable(path: &Path) {
    for _ in 0..200 {
        if std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(path)
            .is_ok()
        {
            return;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    panic!("{} never became readable by the test user", path.display());
}

fn build_source(
    name: &str,
    keys: &[Key],
    relative: &[RelativeAxisType],
) -> Result<VirtualDevice, Box<dyn std::error::Error>> {
    let mut key_set = AttributeSet::<Key>::new();
    for key in keys {
        key_set.insert(*key);
    }
    let mut relative_set = AttributeSet::<RelativeAxisType>::new();
    for axis in relative {
        relative_set.insert(*axis);
    }

    let mut builder = VirtualDeviceBuilder::new()?
        .name(name)
        .input_id(InputId::new(BusType::BUS_USB, 0x1209, 0x0001, 1))
        .with_keys(&key_set)?;
    if !relative.is_empty() {
        builder = builder.with_relative_axes(&relative_set)?;
    }
    Ok(builder.build()?)
}

struct Helper {
    child: Child,
    stdin: ChildStdin,
    stdout: BufReader<ChildStdout>,
}

impl Helper {
    fn start() -> Self {
        let mut child = Command::new(env!("CARGO_BIN_EXE_ozma-seat-input"))
            .args(["--backend", "real"])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .spawn()
            .expect("start real ozma-seat-input helper");
        let stdin = child.stdin.take().expect("helper stdin");
        let stdout = BufReader::new(child.stdout.take().expect("helper stdout"));
        Self {
            child,
            stdin,
            stdout,
        }
    }

    fn request(&mut self, request: &HelperRequest) -> HelperResponse {
        let line = encode_request(request).expect("encode helper request");
        writeln!(self.stdin, "{line}").expect("write helper request");
        self.stdin.flush().expect("flush helper request");
        let mut reply = String::new();
        self.stdout
            .read_line(&mut reply)
            .expect("read helper response");
        assert!(!reply.trim().is_empty(), "helper closed stdout");
        decode_response(reply.trim()).expect("decode helper response")
    }

    fn grab(&mut self, seat: &str, kbd: &Path, mouse: &Path) -> Vec<String> {
        let response = self.request(&HelperRequest::Grab {
            seat_id: seat.to_owned(),
            devices: vec![
                HelperDevice::new(kbd.display().to_string(), InputDeviceKind::Keyboard),
                HelperDevice::new(mouse.display().to_string(), InputDeviceKind::Mouse),
            ],
        });
        match response {
            HelperResponse::Grabbed { uinput_devices, .. } => uinput_devices,
            other => panic!("helper grab for {seat} failed: {other:?}"),
        }
    }
}

impl Drop for Helper {
    fn drop(&mut self) {
        let _ = writeln!(
            self.stdin,
            "{}",
            encode_request(&HelperRequest::Shutdown).unwrap()
        );
        let _ = self.stdin.flush();
        for _ in 0..40 {
            if let Ok(Some(_)) = self.child.try_wait() {
                return;
            }
            std::thread::sleep(Duration::from_millis(25));
        }
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

struct Xorg {
    number: u32,
    config: PathBuf,
    auth: PathBuf,
    cookie: Vec<u8>,
    child: Child,
}

impl Xorg {
    fn display(&self) -> String {
        format!(":{}", self.number)
    }

    fn start(root: &Path, number: u32, entries: &[(String, PathBuf)]) -> Self {
        let display = format!(":{number}");
        let dir = root.join(format!("display-{number}"));
        fs::create_dir_all(&dir).expect("create xorg dir");
        let auth = dir.join("Xauthority");
        let cookie = cookie_bytes();
        let cookie_hex = hex(&cookie);
        let status = Command::new("xauth")
            .arg("-f")
            .arg(&auth)
            .args(["add", &display, "MIT-MAGIC-COOKIE-1", &cookie_hex])
            .status()
            .expect("run xauth");
        assert!(status.success(), "xauth add failed for {display}");
        let config = dir.join("xorg.conf");
        fs::write(&config, xorg_config(entries)).expect("write xorg config");
        let log = dir.join("Xorg.log");

        let child = Command::new("sudo")
            .args(["-n", "/usr/lib/Xorg", &display])
            .arg("-config")
            .arg(&config)
            .arg("-logfile")
            .arg(&log)
            .arg("-auth")
            .arg(&auth)
            .args([
                "-nolisten",
                "tcp",
                "-noreset",
                "-novtswitch",
                "-sharevts",
                "-keeptty",
            ])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .expect("spawn throwaway Xorg");

        let mut xorg = Self {
            number,
            config,
            auth,
            cookie,
            child,
        };
        xorg.wait_ready(root);
        xorg
    }

    fn wait_ready(&mut self, root: &Path) {
        let deadline = Instant::now() + Duration::from_secs(20);
        loop {
            match probe_x11_display(
                &self.display(),
                Some(&self.cookie),
                Duration::from_millis(250),
            ) {
                Ok(true) => return,
                Ok(false) => {}
                Err(error) => {
                    self.dump_logs(root);
                    panic!("{} rejected the X11 handshake: {error}", self.display());
                }
            }
            if Instant::now() >= deadline {
                self.dump_logs(root);
                panic!("{} never answered the X11 handshake", self.display());
            }
            std::thread::sleep(Duration::from_millis(100));
        }
    }

    fn dump_logs(&self, _root: &Path) {
        let log = self.config.with_file_name("Xorg.log");
        if let Ok(text) = fs::read_to_string(&log) {
            eprintln!("--- tail of {} ---", log.display());
            for line in text
                .lines()
                .rev()
                .take(40)
                .collect::<Vec<_>>()
                .into_iter()
                .rev()
            {
                eprintln!("{line}");
            }
        }
    }
}

impl Drop for Xorg {
    fn drop(&mut self) {
        let pattern = format!("Xorg :{} -config {}", self.number, self.config.display());
        let _ = Command::new("sudo")
            .args(["-n", "pkill", "-TERM", "-f", &pattern])
            .status();
        let deadline = Instant::now() + Duration::from_secs(5);
        while Instant::now() < deadline {
            if let Ok(Some(_)) = self.child.try_wait() {
                break;
            }
            std::thread::sleep(Duration::from_millis(25));
        }
        if let Ok(None) = self.child.try_wait() {
            let _ = Command::new("sudo")
                .args(["-n", "pkill", "-KILL", "-f", &pattern])
                .status();
            let _ = self.child.kill();
            let _ = self.child.wait();
        }
    }
}

fn xorg_config(entries: &[(String, PathBuf)]) -> String {
    let mut config = String::from(
        r#"Section "ServerFlags"
    Option "AutoAddDevices" "false"
    Option "AutoEnableDevices" "false"
    Option "DontVTSwitch" "true"
    Option "AllowMouseOpenFail" "true"
EndSection

Section "Device"
    Identifier "ozma-dummy"
    Driver "dummy"
    VideoRam 256000
EndSection

Section "Monitor"
    Identifier "ozma-monitor"
    HorizSync 5.0-1000.0
    VertRefresh 5.0-200.0
    Modeline "1280x800" 83.50 1280 1352 1480 1680 800 803 809 831 -hsync +vsync
EndSection

Section "Screen"
    Identifier "ozma-screen"
    Device "ozma-dummy"
    Monitor "ozma-monitor"
    DefaultDepth 24
    SubSection "Display"
        Depth 24
        Modes "1280x800"
    EndSubSection
EndSection

Section "ServerLayout"
    Identifier "ozma-layout"
    Screen "ozma-screen"
"#,
    );
    for (index, (name, _)) in entries.iter().enumerate() {
        let role = match index {
            0 => "CoreKeyboard",
            1 => "CorePointer",
            _ => "SendCoreEvents",
        };
        config.push_str(&format!("    InputDevice \"{name}\" \"{role}\"\n"));
    }
    config.push_str("EndSection\n\n");
    for (name, event) in entries {
        config.push_str(&format!(
            "Section \"InputDevice\"\n    Identifier \"{name}\"\n    Driver \"libinput\"\n    Option \"Device\" \"{}\"\n    Option \"AccelProfile\" \"flat\"\nEndSection\n\n",
            event.display()
        ));
    }
    config
}

fn xinput(display: &str, auth: &Path, args: &[&str]) -> Command {
    let mut command = Command::new("xinput");
    command
        .env("DISPLAY", display)
        .env("XAUTHORITY", auth)
        .args(args);
    command
}

fn list_devices(display: &str, auth: &Path) -> Vec<ozma_multiseat::XInputDevice> {
    let output = xinput(display, auth, &["list", "--short"])
        .output()
        .expect("run xinput list");
    assert!(
        output.status.success(),
        "xinput list on {display} failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    parse_xinput_list(&String::from_utf8_lossy(&output.stdout))
}

fn device_enabled(display: &str, auth: &Path, name: &str) -> bool {
    let id = list_devices(display, auth)
        .into_iter()
        .find(|device| device.name == name)
        .unwrap_or_else(|| panic!("{name} not visible on {display}"))
        .id;
    let output = xinput(display, auth, &["list-props", &id.to_string()])
        .output()
        .expect("run xinput list-props");
    assert!(output.status.success(), "xinput list-props {id} failed");
    let text = String::from_utf8_lossy(&output.stdout);
    let line = text
        .lines()
        .find(|line| line.contains("Device Enabled"))
        .unwrap_or_else(|| panic!("{name} has no Device Enabled property"));
    line.split(':')
        .nth(1)
        .and_then(|value| value.trim().parse::<u32>().ok())
        .unwrap_or_else(|| panic!("cannot parse Device Enabled from {line:?}"))
        == 1
}

fn start_xev(root: &Path, display: &str, auth: &Path, tag: &str) -> (Child, PathBuf) {
    let log = root.join(format!("xev-{tag}.log"));
    let file = File::create(&log).expect("create xev log");
    let child = Command::new("stdbuf")
        .args(["-oL", "-eL", "xev", "-root", "-event", "keyboard"])
        .env("DISPLAY", display)
        .env("XAUTHORITY", auth)
        .stdin(Stdio::null())
        .stdout(Stdio::from(file))
        .stderr(Stdio::null())
        .spawn()
        .expect("start xev");
    (child, log)
}

fn stop_child(child: &mut Child) {
    let _ = child.kill();
    let _ = child.wait();
}

fn read_log(path: &Path) -> String {
    fs::read_to_string(path).unwrap_or_default()
}

fn inject_key(device: &mut VirtualDevice, key: Key) {
    device
        .emit(&[InputEvent::new(EventType::KEY, key.0, 1)])
        .expect("inject key press");
    std::thread::sleep(Duration::from_millis(80));
    device
        .emit(&[InputEvent::new(EventType::KEY, key.0, 0)])
        .expect("inject key release");
}

#[test]
#[ignore = "opt-in live Xorg + dummy + libinput + uinput smoke: set OZMA_MULTISEAT_REAL_XORG=1"]
fn two_real_xorg_seats_expose_only_their_own_uinput_devices_and_events() {
    if !enabled() {
        eprintln!("skipped: set OZMA_MULTISEAT_REAL_XORG=1");
        return;
    }
    assert!(
        Command::new("sudo")
            .args(["-n", "true"])
            .status()
            .map(|status| status.success())
            .unwrap_or(false),
        "this opt-in test needs passwordless sudo to start throwaway Xorg servers"
    );
    assert!(
        Path::new("/usr/lib/xorg/modules/drivers/dummy_drv.so").exists(),
        "xf86-video-dummy is not installed"
    );
    for number in [DISPLAY_A, DISPLAY_B] {
        assert!(
            !Path::new(&format!("/tmp/.X11-unix/X{number}")).exists(),
            "display :{number} is already in use"
        );
        assert!(
            !Path::new(&format!("/tmp/.X{number}-lock")).exists(),
            "display :{number} has a stale lock"
        );
    }

    let root = tempfile::tempdir().expect("create test root");

    let nonce = std::process::id();
    let mut a_kbd = build_source(
        &format!("ozma-test-src-a-kbd-{nonce}"),
        &[Key::KEY_A, Key::KEY_B],
        &[],
    )
    .expect("create source A keyboard");
    let a_mouse = build_source(
        &format!("ozma-test-src-a-mouse-{nonce}"),
        &[Key::BTN_LEFT],
        &[RelativeAxisType::REL_X, RelativeAxisType::REL_Y],
    )
    .expect("create source A mouse");
    let mut b_kbd = build_source(
        &format!("ozma-test-src-b-kbd-{nonce}"),
        &[Key::KEY_A, Key::KEY_B],
        &[],
    )
    .expect("create source B keyboard");
    let b_mouse = build_source(
        &format!("ozma-test-src-b-mouse-{nonce}"),
        &[Key::BTN_LEFT],
        &[RelativeAxisType::REL_X, RelativeAxisType::REL_Y],
    )
    .expect("create source B mouse");

    let a_kbd_path = find_event(&format!("ozma-test-src-a-kbd-{nonce}"));
    let a_mouse_path = find_event(&format!("ozma-test-src-a-mouse-{nonce}"));
    let b_kbd_path = find_event(&format!("ozma-test-src-b-kbd-{nonce}"));
    let b_mouse_path = find_event(&format!("ozma-test-src-b-mouse-{nonce}"));
    for path in [&a_kbd_path, &a_mouse_path, &b_kbd_path, &b_mouse_path] {
        wait_readable(path);
    }
    println!(
        "source devices: A kbd={} mouse={} B kbd={} mouse={}",
        a_kbd_path.display(),
        a_mouse_path.display(),
        b_kbd_path.display(),
        b_mouse_path.display()
    );

    let mut helper = Helper::start();
    match helper.request(&HelperRequest::Hello { protocol: 1 }) {
        HelperResponse::Ready { backend, .. } => println!("helper ready backend={backend:?}"),
        other => panic!("unexpected helper hello: {other:?}"),
    }
    let a_names = helper.grab("seat-a", &a_kbd_path, &a_mouse_path);
    let b_names = helper.grab("seat-b", &b_kbd_path, &b_mouse_path);
    println!("seat A uinput={a_names:?}");
    println!("seat B uinput={b_names:?}");

    let mut entries = Vec::new();
    for name in a_names.iter().chain(b_names.iter()) {
        entries.push((name.clone(), find_event(name)));
    }
    println!("Xorg input identifiers: {entries:?}");

    let xorg_a = Xorg::start(root.path(), DISPLAY_A, &entries);
    let xorg_b = Xorg::start(root.path(), DISPLAY_B, &entries);
    println!("Xorg {} ready", xorg_a.display());
    println!("Xorg {} ready", xorg_b.display());

    // Both real X servers enumerate all four helper devices before targeting.
    let a_before = list_devices(":90", &xorg_a.auth);
    let b_before = list_devices(":91", &xorg_b.auth);
    for name in a_names.iter().chain(b_names.iter()) {
        assert!(
            a_before.iter().any(|device| &device.name == name),
            ":{DISPLAY_A} did not expose {name}"
        );
        assert!(
            b_before.iter().any(|device| &device.name == name),
            ":{DISPLAY_B} did not expose {name}"
        );
    }
    println!("both displays enumerated all four helper uinput devices");

    let mut targeter = ProcessSeatInputTargeter::new();
    let targeting_a = targeter
        .apply(&SeatInputTargetRequest {
            seat_id: "seat-a".to_owned(),
            display: ":90".to_owned(),
            xauthority: Some(xorg_a.auth.clone()),
            devices: a_names.clone(),
        })
        .expect("target seat A on :90");
    let targeting_b = targeter
        .apply(&SeatInputTargetRequest {
            seat_id: "seat-b".to_owned(),
            display: ":91".to_owned(),
            xauthority: Some(xorg_b.auth.clone()),
            devices: b_names.clone(),
        })
        .expect("target seat B on :91");
    println!(":90 targeting={targeting_a:?}");
    println!(":91 targeting={targeting_b:?}");

    for name in &a_names {
        assert!(
            device_enabled(":90", &xorg_a.auth, name),
            "{name} disabled on :90"
        );
        assert!(
            !device_enabled(":91", &xorg_b.auth, name),
            "{name} enabled on :91"
        );
    }
    for name in &b_names {
        assert!(
            !device_enabled(":90", &xorg_a.auth, name),
            "{name} enabled on :90"
        );
        assert!(
            device_enabled(":91", &xorg_b.auth, name),
            "{name} disabled on :91"
        );
    }
    println!("xinput confirmed per-display enable/disable state");

    let (mut xev_a, xev_a_log) = start_xev(root.path(), ":90", &xorg_a.auth, "90");
    let (mut xev_b, xev_b_log) = start_xev(root.path(), ":91", &xorg_b.auth, "91");
    std::thread::sleep(Duration::from_millis(900));

    inject_key(&mut a_kbd, Key::KEY_A);
    std::thread::sleep(Duration::from_millis(700));
    let a_seen_on_a = read_log(&xev_a_log);
    let a_seen_on_b = read_log(&xev_b_log);
    assert!(
        a_seen_on_a.contains("keysym 0x61, a"),
        "seat A key did not reach :90; xev log:\n{a_seen_on_a}"
    );
    assert!(
        !a_seen_on_b.contains("keysym 0x61, a"),
        "seat A key leaked into :91; xev log:\n{a_seen_on_b}"
    );

    inject_key(&mut b_kbd, Key::KEY_B);
    std::thread::sleep(Duration::from_millis(700));
    let b_seen_on_a = read_log(&xev_a_log);
    let b_seen_on_b = read_log(&xev_b_log);
    assert!(
        b_seen_on_b.contains("keysym 0x62, b"),
        "seat B key did not reach :91; xev log:\n{b_seen_on_b}"
    );
    assert!(
        !b_seen_on_a.contains("keysym 0x62, b"),
        "seat B key leaked into :90; xev log:\n{b_seen_on_a}"
    );
    println!("event routing: A key only on :90, B key only on :91");

    stop_child(&mut xev_a);
    stop_child(&mut xev_b);

    let release_a = targeter.release("seat-a").expect("release seat A");
    let release_b = targeter.release("seat-b").expect("release seat B");
    println!(":90 release={release_a:?}");
    println!(":91 release={release_b:?}");
    assert_eq!(release_a.reenabled.len(), b_names.len());
    assert_eq!(release_b.reenabled.len(), a_names.len());

    drop(targeter);
    drop(xorg_b);
    drop(xorg_a);
    drop(helper);
    drop(a_kbd);
    drop(a_mouse);
    drop(b_kbd);
    drop(b_mouse);

    for number in [DISPLAY_A, DISPLAY_B] {
        let socket = PathBuf::from(format!("/tmp/.X11-unix/X{number}"));
        let lock = PathBuf::from(format!("/tmp/.X{number}-lock"));
        let deadline = Instant::now() + Duration::from_secs(5);
        while (socket.exists() || lock.exists()) && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(50));
        }
        assert!(!socket.exists(), "socket for :{number} was not removed");
        assert!(!lock.exists(), "lock for :{number} was not removed");
    }
    let _ = fs::remove_dir_all(root.path());
    println!("PASS: two real Xorg seats isolated helper uinput devices and events");
}
