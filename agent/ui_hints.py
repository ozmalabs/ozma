# SPDX-License-Identifier: AGPL-3.0-only WITH OzmaPluginException
"""
UI hints — report window state, focused element, and text from inside the OS.

When the ozma agent runs inside a machine, it can provide UI state directly
to the controller. This eliminates the need for OCR in most cases:

  Without agent (hardware KVM only):
    HDMI capture → OmniParser/Tesseract → guess what's on screen

  With agent (inside + outside):
    Agent reports: windows, focused control, text, accessibility tree
    Controller uses this directly — OCR only as validation/fallback

Three levels of hints:

  Level 1: Window list + focused window
    Fast, works everywhere. Just enumerate windows and their titles.
    The controller knows which window is on top, its position, its title.

  Level 2: Focused control + text content
    Windows: UI Automation (IUIAutomation)
    macOS: NSAccessibility
    Linux: AT-SPI2 (D-Bus accessibility interface)
    Reports: focused control type, name, value, bounding box.

  Level 3: Accessibility tree (partial)
    Full child elements of the focused window.
    Button labels, text field values, checkbox states, menu items.
    This is what UiPath/UFO use — most reliable element identification.

The controller merges agent hints with hardware capture:
  - Agent says "dialog titled 'Save As' with text field 'filename.txt'"
  - Hardware capture confirms the visual matches
  - Agent provides exact click targets; hardware capture provides verification
"""

from __future__ import annotations

import logging
import platform
from dataclasses import dataclass, field
from typing import Any

log = logging.getLogger("ozma.agent.ui_hints")


@dataclass
class WindowInfo:
    """A window visible on the desktop."""
    title: str
    x: int = 0
    y: int = 0
    width: int = 0
    height: int = 0
    pid: int = 0
    process: str = ""
    focused: bool = False
    minimised: bool = False
    class_name: str = ""

    def to_dict(self) -> dict:
        return {
            "title": self.title, "x": self.x, "y": self.y,
            "width": self.width, "height": self.height,
            "pid": self.pid, "process": self.process,
            "focused": self.focused, "minimised": self.minimised,
            "class_name": self.class_name,
        }


@dataclass
class UIControl:
    """A UI control (button, text field, etc.) from the accessibility tree."""
    control_type: str   # button, text, edit, checkbox, combobox, menu, menuitem, tab, list, listitem, tree, treeitem, window, dialog, label, link, image
    name: str           # visible label or accessible name
    value: str = ""     # text content, checkbox state, etc.
    x: int = 0
    y: int = 0
    width: int = 0
    height: int = 0
    focused: bool = False
    enabled: bool = True
    clickable: bool = False
    children: list["UIControl"] = field(default_factory=list)
    automation_id: str = ""   # Windows UIA AutomationId
    class_name: str = ""

    @property
    def center(self) -> tuple[int, int]:
        return (self.x + self.width // 2, self.y + self.height // 2)

    def to_dict(self) -> dict:
        d: dict[str, Any] = {
            "type": self.control_type, "name": self.name,
            "value": self.value,
            "x": self.x, "y": self.y,
            "width": self.width, "height": self.height,
            "center": list(self.center),
            "focused": self.focused, "enabled": self.enabled,
            "clickable": self.clickable,
        }
        if self.automation_id:
            d["automation_id"] = self.automation_id
        if self.children:
            d["children"] = [c.to_dict() for c in self.children]
        return d


@dataclass
class UIHints:
    """Complete UI state from inside the machine."""
    windows: list[WindowInfo] = field(default_factory=list)
    focused_window: WindowInfo | None = None
    focused_control: UIControl | None = None
    controls: list[UIControl] = field(default_factory=list)  # top-level controls in focused window
    screen_text: str = ""  # aggregated text from all visible controls

    def to_dict(self) -> dict:
        return {
            "windows": [w.to_dict() for w in self.windows],
            "focused_window": self.focused_window.to_dict() if self.focused_window else None,
            "focused_control": self.focused_control.to_dict() if self.focused_control else None,
            "controls": [c.to_dict() for c in self.controls],
            "screen_text": self.screen_text,
        }


class UIHintProvider:
    """
    Collect UI hints from the local OS.

    Platform-specific backends for window enumeration and accessibility.
    """

    def __init__(self) -> None:
        self._platform = platform.system()

    def get_hints(self, level: int = 2) -> UIHints:
        """
        Collect UI hints at the requested level.

        Level 1: windows only
        Level 2: windows + focused control
        Level 3: windows + focused control + child controls
        """
        hints = UIHints()

        # Level 1: Window list
        hints.windows = self._get_windows()
        hints.focused_window = next((w for w in hints.windows if w.focused), None)

        if level >= 2:
            # Level 2: Focused control
            hints.focused_control = self._get_focused_control()

        if level >= 3 and hints.focused_window:
            # Level 3: Child controls of focused window
            hints.controls = self._get_window_controls(hints.focused_window)
            # Aggregate text
            texts = []
            for c in hints.controls:
                if c.name:
                    texts.append(c.name)
                if c.value:
                    texts.append(c.value)
                for child in c.children:
                    if child.name:
                        texts.append(child.name)
                    if child.value:
                        texts.append(child.value)
            hints.screen_text = " ".join(texts)

        return hints

    # ── Windows ───────────────────────────────────────────────────────

    def _get_windows(self) -> list[WindowInfo]:
        if self._platform == "Windows":
            return self._get_windows_win32()
        elif self._platform == "Linux":
            return self._get_windows_linux()
        elif self._platform == "Darwin":
            return self._get_windows_macos()
        return []

    def _get_windows_win32(self) -> list[WindowInfo]:
        try:
            import ctypes
            from ctypes import wintypes

            user32 = ctypes.windll.user32
            kernel32 = ctypes.windll.kernel32

            windows = []
            fg_hwnd = user32.GetForegroundWindow()

            def enum_callback(hwnd, _):
                if not user32.IsWindowVisible(hwnd):
                    return True
                length = user32.GetWindowTextLengthW(hwnd)
                if length == 0:
                    return True

                buf = ctypes.create_unicode_buffer(length + 1)
                user32.GetWindowTextW(hwnd, buf, length + 1)
                title = buf.value

                # Get window rect
                rect = wintypes.RECT()
                user32.GetWindowRect(hwnd, ctypes.byref(rect))

                # Get class name
                cls_buf = ctypes.create_unicode_buffer(256)
                user32.GetClassNameW(hwnd, cls_buf, 256)

                # Get PID
                pid = wintypes.DWORD()
                user32.GetWindowThreadProcessId(hwnd, ctypes.byref(pid))

                # Get process name
                proc_name = ""
                try:
                    h = kernel32.OpenProcess(0x0400 | 0x0010, False, pid.value)
                    if h:
                        name_buf = ctypes.create_unicode_buffer(260)
                        size = wintypes.DWORD(260)
                        kernel32.QueryFullProcessImageNameW(h, 0, name_buf, ctypes.byref(size))
                        proc_name = name_buf.value.split("\\")[-1]
                        kernel32.CloseHandle(h)
                except Exception:
                    pass

                minimised = bool(user32.IsIconic(hwnd))

                windows.append(WindowInfo(
                    title=title,
                    x=rect.left, y=rect.top,
                    width=rect.right - rect.left,
                    height=rect.bottom - rect.top,
                    pid=pid.value,
                    process=proc_name,
                    focused=(hwnd == fg_hwnd),
                    minimised=minimised,
                    class_name=cls_buf.value,
                ))
                return True

            WNDENUMPROC = ctypes.WINFUNCTYPE(ctypes.c_bool, wintypes.HWND, ctypes.POINTER(ctypes.c_int))
            user32.EnumWindows(WNDENUMPROC(enum_callback), 0)
            return windows

        except Exception as e:
            log.debug("Win32 window enum failed: %s", e)
            return []

    def _get_windows_linux(self) -> list[WindowInfo]:
        try:
            import subprocess
            # Use wmctrl if available
            result = subprocess.run(
                ["wmctrl", "-l", "-G", "-p"],
                capture_output=True, text=True, timeout=3,
            )
            windows = []
            for line in result.stdout.splitlines():
                parts = line.split(None, 8)
                if len(parts) >= 9:
                    windows.append(WindowInfo(
                        title=parts[8],
                        x=int(parts[2]), y=int(parts[3]),
                        width=int(parts[4]), height=int(parts[5]),
                        pid=int(parts[1]),
                    ))
            return windows
        except Exception:
            return []

    def _get_windows_macos(self) -> list[WindowInfo]:
        try:
            import subprocess, json
            # Use osascript to get window list
            script = '''
            tell application "System Events"
                set windowList to {}
                repeat with proc in (every process whose visible is true)
                    repeat with win in (every window of proc)
                        set end of windowList to {name of win, position of win, size of win, name of proc}
                    end repeat
                end repeat
                return windowList
            end tell
            '''
            result = subprocess.run(
                ["osascript", "-e", script],
                capture_output=True, text=True, timeout=5,
            )
            # Parse AppleScript output (rough)
            windows = []
            # This is simplified — real impl would use PyObjC
            return windows
        except Exception:
            return []

    # ── Focused control ───────────────────────────────────────────────

    def _get_focused_control(self) -> UIControl | None:
        if self._platform == "Windows":
            return self._get_focused_win32()
        elif self._platform == "Linux":
            return self._get_focused_atspi()
        return None

    def _get_focused_win32(self) -> UIControl | None:
        try:
            import comtypes.client
            uia = comtypes.client.CreateObject(
                "{ff48dba4-60ef-4201-aa87-54103eef594e}",
                interface=comtypes.gen.UIAutomationClient.IUIAutomation,
            )
            focused = uia.GetFocusedElement()
            if not focused:
                return None

            rect = focused.CurrentBoundingRectangle
            ctrl_type_id = focused.CurrentControlType

            # Map UIA control type IDs to names
            type_names = {
                50000: "button", 50004: "edit", 50002: "checkbox",
                50003: "combobox", 50011: "menu", 50012: "menuitem",
                50018: "tab", 50008: "list", 50007: "listitem",
                50023: "text", 50025: "link", 50032: "window",
                50033: "dialog",
            }
            ctrl_type = type_names.get(ctrl_type_id, f"type_{ctrl_type_id}")

            return UIControl(
                control_type=ctrl_type,
                name=focused.CurrentName or "",
                value=getattr(focused, "CurrentValue", "") or "",
                x=rect.left, y=rect.top,
                width=rect.right - rect.left,
                height=rect.bottom - rect.top,
                focused=True,
                enabled=bool(focused.CurrentIsEnabled),
                clickable=ctrl_type in ("button", "link", "checkbox", "menuitem", "tab"),
                automation_id=focused.CurrentAutomationId or "",
                class_name=focused.CurrentClassName or "",
            )
        except Exception as e:
            log.debug("Win32 UIA focus failed: %s", e)
            return None

    def _get_focused_atspi(self) -> UIControl | None:
        try:
            import subprocess
            # Use gdbus to query AT-SPI
            result = subprocess.run(
                ["gdbus", "call", "--session",
                 "--dest", "org.a11y.atspi.Registry",
                 "--object-path", "/org/a11y/atspi/accessible/root",
                 "--method", "org.a11y.atspi.Accessible.GetChildren"],
                capture_output=True, text=True, timeout=2,
            )
            # Simplified — real impl would walk the tree
            return None
        except Exception:
            return None

    # ── Window controls (Level 3) ─────────────────────────────────────

    def _get_window_controls(self, window: WindowInfo, max_depth: int = 3) -> list[UIControl]:
        if self._platform == "Windows":
            return self._get_controls_win32(window, max_depth)
        return []

    def _get_controls_win32(self, window: WindowInfo, max_depth: int) -> list[UIControl]:
        try:
            import ctypes
            import comtypes.client

            uia = comtypes.client.CreateObject(
                "{ff48dba4-60ef-4201-aa87-54103eef594e}",
                interface=comtypes.gen.UIAutomationClient.IUIAutomation,
            )

            # Find the window element
            user32 = ctypes.windll.user32
            hwnd = user32.FindWindowW(None, window.title)
            if not hwnd:
                return []

            root = uia.ElementFromHandle(hwnd)
            if not root:
                return []

            return self._walk_uia(uia, root, max_depth)

        except Exception as e:
            log.debug("Win32 UIA tree walk failed: %s", e)
            return []

    def _walk_uia(self, uia: Any, element: Any, depth: int) -> list[UIControl]:
        if depth <= 0:
            return []

        controls = []
        try:
            tree_walker = uia.ControlViewWalker
            child = tree_walker.GetFirstChildElement(element)

            while child:
                try:
                    rect = child.CurrentBoundingRectangle
                    ctrl_type_id = child.CurrentControlType
                    type_names = {
                        50000: "button", 50004: "edit", 50002: "checkbox",
                        50003: "combobox", 50011: "menu", 50012: "menuitem",
                        50018: "tab", 50008: "list", 50007: "listitem",
                        50023: "text", 50025: "link", 50032: "window",
                    }
                    ctrl_type = type_names.get(ctrl_type_id, f"type_{ctrl_type_id}")

                    ctrl = UIControl(
                        control_type=ctrl_type,
                        name=child.CurrentName or "",
                        value="",
                        x=rect.left, y=rect.top,
                        width=rect.right - rect.left,
                        height=rect.bottom - rect.top,
                        focused=bool(child.CurrentHasKeyboardFocus),
                        enabled=bool(child.CurrentIsEnabled),
                        clickable=ctrl_type in ("button", "link", "checkbox", "menuitem", "tab", "listitem"),
                        automation_id=child.CurrentAutomationId or "",
                        class_name=child.CurrentClassName or "",
                    )

                    # Recurse into children
                    if depth > 1:
                        ctrl.children = self._walk_uia(uia, child, depth - 1)

                    controls.append(ctrl)
                except Exception:
                    pass

                child = tree_walker.GetNextSiblingElement(child)
        except Exception:
            pass

        return controls
