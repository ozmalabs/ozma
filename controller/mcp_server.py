# SPDX-License-Identifier: AGPL-3.0-only WITH OzmaPluginException
"""
MCP (Model Context Protocol) server for AI agent integration.

Exposes ozma as a set of tools that AI agents (Claude Desktop, Claude Code,
custom agents) can use to control machines on the mesh.

Two transports:
  - stdio: for local integration (Claude Desktop, Claude Code)
  - SSE:   for remote agents over HTTP (port 7381)

Tools exposed:
  - ozma_control:     control a machine (screenshot, click, type, etc.)
  - ozma_list_nodes:  list connected machines
  - ozma_scenarios:   list/switch scenarios
  - ozma_run_test:    run a visual regression test

Usage (stdio — Claude Desktop):
  Add to claude_desktop_config.json:
  {
    "mcpServers": {
      "ozma": {
        "command": "python3",
        "args": ["/path/to/controller/mcp_server.py", "--stdio"]
      }
    }
  }

Usage (SSE — remote agents):
  python3 controller/mcp_server.py --sse --port 7381
  Then connect from any MCP client to http://host:7381/sse
"""

from __future__ import annotations

import asyncio
import json
import logging
import sys
from typing import Any

log = logging.getLogger("ozma.mcp")


class MCPServer:
    """
    Model Context Protocol server.

    Implements the MCP JSON-RPC protocol over stdio or SSE transport.
    Routes tool calls to the AgentEngine and other ozma subsystems.
    """

    def __init__(self, agent_engine: Any = None, state: Any = None,
                 scenarios: Any = None, test_runner: Any = None) -> None:
        self._agent = agent_engine
        self._state = state
        self._scenarios = scenarios
        self._test_runner = test_runner
        self._request_id = 0

    # ── Tool definitions ───────────────────────────────────────────────

    def _get_tools(self) -> list[dict]:
        """Return all available MCP tools."""
        tools = []

        # ozma_control — the main tool
        from agent_engine import OZMA_CONTROL_TOOL
        tools.append(OZMA_CONTROL_TOOL)

        # ozma_list_nodes
        tools.append({
            "name": "ozma_list_nodes",
            "description": (
                "List all machines connected to the Ozma KVM mesh. "
                "Returns node IDs, hostnames, capabilities, and connection state."
            ),
            "input_schema": {
                "type": "object",
                "properties": {},
            },
        })

        # ozma_scenarios
        tools.append({
            "name": "ozma_scenarios",
            "description": (
                "List available scenarios (machine profiles) and optionally switch to one. "
                "A scenario represents a machine on the mesh. Switching activates its "
                "keyboard/mouse/audio/video routing."
            ),
            "input_schema": {
                "type": "object",
                "properties": {
                    "action": {
                        "type": "string",
                        "enum": ["list", "activate"],
                        "description": "list: show all scenarios. activate: switch to a scenario.",
                    },
                    "scenario_id": {
                        "type": "string",
                        "description": "Scenario ID to activate (required for activate action)",
                    },
                },
                "required": ["action"],
            },
        })

        # ozma_run_test
        if self._test_runner:
            tools.append({
                "name": "ozma_run_test",
                "description": (
                    "Run a visual regression test on a machine. "
                    "Tests are defined in YAML and verify screen state through OCR and element detection."
                ),
                "input_schema": {
                    "type": "object",
                    "properties": {
                        "test_file": {"type": "string", "description": "Path to YAML test file"},
                        "node_id": {"type": "string", "description": "Target node"},
                    },
                    "required": ["test_file"],
                },
            })

        return tools

    # ── Tool dispatch ──────────────────────────────────────────────────

    async def _handle_tool_call(self, name: str, arguments: dict) -> Any:
        """Execute a tool call and return the result."""
        if name == "ozma_control":
            if not self._agent:
                return {"error": "Agent engine not available"}
            action = arguments.pop("action", "")
            result = await self._agent.execute(action, **arguments)
            return result.to_dict()

        elif name == "ozma_list_nodes":
            if not self._state:
                return {"nodes": []}
            nodes = []
            for nid, node in self._state.nodes.items():
                nodes.append({
                    "id": nid,
                    "host": node.host,
                    "port": node.port,
                    "hw": node.hw,
                    "capabilities": node.capabilities,
                    "vnc_host": node.vnc_host,
                    "vnc_port": node.vnc_port,
                })
            return {"nodes": nodes, "active_node": self._state.active_node_id}

        elif name == "ozma_scenarios":
            action = arguments.get("action", "list")
            if action == "list" and self._scenarios:
                return {
                    "scenarios": self._scenarios.list_scenarios(),
                    "active": self._scenarios.active_id,
                }
            elif action == "activate" and self._scenarios:
                sid = arguments.get("scenario_id", "")
                ok = await self._scenarios.activate(sid)
                return {"ok": ok, "active": sid}
            return {"error": "Scenarios not available"}

        elif name == "ozma_run_test":
            if not self._test_runner:
                return {"error": "Test runner not available"}
            test_file = arguments.get("test_file", "")
            node_id = arguments.get("node_id", "")
            result = await self._test_runner.run_file(test_file, node_id)
            return result

        return {"error": f"Unknown tool: {name}"}

    # ── JSON-RPC message handling ──────────────────────────────────────

    async def handle_message(self, message: dict) -> dict | None:
        """Handle a single JSON-RPC message."""
        method = message.get("method", "")
        msg_id = message.get("id")
        params = message.get("params", {})

        if method == "initialize":
            return {
                "jsonrpc": "2.0",
                "id": msg_id,
                "result": {
                    "protocolVersion": "2024-11-05",
                    "capabilities": {
                        "tools": {"listChanged": False},
                    },
                    "serverInfo": {
                        "name": "ozma",
                        "version": "1.1.0",
                    },
                },
            }

        elif method == "notifications/initialized":
            return None  # No response for notifications

        elif method == "tools/list":
            return {
                "jsonrpc": "2.0",
                "id": msg_id,
                "result": {"tools": self._get_tools()},
            }

        elif method == "tools/call":
            tool_name = params.get("name", "")
            arguments = params.get("arguments", {})
            try:
                result = await self._handle_tool_call(tool_name, arguments)
                # Format result as MCP content
                content = []
                if isinstance(result, dict):
                    # If there's a screenshot, send as image
                    screenshot = result.pop("screenshot_base64", "")
                    if screenshot:
                        content.append({
                            "type": "image",
                            "data": screenshot,
                            "mimeType": "image/jpeg",
                        })
                    # Send the rest as text
                    content.append({
                        "type": "text",
                        "text": json.dumps(result, indent=2),
                    })
                else:
                    content.append({"type": "text", "text": str(result)})

                return {
                    "jsonrpc": "2.0",
                    "id": msg_id,
                    "result": {"content": content, "isError": False},
                }
            except Exception as e:
                return {
                    "jsonrpc": "2.0",
                    "id": msg_id,
                    "result": {
                        "content": [{"type": "text", "text": f"Error: {e}"}],
                        "isError": True,
                    },
                }

        elif method == "ping":
            return {"jsonrpc": "2.0", "id": msg_id, "result": {}}

        else:
            return {
                "jsonrpc": "2.0",
                "id": msg_id,
                "error": {"code": -32601, "message": f"Method not found: {method}"},
            }

    # ── stdio transport ────────────────────────────────────────────────

    async def run_stdio(self) -> None:
        """Run as stdio MCP server (for Claude Desktop / Claude Code)."""
        log.info("MCP server starting (stdio transport)")
        reader = asyncio.StreamReader()
        protocol = asyncio.StreamReaderProtocol(reader)
        await asyncio.get_event_loop().connect_read_pipe(lambda: protocol, sys.stdin.buffer)

        w_transport, w_protocol = await asyncio.get_event_loop().connect_write_pipe(
            asyncio.streams.FlowControlMixin, sys.stdout.buffer
        )
        writer = asyncio.StreamWriter(w_transport, w_protocol, None, asyncio.get_event_loop())

        while True:
            try:
                # Read Content-Length header
                header = await reader.readline()
                if not header:
                    break
                header_str = header.decode().strip()
                if not header_str.startswith("Content-Length:"):
                    continue
                content_length = int(header_str.split(":")[1].strip())

                # Read empty line separator
                await reader.readline()

                # Read content
                content = await reader.readexactly(content_length)
                message = json.loads(content.decode())

                response = await self.handle_message(message)
                if response:
                    response_bytes = json.dumps(response).encode()
                    header = f"Content-Length: {len(response_bytes)}\r\n\r\n".encode()
                    writer.write(header + response_bytes)
                    await writer.drain()

            except (asyncio.IncompleteReadError, ConnectionError):
                break
            except Exception as e:
                log.error("MCP stdio error: %s", e)

    # ── SSE transport ──────────────────────────────────────────────────

    async def run_sse(self, host: str = "0.0.0.0", port: int = 7381) -> None:
        """Run as SSE MCP server (for remote agents)."""
        from aiohttp import web

        async def handle_sse(request: web.Request) -> web.StreamResponse:
            """SSE endpoint — server sends events, client sends JSON-RPC via POST."""
            response = web.StreamResponse(
                status=200,
                reason="OK",
                headers={
                    "Content-Type": "text/event-stream",
                    "Cache-Control": "no-cache",
                    "Connection": "keep-alive",
                    "Access-Control-Allow-Origin": "*",
                },
            )
            await response.prepare(request)

            # Send endpoint URL for client to POST to
            session_id = f"session-{id(request)}"
            endpoint_event = f"data: {json.dumps({'endpoint': f'/message?session={session_id}'})}\n\n"
            await response.write(endpoint_event.encode())

            # Keep connection alive
            try:
                while True:
                    await asyncio.sleep(30)
                    await response.write(b": keepalive\n\n")
            except (ConnectionResetError, asyncio.CancelledError):
                pass

            return response

        async def handle_message(request: web.Request) -> web.Response:
            """POST endpoint for JSON-RPC messages."""
            body = await request.json()
            result = await self.handle_message(body)
            if result:
                return web.json_response(result)
            return web.Response(status=204)

        app = web.Application()
        app.router.add_get("/sse", handle_sse)
        app.router.add_post("/message", handle_message)

        runner = web.AppRunner(app)
        await runner.setup()
        site = web.TCPSite(runner, host, port)
        await site.start()
        log.info("MCP SSE server on %s:%d", host, port)


async def start_mcp_server(agent_engine: Any, state: Any, scenarios: Any,
                            test_runner: Any = None,
                            host: str = "0.0.0.0", port: int = 7381) -> MCPServer:
    """Start the MCP SSE server as a background task."""
    server = MCPServer(agent_engine, state, scenarios, test_runner)
    asyncio.create_task(server.run_sse(host, port), name="mcp-sse")
    return server


# ── CLI entry point ───────────────────────────────────────────────────────────

if __name__ == "__main__":
    import argparse
    parser = argparse.ArgumentParser(description="Ozma MCP Server")
    parser.add_argument("--stdio", action="store_true", help="stdio transport")
    parser.add_argument("--sse", action="store_true", help="SSE transport")
    parser.add_argument("--port", type=int, default=7381, help="SSE port")
    parser.add_argument("--controller", default="http://localhost:7380",
                        help="Controller API URL (for standalone mode)")
    args = parser.parse_args()

    logging.basicConfig(level=logging.INFO)

    # In standalone mode, create a proxy AgentEngine that forwards to the controller API
    # For embedded mode, main.py passes the real engine
    server = MCPServer()

    if args.stdio:
        asyncio.run(server.run_stdio())
    elif args.sse:
        async def run():
            await server.run_sse(port=args.port)
            await asyncio.Event().wait()
        asyncio.run(run())
    else:
        print("Specify --stdio or --sse")
        sys.exit(1)
