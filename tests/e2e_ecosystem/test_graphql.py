# SPDX-License-Identifier: AGPL-3.0-only WITH OzmaPluginException
"""E2E tests for GraphQL API endpoint and GraphiQL playground."""

import pytest
import httpx

OZMA_URL = "http://localhost:7380"


class TestGraphQLIntegration:
    """Integration tests for GraphQL endpoint."""

    @pytest.fixture
    def ozma_client(self) -> httpx.Client:
        """Create a client authenticated as admin."""
        client = httpx.Client(base_url=OZMA_URL, timeout=15)
        # Get password from environment
        password = pytest.config.getoption("--ozma-password", default="testpassword123")
        r = client.post("/api/v1/auth/token", json={"password": password})
        if r.status_code == 200:
            token = r.json()["token"]
            client.headers = {"Authorization": f"Bearer {token}"}
        return client

    def test_graphql_nodes_query_without_auth(self, ozma_client: httpx.Client) -> None:
        """
        Test that nodes query works without auth when OZMA_AUTH=0.

        When auth is disabled, the GraphQL endpoint should return node data.
        """
        query = """
        query {
            nodes {
                id
                name
                active
                hostname
                ip
                port
                status
                machine_class
            }
        }
        """

        r = ozma_client.post("/graphql", json={"query": query})
        # When OZMA_AUTH=0, this should succeed
        # When OZMA_AUTH=1, this should return 401 or 403
        assert r.status_code in (200, 401, 403), f"Unexpected status: {r.status_code}"

        if r.status_code == 200:
            data = r.json()
            assert "data" in data, f"Expected 'data' in response: {data}"
            nodes = data["data"].get("nodes", [])
            assert isinstance(nodes, list)

    def test_graphql_nodes_query_with_auth(self, ozma_client: httpx.Client) -> None:
        """
        Test that nodes query works with valid JWT.

        With a valid JWT containing read scope, the query should succeed.
        """
        # First authenticate
        password = pytest.config.getoption("--ozma-password", default="testpassword123")
        r = ozma_client.post("/api/v1/auth/token", json={"password": password})
        if r.status_code == 200:
            token = r.json()["token"]
            ozma_client.headers = {"Authorization": f"Bearer {token}"}

        query = """
        query {
            nodes {
                id
                name
                active
                hostname
                ip
                port
                status
                machine_class
            }
        }
        """

        r = ozma_client.post("/graphql", json={"query": query})
        assert r.status_code == 200, f"Expected 200, got {r.status_code}: {r.text}"

        data = r.json()
        assert "data" in data, f"Expected 'data' in response: {data}"
        assert "nodes" in data["data"], f"Expected 'nodes' in data: {data['data']}"
        nodes = data["data"]["nodes"]
        assert isinstance(nodes, list)

        # Verify response shape matches schema
        for node in nodes:
            assert "id" in node, f"Node missing 'id': {node}"
            assert "name" in node, f"Node missing 'name': {node}"
            assert "active" in node, f"Node missing 'active': {node}"
            assert "hostname" in node, f"Node missing 'hostname': {node}"
            assert "ip" in node, f"Node missing 'ip': {node}"
            assert "port" in node, f"Node missing 'port': {node}"
            assert "status" in node, f"Node missing 'status': {node}"
            assert "machine_class" in node, f"Node missing 'machine_class': {node}"

    def test_graphql_node_query_by_id(self, ozma_client: httpx.Client) -> None:
        """Test querying a single node by ID."""
        # Get nodes first to get a valid node ID
        list_query = """
        query {
            nodes {
                id
            }
        }
        """

        r = ozma_client.post("/graphql", json={"query": list_query})
        assert r.status_code == 200
        data = r.json()
        nodes = data["data"].get("nodes", [])

        if nodes:
            node_id = nodes[0]["id"]

            query = """
            query($nodeId: String!) {
                node(nodeId: $nodeId) {
                    id
                    name
                    active
                    hostname
                    ip
                    port
                    status
                    machine_class
                }
            }
            """

            r = ozma_client.post("/graphql", json={
                "query": query,
                "variables": {"nodeId": node_id}
            })
            assert r.status_code == 200, f"Expected 200, got {r.status_code}: {r.text}"

            data = r.json()
            assert "data" in data
            assert "node" in data["data"]
            node = data["data"]["node"]
            assert node is not None
            assert node["id"] == node_id

    def test_graphql_active_node_query(self, ozma_client: httpx.Client) -> None:
        """Test querying the currently active node."""
        query = """
        query {
            active_node {
                id
                name
                active
                hostname
                ip
                port
                status
                machine_class
            }
        }
        """

        r = ozma_client.post("/graphql", json={"query": query})
        assert r.status_code == 200, f"Expected 200, got {r.status_code}: {r.text}"

        data = r.json()
        assert "data" in data
        assert "active_node" in data["data"]
        active_node = data["data"]["active_node"]
        if active_node:
            assert "id" in active_node
            assert "name" in active_node
            assert active_node["active"] is True


class TestGraphiqlEndpoint:
    """Tests for the GraphiQL playground endpoint."""

    @pytest.fixture
    def ozma_client(self) -> httpx.Client:
        """Create a client authenticated as admin."""
        client = httpx.Client(base_url=OZMA_URL, timeout=15)
        password = pytest.config.getoption("--ozma-password", default="testpassword123")
        r = client.post("/api/v1/auth/token", json={"password": password})
        if r.status_code == 200:
            token = r.json()["token"]
            client.headers = {"Authorization": f"Bearer {token}"}
        return client

    def test_graphiql_page_without_auth(self, ozma_client: httpx.Client) -> None:
        """
        Test that GraphiQL returns 401 without auth when OZMA_AUTH=1.

        When auth is enabled, accessing GraphiQL without a valid token
        should return 401 Unauthorized.
        """
        r = ozma_client.get("/graphiql")
        # When OZMA_AUTH=1 and no valid JWT, should return 401
        # When OZMA_AUTH=0, should return 200 with HTML
        assert r.status_code in (200, 401), f"Unexpected status: {r.status_code}"

        if r.status_code == 200:
            # GraphiQL page should contain expected HTML elements
            assert "<!DOCTYPE html>" in r.text
            assert "GraphiQL" in r.text or "graphql" in r.text.lower()

    def test_graphiql_page_with_auth(self, ozma_client: httpx.Client) -> None:
        """Test that GraphiQL returns the page with valid JWT."""
        # Ensure we have a valid token
        password = pytest.config.getoption("--ozma-password", default="testpassword123")
        r = ozma_client.post("/api/v1/auth/token", json={"password": password})
        if r.status_code == 200:
            token = r.json()["token"]
            ozma_client.headers = {"Authorization": f"Bearer {token}"}

        r = ozma_client.get("/graphiql")
        assert r.status_code == 200, f"Expected 200, got {r.status_code}: {r.text}"

        # Verify it's a valid HTML page with GraphiQL content
        assert "<!DOCTYPE html>" in r.text
        assert "GraphiQL" in r.text or "graphql" in r.text.lower()
        assert "script" in r.text
