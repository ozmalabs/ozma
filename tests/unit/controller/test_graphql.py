# SPDX-License-Identifier: AGPL-3.0-only WITH OzmaPluginException
"""Unit tests for graphql.py — GraphQL schema, auth context, and permissions."""
import sys
from pathlib import Path

import pytest

sys.path.insert(0, str(Path(__file__).parent.parent.parent.parent / "controller"))
pytestmark = pytest.mark.unit


class TestGraphQLSchema:
    """Test GraphQL schema definitions."""

    def test_create_schema(self):
        """Test that the schema can be created."""
        from graphql import create_schema

        schema = create_schema()
        assert schema is not None

    def test_node_info_type(self):
        """Test NodeInfo type has all required fields."""
        from graphql import NodeInfo
        import strawberry

        # Check that NodeInfo is a strawberry type
        assert hasattr(NodeInfo, "__strawberry_definition__")

        # Check fields exist
        fields = {f.name for f in NodeInfo.__strawberry_definition__.fields}
        assert "id" in fields
        assert "name" in fields
        assert "active" in fields
        assert "hostname" in fields
        assert "ip" in fields
        assert "port" in fields
        assert "status" in fields
        assert "machine_class" in fields

    def test_query_type(self):
        """Test Query type has all required fields."""
        from graphql import Query
        import strawberry

        assert hasattr(Query, "__strawberry_definition__")

        fields = {f.name for f in Query.__strawberry_definition__.fields}
        assert "nodes" in fields
        assert "node" in fields
        assert "active_node" in fields

    def test_mutation_type(self):
        """Test Mutation type has required fields."""
        from graphql import Mutation
        import strawberry

        assert hasattr(Mutation, "__strawberry_definition__")

        fields = {f.name for f in Mutation.__strawberry_definition__.fields}
        assert "activate_node" in fields


class TestRequireWritePermission:
    """Test the RequireWritePermission permission class."""

    def test_has_permission_class(self):
        """Test that permission class exists."""
        from graphql import RequireWritePermission

        perm = RequireWritePermission()
        assert perm is not None
        assert hasattr(perm, "get_unauthenticated_message")
        assert hasattr(perm, "has_permission")

    def test_permission_message(self):
        """Test permission message."""
        from graphql import RequireWritePermission

        perm = RequireWritePermission()
        assert perm.get_unauthenticated_message(None) == "Write scope required for this mutation"


class TestGraphQLContext:
    """Test GraphQL context builder."""

    def test_context_builder_exists(self):
        """Test that GraphQLContext class exists."""
        from graphql import GraphQLContext

        assert GraphQLContext is not None

    def test_context_builder_init(self, app_state, auth_config):
        """Test GraphQLContext initialization."""
        from graphql import GraphQLContext

        ctx = GraphQLContext(app_state, auth_config)
        assert ctx.app_state == app_state
        assert ctx.auth_config == auth_config


class TestShouldEnableGraphiql:
    """Test the should_enable_graphiql function."""

    def test_graphiql_enabled_when_auth_disabled(self):
        """GraphiQL is enabled when auth is disabled."""
        from auth import AuthConfig
        from graphql import should_enable_graphiql
        from auth import AuthContext

        auth_cfg = AuthConfig(enabled=False)
        auth_ctx = AuthContext(
            authenticated=True,
            scopes=["read"],
            source_ip="127.0.0.1",
            auth_method="none",
        )

        assert should_enable_graphiql(auth_cfg, auth_ctx) is True

    def test_graphiql_enabled_with_valid_jwt(self):
        """GraphiQL is enabled when valid JWT is present."""
        from auth import AuthConfig
        from graphql import should_enable_graphiql
        from auth import AuthContext

        auth_cfg = AuthConfig(enabled=True)
        auth_ctx = AuthContext(
            authenticated=True,
            scopes=["read"],
            source_ip="127.0.0.1",
            auth_method="jwt",
        )

        assert should_enable_graphiql(auth_cfg, auth_ctx) is True

    def test_graphiql_enabled_with_write_scope(self):
        """GraphiQL is enabled when write scope is present."""
        from auth import AuthConfig
        from graphql import should_enable_graphiql
        from auth import AuthContext

        auth_cfg = AuthConfig(enabled=True)
        auth_ctx = AuthContext(
            authenticated=True,
            scopes=["write"],
            source_ip="127.0.0.1",
            auth_method="jwt",
        )

        assert should_enable_graphiql(auth_cfg, auth_ctx) is True

    def test_graphiql_disabled_without_auth(self):
        """GraphiQL is disabled when no auth context."""
        from auth import AuthConfig
        from graphql import should_enable_graphiql

        auth_cfg = AuthConfig(enabled=True)
        auth_ctx = None

        assert should_enable_graphiql(auth_cfg, auth_ctx) is False

    def test_graphiql_disabled_with_no_scopes(self):
        """GraphiQL is disabled when auth enabled but no valid scopes."""
        from auth import AuthConfig
        from graphql import should_enable_graphiql
        from auth import AuthContext

        auth_cfg = AuthConfig(enabled=True)
        auth_ctx = AuthContext(
            authenticated=False,
            scopes=[],
            source_ip="127.0.0.1",
            auth_method="none",
        )

        assert should_enable_graphiql(auth_cfg, auth_ctx) is False


# Fixtures
@pytest.fixture
def app_state():
    """Create a mock app state."""
    from state import AppState

    state = AppState()
    return state


@pytest.fixture
def auth_config():
    """Create a mock auth config."""
    from auth import AuthConfig

    config = AuthConfig(enabled=False)
    return config
