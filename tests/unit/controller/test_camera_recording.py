# SPDX-License-Identifier: AGPL-3.0-only WITH OzmaPluginException
"""Unit tests for controller/camera_recording.py."""

from __future__ import annotations

import asyncio
import json
import os
import sys
import time
import uuid
from dataclasses import dataclass, field
from pathlib import Path
from unittest.mock import AsyncMock, MagicMock, patch

import pytest

sys.path.insert(0, str(Path(__file__).parents[3] / "controller"))

from camera_recording import (
    CameraRecordingManager,
    RecordingPolicy,
    RecordingJob,
    RecordingState,
    RecordingTrigger,
    StorageBackend,
    _encrypt_segment,
    _decrypt_segment,
)


# ---------------------------------------------------------------------------
# Helpers
# ---------------------------------------------------------------------------

def run(coro):
    return asyncio.get_event_loop().run_until_complete(coro)


def make_manager(tmp_path, key_store=None, state_ref=None):
    mgr = CameraRecordingManager(
        data_dir=tmp_path / "recording_data",
        key_store=key_store,
        state_ref=state_ref,
        event_queue=None,
    )
    return mgr


def make_policy(**kwargs):
    defaults = dict(
        name="Test Policy",
        camera_node_id=None,
        trigger=RecordingTrigger.CONTINUOUS,
    )
    defaults.update(kwargs)
    return defaults


@dataclass
class FakeNode:
    id: str = field(default_factory=lambda: str(uuid.uuid4()))
    name: str = "front_door"
    machine_class: str = "camera"
    camera_streams: list = field(default_factory=list)
    frigate_host: str | None = None
    frigate_port: int | None = None


@dataclass
class FakeState:
    nodes: dict = field(default_factory=dict)


# ---------------------------------------------------------------------------
# Encryption tests
# ---------------------------------------------------------------------------

class TestEncryption:
    def test_encrypt_decrypt_roundtrip(self):
        key = os.urandom(32)
        plaintext = b"segment data goes here" * 100
        encrypted = _encrypt_segment(plaintext, key)
        decrypted = _decrypt_segment(encrypted, key)
        assert decrypted == plaintext

    def test_encrypted_length_longer(self):
        key = os.urandom(32)
        plain = b"test"
        enc = _encrypt_segment(plain, key)
        # nonce (12) + ciphertext (4) + tag (16) = 32 bytes
        assert len(enc) == 12 + len(plain) + 16

    def test_nonce_is_random(self):
        key = os.urandom(32)
        plain = b"same plaintext"
        enc1 = _encrypt_segment(plain, key)
        enc2 = _encrypt_segment(plain, key)
        # Nonces should differ
        assert enc1[:12] != enc2[:12]

    def test_wrong_key_fails(self):
        key1 = os.urandom(32)
        key2 = os.urandom(32)
        enc = _encrypt_segment(b"secret", key1)
        from cryptography.exceptions import InvalidTag
        with pytest.raises(InvalidTag):
            _decrypt_segment(enc, key2)

    def test_tampered_ciphertext_fails(self):
        key = os.urandom(32)
        enc = bytearray(_encrypt_segment(b"data", key))
        enc[20] ^= 0xFF  # flip a bit in ciphertext
        from cryptography.exceptions import InvalidTag
        with pytest.raises(InvalidTag):
            _decrypt_segment(bytes(enc), key)


# ---------------------------------------------------------------------------
# RecordingPolicy model
# ---------------------------------------------------------------------------

class TestRecordingPolicy:
    def test_defaults(self):
        p = RecordingPolicy(
            id="abc",
            name="Test",
            camera_node_id=None,
            trigger=RecordingTrigger.CONTINUOUS,
        )
        assert p.backend == StorageBackend.LOCAL
        assert p.encrypted is False
        assert p.segment_seconds == 60
        assert p.retention_days == 30
        assert p.enabled is True

    def test_to_dict_redacts_secret(self):
        p = RecordingPolicy(
            id="abc", name="S3 policy", camera_node_id=None,
            trigger=RecordingTrigger.CONTINUOUS,
            backend=StorageBackend.S3,
            backend_config={"access_key_id": "AKID", "secret_access_key": "secret123"},
        )
        d = p.to_dict()
        assert d["backend_config"]["secret_access_key"] == "***"
        assert d["backend_config"]["access_key_id"] == "AKID"

    def test_from_dict_roundtrip(self):
        p = RecordingPolicy(
            id="xyz", name="My policy", camera_node_id="node1",
            trigger=RecordingTrigger.OBJECT,
            object_classes=["person", "car"],
            backend=StorageBackend.S3,
            backend_config={"bucket": "cams"},
            encrypted=True,
            retention_days=7,
        )
        d = p.to_dict()
        # to_dict redacts credentials but from_dict reads raw backend_config
        # so simulate what persistence does (stores raw)
        raw = {**d, "backend_config": p.backend_config}
        restored = RecordingPolicy.from_dict(raw)
        assert restored.id == p.id
        assert restored.name == p.name
        assert restored.trigger == RecordingTrigger.OBJECT
        assert restored.object_classes == ["person", "car"]
        assert restored.encrypted is True

    def test_all_triggers(self):
        for trigger in RecordingTrigger:
            p = RecordingPolicy(id="x", name="t", camera_node_id=None, trigger=trigger)
            assert p.trigger == trigger

    def test_all_backends(self):
        for backend in StorageBackend:
            p = RecordingPolicy(id="x", name="t", camera_node_id=None,
                                trigger=RecordingTrigger.CONTINUOUS, backend=backend)
            assert p.backend == backend


# ---------------------------------------------------------------------------
# RecordingJob model
# ---------------------------------------------------------------------------

class TestRecordingJob:
    def test_to_dict(self):
        job = RecordingJob(
            id="job1", policy_id="pol1", camera_node_id="node1",
        )
        d = job.to_dict()
        assert d["id"] == "job1"
        assert d["state"] == RecordingState.IDLE
        assert d["segments_written"] == 0
        assert "_proc" not in d

    def test_error_state(self):
        job = RecordingJob(
            id="j", policy_id="p", camera_node_id="n",
            state=RecordingState.ERROR,
            last_error="ffmpeg not found",
        )
        d = job.to_dict()
        assert d["last_error"] == "ffmpeg not found"


# ---------------------------------------------------------------------------
# Manager CRUD
# ---------------------------------------------------------------------------

class TestManagerCRUD:
    def test_add_policy(self, tmp_path):
        mgr = make_manager(tmp_path)
        p = mgr.add_policy(**make_policy())
        assert p.id in mgr._policies
        assert p.name == "Test Policy"

    def test_get_policy(self, tmp_path):
        mgr = make_manager(tmp_path)
        p = mgr.add_policy(**make_policy())
        found = mgr.get_policy(p.id)
        assert found is p

    def test_get_policy_missing(self, tmp_path):
        mgr = make_manager(tmp_path)
        assert mgr.get_policy("nonexistent") is None

    def test_update_policy(self, tmp_path):
        mgr = make_manager(tmp_path)
        p = mgr.add_policy(**make_policy())
        updated = mgr.update_policy(p.id, name="Renamed", retention_days=14)
        assert updated.name == "Renamed"
        assert updated.retention_days == 14

    def test_update_policy_missing(self, tmp_path):
        mgr = make_manager(tmp_path)
        assert mgr.update_policy("bogus", name="X") is None

    def test_remove_policy(self, tmp_path):
        mgr = make_manager(tmp_path)
        p = mgr.add_policy(**make_policy())
        assert mgr.remove_policy(p.id) is True
        assert p.id not in mgr._policies

    def test_remove_policy_missing(self, tmp_path):
        mgr = make_manager(tmp_path)
        assert mgr.remove_policy("bogus") is False

    def test_list_policies(self, tmp_path):
        mgr = make_manager(tmp_path)
        mgr.add_policy(**make_policy(name="P1"))
        mgr.add_policy(**make_policy(name="P2"))
        policies = mgr.list_policies()
        assert len(policies) == 2
        names = {p.name for p in policies}
        assert "P1" in names and "P2" in names


# ---------------------------------------------------------------------------
# Persistence
# ---------------------------------------------------------------------------

class TestPersistence:
    def test_save_and_load(self, tmp_path):
        mgr1 = make_manager(tmp_path)
        mgr1.add_policy(**make_policy(
            name="Saved Policy",
            trigger=RecordingTrigger.MOTION,
            backend=StorageBackend.NAS,
            backend_config={"path": "/mnt/nas"},
        ))
        mgr1._save()

        mgr2 = make_manager(tmp_path)
        mgr2._load()
        policies = mgr2.list_policies()
        assert len(policies) == 1
        assert policies[0].name == "Saved Policy"
        assert policies[0].trigger == RecordingTrigger.MOTION
        assert policies[0].backend_config["path"] == "/mnt/nas"

    def test_saves_credentials_unredacted(self, tmp_path):
        mgr = make_manager(tmp_path)
        mgr.add_policy(**make_policy(
            backend=StorageBackend.S3,
            backend_config={"bucket": "my-bucket", "secret_access_key": "topsecret"},
        ))
        # Load fresh instance
        mgr2 = make_manager(tmp_path)
        mgr2._load()
        pol = mgr2.list_policies()[0]
        assert pol.backend_config["secret_access_key"] == "topsecret"

    def test_load_empty(self, tmp_path):
        mgr = make_manager(tmp_path)
        mgr._load()  # no file exists — should not raise
        assert len(mgr._policies) == 0

    def test_file_permissions(self, tmp_path):
        mgr = make_manager(tmp_path)
        mgr.add_policy(**make_policy())
        mgr._save()
        stat = (tmp_path / "recording_data" / "policies.json").stat()
        assert oct(stat.st_mode)[-3:] == "600"


# ---------------------------------------------------------------------------
# Status
# ---------------------------------------------------------------------------

class TestStatus:
    def test_get_status_empty(self, tmp_path):
        mgr = make_manager(tmp_path)
        status = mgr.get_status()
        assert status["policies"] == 0
        assert status["active_jobs"] == 0
        assert status["jobs"] == []

    def test_get_status_with_policies(self, tmp_path):
        mgr = make_manager(tmp_path)
        mgr.add_policy(**make_policy())
        mgr.add_policy(**make_policy(name="P2"))
        status = mgr.get_status()
        assert status["policies"] == 2

    def test_list_jobs_empty(self, tmp_path):
        mgr = make_manager(tmp_path)
        assert mgr.list_jobs() == []


# ---------------------------------------------------------------------------
# Continuous reconcile
# ---------------------------------------------------------------------------

class TestContinuousReconcile:
    def test_no_state_ref_noop(self, tmp_path):
        mgr = make_manager(tmp_path)
        mgr.add_policy(**make_policy())
        run(mgr._reconcile_continuous())
        assert len(mgr._active_jobs) == 0

    def test_skips_non_camera_nodes(self, tmp_path):
        node = FakeNode(machine_class="workstation")
        state = FakeState(nodes={node.id: node})
        mgr = make_manager(tmp_path, state_ref=state)
        mgr.add_policy(**make_policy())
        run(mgr._reconcile_continuous())
        assert len(mgr._active_jobs) == 0

    def test_skips_motion_trigger(self, tmp_path):
        node = FakeNode()
        state = FakeState(nodes={node.id: node})
        mgr = make_manager(tmp_path, state_ref=state)
        mgr.add_policy(**make_policy(trigger=RecordingTrigger.MOTION))
        run(mgr._reconcile_continuous())
        assert len(mgr._active_jobs) == 0

    def test_starts_job_for_camera_node(self, tmp_path):
        node = FakeNode(
            camera_streams=[{"rtsp_inbound": "rtsp://cam:554/stream"}],
        )
        state = FakeState(nodes={node.id: node})
        mgr = make_manager(tmp_path, state_ref=state)
        mgr.add_policy(**make_policy())

        # Patch _start_job to avoid actually launching ffmpeg
        started = []
        async def fake_start(policy, n):
            job = RecordingJob(id="j", policy_id=policy.id, camera_node_id=n.id)
            mgr._active_jobs[f"{policy.id}:{n.id}"] = job
            started.append((policy.id, n.id))
            return job
        mgr._start_job = fake_start

        run(mgr._reconcile_continuous())
        assert len(started) == 1
        assert started[0][1] == node.id

    def test_skips_specific_camera_node_mismatch(self, tmp_path):
        node = FakeNode()
        state = FakeState(nodes={node.id: node})
        mgr = make_manager(tmp_path, state_ref=state)
        mgr.add_policy(**make_policy(camera_node_id="different_node"))
        run(mgr._reconcile_continuous())
        assert len(mgr._active_jobs) == 0

    def test_no_duplicate_jobs(self, tmp_path):
        node = FakeNode(
            camera_streams=[{"rtsp_inbound": "rtsp://cam:554/stream"}],
        )
        state = FakeState(nodes={node.id: node})
        mgr = make_manager(tmp_path, state_ref=state)
        pol = mgr.add_policy(**make_policy())
        # Pre-insert an existing job
        job_key = f"{pol.id}:{node.id}"
        mgr._active_jobs[job_key] = RecordingJob(id="existing", policy_id=pol.id, camera_node_id=node.id)

        started = []
        async def fake_start(policy, n):
            started.append(1)
            return None
        mgr._start_job = fake_start

        run(mgr._reconcile_continuous())
        assert len(started) == 0  # already running — no new job


# ---------------------------------------------------------------------------
# Event-triggered recording
# ---------------------------------------------------------------------------

class TestEventTriggering:
    def test_motion_trigger_starts_job(self, tmp_path):
        node = FakeNode()
        state = FakeState(nodes={node.id: node})
        mgr = make_manager(tmp_path, state_ref=state)
        mgr.add_policy(**make_policy(trigger=RecordingTrigger.MOTION))

        started = []
        async def fake_start(policy, n):
            job = RecordingJob(id="j", policy_id=policy.id, camera_node_id=n.id)
            mgr._active_jobs[f"{policy.id}:{n.id}"] = job
            started.append(n.id)
            return job
        mgr._start_job = fake_start

        run(mgr.on_event("frigate.motion_started", {"node_id": node.id}))
        assert node.id in started

    def test_object_trigger_all_classes(self, tmp_path):
        node = FakeNode()
        state = FakeState(nodes={node.id: node})
        mgr = make_manager(tmp_path, state_ref=state)
        mgr.add_policy(**make_policy(trigger=RecordingTrigger.OBJECT, object_classes=[]))

        started = []
        async def fake_start(policy, n):
            job = RecordingJob(id="j", policy_id=policy.id, camera_node_id=n.id)
            mgr._active_jobs[f"{policy.id}:{n.id}"] = job
            started.append(n.id)
            return job
        mgr._start_job = fake_start

        run(mgr.on_event("frigate.object_entered", {"node_id": node.id, "label": "car"}))
        assert node.id in started

    def test_object_trigger_filtered_class_matches(self, tmp_path):
        node = FakeNode()
        state = FakeState(nodes={node.id: node})
        mgr = make_manager(tmp_path, state_ref=state)
        mgr.add_policy(**make_policy(
            trigger=RecordingTrigger.OBJECT,
            object_classes=["person"],
        ))
        started = []
        async def fake_start(policy, n):
            job = RecordingJob(id="j", policy_id=policy.id, camera_node_id=n.id)
            mgr._active_jobs[f"{policy.id}:{n.id}"] = job
            started.append(n.id)
            return job
        mgr._start_job = fake_start

        # "person" matches
        run(mgr.on_event("frigate.object_entered", {"node_id": node.id, "label": "person"}))
        assert len(started) == 1

    def test_object_trigger_filtered_class_no_match(self, tmp_path):
        node = FakeNode()
        state = FakeState(nodes={node.id: node})
        mgr = make_manager(tmp_path, state_ref=state)
        mgr.add_policy(**make_policy(
            trigger=RecordingTrigger.OBJECT,
            object_classes=["person"],
        ))
        started = []
        async def fake_start(policy, n):
            started.append(1)
            return None
        mgr._start_job = fake_start

        # "car" does not match the "person" filter
        run(mgr.on_event("frigate.object_entered", {"node_id": node.id, "label": "car"}))
        assert len(started) == 0

    def test_event_trigger_prefix_match(self, tmp_path):
        node = FakeNode()
        state = FakeState(nodes={node.id: node})
        mgr = make_manager(tmp_path, state_ref=state)
        mgr.add_policy(**make_policy(
            trigger=RecordingTrigger.EVENT,
            event_types=["doorbell."],
        ))
        started = []
        async def fake_start(policy, n):
            job = RecordingJob(id="j", policy_id=policy.id, camera_node_id=n.id)
            mgr._active_jobs[f"{policy.id}:{n.id}"] = job
            started.append(n.id)
            return job
        mgr._start_job = fake_start

        run(mgr.on_event("doorbell.press", {"node_id": node.id}))
        assert node.id in started

    def test_event_trigger_no_match(self, tmp_path):
        node = FakeNode()
        state = FakeState(nodes={node.id: node})
        mgr = make_manager(tmp_path, state_ref=state)
        mgr.add_policy(**make_policy(
            trigger=RecordingTrigger.EVENT,
            event_types=["doorbell."],
        ))
        started = []
        async def fake_start(policy, n):
            started.append(1)
            return None
        mgr._start_job = fake_start

        run(mgr.on_event("alert.created", {"node_id": node.id}))
        assert len(started) == 0

    def test_disabled_policy_skipped(self, tmp_path):
        node = FakeNode()
        state = FakeState(nodes={node.id: node})
        mgr = make_manager(tmp_path, state_ref=state)
        mgr.add_policy(**make_policy(
            trigger=RecordingTrigger.MOTION,
            enabled=False,
        ))
        started = []
        async def fake_start(policy, n):
            started.append(1)
            return None
        mgr._start_job = fake_start

        run(mgr.on_event("frigate.motion_started", {"node_id": node.id}))
        assert len(started) == 0

    def test_camera_node_id_filter(self, tmp_path):
        node = FakeNode()
        state = FakeState(nodes={node.id: node})
        mgr = make_manager(tmp_path, state_ref=state)
        # Policy only for a different camera
        mgr.add_policy(**make_policy(
            trigger=RecordingTrigger.MOTION,
            camera_node_id="other_camera",
        ))
        started = []
        async def fake_start(policy, n):
            started.append(1)
            return None
        mgr._start_job = fake_start

        run(mgr.on_event("frigate.motion_started", {"node_id": node.id}))
        assert len(started) == 0

    def test_no_duplicate_event_jobs(self, tmp_path):
        node = FakeNode()
        state = FakeState(nodes={node.id: node})
        mgr = make_manager(tmp_path, state_ref=state)
        pol = mgr.add_policy(**make_policy(trigger=RecordingTrigger.MOTION))
        # Pre-insert existing job
        job_key = f"{pol.id}:{node.id}"
        mgr._active_jobs[job_key] = RecordingJob(id="existing", policy_id=pol.id, camera_node_id=node.id)

        started = []
        async def fake_start(policy, n):
            started.append(1)
            return None
        mgr._start_job = fake_start

        run(mgr.on_event("frigate.motion_started", {"node_id": node.id}))
        assert len(started) == 0


# ---------------------------------------------------------------------------
# RTSP URL resolution
# ---------------------------------------------------------------------------

class TestRTSPResolution:
    def test_prefers_record_quality(self):
        node = FakeNode(camera_streams=[
            {"name": "front_door_detect", "rtsp_inbound": "rtsp://cam/detect"},
            {"name": "front_door_record", "rtsp_inbound": "rtsp://cam/record"},
        ])
        mgr = CameraRecordingManager(data_dir=Path("/tmp"), key_store=None)
        url = mgr._get_rtsp_url(node, quality="record")
        assert url == "rtsp://cam/record"

    def test_falls_back_to_first_stream(self):
        node = FakeNode(camera_streams=[
            {"name": "main", "rtsp_inbound": "rtsp://cam/main"},
        ])
        mgr = CameraRecordingManager(data_dir=Path("/tmp"), key_store=None)
        url = mgr._get_rtsp_url(node)
        assert url == "rtsp://cam/main"

    def test_no_streams_returns_none(self):
        node = FakeNode(camera_streams=[])
        mgr = CameraRecordingManager(data_dir=Path("/tmp"), key_store=None)
        assert mgr._get_rtsp_url(node) is None

    def test_stream_without_rtsp_skipped(self):
        node = FakeNode(camera_streams=[
            {"name": "hls_only", "hls": "http://cam/stream.m3u8"},
        ])
        mgr = CameraRecordingManager(data_dir=Path("/tmp"), key_store=None)
        assert mgr._get_rtsp_url(node) is None


# ---------------------------------------------------------------------------
# Camera name helper
# ---------------------------------------------------------------------------

class TestCameraName:
    def test_spaces_replaced(self):
        mgr = CameraRecordingManager(data_dir=Path("/tmp"), key_store=None)
        node = FakeNode(name="Front Door")
        assert mgr._camera_name(node) == "front_door"

    def test_hyphens_replaced(self):
        mgr = CameraRecordingManager(data_dir=Path("/tmp"), key_store=None)
        node = FakeNode(name="back-yard")
        assert mgr._camera_name(node) == "back_yard"


# ---------------------------------------------------------------------------
# Frigate local backend configuration
# ---------------------------------------------------------------------------

class TestFrigateLocalBackend:
    def test_frigate_api_url(self):
        mgr = CameraRecordingManager(data_dir=Path("/tmp"), key_store=None)
        node = FakeNode(frigate_host="192.168.1.10", frigate_port=5000)
        assert mgr._frigate_api_for(node) == "http://192.168.1.10:5000"

    def test_frigate_api_no_host(self):
        mgr = CameraRecordingManager(data_dir=Path("/tmp"), key_store=None)
        node = FakeNode(frigate_host=None)
        assert mgr._frigate_api_for(node) is None

    def test_frigate_api_default_port(self):
        mgr = CameraRecordingManager(data_dir=Path("/tmp"), key_store=None)
        node = FakeNode(frigate_host="192.168.1.10", frigate_port=None)
        assert mgr._frigate_api_for(node) == "http://192.168.1.10:5000"


# ---------------------------------------------------------------------------
# Key store integration (subkey derivation)
# ---------------------------------------------------------------------------

class TestKeyStoreIntegration:
    def test_encrypted_upload_derives_footage_key(self, tmp_path):
        """_upload_loop should call key_store.derive_subkey("footage") when encrypted=True."""
        fake_key = os.urandom(32)
        key_store = MagicMock()
        key_store.derive_subkey.return_value = fake_key

        mgr = make_manager(tmp_path, key_store=key_store)
        pol = mgr.add_policy(**make_policy(
            backend=StorageBackend.NAS,
            backend_config={"path": str(tmp_path / "nas")},
            encrypted=True,
        ))
        job = RecordingJob(id="j1", policy_id=pol.id, camera_node_id="node1")
        seg_dir = tmp_path / "segs"
        seg_dir.mkdir()
        # No segment files — loop exits after first iteration via CancelledError
        seg_path = seg_dir / "20240101_120000.mp4"
        seg_path.write_bytes(b"fake segment data")

        async def run_loop():
            task = asyncio.create_task(mgr._upload_loop(job, pol, seg_dir))
            await asyncio.sleep(0.05)
            task.cancel()
            await asyncio.gather(task, return_exceptions=True)

        run(run_loop())
        key_store.derive_subkey.assert_called_once_with("footage")

    def test_no_key_store_upload_proceeds_unencrypted(self, tmp_path):
        """With encrypted=True but no key_store, segments are uploaded unencrypted with a warning."""
        mgr = make_manager(tmp_path, key_store=None)
        pol = mgr.add_policy(**make_policy(
            backend=StorageBackend.NAS,
            backend_config={"path": str(tmp_path / "nas")},
            encrypted=True,  # requested but can't do it
        ))
        job = RecordingJob(id="j1", policy_id=pol.id, camera_node_id="node1")
        seg_dir = tmp_path / "segs"
        seg_dir.mkdir()
        seg_path = seg_dir / "seg1.mp4"
        seg_path.write_bytes(b"data")

        async def run_loop():
            task = asyncio.create_task(mgr._upload_loop(job, pol, seg_dir))
            await asyncio.sleep(0.05)
            task.cancel()
            await asyncio.gather(task, return_exceptions=True)

        run(run_loop())  # should not raise


# ---------------------------------------------------------------------------
# NAS backend
# ---------------------------------------------------------------------------

class TestNASBackend:
    def test_nas_write_plaintext(self, tmp_path):
        from camera_recording import _write_to_nas
        seg = tmp_path / "seg.mp4"
        seg.write_bytes(b"video data")
        dest = tmp_path / "nas"
        run(_write_to_nas(seg, {"path": str(dest)}, encrypted=False, footage_key=None))
        assert (dest / "seg.mp4").read_bytes() == b"video data"

    def test_nas_write_encrypted(self, tmp_path):
        from camera_recording import _write_to_nas
        key = os.urandom(32)
        seg = tmp_path / "seg.mp4"
        seg.write_bytes(b"secret video")
        dest = tmp_path / "nas"
        run(_write_to_nas(seg, {"path": str(dest)}, encrypted=True, footage_key=key))
        enc_file = dest / "seg.mp4.enc"
        assert enc_file.exists()
        decrypted = _decrypt_segment(enc_file.read_bytes(), key)
        assert decrypted == b"secret video"

    def test_nas_creates_directory(self, tmp_path):
        from camera_recording import _write_to_nas
        seg = tmp_path / "seg.mp4"
        seg.write_bytes(b"data")
        dest = tmp_path / "deep" / "nested" / "dir"
        run(_write_to_nas(seg, {"path": str(dest)}, encrypted=False, footage_key=None))
        assert (dest / "seg.mp4").exists()


# ---------------------------------------------------------------------------
# Lifecycle
# ---------------------------------------------------------------------------

class TestLifecycle:
    def test_start_stop(self, tmp_path):
        mgr = make_manager(tmp_path)

        async def run_lifecycle():
            await mgr.start()
            assert len(mgr._tasks) == 3
            await mgr.stop()
            assert len(mgr._tasks) == 0

        run(run_lifecycle())

    def test_stop_cancels_tasks(self, tmp_path):
        mgr = make_manager(tmp_path)

        async def run_lifecycle():
            await mgr.start()
            tasks = list(mgr._tasks)
            await mgr.stop()
            for t in tasks:
                assert t.cancelled() or t.done()

        run(run_lifecycle())
