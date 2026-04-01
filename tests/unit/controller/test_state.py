"""Unit tests for AppState and NodeInfo."""

import pytest

pytestmark = pytest.mark.unit


class TestNodeInfo:
    def test_construction(self, make_node):
        node = make_node(id="test._ozma._udp.local.")
        assert node.id == "test._ozma._udp.local."
        assert node.host == "10.0.0.5"
        assert node.port == 7331

    def test_default_capabilities(self, make_node):
        node = make_node()
        assert "hid" in node.capabilities

    def test_custom_fields(self, make_node):
        node = make_node(hw="rpi4", vnc_host="192.168.1.5", vnc_port=5901)
        assert node.hw == "rpi4"
        assert node.vnc_host == "192.168.1.5"
        assert node.vnc_port == 5901


class TestAppState:
    def test_empty_state(self, app_state):
        assert len(app_state.nodes) == 0
        assert app_state.active_node_id is None

    def test_add_node(self, app_state, make_node):
        node = make_node(id="n1._ozma._udp.local.")
        app_state.nodes[node.id] = node
        assert "n1._ozma._udp.local." in app_state.nodes

    def test_remove_node(self, app_state, make_node):
        node = make_node(id="n1._ozma._udp.local.")
        app_state.nodes[node.id] = node
        del app_state.nodes[node.id]
        assert "n1._ozma._udp.local." not in app_state.nodes

    def test_populated_state(self, populated_state):
        assert len(populated_state.nodes) == 2
        assert "hw-1._ozma._udp.local." in populated_state.nodes
        assert "sw-1._ozma._udp.local." in populated_state.nodes

    def test_set_active(self, populated_state):
        populated_state.active_node_id = "hw-1._ozma._udp.local."
        assert populated_state.active_node_id == "hw-1._ozma._udp.local."

    def test_get_active_node(self, populated_state):
        populated_state.active_node_id = "hw-1._ozma._udp.local."
        if hasattr(populated_state, 'get_active_node'):
            node = populated_state.get_active_node()
            assert node is not None
            assert node.id == "hw-1._ozma._udp.local."
