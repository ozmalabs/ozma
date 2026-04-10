# SPDX-License-Identifier: AGPL-3.0-only WITH OzmaPluginException
from setuptools import setup, find_packages

setup(
    name="ozma-softnode",
    version="0.1.0",
    description="Ozma Soft Node — virtual node for QEMU/KVM/Proxmox VMs (runs OUTSIDE the VM)",
    long_description=open("README_SOFTNODE.md").read() if __import__("pathlib").Path("README_SOFTNODE.md").exists() else "",
    long_description_content_type="text/markdown",
    author="Ozma Labs Pty Ltd",
    license="AGPL-3.0",
    packages=find_packages(),
    py_modules=["soft_node", "virtual_node", "hyperv_node", "virtual_capture",
                "qmp_client", "hid_to_qmp", "connect_client",
                "screen_capture", "prometheus_metrics",
                "meeting_detect", "wallpaper", "mesh_interface"],
    install_requires=[
        "aiohttp>=3.8",
        "zeroconf>=0.132",
    ],
    extras_require={
        "libvirt": ["libvirt-python>=10.0"],
        "hyperv": ["pywin32>=306"],   # Windows only; WMI event subscriptions
    },
    entry_points={
        "console_scripts": [
            "ozma-softnode=soft_node:main",
            "ozma-virtual-node=virtual_node:main",
            "ozma-hyperv-node=hyperv_node:main",
        ],
    },
    python_requires=">=3.11",
    classifiers=[
        "Development Status :: 3 - Alpha",
        "License :: OSI Approved :: GNU Affero General Public License v3",
        "Operating System :: POSIX :: Linux",
        "Operating System :: Microsoft :: Windows",
        "Programming Language :: Python :: 3.11",
        "Programming Language :: Python :: 3.12",
        "Programming Language :: Python :: 3.13",
        "Topic :: System :: Networking",
    ],
)
