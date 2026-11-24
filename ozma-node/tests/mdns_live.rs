//! Opt-in live mDNS interoperability test.
//!
//! The Phase 4 demo found that avahi-browse and python-zeroconf could not see
//! the Rust node's `_ozma._udp.local.` advertisement. This test advertises a
//! real service with `ozma_node::mdns::advertise_on` and resolves it through
//! python-zeroconf, the browser used by `controller/discovery.py`.
//!
//! Run with:
//!
//! ```text
//! OZMA_MDNS_LIVE=1 cargo test -p ozma-node --test mdns_live -- --ignored --nocapture
//! ```
//!
//! It needs a non-loopback IPv4 interface, `python3`, and the `zeroconf`
//! module. It is ignored by default so the normal suite needs no multicast.

use std::collections::HashMap;
use std::path::PathBuf;
use std::process::Command;

use serde_json::Value;

#[test]
#[ignore = "requires standalone multicast DNS and python-zeroconf (OZMA_MDNS_LIVE=1)"]
fn python_zeroconf_browses_the_rust_advertisement() {
    if std::env::var("OZMA_MDNS_LIVE").ok().as_deref() != Some("1") {
        eprintln!("skipped: set OZMA_MDNS_LIVE=1 to run the live mDNS test");
        return;
    }

    let local_ip = local_ip_address::local_ip().expect("resolve a local IP for mDNS");
    if local_ip.is_loopback() {
        eprintln!(
            "skipped: only a loopback address is available; libmdns does not advertise loopback"
        );
        return;
    }

    let instance = format!("ozma-mdns-live-{}", std::process::id());
    let port = 47123;
    let mut txt = HashMap::new();
    txt.insert("proto".to_owned(), "1".to_owned());
    txt.insert("role".to_owned(), "compute".to_owned());
    txt.insert("hw".to_owned(), "live-test".to_owned());
    txt.insert("cap".to_owned(), "hid,video".to_owned());
    txt.insert("api_port".to_owned(), "47124".to_owned());

    let _handle = ozma_node::mdns::advertise_on(&instance, port, &txt, Some(local_ip))
        .expect("advertise mDNS service");

    let script = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/mdns_browse.py");
    let output = Command::new("python3")
        .arg(&script)
        .arg(format!("{instance}._ozma._udp.local."))
        .arg("8")
        .output()
        .expect("run python3 tests/mdns_browse.py (is python-zeroconf installed?)");

    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        output.status.success(),
        "zeroconf browse failed\nstdout: {stdout}\nstderr: {stderr}"
    );

    let document: Value = serde_json::from_str(stdout.trim()).expect("browse JSON");
    assert_eq!(document["found"], true, "browse result: {document}");
    assert_eq!(document["type"], "_ozma._udp.local.");
    assert_eq!(document["name"], format!("{instance}._ozma._udp.local."));
    assert_eq!(document["port"], port);

    let addresses = document["addresses"]
        .as_array()
        .expect("addresses array")
        .iter()
        .filter_map(Value::as_str)
        .collect::<Vec<_>>();
    assert!(
        addresses.contains(&local_ip.to_string().as_str()),
        "expected {local_ip} in {addresses:?}"
    );

    let properties = document["properties"].as_object().expect("TXT properties");
    for (key, value) in &txt {
        assert_eq!(
            properties.get(key).and_then(Value::as_str),
            Some(value.as_str()),
            "TXT {key}"
        );
    }
}
