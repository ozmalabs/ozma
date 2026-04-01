# -*- mode: python ; coding: utf-8 -*-
# PyInstaller spec for Ozma Agent (Windows + macOS)
#
# Build on Windows:  pyinstaller agent/installer/ozma-agent.spec
# Build on macOS:    pyinstaller agent/installer/ozma-agent.spec
#
# Output: dist/ozma-agent/ozma-agent.exe (Windows) or dist/ozma-agent (macOS)

import sys
from pathlib import Path

block_cipher = None

agent_dir = Path('agent')
controller_dir = Path('controller')

# Collect all agent modules
agent_modules = [
    str(agent_dir / 'ozma_desktop_agent.py'),
    str(agent_dir / 'ozma_agent.py'),
    str(agent_dir / 'screen_capture.py'),
    str(agent_dir / 'connect_client.py'),
    str(agent_dir / 'prometheus_metrics.py'),
]

# Collect controller modules the agent needs for room correction
controller_modules = [
    str(controller_dir / 'room_correction.py'),
]

# Data files to bundle
datas = [
    # Demo reference tracks for room correction A/B
    (str(controller_dir / 'static' / 'demo_tracks'), 'demo_tracks'),
]

a = Analysis(
    [str(agent_dir / 'cli.py')],
    pathex=[str(agent_dir), str(controller_dir)],
    binaries=[],
    datas=datas,
    hiddenimports=[
        'aiohttp',
        'zeroconf',
        'numpy',
        'json',
        'asyncio',
    ],
    hookspath=[],
    hooksconfig={},
    runtime_hooks=[],
    excludes=[
        'tkinter', 'matplotlib', 'scipy', 'pandas',
        'PIL.ImageTk', 'PyQt5', 'PyQt6',
    ],
    win_no_prefer_redirects=False,
    win_private_assemblies=False,
    cipher=block_cipher,
    noarchive=False,
)

pyz = PYZ(a.pure, a.zipped_data, cipher=block_cipher)

exe = EXE(
    pyz,
    a.scripts,
    [],
    exclude_binaries=True,
    name='ozma-agent',
    debug=False,
    bootloader_ignore_signals=False,
    strip=False,
    upx=True,
    console=True,  # Console app (not windowed) — shows logs
    disable_windowed_traceback=False,
    argv_emulation=False,
    target_arch=None,
    codesign_identity=None,
    entitlements_file=None,
    icon=str(controller_dir / 'static' / 'favicon.ico') if (controller_dir / 'static' / 'favicon.ico').exists() else None,
)

coll = COLLECT(
    exe,
    a.binaries,
    a.zipfiles,
    a.datas,
    strip=False,
    upx=True,
    upx_exclude=[],
    name='ozma-agent',
)
