# SPDX-License-Identifier: AGPL-3.0-only WITH OzmaPluginException
"""Validation tests for persisted intent-led setup profiles."""

import pytest
from pydantic import ValidationError

from api import SetupProfilePayload

pytestmark = pytest.mark.unit


@pytest.fixture
def profile_payload():
    return {
        "version": 1,
        "intent": "control",
        "goal_id": "control",
        "source_endpoint_id": "controller",
        "target_node_id": "node:workstation",
        "permissions": {
            "view": True,
            "control": True,
            "audio": False,
            "microphone": False,
            "clipboard": False,
        },
        "session_options": {"ttl_seconds": 3600, "consent": "always", "indicate": True},
        "quality": {"max_fps": 30, "max_bitrate_kbps": 2000},
        "ui_aperture_level": "fine",
    }


def test_valid_profile_round_trips_intent_aperture_and_session_scope(profile_payload):
    profile = SetupProfilePayload.model_validate(profile_payload)

    assert profile.model_dump(mode="json") == profile_payload


@pytest.mark.parametrize(
    "mutate",
    [
        lambda payload: payload.update(version=2),
        lambda payload: payload.update(ui_aperture_level="hidden"),
        lambda payload: payload.update(quality={"foveation": True}),
        lambda payload: payload.update(quality={"max_fps": 60}),
        lambda payload: payload.update(quality={"max_bitrate_kbps": 10000}),
        lambda payload: payload["permissions"].update(microphone=True),
        lambda payload: payload["permissions"].update(view=False),
        lambda payload: payload["session_options"].update(ttl_seconds=86401),
        lambda payload: payload["session_options"].update(consent="never"),
        lambda payload: payload["session_options"].update(indicate=False),
    ],
)
def test_invalid_or_unsupported_profile_is_rejected(profile_payload, mutate):
    mutate(profile_payload)

    with pytest.raises(ValidationError):
        SetupProfilePayload.model_validate(profile_payload)


def test_older_v1_profile_defaults_source_to_controller(profile_payload):
    profile_payload.pop("source_endpoint_id")

    profile = SetupProfilePayload.model_validate(profile_payload)

    assert profile.source_endpoint_id == "controller"


def test_older_v1_profile_defaults_missing_quality_to_recommended_rate(profile_payload):
    profile_payload.pop("quality")

    profile = SetupProfilePayload.model_validate(profile_payload)

    assert profile.quality.max_fps == 30
    assert profile.quality.max_bitrate_kbps == 2000


def test_older_quality_profile_defaults_missing_bitrate(profile_payload):
    profile_payload["quality"].pop("max_bitrate_kbps")

    profile = SetupProfilePayload.model_validate(profile_payload)

    assert profile.quality.max_fps == 30
    assert profile.quality.max_bitrate_kbps == 2000


def test_profile_keeps_user_goal_separate_from_routing_intent(profile_payload):
    profile_payload["intent"] = "desktop"
    profile_payload["goal_id"] = "control"

    profile = SetupProfilePayload.model_validate(profile_payload)

    assert profile.intent == "desktop"
    assert profile.goal_id == "control"


def test_older_profile_without_goal_id_remains_loadable(profile_payload):
    profile_payload.pop("goal_id")

    profile = SetupProfilePayload.model_validate(profile_payload)

    assert profile.goal_id is None


def test_custom_goal_persists_separately_from_its_routing_intent(profile_payload):
    profile_payload["intent"] = "observe"
    profile_payload["goal_id"] = "custom"
    profile_payload["permissions"].update(control=False, audio=False)

    profile = SetupProfilePayload.model_validate(profile_payload)

    assert profile.intent == "observe"
    assert profile.goal_id == "custom"
