// SPDX-License-Identifier: AGPL-3.0-only WITH OzmaPluginException
//! Thermal policy and status API integration tests.
//!
//! These use synthetic sysfs trees and never write to real `/sys`.

use std::path::{Path, PathBuf};
use std::sync::{Arc, RwLock};

use axum::body::Body;
use axum::http::{Request, StatusCode};
use http_body_util::BodyExt;
use ozma_node::thermal::{load_policy, router, run, ThermalOptions};
use ozma_thermal::ThermalStatus;
use tokio_util::sync::CancellationToken;
use tower::ServiceExt;

fn temp_dir(label: &str) -> PathBuf {
    let path = std::env::temp_dir().join(format!(
        "ozma-node-thermal-{label}-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    std::fs::create_dir_all(&path).unwrap();
    path
}

#[test]
fn example_policy_loads_and_validates() {
    let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("../ozma-thermal/examples/policy.json");
    let policy = load_policy(&path).expect("example policy");
    assert!(!policy.zones.is_empty());
    assert!(!policy.curves.is_empty());
    assert!(!policy.fans.is_empty());
}

#[tokio::test]
async fn thermal_status_route_returns_json() {
    let status = Arc::new(RwLock::new(ThermalStatus {
        ok: true,
        ..ThermalStatus::default()
    }));
    let app = router(status, None);
    let response = app
        .oneshot(
            Request::builder()
                .uri("/thermal")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let body = response.into_body().collect().await.unwrap().to_bytes();
    let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(json["ok"], true);
    assert!(json["fans"].is_array());
}

#[tokio::test]
async fn thermal_profiles_route_exposes_the_policy() {
    let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("../ozma-thermal/examples/policy.json");
    let policy = Arc::new(load_policy(&path).unwrap());
    let status = Arc::new(RwLock::new(ThermalStatus::default()));
    let app = router(status, Some(policy));
    let response = app
        .oneshot(
            Request::builder()
                .uri("/api/v1/thermal/profiles")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let body = response.into_body().collect().await.unwrap().to_bytes();
    let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert!(!json["curves"].as_array().unwrap().is_empty());
    assert!(!json["zones"].as_array().unwrap().is_empty());
}

#[tokio::test]
async fn read_only_run_publishes_hwmon_status_without_control() {
    let root = temp_dir("read-only");
    let chip = root.join("hwmon0");
    std::fs::create_dir_all(&chip).unwrap();
    std::fs::write(chip.join("name"), "coretemp\n").unwrap();
    std::fs::write(chip.join("temp1_input"), "55000\n").unwrap();
    std::fs::write(chip.join("temp1_label"), "Package id 0\n").unwrap();
    std::fs::write(chip.join("fan1_input"), "900\n").unwrap();
    std::fs::write(chip.join("pwm1"), "128\n").unwrap();
    std::fs::write(chip.join("pwm1_enable"), "2\n").unwrap();

    let status = Arc::new(RwLock::new(ThermalStatus::default()));
    let cancel = CancellationToken::new();
    cancel.cancel();
    run(
        ThermalOptions {
            policy_path: None,
            control: false,
            interval_secs: 1,
            hwmon_root: root.clone(),
        },
        status.clone(),
        cancel,
    )
    .await;

    let snapshot = status.read().unwrap().clone();
    assert!(snapshot.ok);
    assert!(!snapshot.control_enabled);
    assert_eq!(snapshot.sensors.len(), 1);
    assert!((snapshot.sensors[0].temp_c - 55.0).abs() < 0.01);
    assert_eq!(snapshot.fans.len(), 1);
    assert_eq!(snapshot.fans[0].rpm, Some(900));
    assert!(snapshot
        .warnings
        .iter()
        .any(|warning| warning.contains("read-only")));

    let _ = std::fs::remove_dir_all(root);
}

#[tokio::test]
async fn opt_in_control_writes_pwm_and_restores_automatic_mode() {
    let root = temp_dir("control");
    let coretemp = root.join("hwmon0");
    let it87 = root.join("hwmon1");
    std::fs::create_dir_all(&coretemp).unwrap();
    std::fs::create_dir_all(&it87).unwrap();
    std::fs::write(coretemp.join("name"), "coretemp\n").unwrap();
    std::fs::write(coretemp.join("temp1_input"), "30000\n").unwrap();
    std::fs::write(it87.join("name"), "it87\n").unwrap();
    std::fs::write(it87.join("fan1_input"), "800\n").unwrap();
    std::fs::write(it87.join("pwm1"), "128\n").unwrap();
    std::fs::write(it87.join("pwm1_enable"), "2\n").unwrap();

    let policy = serde_json::json!({
        "poll_interval_secs": 5,
        "min_dwell_secs": 0,
        "fans": [{
            "id": "hwmon:it87:pwm1",
            "name": "CPU fan",
            "min_rpm": 400,
            "max_rpm": 1600,
            "noise_dba": 30.0,
            "noise_at_idle_dba": 16.0,
            "controllable": true
        }],
        "zones": [{
            "id": "cpu",
            "name": "CPU",
            "sensor_ids": ["hwmon:coretemp:temp1"],
            "fan_ids": ["hwmon:it87:pwm1"],
            "curve_id": "cpu",
            "limits": {
                "safe_c": 80.0,
                "warning_c": 88.0,
                "critical_c": 95.0
            }
        }],
        "curves": [{
            "id": "cpu",
            "name": "CPU",
            "zone": "cpu",
            "sensor_ids": ["hwmon:coretemp:temp1"],
            "points": [
                {"temp_c": 20.0, "duty_percent": 40.0},
                {"temp_c": 80.0, "duty_percent": 100.0}
            ],
            "hysteresis_c": 2.0,
            "min_duty_percent": 20.0,
            "max_duty_percent": 100.0
        }]
    });
    let policy_path = root.join("policy.json");
    std::fs::write(&policy_path, policy.to_string()).unwrap();

    let status = Arc::new(RwLock::new(ThermalStatus::default()));
    let cancel = CancellationToken::new();
    cancel.cancel();
    run(
        ThermalOptions {
            policy_path: Some(policy_path),
            control: true,
            interval_secs: 1,
            hwmon_root: root.clone(),
        },
        status.clone(),
        cancel,
    )
    .await;

    // 30 C interpolates to 50% on the 20..80 C curve (40..100% duty).
    let pwm: u32 = std::fs::read_to_string(it87.join("pwm1"))
        .unwrap()
        .trim()
        .parse()
        .unwrap();
    assert!((pwm as f64 - 127.5).abs() <= 2.0, "unexpected PWM {pwm}");
    // Shutdown restored firmware automatic control.
    let enable = std::fs::read_to_string(it87.join("pwm1_enable")).unwrap();
    assert_eq!(enable.trim(), "2");

    let snapshot = status.read().unwrap().clone();
    assert!(snapshot.policy_loaded);
    assert!(!snapshot.control_enabled);
    assert!(snapshot
        .warnings
        .iter()
        .any(|warning| warning.contains("restored")));

    let _ = std::fs::remove_dir_all(root);
}
