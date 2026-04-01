# SPDX-License-Identifier: AGPL-3.0-only WITH OzmaPluginException
"""
Plugin / extension API — third-party extensions without forking.

Plugins can add:
  - Control surface drivers (new input devices)
  - Screen drivers (new display outputs)
  - Overlay sources (new video sources)
  - Metric sources (new data providers)
  - Automation commands (new DSL commands)
  - Widget types (new screen widgets)
  - Widget packs (new visual themes)
  - Description packs (new sensor personality)
  - Wallpaper sources (new background providers)
  - Export targets (new monitoring integrations)

Plugin structure (controller/plugins/{plugin_id}/):
  manifest.json     — metadata, capabilities, entry point
  plugin.py         — Python module loaded at startup
  requirements.txt  — pip dependencies (optional)
  static/           — web assets (optional)
  README.md         — documentation

manifest.json:
  {
    "id": "my-plugin",
    "name": "My Custom Plugin",
    "version": "1.0.0",
    "author": "community",
    "description": "Adds XYZ capability",
    "entry_point": "plugin.py",
    "capabilities": ["control_surface", "metric_source"],
    "ozma_min_version": "1.0.0"
  }

Plugin entry point (plugin.py):
  def register(ozma):
      # ozma is the plugin context with access to all subsystems
      ozma.register_control_surface(MyCustomSurface())
      ozma.register_metric_source("my-metrics", my_collector)
      ozma.register_automation_command("my-command", my_handler)
"""

from __future__ import annotations

import importlib.util
import json
import logging
import sys
from dataclasses import dataclass, field
from pathlib import Path
from typing import Any

log = logging.getLogger("ozma.plugins")

PLUGINS_DIR = Path(__file__).parent / "plugins"


@dataclass
class PluginManifest:
    """Plugin metadata."""
    id: str
    name: str
    version: str = "1.0.0"
    author: str = ""
    description: str = ""
    entry_point: str = "plugin.py"
    capabilities: list[str] = field(default_factory=list)
    ozma_min_version: str = "1.0.0"
    enabled: bool = True
    loaded: bool = False
    error: str = ""

    def to_dict(self) -> dict[str, Any]:
        return {
            "id": self.id, "name": self.name, "version": self.version,
            "author": self.author, "description": self.description,
            "capabilities": self.capabilities,
            "enabled": self.enabled, "loaded": self.loaded,
            "error": self.error,
        }


class PluginContext:
    """
    Context object passed to plugins during registration.

    Provides access to ozma subsystems for extending functionality.
    """

    def __init__(self) -> None:
        self._control_surfaces: list[Any] = []
        self._metric_sources: list[tuple[str, Any]] = []
        self._automation_commands: dict[str, Any] = {}
        self._overlay_sources: list[Any] = []
        self._screen_drivers: list[Any] = []
        self._export_targets: list[Any] = []

    def register_control_surface(self, surface: Any) -> None:
        self._control_surfaces.append(surface)

    def register_metric_source(self, source_id: str, collector: Any) -> None:
        self._metric_sources.append((source_id, collector))

    def register_automation_command(self, command: str, handler: Any) -> None:
        self._automation_commands[command] = handler

    def register_overlay_source(self, source: Any) -> None:
        self._overlay_sources.append(source)

    def register_screen_driver(self, driver: Any) -> None:
        self._screen_drivers.append(driver)

    def register_export_target(self, target: Any) -> None:
        self._export_targets.append(target)


class PluginManager:
    """
    Loads and manages plugins from the plugins/ directory.
    """

    def __init__(self) -> None:
        self._plugins: dict[str, PluginManifest] = {}
        self._context = PluginContext()

    @property
    def context(self) -> PluginContext:
        return self._context

    def load_all(self) -> int:
        """Load all plugins from the plugins/ directory."""
        if not PLUGINS_DIR.exists():
            PLUGINS_DIR.mkdir(parents=True, exist_ok=True)
            return 0

        count = 0
        for plugin_dir in sorted(PLUGINS_DIR.iterdir()):
            if not plugin_dir.is_dir():
                continue
            manifest_path = plugin_dir / "manifest.json"
            if not manifest_path.exists():
                continue

            try:
                data = json.loads(manifest_path.read_text())
                manifest = PluginManifest(**{k: v for k, v in data.items() if hasattr(PluginManifest, k)})
                manifest.id = manifest.id or plugin_dir.name

                if not manifest.enabled:
                    self._plugins[manifest.id] = manifest
                    continue

                # Load the plugin module
                entry = plugin_dir / manifest.entry_point
                if entry.exists():
                    spec = importlib.util.spec_from_file_location(
                        f"ozma_plugin_{manifest.id}", str(entry)
                    )
                    if spec and spec.loader:
                        module = importlib.util.module_from_spec(spec)
                        sys.modules[f"ozma_plugin_{manifest.id}"] = module
                        spec.loader.exec_module(module)

                        # Call register() if it exists
                        if hasattr(module, "register"):
                            module.register(self._context)

                        manifest.loaded = True
                        count += 1
                        log.info("Plugin loaded: %s v%s (%s)",
                                 manifest.name, manifest.version,
                                 ", ".join(manifest.capabilities))

                self._plugins[manifest.id] = manifest

            except Exception as e:
                log.warning("Failed to load plugin %s: %s", plugin_dir.name, e)
                if plugin_dir.name not in self._plugins:
                    self._plugins[plugin_dir.name] = PluginManifest(
                        id=plugin_dir.name, name=plugin_dir.name,
                        error=str(e), loaded=False,
                    )

        return count

    def list_plugins(self) -> list[dict]:
        return [p.to_dict() for p in self._plugins.values()]

    def get_plugin(self, plugin_id: str) -> PluginManifest | None:
        return self._plugins.get(plugin_id)

    def enable_plugin(self, plugin_id: str) -> bool:
        p = self._plugins.get(plugin_id)
        if p:
            p.enabled = True
            return True
        return False

    def disable_plugin(self, plugin_id: str) -> bool:
        p = self._plugins.get(plugin_id)
        if p:
            p.enabled = False
            return True
        return False
