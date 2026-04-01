# SPDX-License-Identifier: AGPL-3.0-only WITH OzmaPluginException
"""
Ozma Agent CLI — the single entry point.

Three execution modes:
  ozma-agent                          Run with tray icon (default)
  ozma-agent run                      Run in foreground (no tray, logs to stdout)
  ozma-agent install                  Install as background service
  ozma-agent uninstall                Remove background service
  ozma-agent status                   Check service status
  ozma-agent config                   Show/set configuration
  ozma-agent logs                     Show recent agent logs
"""

from __future__ import annotations

import argparse
import asyncio
import json
import logging
import os
import platform
import sys
from pathlib import Path

CONFIG_DIR = Path.home() / ".config" / "ozma"
CONFIG_FILE = CONFIG_DIR / "agent.json"
LOG_FILE = CONFIG_DIR / "agent.log"


def load_config() -> dict:
    """Load saved agent config."""
    if CONFIG_FILE.exists():
        try:
            return json.loads(CONFIG_FILE.read_text())
        except Exception:
            pass
    return {}


def save_config(config: dict) -> None:
    """Save agent config."""
    CONFIG_DIR.mkdir(parents=True, exist_ok=True)
    CONFIG_FILE.write_text(json.dumps(config, indent=2))


def main() -> None:
    config = load_config()

    p = argparse.ArgumentParser(
        prog="ozma-agent",
        description="Ozma Agent — make any machine part of your ozma mesh",
    )
    sub = p.add_subparsers(dest="command")

    # No subcommand = run with tray icon
    p.add_argument("--name", default=config.get("name", platform.node()),
                   help="Machine name (default: hostname)")
    p.add_argument("--controller", default=config.get("controller", ""),
                   help="Controller URL")
    p.add_argument("--port", type=int, default=config.get("port", 7331),
                   help="UDP listen port")
    p.add_argument("--api-port", type=int, default=config.get("api_port", 7382),
                   help="HTTP API port")
    p.add_argument("--fps", type=int, default=config.get("fps", 15),
                   help="Screen capture FPS")
    p.add_argument("--no-capture", action="store_true", help="Disable screen capture")
    p.add_argument("--no-tray", action="store_true", help="Run without tray icon")
    p.add_argument("--debug", action="store_true")

    # `run` — foreground, logs to stdout, no tray
    run_p = sub.add_parser("run", help="Run in foreground (no tray icon)")
    run_p.add_argument("--name", default=config.get("name", platform.node()))
    run_p.add_argument("--controller", default=config.get("controller", ""))
    run_p.add_argument("--port", type=int, default=config.get("port", 7331))
    run_p.add_argument("--api-port", type=int, default=config.get("api_port", 7382))
    run_p.add_argument("--fps", type=int, default=config.get("fps", 15))
    run_p.add_argument("--no-capture", action="store_true")
    run_p.add_argument("--debug", action="store_true")

    # `install` — background service
    inst_p = sub.add_parser("install", help="Install as background service")
    inst_p.add_argument("--name", default=config.get("name", platform.node()))
    inst_p.add_argument("--controller", required=not config.get("controller"),
                        default=config.get("controller", ""),
                        help="Controller URL")

    # `uninstall`
    sub.add_parser("uninstall", help="Remove background service")

    # `status`
    sub.add_parser("status", help="Check service status")

    # `config`
    cfg_p = sub.add_parser("config", help="Show or set configuration")
    cfg_p.add_argument("--set", nargs=2, metavar=("KEY", "VALUE"), action="append",
                       help="Set a config value (e.g., --set controller https://...)")

    # `logs`
    logs_p = sub.add_parser("logs", help="Show recent agent logs")
    logs_p.add_argument("-n", type=int, default=50, help="Number of lines")

    args = p.parse_args()

    # ── Service commands (no agent, just manage) ──────────────────────

    if args.command == "install":
        from service import install_service
        logging.basicConfig(level=logging.INFO, format="%(message)s")
        # Save config for future runs
        save_config({"name": args.name, "controller": args.controller,
                      "port": getattr(args, "port", 7331),
                      "api_port": getattr(args, "api_port", 7382)})
        ok = install_service(args.name, args.controller)
        print(f"{'Installed' if ok else 'Install failed'}. Use 'ozma-agent status' to check.")
        sys.exit(0 if ok else 1)

    elif args.command == "uninstall":
        from service import uninstall_service
        logging.basicConfig(level=logging.INFO, format="%(message)s")
        ok = uninstall_service()
        print("Uninstalled" if ok else "Uninstall failed")
        sys.exit(0 if ok else 1)

    elif args.command == "status":
        from service import service_status
        s = service_status()
        print(f"Platform:  {s['platform']}")
        print(f"Installed: {'yes' if s['installed'] else 'no'}")
        print(f"Running:   {'yes' if s['running'] else 'no'}")
        cfg = load_config()
        if cfg:
            print(f"Name:      {cfg.get('name', '?')}")
            print(f"Controller:{cfg.get('controller', '?')}")
        sys.exit(0)

    elif args.command == "config":
        if args.set:
            for key, value in args.set:
                config[key] = value
            save_config(config)
            print(f"Config saved to {CONFIG_FILE}")
        else:
            if config:
                for k, v in config.items():
                    print(f"  {k}: {v}")
            else:
                print("No config. Use: ozma-agent config --set controller https://...")
            print(f"\nConfig file: {CONFIG_FILE}")
        sys.exit(0)

    elif args.command == "logs":
        if LOG_FILE.exists():
            lines = LOG_FILE.read_text().splitlines()
            for line in lines[-args.n:]:
                print(line)
        else:
            print(f"No log file at {LOG_FILE}")
        sys.exit(0)

    # ── Run the agent ─────────────────────────────────────────────────

    # Save config from CLI args
    if args.controller:
        config["controller"] = args.controller
        config["name"] = args.name
        save_config(config)

    # Set up logging
    handlers = [logging.StreamHandler()]
    CONFIG_DIR.mkdir(parents=True, exist_ok=True)
    try:
        handlers.append(logging.FileHandler(LOG_FILE, maxBytes=5_000_000, backupCount=2))  # type: ignore
    except Exception:
        try:
            handlers.append(logging.FileHandler(str(LOG_FILE)))
        except Exception:
            pass

    logging.basicConfig(
        level=logging.DEBUG if args.debug else logging.INFO,
        format="%(asctime)s %(levelname)s %(name)s: %(message)s",
        datefmt="%H:%M:%S",
        handlers=handlers,
    )

    # Import here to avoid slow import on service commands
    from ozma_desktop_agent import DesktopSoftNode

    agent = DesktopSoftNode(
        name=args.name,
        port=args.port,
        api_port=args.api_port,
        controller_url=args.controller,
        capture_fps=0 if args.no_capture else args.fps,
    )

    # Decide: tray icon or headless
    use_tray = (args.command is None or args.command != "run") and not getattr(args, "no_tray", False)

    if use_tray:
        from tray import AgentTray
        tray = AgentTray(agent, controller_url=args.controller, name=args.name)
        tray.run()
    else:
        # Foreground, no tray
        loop = asyncio.new_event_loop()
        import signal
        def _on_signal():
            loop.call_soon_threadsafe(agent._stop_event.set)
        for sig in (signal.SIGINT, signal.SIGTERM):
            try:
                loop.add_signal_handler(sig, _on_signal)
            except NotImplementedError:
                pass  # Windows doesn't support add_signal_handler
        try:
            loop.run_until_complete(agent.run())
        except KeyboardInterrupt:
            pass
        finally:
            loop.close()


if __name__ == "__main__":
    main()
