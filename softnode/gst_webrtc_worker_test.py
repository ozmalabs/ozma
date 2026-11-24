#!/usr/bin/env python3
"""Pure tests for the GStreamer WebRTC worker helpers.

These tests never import GStreamer.  Run with:

    python3 softnode/gst_webrtc_worker_test.py
"""

import unittest

import gst_webrtc_worker as worker


class SyntaxTests(unittest.TestCase):
    def test_worker_compiles(self):
        import py_compile
        from pathlib import Path

        py_compile.compile(
            str(Path(__file__).with_name("gst_webrtc_worker.py")),
            doraise=True,
        )


class ClampBitrateTests(unittest.TestCase):
    def test_clamps_below_minimum(self):
        self.assertEqual(worker.clamp_bitrate_bps(100_000), 500_000)

    def test_clamps_above_maximum(self):
        self.assertEqual(worker.clamp_bitrate_bps(99_000_000), 50_000_000)

    def test_keeps_in_range_value(self):
        self.assertEqual(worker.clamp_bitrate_bps(8_000_000), 8_000_000)


class AudioSelectionTests(unittest.TestCase):
    def test_auto_prefers_pulsesrc(self):
        self.assertEqual(
            worker.select_audio_source_element(
                "auto", {"pulsesrc", "pipewiresrc"}
            ),
            ("pulsesrc", "device"),
        )

    def test_auto_falls_back_to_pipewiresrc(self):
        self.assertEqual(
            worker.select_audio_source_element("auto", {"pipewiresrc"}),
            ("pipewiresrc", "target-object"),
        )

    def test_explicit_backend_does_not_fall_back(self):
        self.assertIsNone(
            worker.select_audio_source_element("pulse", {"pipewiresrc"})
        )

    def test_no_capture_element(self):
        self.assertIsNone(worker.select_audio_source_element("auto", set()))


class PipelineStringTests(unittest.TestCase):
    def test_video_only_pipeline_is_unchanged(self):
        pipeline = worker.build_pipeline_string(
            640, 480, 30, worker.video_encoder_description(False)
        )
        self.assertIn("appsrc name=src", pipeline)
        self.assertIn("name=venc", pipeline)
        self.assertTrue(
            pipeline.endswith("! webrtcbin name=webrtc bundle-policy=max-bundle")
        )
        self.assertNotIn("opusenc", pipeline)
        self.assertNotIn("pulsesrc", pipeline)

    def test_audio_pipeline_adds_opus_branch(self):
        pipeline = worker.build_pipeline_string(
            640,
            480,
            30,
            worker.video_encoder_description(True),
            audio_element="pulsesrc",
        )
        self.assertIn("webrtcbin name=webrtc bundle-policy=max-bundle", pipeline)
        self.assertIn("nvh264enc name=venc", pipeline)
        self.assertIn("pulsesrc name=asrc", pipeline)
        self.assertIn("opusenc name=aenc", pipeline)
        self.assertIn("rtpopuspay pt=97", pipeline)
        self.assertIn("application/x-rtp,media=audio,encoding-name=OPUS", pipeline)
        self.assertIn("! webrtc.", pipeline)
        # opusenc only accepts interleaved S16LE; F32LE makes the audio branch
        # fail to link and silently degrades the session to video-only.
        self.assertIn("audio/x-raw,format=S16LE,rate=48000,channels=2", pipeline)
        self.assertNotIn("format=F32LE", pipeline)

    def test_unsupported_audio_element_is_rejected(self):
        with self.assertRaises(ValueError):
            worker.build_pipeline_string(
                640, 480, 30, worker.video_encoder_description(False), "alsasrc"
            )


class SdpCandidateTests(unittest.TestCase):
    SDP = (
        "v=0\r\n"
        "o=- 1 0 IN IP4 0.0.0.0\r\n"
        "s=-\r\n"
        "t=0 0\r\n"
        "m=video 9 UDP/TLS/RTP/SAVPF 96\r\n"
        "a=mid:video0\r\n"
        "a=candidate:1 1 UDP 1 127.0.0.1 5000 typ host\r\n"
        "a=end-of-candidates\r\n"
        "m=audio 9 UDP/TLS/RTP/SAVPF 97\r\n"
        "a=mid:audio1\r\n"
        "a=candidate:2 1 UDP 1 127.0.0.1 6000 typ host\r\n"
    )

    def test_remote_candidates_are_grouped_by_mline(self):
        self.assertEqual(
            worker.remote_ice_candidates(self.SDP),
            [
                (0, "candidate:1 1 UDP 1 127.0.0.1 5000 typ host"),
                (1, "candidate:2 1 UDP 1 127.0.0.1 6000 typ host"),
            ],
        )

    def test_append_candidates_goes_after_the_matching_section(self):
        result = worker.append_ice_candidates(
            self.SDP, [(0, "candidate:3 1 UDP 1 127.0.0.1 7000 typ host")]
        )
        video_section, audio_section = result.split("m=audio", 1)
        self.assertIn("candidate:3", video_section)
        self.assertNotIn("candidate:3", audio_section)
        self.assertTrue(result.endswith("\r\n"))

    def test_append_candidates_is_idempotent(self):
        once = worker.append_ice_candidates(
            self.SDP, [(1, "candidate:3 1 UDP 1 127.0.0.1 7000 typ host")]
        )
        twice = worker.append_ice_candidates(
            once, [(1, "candidate:3 1 UDP 1 127.0.0.1 7000 typ host")]
        )
        self.assertEqual(once, twice)
        self.assertEqual(twice.count("candidate:3"), 1)

    def test_candidates_stay_before_end_of_candidates(self):
        result = worker.append_ice_candidates(
            self.SDP, [(0, "candidate:3 1 UDP 1 127.0.0.1 7000 typ host")]
        )
        self.assertLess(
            result.index("candidate:3"), result.index("a=end-of-candidates")
        )

    def test_candidate_without_sdp_media_is_ignored(self):
        plain = "v=0\r\no=- 1 0 IN IP4 0.0.0.0\r\n"
        self.assertEqual(worker.append_ice_candidates(plain, [(0, "candidate:1")]), plain)


class ArgumentTests(unittest.TestCase):
    def test_audio_is_opt_in(self):
        args = worker.build_arg_parser().parse_args(
            ["--sock", "/tmp/x.sock", "--shm", "/tmp/frame.bgra"]
        )
        self.assertIsNone(args.audio_source)
        self.assertEqual(args.audio_backend, "auto")

    def test_audio_arguments_are_accepted(self):
        args = worker.build_arg_parser().parse_args(
            [
                "--sock", "/tmp/x.sock",
                "--shm", "/tmp/frame.bgra",
                "--audio-source", "ozma-vm1.monitor",
                "--audio-backend", "pipewire",
            ]
        )
        self.assertEqual(args.audio_source, "ozma-vm1.monitor")
        self.assertEqual(args.audio_backend, "pipewire")


class FakeEncoder:
    def __init__(self):
        self.properties = {}

    def set_property(self, name, value):
        self.properties[name] = value


class BitrateAckTests(unittest.TestCase):
    def worker_with_sessions(self, *sessions):
        instance = worker.WebRTCWorker.__new__(worker.WebRTCWorker)
        instance.sessions = {
            f"session-{index}": {"video_encoder": encoder}
            for index, encoder in enumerate(sessions)
        }
        return instance

    def test_ack_only_with_an_encoder(self):
        encoder = FakeEncoder()
        instance = self.worker_with_sessions(encoder)
        self.assertEqual(
            instance.set_bitrate(8_000_000), {"ok": True, "bitrate": 8_000_000}
        )
        # nvh264enc/x264enc expose the property in kbps.
        self.assertEqual(encoder.properties["bitrate"], 8_000)

    def test_clamped_value_is_reported(self):
        instance = self.worker_with_sessions(FakeEncoder())
        self.assertEqual(
            instance.set_bitrate(100_000), {"ok": True, "bitrate": 500_000}
        )

    def test_no_session_is_an_explicit_failure(self):
        instance = self.worker_with_sessions()
        reply = instance.set_bitrate(4_000_000)
        self.assertFalse(reply["ok"])
        self.assertIn("no video encoder", reply["error"])

    def test_garbage_is_an_explicit_failure(self):
        instance = self.worker_with_sessions(FakeEncoder())
        self.assertFalse(instance.set_bitrate("lots")["ok"])


if __name__ == "__main__":
    unittest.main()
