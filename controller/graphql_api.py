# SPDX-License-Identifier: AGPL-3.0-only WITH OzmaPluginException
"""
GraphQL API with Strawberry.

Provides:
  - JWT auth context injection from Authorization header
  - GraphiQL playground at /graphql (enabled when OZMA_AUTH=0 or valid JWT)
  - Permission system for mutations with @strawberry.permission_class
"""

import asyncio
import logging
from typing import Any

import strawberry
from fastapi import Request
from fastapi.responses import HTMLResponse
from fastapi.routing import APIRouter
from strawberry.fastapi import GraphQLRouter
from strawberry.schema.schema_base import SchemaOptions

from auth import AuthConfig, AuthContext, verify_jwt, SCOPE_WRITE, ALL_SCOPES

log = logging.getLogger("ozma.graphql")

# ─────────────────────────────────────────────────────────────────────────────
# Schema definitions
# ─────────────────────────────────────────────────────────────────────────────


@strawberry.type
class NodeInfo:
    """Represents a node in the KVMA router."""
    id: str
    name: str
    active: bool
    hostname: str
    ip: str
    port: int
    status: str
    machine_class: str


@strawberry.type
class Query:
    """GraphQL queries."""

    @strawberry.field
    def nodes(self, info: strawberry.Info) -> list[NodeInfo]:
        """List all nodes."""
        app_state = info.context.app_state
        return [
            NodeInfo(
                id=node.id,
                name=node.name,
                active=node.id == app_state.active_node_id,
                hostname=node.hostname,
                ip=node.ip or "",
                port=node.port,
                status=node.status.value if hasattr(node, "status") else "unknown",
                machine_class=getattr(node, "machine_class", "workstation"),
            )
            for node in app_state.nodes.values()
        ]

    @strawberry.field
    def node(self, info: strawberry.Info, node_id: str) -> NodeInfo | None:
        """Get a single node by ID."""
        app_state = info.context.app_state
        node = app_state.nodes.get(node_id)
        if not node:
            return None
        return NodeInfo(
            id=node.id,
            name=node.name,
            active=node.id == app_state.active_node_id,
            hostname=node.hostname,
            ip=node.ip or "",
            port=node.port,
            status=node.status.value if hasattr(node, "status") else "unknown",
            machine_class=getattr(node, "machine_class", "workstation"),
        )

    @strawberry.field
    def active_node(self, info: strawberry.Info) -> NodeInfo | None:
        """Get the currently active node."""
        app_state = info.context.app_state
        if not app_state.active_node_id:
            return None
        node = app_state.nodes.get(app_state.active_node_id)
        if not node:
            return None
        return NodeInfo(
            id=node.id,
            name=node.name,
            active=True,
            hostname=node.hostname,
            ip=node.ip or "",
            port=node.port,
            status=node.status.value if hasattr(node, "status") else "unknown",
            machine_class=getattr(node, "machine_class", "workstation"),
        )


# ─────────────────────────────────────────────────────────────────────────────
# Permissions
# ─────────────────────────────────────────────────────────────────────────────


class RequireWritePermission:
    """Permission class requiring 'write' scope for mutations."""

    def __init__(self):
        self.message = "Write scope required for this mutation"

    def get_unauthenticated_message(self, info: strawberry.Info) -> str | None:
        """Message when user is not authenticated."""
        return self.message

    def has_permission(self, info: strawberry.Info, **kwargs: Any) -> bool:
        """Check if user has write scope."""
        auth = info.context.auth
        if not auth or not auth.authenticated:
            return False
        if hasattr(auth, "scopes") and SCOPE_WRITE in auth.scopes:
            return True
        return False


# ─────────────────────────────────────────────────────────────────────────────
# Mutations
# ─────────────────────────────────────────────────────────────────────────────


@strawberry.type
class Mutation:
    """GraphQL mutations - require write scope."""

    @strawberry.mutation(permission_classes=[RequireWritePermission])
    def activate_node(self, info: strawberry.Info, node_id: str) -> NodeInfo:
        """Activate a node - requires write scope."""
        app_state = info.context.app_state
        node = app_state.nodes.get(node_id)
        if not node:
            raise strawberry.ExceptionField("Node not found", extensions={"code": "NOT_FOUND"})

        # Set active node via the app state
        async def _activate():
            await app_state.activate_node(node_id)
            return NodeInfo(
                id=node.id,
                name=node.name,
                active=True,
                hostname=node.hostname,
                ip=node.ip or "",
                port=node.port,
                status=node.status.value if hasattr(node, "status") else "unknown",
                machine_class=getattr(node, "machine_class", "workstation"),
            )

        # Run in executor to avoid blocking
        loop = asyncio.get_event_loop()
        return asyncio.run_coroutine_threadsafe(_activate(), loop).result()


# ─────────────────────────────────────────────────────────────────────────────
# Context builder for Strawberry FastAPI
# ─────────────────────────────────────────────────────────────────────────────


class GraphQLContext:
    """Context builder for Strawberry FastAPI integration."""

    def __init__(self, app_state: Any, auth_config: AuthConfig | None):
        self.app_state = app_state
        self.auth_config = auth_config

    async def get_context(self, request: Request) -> dict[str, Any]:
        """
        Build context from FastAPI request.

        Injects auth context (from FastAPI middleware or Authorization header)
        and app state into Strawberry context.
        """
        # Get auth context from request state (set by FastAPI middleware)
        auth_ctx = getattr(request.state, "auth", None)

        context = {
            "request": request,
            "app_state": self.app_state,
            "auth": auth_ctx,
        }

        # If no auth from middleware, try Authorization header
        if not auth_ctx or not auth_ctx.authenticated:
            auth_header = request.headers.get("authorization", "")
            if auth_header.startswith("Bearer ") and self.auth_config:
                token = auth_header[7:]
                # Check for mesh_ca on auth_config
                public_key = None
                if hasattr(self.auth_config, "mesh_ca") and self.auth_config.mesh_ca:
                    public_key = self.auth_config.mesh_ca.controller_keypair.public_key
                elif hasattr(self.auth_config, "verify_key") and self.auth_config.verify_key:
                    public_key = self.auth_config.verify_key

                if public_key:
                    claims = verify_jwt(token, public_key)
                    if claims:
                        sub = claims.get("sub", "")
                        user_id = sub if sub != "admin" else ""
                        context["auth"] = AuthContext(
                            authenticated=True,
                            scopes=claims.get("scopes", []),
                            source_ip=request.client.host if request.client else "127.0.0.1",
                            auth_method="jwt",
                            user_id=user_id,
                        )

        return context


# ─────────────────────────────────────────────────────────────────────────────
# GraphQL schema and router
# ─────────────────────────────────────────────────────────────────────────────


def create_schema() -> strawberry.Schema:
    """Create the Strawberry schema."""
    return strawberry.Schema(query=Query, mutation=Mutation)


def create_router(
    app_state: Any,
    auth_config: AuthConfig | None,
) -> APIRouter:
    """
    Create a GraphQL router with auth context injection.

    The router provides:
      - POST /graphql - GraphQL endpoint
      - GET /graphiql - GraphiQL playground (when auth disabled or valid JWT)
    """
    # Create the GraphQL schema
    schema = create_schema()

    # Create context builder
    graphql_context = GraphQLContext(app_state, auth_config)

    # Create the main GraphQL router (handles POST /graphql)
    graphql_router = GraphQLRouter(
        schema=schema,
        context=graphql_context,
        graphiql=False,  # Disable default, we'll add custom GraphiQL route
    )

    return graphql_router


def add_graphiql_route(
    router: APIRouter,
    app_state: Any,
    auth_config: AuthConfig | None,
) -> None:
    """
    Add GraphiQL playground route to an existing router.

    The GraphiQL playground is accessible at GET /graphiql
    and is enabled when:
      - Auth is disabled (OZMA_AUTH=0)
      - Auth is enabled but request has valid JWT with read scope
    """
    from fastapi.responses import HTMLResponse

    @router.get("/graphiql")
    async def graphiql_endpoint(
        request: Request,
    ) -> HTMLResponse:
        """Serve GraphiQL playground at /graphiql."""
        # Check if GraphiQL should be enabled
        auth_ctx = getattr(request.state, "auth", None)
        if not should_enable_graphiql(auth_config, auth_ctx):
            return HTMLResponse(status_code=401, content="Authentication required")

        return get_graphiql_page("/graphql")


# ─────────────────────────────────────────────────────────────────────────────
# GraphiQL page generator
# ─────────────────────────────────────────────────────────────────────────────


def get_graphiql_page(graphql_url: str) -> HTMLResponse:
    """Generate GraphiQL HTML page."""
    graphiql_html = """<!DOCTYPE html>
<html>
<head>
    <meta charset="utf-8">
    <title>Ozma GraphQL Explorer</title>
    <meta name="viewport" content="width=device-width, initial-scale=1">
    <link rel="stylesheet" href="https://cdn.jsdelivr.net/npm/graphiql@2.2.3/graphiql.min.css" />
    <style>
        body {
            margin: 0;
            padding: 0;
            font-family: -apple-system, BlinkMacSystemFont, 'Segoe UI', Roboto, 'Helvetica Neue', Arial, sans-serif;
        }
        #graphiql {
            height: 100vh;
        }
    </style>
</head>
<body>
    <div id="graphiql">Loading...</div>
    <script src="https://cdn.jsdelivr.net/npm/graphiql@2.2.3/graphiql.min.js"></script>
    <script>
        const fetcher = async (graphQLParams) => {
            const token = localStorage.getItem('graphql_token') || '';
            const headers = { 'Content-Type': 'application/json' };
            if (token) {
                headers['Authorization'] = `Bearer ${token}`;
            }
            return fetch(
                '${graphql_url}',
                {
                    method: 'POST',
                    headers: headers,
                    body: JSON.stringify(graphQLParams),
                }
            ).then(response => response.json());
        }

        const graphiql = GraphiQL.create({
            fetcher,
            defaultEditorToolsVisibility: true,
            defaultVariableEditorOpen: false,
        });

        ReactDOM.render(graphiql, document.getElementById('graphiql'));
    </script>
</body>
</html>"""
    return HTMLResponse(content=graphiql_html)


def should_enable_graphiql(auth_config: AuthConfig | None, auth_ctx: AuthContext | None) -> bool:
    """
    Determine if GraphiQL should be enabled.

    GraphiQL is enabled when:
      - Auth is disabled (OZMA_AUTH=0)
      - Auth is enabled but request has valid JWT with read scope
    """
    if not auth_config or not auth_config.enabled:
        return True

    if not auth_ctx or not auth_ctx.authenticated:
        return False

    # Check if user has read scope
    if hasattr(auth_ctx, "scopes"):
        if SCOPE_WRITE in auth_ctx.scopes or SCOPE_READ in auth_ctx.scopes:
            return True

    return False
