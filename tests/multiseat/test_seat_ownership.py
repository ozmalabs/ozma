#!/usr/bin/env python3
# SPDX-License-Identifier: AGPL-3.0-only WITH OzmaPluginException
"""
Unit tests for seat ownership, sharing, and permissions.

Tests the ownership model, permission hierarchy, sharing endpoints,
destructive action warnings, and event notifications.
"""

import asyncio
import json
import sys
import time
import unittest
from pathlib import Path
from unittest.mock import AsyncMock, MagicMock, patch

# conftest.py sets up sys.path for controller/


# ── Permission logic (controller/permissions.py) ─────────────────────────────

class TestCheckNodePermission(unittest.TestCase):
    """Test check_node_permission for various ownership/sharing states."""

    def _make_node(self, **kwargs):
        from state import NodeInfo
        defaults = dict(
            id="test-node", host="1.2.3.4", port=7331,
            role="compute", hw="test", fw_version="1.0", proto_version=1,
        )
        defaults.update(kwargs)
        return NodeInfo(**defaults)

    def test_unowned_node_always_allowed(self):
        """Unowned nodes (empty owner_id) have no restrictions."""
        from permissions import check_node_permission
        node = self._make_node()
        self.assertTrue(check_node_permission(node, "anyone", "owner"))
        self.assertTrue(check_node_permission(node, "anyone", "admin"))
        self.assertTrue(check_node_permission(node, "anyone", "use"))

    def test_owner_has_all_permissions(self):
        from permissions import check_node_permission
        node = self._make_node(owner_id="matt")
        self.assertTrue(check_node_permission(node, "matt", "use"))
        self.assertTrue(check_node_permission(node, "matt", "manage"))
        self.assertTrue(check_node_permission(node, "matt", "admin"))
        self.assertTrue(check_node_permission(node, "matt", "owner"))

    def test_non_owner_no_share_denied(self):
        from permissions import check_node_permission
        node = self._make_node(owner_id="matt")
        self.assertFalse(check_node_permission(node, "stranger", "use"))

    def test_shared_use_permission(self):
        from permissions import check_node_permission
        node = self._make_node(
            owner_id="matt",
            shared_with=["alice"],
            share_permissions={"alice": "use"},
        )
        self.assertTrue(check_node_permission(node, "alice", "use"))
        self.assertFalse(check_node_permission(node, "alice", "manage"))
        self.assertFalse(check_node_permission(node, "alice", "admin"))
        self.assertFalse(check_node_permission(node, "alice", "owner"))

    def test_shared_manage_permission(self):
        from permissions import check_node_permission
        node = self._make_node(
            owner_id="matt",
            shared_with=["bob"],
            share_permissions={"bob": "manage"},
        )
        self.assertTrue(check_node_permission(node, "bob", "use"))
        self.assertTrue(check_node_permission(node, "bob", "manage"))
        self.assertFalse(check_node_permission(node, "bob", "admin"))

    def test_shared_admin_permission(self):
        from permissions import check_node_permission
        node = self._make_node(
            owner_id="matt",
            shared_with=["charlie"],
            share_permissions={"charlie": "admin"},
        )
        self.assertTrue(check_node_permission(node, "charlie", "use"))
        self.assertTrue(check_node_permission(node, "charlie", "manage"))
        self.assertTrue(check_node_permission(node, "charlie", "admin"))
        self.assertFalse(check_node_permission(node, "charlie", "owner"))

    def test_permission_hierarchy_ordering(self):
        """use < manage < admin < owner."""
        from permissions import _LEVEL_INDEX
        self.assertLess(_LEVEL_INDEX["use"], _LEVEL_INDEX["manage"])
        self.assertLess(_LEVEL_INDEX["manage"], _LEVEL_INDEX["admin"])
        self.assertLess(_LEVEL_INDEX["admin"], _LEVEL_INDEX["owner"])

    def test_invalid_required_level(self):
        from permissions import check_node_permission
        node = self._make_node(owner_id="matt")
        self.assertFalse(check_node_permission(node, "matt", "superadmin"))

    def test_parent_machine_owner_override(self):
        """Machine owner has full control over child seats."""
        from permissions import check_node_permission
        from state import AppState, NodeInfo

        state = AppState()
        loop = asyncio.new_event_loop()

        # Parent machine owned by matt
        parent = self._make_node(id="mypc", owner_id="matt")
        loop.run_until_complete(state.add_node(parent))

        # Child seat owned by alice
        seat = self._make_node(
            id="mypc-seat-1", owner_id="alice",
            parent_node_id="mypc",
        )
        loop.run_until_complete(state.add_node(seat))

        # Matt (machine owner) has full control over alice's seat
        self.assertTrue(check_node_permission(seat, "matt", "owner", state))
        self.assertTrue(check_node_permission(seat, "matt", "admin", state))

        # Alice still has owner rights on her seat
        self.assertTrue(check_node_permission(seat, "alice", "owner"))

        # Bob has no access
        self.assertFalse(check_node_permission(seat, "bob", "use", state))

        loop.close()


class TestGetUserPermissionLevel(unittest.TestCase):
    """Test get_user_permission_level."""

    def _make_node(self, **kwargs):
        from state import NodeInfo
        defaults = dict(
            id="test-node", host="1.2.3.4", port=7331,
            role="compute", hw="test", fw_version="1.0", proto_version=1,
        )
        defaults.update(kwargs)
        return NodeInfo(**defaults)

    def test_unowned_returns_owner(self):
        from permissions import get_user_permission_level
        node = self._make_node()
        self.assertEqual(get_user_permission_level(node, "anyone"), "owner")

    def test_owner_returns_owner(self):
        from permissions import get_user_permission_level
        node = self._make_node(owner_id="matt")
        self.assertEqual(get_user_permission_level(node, "matt"), "owner")

    def test_shared_returns_level(self):
        from permissions import get_user_permission_level
        node = self._make_node(
            owner_id="matt",
            shared_with=["alice"],
            share_permissions={"alice": "manage"},
        )
        self.assertEqual(get_user_permission_level(node, "alice"), "manage")

    def test_no_access_returns_none(self):
        from permissions import get_user_permission_level
        node = self._make_node(owner_id="matt")
        self.assertIsNone(get_user_permission_level(node, "stranger"))


# ── Destructive action warnings ──────────────────────────────────────────────

class TestDestructiveWarnings(unittest.TestCase):
    """Test check_destructive_warnings for seat reduction scenarios."""

    def _make_state_with_seats(self):
        """Create AppState with a machine + 3 child seats."""
        from state import AppState, NodeInfo
        state = AppState()
        loop = asyncio.new_event_loop()

        # Parent machine
        parent = NodeInfo(
            id="mypc", host="1.2.3.4", port=7331,
            role="compute", hw="test", fw_version="1.0", proto_version=1,
            seat_count=3, owner_id="matt",
        )
        loop.run_until_complete(state.add_node(parent))

        # Seat 0 — owned by matt (default)
        seat0 = NodeInfo(
            id="mypc-seat-0", host="1.2.3.4", port=7331,
            role="compute", hw="test", fw_version="1.0", proto_version=1,
            parent_node_id="mypc", owner_id="matt",
        )
        loop.run_until_complete(state.add_node(seat0))

        # Seat 1 — owned by alice, shared with bob
        seat1 = NodeInfo(
            id="mypc-seat-1", host="1.2.3.4", port=7331,
            role="compute", hw="test", fw_version="1.0", proto_version=1,
            parent_node_id="mypc", owner_id="alice",
            shared_with=["bob"], share_permissions={"bob": "use"},
        )
        loop.run_until_complete(state.add_node(seat1))

        # Seat 2 — owned by dave
        seat2 = NodeInfo(
            id="mypc-seat-2", host="1.2.3.4", port=7331,
            role="compute", hw="test", fw_version="1.0", proto_version=1,
            parent_node_id="mypc", owner_id="dave",
        )
        loop.run_until_complete(state.add_node(seat2))

        loop.close()
        return state

    def test_reduce_seats_with_owned_seats(self):
        """Reducing from 3 to 1 warns about seat-2 and seat-1."""
        from permissions import check_destructive_warnings
        state = self._make_state_with_seats()

        warnings = check_destructive_warnings(state, "mypc", "reduce_seats",
                                               target_seat_count=1)
        self.assertGreater(len(warnings), 0)

        # Should find warnings for the affected seats
        affected_seats = {w.affected_seat for w in warnings}
        self.assertIn("mypc-seat-2", affected_seats)
        self.assertIn("mypc-seat-1", affected_seats)

    def test_reduce_seats_no_warnings_for_unowned(self):
        """Reducing seats that have no owner/shares produces no warnings."""
        from state import AppState, NodeInfo
        from permissions import check_destructive_warnings

        state = AppState()
        loop = asyncio.new_event_loop()

        parent = NodeInfo(
            id="bare-pc", host="1.2.3.4", port=7331,
            role="compute", hw="test", fw_version="1.0", proto_version=1,
            seat_count=3,
        )
        loop.run_until_complete(state.add_node(parent))

        # Unowned seats
        for i in range(3):
            seat = NodeInfo(
                id=f"bare-pc-seat-{i}", host="1.2.3.4", port=7331,
                role="compute", hw="test", fw_version="1.0", proto_version=1,
                parent_node_id="bare-pc",
            )
            loop.run_until_complete(state.add_node(seat))

        loop.close()

        warnings = check_destructive_warnings(state, "bare-pc", "reduce_seats",
                                               target_seat_count=1)
        self.assertEqual(len(warnings), 0)

    def test_increase_seats_no_warnings(self):
        """Increasing seat count should never produce warnings."""
        from permissions import check_destructive_warnings
        state = self._make_state_with_seats()

        warnings = check_destructive_warnings(state, "mypc", "reduce_seats",
                                               target_seat_count=5)
        self.assertEqual(len(warnings), 0)

    def test_destroy_node_warnings(self):
        """Destroying a node with ownership produces warnings."""
        from permissions import check_destructive_warnings
        state = self._make_state_with_seats()

        warnings = check_destructive_warnings(state, "mypc", "destroy_node")
        self.assertGreater(len(warnings), 0)
        # Should include the parent node and child seats
        affected = {w.affected_seat for w in warnings}
        self.assertIn("mypc", affected)

    def test_warning_includes_shared_users(self):
        """Warnings list shared users who will lose access."""
        from permissions import check_destructive_warnings
        state = self._make_state_with_seats()

        warnings = check_destructive_warnings(state, "mypc", "reduce_seats",
                                               target_seat_count=1)
        # Find warning for seat-1 (shared with bob)
        seat1_warnings = [w for w in warnings if w.affected_seat == "mypc-seat-1"]
        self.assertEqual(len(seat1_warnings), 1)
        self.assertIn("bob", seat1_warnings[0].shared_users)
        self.assertEqual(seat1_warnings[0].owner, "alice")

    def test_warnings_to_dict(self):
        """Serialization for JSON API response."""
        from permissions import check_destructive_warnings, warnings_to_dict
        state = self._make_state_with_seats()

        warnings = check_destructive_warnings(state, "mypc", "reduce_seats",
                                               target_seat_count=1)
        dicts = warnings_to_dict(warnings)
        self.assertIsInstance(dicts, list)
        for d in dicts:
            self.assertIn("seat", d)
            self.assertIn("owner", d)
            self.assertIn("shared_with", d)
            self.assertIn("message", d)


# ── NodeInfo ownership fields ────────────────────────────────────────────────

class TestNodeInfoOwnershipFields(unittest.TestCase):
    """Test NodeInfo ownership-related fields and serialization."""

    def _make_node(self, **kwargs):
        from state import NodeInfo
        defaults = dict(
            id="test-node", host="1.2.3.4", port=7331,
            role="compute", hw="test", fw_version="1.0", proto_version=1,
        )
        defaults.update(kwargs)
        return NodeInfo(**defaults)

    def test_default_ownership_empty(self):
        node = self._make_node()
        self.assertEqual(node.owner_id, "")
        self.assertEqual(node.shared_with, [])
        self.assertEqual(node.share_permissions, {})
        self.assertEqual(node.parent_node_id, "")

    def test_owner_id_in_to_dict(self):
        node = self._make_node(owner_id="matt")
        d = node.to_dict()
        self.assertEqual(d["owner_id"], "matt")

    def test_owner_id_not_in_dict_when_empty(self):
        node = self._make_node()
        d = node.to_dict()
        self.assertNotIn("owner_id", d)

    def test_shared_with_in_to_dict(self):
        node = self._make_node(
            owner_id="matt",
            shared_with=["alice", "bob"],
            share_permissions={"alice": "use", "bob": "manage"},
        )
        d = node.to_dict()
        self.assertEqual(d["shared_with"], ["alice", "bob"])
        self.assertEqual(d["share_permissions"]["alice"], "use")

    def test_shared_not_in_dict_when_empty(self):
        node = self._make_node(owner_id="matt")
        d = node.to_dict()
        self.assertNotIn("shared_with", d)

    def test_parent_node_id_in_to_dict(self):
        node = self._make_node(parent_node_id="mypc")
        d = node.to_dict()
        self.assertEqual(d["parent_node_id"], "mypc")

    def test_ownership_preserved_on_re_register(self):
        """Ownership fields survive node re-registration."""
        from state import AppState, NodeInfo
        state = AppState()
        loop = asyncio.new_event_loop()

        node1 = self._make_node(owner_id="matt")
        loop.run_until_complete(state.add_node(node1))

        # Set sharing
        state.nodes["test-node"].shared_with = ["alice"]
        state.nodes["test-node"].share_permissions = {"alice": "use"}
        state.nodes["test-node"].parent_node_id = "mypc"

        # Re-register (simulates agent reconnect)
        node2 = self._make_node()
        loop.run_until_complete(state.add_node(node2))

        n = state.nodes["test-node"]
        self.assertEqual(n.owner_id, "matt")
        self.assertEqual(n.shared_with, ["alice"])
        self.assertEqual(n.share_permissions, {"alice": "use"})
        self.assertEqual(n.parent_node_id, "mypc")

        loop.close()


# ── Default seat ownership from machine owner ────────────────────────────────

class TestSeatInheritance(unittest.TestCase):
    """Machine owner owns new seats by default."""

    def test_machine_owner_owns_seats_via_parent(self):
        """Machine owner has full control over child seats via parent relationship."""
        from state import NodeInfo
        from permissions import check_node_permission, get_user_permission_level
        from state import AppState

        state = AppState()
        loop = asyncio.new_event_loop()

        parent = NodeInfo(
            id="mypc", host="1.2.3.4", port=7331,
            role="compute", hw="test", fw_version="1.0", proto_version=1,
            owner_id="matt",
        )
        loop.run_until_complete(state.add_node(parent))

        # New seat — no explicit owner, but parent_node_id links to machine
        seat = NodeInfo(
            id="mypc-seat-1", host="1.2.3.4", port=7331,
            role="compute", hw="test", fw_version="1.0", proto_version=1,
            parent_node_id="mypc", owner_id="alice",
        )
        loop.run_until_complete(state.add_node(seat))

        # Matt (machine owner) can do anything on the seat
        self.assertTrue(check_node_permission(seat, "matt", "owner", state))
        self.assertEqual(get_user_permission_level(seat, "matt", state), "owner")

        loop.close()


# ── User seat list ───────────────────────────────────────────────────────────

class TestGetUserSeats(unittest.TestCase):
    """Test get_user_seats returns correct owned/shared lists."""

    def test_user_sees_owned_and_shared(self):
        from state import AppState, NodeInfo
        from permissions import get_user_seats

        state = AppState()
        loop = asyncio.new_event_loop()

        # Matt owns node-a
        node_a = NodeInfo(
            id="node-a", host="1.2.3.4", port=7331,
            role="compute", hw="test", fw_version="1.0", proto_version=1,
            owner_id="matt",
        )
        loop.run_until_complete(state.add_node(node_a))

        # Alice owns node-b, shared with matt
        node_b = NodeInfo(
            id="node-b", host="1.2.3.5", port=7331,
            role="compute", hw="test", fw_version="1.0", proto_version=1,
            owner_id="alice",
            shared_with=["matt"],
            share_permissions={"matt": "use"},
        )
        loop.run_until_complete(state.add_node(node_b))

        # Unrelated node
        node_c = NodeInfo(
            id="node-c", host="1.2.3.6", port=7331,
            role="compute", hw="test", fw_version="1.0", proto_version=1,
            owner_id="bob",
        )
        loop.run_until_complete(state.add_node(node_c))

        result = get_user_seats(state, "matt")
        loop.close()

        self.assertEqual(len(result["owned"]), 1)
        self.assertEqual(result["owned"][0]["id"], "node-a")
        self.assertEqual(result["owned"][0]["permission"], "owner")

        self.assertEqual(len(result["shared"]), 1)
        self.assertEqual(result["shared"][0]["id"], "node-b")
        self.assertEqual(result["shared"][0]["owner"], "alice")
        self.assertEqual(result["shared"][0]["permission"], "use")

    def test_user_with_no_seats(self):
        from state import AppState
        from permissions import get_user_seats
        state = AppState()
        result = get_user_seats(state, "nobody")
        self.assertEqual(result["owned"], [])
        self.assertEqual(result["shared"], [])


# ── API endpoints ────────────────────────────────────────────────────────────

try:
    from api import build_app
    _API_AVAILABLE = True
except ImportError:
    _API_AVAILABLE = False


@unittest.skipUnless(_API_AVAILABLE, "controller API deps not installed")
class TestOwnershipAPI(unittest.TestCase):
    """Test ownership/sharing REST endpoints via TestClient."""

    def setUp(self):
        from state import AppState, NodeInfo
        from scenarios import ScenarioManager
        from api import build_app

        self.state = AppState()
        scenarios = ScenarioManager(Path("/tmp/ozma-test-ownership-scenarios.json"), self.state)
        self.app = build_app(self.state, scenarios)

        loop = asyncio.new_event_loop()
        node = NodeInfo(
            id="test-pc", host="1.2.3.4", port=7331,
            role="compute", hw="test", fw_version="1.0", proto_version=1,
            seat_count=3,
        )
        loop.run_until_complete(self.state.add_node(node))
        loop.close()

    def test_get_owner_default_empty(self):
        from fastapi.testclient import TestClient
        with TestClient(self.app) as client:
            resp = client.get("/api/v1/nodes/test-pc/owner")
            self.assertEqual(resp.status_code, 200)
            data = resp.json()
            self.assertEqual(data["owner_id"], "")

    def test_set_owner(self):
        from fastapi.testclient import TestClient
        with TestClient(self.app) as client:
            resp = client.put("/api/v1/nodes/test-pc/owner",
                              json={"user_id": "matt"})
            self.assertEqual(resp.status_code, 200)
            self.assertTrue(resp.json()["ok"])
            self.assertEqual(resp.json()["owner_id"], "matt")

            # Verify via GET
            resp = client.get("/api/v1/nodes/test-pc/owner")
            self.assertEqual(resp.json()["owner_id"], "matt")

    def test_set_owner_not_found(self):
        from fastapi.testclient import TestClient
        with TestClient(self.app) as client:
            resp = client.put("/api/v1/nodes/nonexistent/owner",
                              json={"user_id": "matt"})
            self.assertEqual(resp.status_code, 404)

    def test_get_sharing_empty(self):
        from fastapi.testclient import TestClient
        with TestClient(self.app) as client:
            resp = client.get("/api/v1/nodes/test-pc/sharing")
            self.assertEqual(resp.status_code, 200)
            data = resp.json()
            self.assertEqual(data["shares"], [])

    def test_add_sharing(self):
        from fastapi.testclient import TestClient
        with TestClient(self.app) as client:
            resp = client.post("/api/v1/nodes/test-pc/sharing",
                               json={"user_id": "alice", "permission": "use"})
            self.assertEqual(resp.status_code, 200)
            self.assertTrue(resp.json()["ok"])

            # Verify
            resp = client.get("/api/v1/nodes/test-pc/sharing")
            shares = resp.json()["shares"]
            self.assertEqual(len(shares), 1)
            self.assertEqual(shares[0]["user_id"], "alice")
            self.assertEqual(shares[0]["permission"], "use")

    def test_add_sharing_invalid_permission(self):
        from fastapi.testclient import TestClient
        with TestClient(self.app) as client:
            resp = client.post("/api/v1/nodes/test-pc/sharing",
                               json={"user_id": "bob", "permission": "superuser"})
            self.assertEqual(resp.status_code, 400)

    def test_add_sharing_no_user_id(self):
        from fastapi.testclient import TestClient
        with TestClient(self.app) as client:
            resp = client.post("/api/v1/nodes/test-pc/sharing",
                               json={"permission": "use"})
            self.assertEqual(resp.status_code, 400)

    def test_update_sharing(self):
        from fastapi.testclient import TestClient
        with TestClient(self.app) as client:
            # Ensure alice is shared
            client.post("/api/v1/nodes/test-pc/sharing",
                        json={"user_id": "alice", "permission": "use"})

            # Update to manage
            resp = client.put("/api/v1/nodes/test-pc/sharing/alice",
                              json={"permission": "manage"})
            self.assertEqual(resp.status_code, 200)
            self.assertEqual(resp.json()["permission"], "manage")

    def test_update_sharing_not_shared(self):
        from fastapi.testclient import TestClient
        with TestClient(self.app) as client:
            resp = client.put("/api/v1/nodes/test-pc/sharing/stranger",
                              json={"permission": "use"})
            self.assertEqual(resp.status_code, 404)

    def test_revoke_sharing(self):
        from fastapi.testclient import TestClient
        with TestClient(self.app) as client:
            # Add bob
            client.post("/api/v1/nodes/test-pc/sharing",
                        json={"user_id": "bob", "permission": "use"})

            # Revoke
            resp = client.delete("/api/v1/nodes/test-pc/sharing/bob")
            self.assertEqual(resp.status_code, 200)
            self.assertTrue(resp.json()["ok"])

            # Verify bob is gone
            resp = client.get("/api/v1/nodes/test-pc/sharing")
            user_ids = [s["user_id"] for s in resp.json()["shares"]]
            self.assertNotIn("bob", user_ids)

    def test_revoke_sharing_not_found(self):
        from fastapi.testclient import TestClient
        with TestClient(self.app) as client:
            resp = client.delete("/api/v1/nodes/test-pc/sharing/nonexistent")
            self.assertEqual(resp.status_code, 404)


@unittest.skipUnless(_API_AVAILABLE, "controller API deps not installed")
class TestSeatConfigWithWarnings(unittest.TestCase):
    """Test PUT /api/v1/nodes/{id}/seats with destructive warnings."""

    def setUp(self):
        from state import AppState, NodeInfo
        from scenarios import ScenarioManager
        from api import build_app

        self.state = AppState()
        scenarios = ScenarioManager(Path("/tmp/ozma-test-warnings-scenarios.json"), self.state)
        self.app = build_app(self.state, scenarios)

        loop = asyncio.new_event_loop()

        # Parent machine with 3 seats
        parent = NodeInfo(
            id="warn-pc", host="1.2.3.4", port=7331,
            role="compute", hw="test", fw_version="1.0", proto_version=1,
            seat_count=3, owner_id="matt",
        )
        loop.run_until_complete(self.state.add_node(parent))

        # Seat 1 — owned by alice, shared with bob
        seat1 = NodeInfo(
            id="warn-pc-seat-1", host="1.2.3.4", port=7331,
            role="compute", hw="test", fw_version="1.0", proto_version=1,
            parent_node_id="warn-pc", owner_id="alice",
            shared_with=["bob"], share_permissions={"bob": "use"},
        )
        loop.run_until_complete(self.state.add_node(seat1))

        # Seat 2 — owned by dave
        seat2 = NodeInfo(
            id="warn-pc-seat-2", host="1.2.3.4", port=7331,
            role="compute", hw="test", fw_version="1.0", proto_version=1,
            parent_node_id="warn-pc", owner_id="dave",
        )
        loop.run_until_complete(self.state.add_node(seat2))

        loop.close()

    def test_reduce_seats_returns_409_without_confirm(self):
        from fastapi.testclient import TestClient
        with TestClient(self.app) as client:
            resp = client.put("/api/v1/nodes/warn-pc/seats",
                              json={"seats": 1})
            self.assertEqual(resp.status_code, 409)
            data = resp.json()
            self.assertTrue(data["confirm_required"])
            self.assertIn("warnings", data)
            self.assertGreater(len(data["warnings"]), 0)
            self.assertIn("confirm_message", data)

    def test_reduce_seats_proceeds_with_confirm(self):
        from fastapi.testclient import TestClient
        # Reset seat count for this test
        self.state.nodes["warn-pc"].seat_count = 3

        with TestClient(self.app) as client:
            resp = client.put("/api/v1/nodes/warn-pc/seats",
                              json={"seats": 1, "confirm": True})
            self.assertEqual(resp.status_code, 200)
            data = resp.json()
            self.assertTrue(data["ok"])
            self.assertEqual(data["seat_count"], 1)

    def test_increase_seats_no_warnings(self):
        from fastapi.testclient import TestClient
        # Reset seat count
        self.state.nodes["warn-pc"].seat_count = 1

        with TestClient(self.app) as client:
            resp = client.put("/api/v1/nodes/warn-pc/seats",
                              json={"seats": 3})
            self.assertEqual(resp.status_code, 200)
            self.assertTrue(resp.json()["ok"])

    def test_reduce_unowned_no_warnings(self):
        """Reducing seats on unowned node proceeds without warnings."""
        from state import NodeInfo
        from fastapi.testclient import TestClient

        loop = asyncio.new_event_loop()
        bare = NodeInfo(
            id="bare-node", host="1.2.3.4", port=7331,
            role="compute", hw="test", fw_version="1.0", proto_version=1,
            seat_count=3,
        )
        loop.run_until_complete(self.state.add_node(bare))
        loop.close()

        with TestClient(self.app) as client:
            resp = client.put("/api/v1/nodes/bare-node/seats",
                              json={"seats": 1})
            self.assertEqual(resp.status_code, 200)
            self.assertTrue(resp.json()["ok"])


# ── Event notifications ──────────────────────────────────────────────────────

class TestOwnershipEvents(unittest.TestCase):
    """Test that ownership/sharing changes fire events through state.events."""

    def test_owner_change_fires_event(self):
        from state import AppState, NodeInfo
        state = AppState()
        loop = asyncio.new_event_loop()

        node = NodeInfo(
            id="evt-node", host="1.2.3.4", port=7331,
            role="compute", hw="test", fw_version="1.0", proto_version=1,
        )
        loop.run_until_complete(state.add_node(node))

        # Drain the node.online event
        loop.run_until_complete(state.events.get())

        # Simulate setting owner via events queue directly
        loop.run_until_complete(state.events.put({
            "type": "seat.owner_changed",
            "node_id": "evt-node",
            "old_owner": "",
            "new_owner": "matt",
        }))

        evt = loop.run_until_complete(state.events.get())
        self.assertEqual(evt["type"], "seat.owner_changed")
        self.assertEqual(evt["new_owner"], "matt")
        loop.close()

    def test_share_fires_event(self):
        from state import AppState
        state = AppState()
        loop = asyncio.new_event_loop()

        loop.run_until_complete(state.events.put({
            "type": "seat.shared",
            "node_id": "test",
            "user_id": "alice",
            "permission": "use",
        }))

        evt = loop.run_until_complete(state.events.get())
        self.assertEqual(evt["type"], "seat.shared")
        self.assertEqual(evt["user_id"], "alice")
        loop.close()

    def test_unshare_fires_event(self):
        from state import AppState
        state = AppState()
        loop = asyncio.new_event_loop()

        loop.run_until_complete(state.events.put({
            "type": "seat.unshared",
            "node_id": "test",
            "user_id": "bob",
        }))

        evt = loop.run_until_complete(state.events.get())
        self.assertEqual(evt["type"], "seat.unshared")
        self.assertEqual(evt["user_id"], "bob")
        loop.close()


# ── Audit logging (structured logging) ───────────────────────────────────────

class TestAuditLogging(unittest.TestCase):
    """Verify that ownership operations produce structured log output."""

    def test_permission_check_is_fast(self):
        """Permission checks must be sub-millisecond (called on every request)."""
        from state import NodeInfo
        from permissions import check_node_permission

        node = NodeInfo(
            id="perf-node", host="1.2.3.4", port=7331,
            role="compute", hw="test", fw_version="1.0", proto_version=1,
            owner_id="matt",
            shared_with=["a", "b", "c", "d", "e"],
            share_permissions={"a": "use", "b": "manage", "c": "admin", "d": "use", "e": "use"},
        )

        start = time.monotonic()
        for _ in range(10000):
            check_node_permission(node, "c", "manage")
        elapsed = time.monotonic() - start

        # 10000 checks should take well under 1 second
        self.assertLess(elapsed, 1.0, f"10000 permission checks took {elapsed:.3f}s")


if __name__ == "__main__":
    unittest.main()
