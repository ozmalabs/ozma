# SPDX-License-Identifier: AGPL-3.0-only WITH OzmaPluginException
"""
Prometheus metrics aggregator for ozma nodes.

Collects metrics from every node subsystem and renders them in
Prometheus exposition format at /metrics. This is the single endpoint
that Prometheus scrapes to get everything about a node:

  System:       CPU, memory, disk, network, WiFi, temperature, load, uptime
  Connection:   controller RTT, packet loss, jitter, relay status, HID rate
  USB:          gadget state, current/voltage/power, PD negotiation
  Capture:      video pipeline state, fps, bitrate (when active)
  Audio:        UAC2 gadget status, sample rate, bridge state
  Power:        target machine power state (LED sense)
  RGB:          LED count, current color, effect
  Sensors:      expansion I2C sensors (temp, humidity, CO2, PM2.5, vibration)
  Phone:        USB phone endpoint connection, audio bridge

Every metric is labelled with node={name} for multi-node scraping.
"""

from __future__ import annotations

import subprocess
from pathlib import Path
from typing import Any


def _g(name: str, help_text: str, value: Any, labels: str = "") -> str:
    """Render a gauge metric."""
    return (
        f"# HELP {name} {help_text}\n"
        f"# TYPE {name} gauge\n"
        f"{name}{{{labels}}} {value}\n"
    )


def _c(name: str, help_text: str, value: Any, labels: str = "") -> str:
    """Render a counter metric."""
    return (
        f"# HELP {name} {help_text}\n"
        f"# TYPE {name} counter\n"
        f"{name}{{{labels}}} {value}\n"
    )


def collect_all(node_name: str,
                connect_client: Any = None,
                self_manager: Any = None,
                current_sensor: Any = None,
                pd_controller: Any = None,
                power_controller: Any = None,
                rgb_controller: Any = None,
                capture: Any = None,
                audio_gadget: Any = None,
                phone_endpoint: Any = None,
                expansion_sensors: Any = None,
                serial_capture: Any = None) -> str:
    """
    Collect all metrics from every subsystem and return Prometheus text.

    Each subsystem is optional — if the manager is None, those metrics
    are simply omitted. This means soft nodes emit system + connection
    metrics, hardware nodes emit everything.
    """
    lb = f'node="{node_name}"'
    lines: list[str] = []

    # ── Connection state (from connect_client) ─────────────────────────
    if connect_client:
        s = connect_client.state
        lines.append(_g("ozma_node_controller_rtt_ms", "RTT to controller (ms)", f"{s.controller_rtt_ms:.1f}", lb))
        lines.append(_g("ozma_node_controller_packet_loss", "Packet loss ratio to controller", f"{s.controller_packet_loss:.4f}", lb))
        lines.append(_g("ozma_node_controller_jitter_ms", "Jitter to controller (ms)", f"{s.controller_jitter_ms:.1f}", lb))
        lines.append(_g("ozma_node_relay_rtt_ms", "RTT to Connect relay (ms)", f"{s.relay_rtt_ms:.1f}", lb))
        lines.append(_g("ozma_node_relay_connected", "Relay tunnel up", int(s.relay_connected), lb))
        lines.append(_g("ozma_node_connect_reachable", "Connect API reachable", int(s.connect_reachable), lb))
        lines.append(_c("ozma_node_hid_packets_total", "Total HID packets received", s.hid_packets_received, lb))
        lines.append(_g("ozma_node_hid_packets_per_second", "Current HID packet rate", f"{s.hid_packets_per_second:.1f}", lb))

    # ── System resources (from self_manager) ───────────────────────────
    if self_manager:
        r = self_manager.get_report()

        # CPU
        lines.append(_g("ozma_node_cpu_usage_percent", "CPU usage percentage", f"{r.cpu_usage_pct:.1f}", lb))
        lines.append(_g("ozma_node_cpu_temperature_celsius", "CPU temperature", f"{r.cpu_temp_c:.1f}", lb))
        lines.append(_g("ozma_node_cpu_cores", "CPU core count", r.cpu_count, lb))

        # Load
        lines.append(_g("ozma_node_load_1m", "1-minute load average", f"{r.load_avg[0]:.2f}", lb))
        lines.append(_g("ozma_node_load_5m", "5-minute load average", f"{r.load_avg[1]:.2f}", lb))
        lines.append(_g("ozma_node_load_15m", "15-minute load average", f"{r.load_avg[2]:.2f}", lb))

        # Memory
        lines.append(_g("ozma_node_memory_total_bytes", "Total memory", int(r.mem_total_mb * 1048576), lb))
        lines.append(_g("ozma_node_memory_used_bytes", "Used memory", int(r.mem_used_mb * 1048576), lb))
        lines.append(_g("ozma_node_memory_usage_percent", "Memory usage percentage", f"{r.mem_pct:.1f}", lb))
        lines.append(_g("ozma_node_swap_used_bytes", "Swap used", int(r.swap_used_mb * 1048576), lb))

        # Storage (per mount)
        for vol in r.storage:
            mount = vol.get("mount", "/").replace("/", "_").strip("_") or "root"
            mlb = f'{lb},mount="{vol.get("mount", "/")}"'
            lines.append(_g("ozma_node_disk_total_bytes", "Disk total", int(vol.get("total_mb", 0) * 1048576), mlb))
            lines.append(_g("ozma_node_disk_used_bytes", "Disk used", int(vol.get("used_mb", 0) * 1048576), mlb))
            lines.append(_g("ozma_node_disk_usage_percent", "Disk usage percentage", f"{vol.get('pct', 0):.1f}", mlb))

        # Network
        if r.net_interface:
            nlb = f'{lb},interface="{r.net_interface}"'
            lines.append(_c("ozma_node_network_tx_bytes_total", "Network bytes transmitted", r.net_tx_bytes, nlb))
            lines.append(_c("ozma_node_network_rx_bytes_total", "Network bytes received", r.net_rx_bytes, nlb))
            lines.append(_c("ozma_node_network_tx_packets_total", "Network packets transmitted", r.net_tx_packets, nlb))
            lines.append(_c("ozma_node_network_rx_packets_total", "Network packets received", r.net_rx_packets, nlb))
            lines.append(_c("ozma_node_network_tx_errors_total", "Network transmit errors", r.net_tx_errors, nlb))
            lines.append(_c("ozma_node_network_rx_errors_total", "Network receive errors", r.net_rx_errors, nlb))
            lines.append(_c("ozma_node_network_tx_drops_total", "Network transmit drops", r.net_tx_drops, nlb))
            lines.append(_c("ozma_node_network_rx_drops_total", "Network receive drops", r.net_rx_drops, nlb))
            lines.append(_g("ozma_node_network_speed_mbps", "Link speed (Mbps)", r.net_speed_mbps, nlb))

        # WiFi
        if r.wifi_signal_dbm:
            wlb = f'{lb},ssid="{r.wifi_ssid}"'
            lines.append(_g("ozma_node_wifi_signal_dbm", "WiFi signal strength (dBm)", r.wifi_signal_dbm, wlb))
            lines.append(_g("ozma_node_wifi_noise_dbm", "WiFi noise floor (dBm)", r.wifi_noise_dbm, wlb))
            lines.append(_g("ozma_node_wifi_channel", "WiFi channel", r.wifi_channel, wlb))

        # USB gadget
        lines.append(_g("ozma_node_usb_gadget_connected", "USB gadget connected to target", int(r.usb_connected), lb))

        # Uptime
        lines.append(_c("ozma_node_uptime_seconds", "Node uptime (seconds)", f"{r.uptime_s:.0f}", lb))

    # ── USB current/voltage/power (from INA219/INA226) ─────────────────
    if current_sensor and current_sensor.available:
        reading = current_sensor.latest
        if reading:
            lines.append(_g("ozma_node_usb_current_ma", "USB output current (mA)", f"{reading.get('current_ma', 0):.1f}", lb))
            lines.append(_g("ozma_node_usb_voltage_v", "USB output voltage (V)", f"{reading.get('voltage_v', 0):.2f}", lb))
            lines.append(_g("ozma_node_usb_power_mw", "USB output power (mW)", f"{reading.get('power_mw', 0):.1f}", lb))

    # ── USB Power Delivery ─────────────────────────────────────────────
    if pd_controller and pd_controller.available:
        pd = pd_controller.state
        if pd:
            lines.append(_g("ozma_node_pd_voltage_v", "USB-PD negotiated voltage (V)", f"{pd.get('voltage_v', 0):.1f}", lb))
            lines.append(_g("ozma_node_pd_current_a", "USB-PD negotiated current (A)", f"{pd.get('current_a', 0):.1f}", lb))
            lines.append(_g("ozma_node_pd_power_w", "USB-PD power delivery (W)", f"{pd.get('power_w', 0):.1f}", lb))
            lines.append(_g("ozma_node_pd_connected", "USB-PD device connected", int(pd.get("role", "none") != "none"), lb))

    # ── Target machine power state ─────────────────────────────────────
    if power_controller and power_controller.available:
        powered = power_controller.is_powered
        lines.append(_g("ozma_node_target_powered", "Target machine power state (LED sense)", int(powered) if powered is not None else -1, lb))

    # ── RGB LEDs ───────────────────────────────────────────────────────
    if rgb_controller and rgb_controller.available:
        lines.append(_g("ozma_node_rgb_led_count", "Number of addressable LEDs", rgb_controller.led_count, lb))

    # ── Video capture pipeline ─────────────────────────────────────────
    if capture and capture.running:
        lines.append(_g("ozma_node_capture_active", "Video capture pipeline active", 1, lb))
        stats = getattr(capture, "stats", None)
        if stats:
            lines.append(_g("ozma_node_capture_fps", "Capture frame rate", f"{stats.get('fps', 0):.1f}", lb))
            lines.append(_g("ozma_node_capture_bitrate_kbps", "Capture bitrate (kbps)", stats.get("bitrate_kbps", 0), lb))
            lines.append(_c("ozma_node_capture_frames_total", "Total frames captured", stats.get("frames", 0), lb))
            lines.append(_c("ozma_node_capture_drops_total", "Dropped frames", stats.get("drops", 0), lb))
    elif capture:
        lines.append(_g("ozma_node_capture_active", "Video capture pipeline active", 0, lb))

    # ── Audio gadget (UAC2) ────────────────────────────────────────────
    if audio_gadget and audio_gadget.available:
        lines.append(_g("ozma_node_audio_gadget_active", "UAC2 audio gadget active", 1, lb))
        lines.append(_g("ozma_node_audio_sample_rate", "Audio sample rate (Hz)", audio_gadget.sample_rate or 0, lb))

    # ── Phone endpoint ─────────────────────────────────────────────────
    if phone_endpoint and phone_endpoint.connected:
        lines.append(_g("ozma_node_phone_connected", "Phone USB endpoint connected", 1, lb))
        lines.append(_g("ozma_node_phone_audio_bridged", "Phone audio bridge active", int(phone_endpoint.bridged), lb))
    elif phone_endpoint:
        lines.append(_g("ozma_node_phone_connected", "Phone USB endpoint connected", 0, lb))

    # ── Expansion sensors (I2C) ────────────────────────────────────────
    if expansion_sensors and expansion_sensors.available:
        for sensor_type, readings in expansion_sensors.latest_readings.items():
            for key, value in readings.items():
                if isinstance(value, (int, float)):
                    metric_name = f"ozma_node_sensor_{sensor_type}_{key}".replace("-", "_").replace(".", "_")
                    slb = f'{lb},sensor="{sensor_type}"'
                    unit = _sensor_unit(sensor_type, key)
                    lines.append(_g(metric_name, f"{sensor_type} {key} ({unit})", f"{value:.2f}", slb))

    # ── Serial console ─────────────────────────────────────────────────
    if serial_capture and serial_capture.connected:
        lines.append(_g("ozma_node_serial_connected", "Serial console connected", 1, lb))
    elif serial_capture:
        lines.append(_g("ozma_node_serial_connected", "Serial console connected", 0, lb))

    # ── Displays (DRM connectors + capture card input signal) ──────────
    lines.append(_collect_displays(lb))

    # Capture card input signal (what the target machine is outputting)
    if capture:
        cap_dev = getattr(capture, "device", None)
        if cap_dev:
            clb = f'{lb},device="{getattr(cap_dev, "path", "")}"'
            res = getattr(cap_dev, "current_resolution", None)
            if res:
                lines.append(_g("ozma_node_capture_input_width_pixels", "Capture input width", getattr(res, "width", 0), clb))
                lines.append(_g("ozma_node_capture_input_height_pixels", "Capture input height", getattr(res, "height", 0), clb))
                lines.append(_g("ozma_node_capture_input_fps", "Capture input frame rate", getattr(res, "fps", 0), clb))

    return "".join(lines)


def _collect_displays(lb: str) -> str:
    """Collect connected display info from DRM sysfs and EDID."""
    lines: list[str] = []
    drm_dir = Path("/sys/class/drm")
    if not drm_dir.exists():
        return ""

    display_count = 0
    for connector in sorted(drm_dir.iterdir()):
        name = connector.name
        if not ("-" in name and name.startswith("card")):
            continue

        try:
            status = (connector / "status").read_text().strip()
        except OSError:
            continue

        connected = status == "connected"
        connector_type = name.split("-", 1)[1] if "-" in name else name
        dlb = f'{lb},output="{connector_type}"'
        lines.append(_g("ozma_node_display_connected", "Display connected", int(connected), dlb))

        if not connected:
            continue
        display_count += 1

        edid_path = connector / "edid"
        if edid_path.exists():
            try:
                edid = edid_path.read_bytes()
                if len(edid) >= 128 and edid[:8] == b"\x00\xff\xff\xff\xff\xff\xff\x00":
                    phys_w = edid[21] * 10
                    phys_h = edid[22] * 10
                    if phys_w > 0:
                        lines.append(_g("ozma_node_display_physical_width_mm", "Display width (mm)", phys_w, dlb))
                        lines.append(_g("ozma_node_display_physical_height_mm", "Display height (mm)", phys_h, dlb))

                    if len(edid) >= 71:
                        pixel_clock = int.from_bytes(edid[54:56], "little")
                        if pixel_clock > 0:
                            h_active = edid[56] | ((edid[58] & 0xF0) << 4)
                            v_active = edid[59] | ((edid[61] & 0xF0) << 4)
                            h_blank = edid[57] | ((edid[58] & 0x0F) << 8)
                            v_blank = edid[60] | ((edid[61] & 0x0F) << 8)
                            if h_active > 0 and v_active > 0:
                                lines.append(_g("ozma_node_display_width_pixels", "Display width (px)", h_active, dlb))
                                lines.append(_g("ozma_node_display_height_pixels", "Display height (px)", v_active, dlb))
                                total_pixels = (h_active + h_blank) * (v_active + v_blank)
                                if total_pixels > 0:
                                    refresh = (pixel_clock * 10000) / total_pixels
                                    lines.append(_g("ozma_node_display_refresh_hz", "Display refresh rate (Hz)", f"{refresh:.1f}", dlb))

                    for desc_offset in (54, 72, 90, 108):
                        if len(edid) > desc_offset + 17:
                            if edid[desc_offset] == 0 and edid[desc_offset + 3] == 0xFC:
                                mon_name = edid[desc_offset + 5:desc_offset + 18].decode("ascii", errors="ignore").strip()
                                if mon_name:
                                    lines.append(_info("ozma_node_display_info", "Display model", f'{dlb},name="{mon_name}"'))
            except OSError:
                pass

        modes_path = connector / "modes"
        if modes_path.exists():
            try:
                modes = modes_path.read_text().strip().splitlines()
                if modes:
                    lines.append(_info("ozma_node_display_modes", "Display modes",
                                        f'{dlb},preferred="{modes[0]}",count="{len(modes)}"'))
            except OSError:
                pass

    lines.append(_g("ozma_node_displays_connected", "Total connected displays", display_count, lb))
    return "".join(lines)


def _sensor_unit(sensor_type: str, key: str) -> str:
    """Human-readable unit for expansion sensor readings."""
    units = {
        "temperature": "°C", "humidity": "%RH", "pressure": "hPa",
        "co2": "ppm", "pm25": "µg/m³", "pm10": "µg/m³",
        "voc": "index", "x": "g", "y": "g", "z": "g",
    }
    return units.get(key, units.get(sensor_type, ""))
