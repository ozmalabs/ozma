# SPDX-License-Identifier: AGPL-3.0-only WITH OzmaPluginException
from setuptools import setup, find_packages

setup(
    name="ozma-agent",
    version="0.1.0",
    description="Ozma Agent — make any PC part of your ozma mesh",
    long_description=open("../README.md").read() if __import__("pathlib").Path("../README.md").exists() else "",
    long_description_content_type="text/markdown",
    author="Ozma Labs Pty Ltd",
    license="AGPL-3.0",
    packages=find_packages(),
    py_modules=["cli", "ozma_desktop_agent", "ozma_agent", "screen_capture",
                "connect_client", "prometheus_metrics", "service", "tray"],
    packages=find_packages(),
    install_requires=[
        "aiohttp>=3.8",
        "zeroconf>=0.132",
    ],
    extras_require={
        "linux": ["evdev>=1.7"],
        "gui": ["pystray>=0.19", "Pillow>=9.0"],
        "macos": ["rumps>=0.4", "pyobjc-framework-Quartz>=9.0"],
        "windows": ["pystray>=0.19", "Pillow>=9.0", "dxcam>=0.0.5"],
        "multiseat": ["numpy>=1.24", "aiortc>=1.6", "Pillow>=9.0"],
        "full": ["numpy>=1.24", "pynacl>=1.5", "pystray>=0.19", "Pillow>=9.0",
                 "aiortc>=1.6", "dxcam>=0.0.5"],
    },
    entry_points={
        "console_scripts": [
            "ozma-agent=cli:main",
        ],
    },
    python_requires=">=3.11",
    classifiers=[
        "Development Status :: 3 - Alpha",
        "License :: OSI Approved :: GNU Affero General Public License v3",
        "Operating System :: POSIX :: Linux",
        "Operating System :: MacOS",
        "Operating System :: Microsoft :: Windows",
        "Programming Language :: Python :: 3.11",
        "Programming Language :: Python :: 3.12",
        "Programming Language :: Python :: 3.13",
        "Topic :: System :: Networking",
    ],
)
