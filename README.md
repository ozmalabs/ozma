# Frigate NVR

Frigate is an open-source NVR optimized for real-time AI object detection. Ships with a local MQTT broker for home automation integration.

**Port:** 5000 (web UI), 8554 (RTSP), 1883 (MQTT)

**Status check:** `curl -sf http://localhost:5000/api/stats || exit 1`
