// SPDX-License-Identifier: AGPL-3.0-only
//! UAC2 descriptor/capability model and ConfigFS attribute projection.
//!
//! No ALSA device is opened, no isochronous endpoint is serviced, and no
//! audio bridge is created. ConfigFS writes go to a temporary root.

use std::fs;
use std::path::{Path, PathBuf};

use ozma_usb::{
    ConfigFsUsbController, Uac2BridgeStatus, Uac2Config, Uac2Mode, UsbFunction, UsbFunctionKind,
    UsbGadgetConfig, UsbGadgetController,
};

struct Fixture {
    _temp: tempfile::TempDir,
    root: PathBuf,
    controller: ConfigFsUsbController,
}

impl Fixture {
    fn new() -> Self {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("usb_gadget");
        fs::create_dir(&root).unwrap();
        let controller = ConfigFsUsbController::new(&root);
        Self {
            _temp: temp,
            root,
            controller,
        }
    }

    fn gadget(&self) -> PathBuf {
        self.root.join("ozma-test")
    }

    fn read(&self, path: impl AsRef<Path>) -> String {
        fs::read_to_string(path).unwrap().trim().to_owned()
    }
}

fn uac2_function(config: Uac2Config) -> UsbFunction {
    UsbFunction::uac2_with_config("audio", config)
}

#[test]
fn headset_descriptors_are_consistent_with_channels_rates_and_sizes() {
    let config = Uac2Config::headset(2, 1, 48_000, 24)
        .with_sample_rates([44_100, 48_000])
        .with_sample_sizes([16, 24])
        .with_volume(true)
        .with_mute(true);
    assert!(config.validate().is_ok());
    assert!(config.playback_enabled());
    assert!(config.capture_enabled());
    assert_eq!(config.mode, Uac2Mode::Headset);
    assert_eq!(Uac2Config::channel_mask(2), 3);
    assert_eq!(Uac2Config::channel_mask(1), 1);
    assert_eq!(config.sample_size_bytes(), 3);

    let descriptors = config.descriptors();
    let summary = descriptors.summary();
    assert_eq!(summary.interfaces, 3);
    assert_eq!(summary.alternate_settings, 4);
    assert_eq!(summary.endpoints, 2);
    assert_eq!(summary.playback_endpoint, Some(0x01));
    assert_eq!(summary.capture_endpoint, Some(0x82));
    assert!(summary.descriptor_bytes > 0);

    // Both formats carry the advertised rates as 24-bit little-endian values.
    for rate in [44_100u32, 48_000] {
        let bytes = rate.to_le_bytes();
        assert!(descriptors
            .bytes
            .windows(3)
            .any(|window| window == &bytes[..3]));
    }
    // Feature units are present for volume/mute.
    assert!(descriptors
        .bytes
        .windows(2)
        .any(|window| window == [0x24, 0x06]));
}

#[test]
fn capture_only_microphone_and_playback_only_speaker() {
    let microphone = Uac2Config::microphone(1, 16_000, 16);
    assert!(microphone.validate().is_ok());
    assert_eq!(microphone.mode, Uac2Mode::Microphone);
    assert!(!microphone.playback_enabled());
    assert!(microphone.capture_enabled());
    let summary = microphone.descriptors().summary();
    assert_eq!(summary.interfaces, 2);
    assert_eq!(summary.endpoints, 1);
    assert_eq!(summary.playback_endpoint, None);

    let speaker = Uac2Config::speaker(2, 44_100, 16);
    assert!(speaker.validate().is_ok());
    assert_eq!(speaker.mode, Uac2Mode::Speaker);
    assert!(speaker.playback_enabled());
    assert!(!speaker.capture_enabled());
    let summary = speaker.descriptors().summary();
    assert_eq!(summary.interfaces, 2);
    assert_eq!(summary.endpoints, 1);
    assert_eq!(summary.capture_endpoint, None);

    let duplex = Uac2Config::new(2, 2, 48_000, 24);
    assert!(duplex.validate().is_ok());
    assert_eq!(duplex.mode, Uac2Mode::Headset);
    assert!(duplex.is_duplex());
}

#[test]
fn invalid_uac2_configurations_are_typed() {
    let no_channels = Uac2Config::new(0, 0, 48_000, 16);
    assert_eq!(no_channels.validate().unwrap_err().kind(), "invalid-config");

    let too_many_channels = Uac2Config::new(9, 0, 48_000, 16);
    assert_eq!(
        too_many_channels.validate().unwrap_err().kind(),
        "invalid-config"
    );

    let low_rate = Uac2Config::new(2, 0, 1_000, 16);
    assert_eq!(low_rate.validate().unwrap_err().kind(), "invalid-config");

    let high_rate = Uac2Config::new(2, 0, 200_000, 16);
    assert_eq!(high_rate.validate().unwrap_err().kind(), "invalid-config");

    let bad_depth = Uac2Config::new(2, 0, 48_000, 20);
    assert_eq!(bad_depth.validate().unwrap_err().kind(), "invalid-config");

    let missing_primary = Uac2Config::new(2, 0, 48_000, 16).with_sample_rates([44_100]);
    assert_eq!(
        missing_primary.validate().unwrap_err().kind(),
        "invalid-config"
    );

    let duplicate_rates = Uac2Config::new(2, 0, 48_000, 16).with_sample_rates([48_000, 48_000]);
    assert_eq!(
        duplicate_rates.validate().unwrap_err().kind(),
        "invalid-config"
    );

    let duplicate_sizes = Uac2Config::new(2, 0, 48_000, 16).with_sample_sizes([16, 16]);
    assert_eq!(
        duplicate_sizes.validate().unwrap_err().kind(),
        "invalid-config"
    );

    let mode_mismatch = Uac2Config {
        mode: Uac2Mode::Microphone,
        ..Uac2Config::new(2, 0, 48_000, 16)
    };
    assert_eq!(
        mode_mismatch.validate().unwrap_err().kind(),
        "invalid-config"
    );
}

#[test]
fn configfs_writes_primary_attributes_and_status_reports_unsupported_bridge() {
    let fixture = Fixture::new();
    let config = Uac2Config::headset(2, 1, 48_000, 24)
        .with_sample_rates([44_100, 48_000])
        .with_sample_sizes([16, 24])
        .with_volume(true);
    let gadget = UsbGadgetConfig::new("ozma-test", "dummy_udc.0")
        .with_function(uac2_function(config.clone()));
    let status = fixture.controller.create_gadget(&gadget).unwrap();

    let uac2 = fixture.gadget().join("functions/uac2.audio");
    assert_eq!(fixture.read(uac2.join("p_chmask")), "3");
    assert_eq!(fixture.read(uac2.join("c_chmask")), "1");
    assert_eq!(fixture.read(uac2.join("p_srate")), "48000");
    assert_eq!(fixture.read(uac2.join("c_srate")), "48000");
    assert_eq!(fixture.read(uac2.join("p_ssize")), "3");
    assert_eq!(fixture.read(uac2.join("c_ssize")), "3");
    assert_eq!(fixture.read(uac2.join("req_number")), "2");

    let function = status.function("uac2.audio").unwrap();
    assert_eq!(function.kind, UsbFunctionKind::Uac2);
    let uac2_status = function.uac2.as_ref().unwrap();
    assert_eq!(uac2_status.mode, Uac2Mode::Headset);
    assert_eq!(uac2_status.bridge, Uac2BridgeStatus::Unsupported);
    assert_eq!(uac2_status.descriptors.endpoints, 2);
    assert!(uac2_status.playback.is_some());
    assert!(uac2_status.capture.is_some());

    let json = serde_json::to_value(uac2_status).unwrap();
    assert_eq!(json["bridge"], "unsupported");
    assert_eq!(json["mode"], "headset");
    assert_eq!(json["descriptors"]["endpoints"], 2);

    let observed = fixture
        .controller
        .uac2_status("ozma-test", "audio")
        .unwrap();
    assert_eq!(observed.mode, Uac2Mode::Headset);
    assert_eq!(observed.descriptors.endpoints, 2);
    assert_eq!(
        config.require_bridge().unwrap_err().kind(),
        "unsupported-uac2-bridge"
    );
}

#[test]
fn capture_only_configfs_attributes_and_status() {
    let fixture = Fixture::new();
    let config = Uac2Config::microphone(1, 16_000, 16);
    let gadget =
        UsbGadgetConfig::new("ozma-test", "dummy_udc.0").with_function(uac2_function(config));
    let status = fixture.controller.create_gadget(&gadget).unwrap();

    let uac2 = fixture.gadget().join("functions/uac2.audio");
    assert_eq!(fixture.read(uac2.join("p_chmask")), "0");
    assert_eq!(fixture.read(uac2.join("c_chmask")), "1");
    assert_eq!(fixture.read(uac2.join("p_srate")), "16000");
    assert_eq!(fixture.read(uac2.join("c_srate")), "16000");
    assert_eq!(fixture.read(uac2.join("p_ssize")), "2");
    assert_eq!(fixture.read(uac2.join("c_ssize")), "2");

    let observed = status.function("uac2.audio").unwrap();
    let uac2_status = observed.uac2.as_ref().unwrap();
    assert_eq!(uac2_status.mode, Uac2Mode::Microphone);
    assert!(uac2_status.playback.is_none());
    assert_eq!(uac2_status.descriptors.endpoints, 1);
}
