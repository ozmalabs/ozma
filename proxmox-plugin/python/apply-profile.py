#!/usr/bin/python3
"""Apply an ozma VM profile to a Proxmox VM configuration."""

import argparse
import json
import sys
from pathlib import Path

sys.path.insert(0, str(Path(__file__).parent))
from vm_profiles import VMProfile


def main():
    p = argparse.ArgumentParser()
    p.add_argument("--profile", required=True, choices=["gaming", "workstation", "server", "media"])
    p.add_argument("--vmid", required=True, type=int)
    p.add_argument("--gpu", default="")
    p.add_argument("--cores", type=int, default=0)
    p.add_argument("--memory", type=int, default=0)
    p.add_argument("--dry-run", action="store_true")
    args = p.parse_args()

    # Generate profile
    if args.profile == "gaming":
        profile = VMProfile.gaming(args.vmid, gpu_pci=args.gpu,
                                    cores=args.cores or 8,
                                    memory_mb=args.memory or 16384)
    elif args.profile == "workstation":
        profile = VMProfile.workstation(args.vmid,
                                         cores=args.cores or 4,
                                         memory_mb=args.memory or 8192)
    elif args.profile == "server":
        profile = VMProfile.server(args.vmid,
                                    cores=args.cores or 2,
                                    memory_mb=args.memory or 4096)
    elif args.profile == "media":
        profile = VMProfile.media(args.vmid,
                                   cores=args.cores or 4,
                                   memory_mb=args.memory or 8192)

    if args.dry_run:
        print(json.dumps(profile.to_dict(), indent=2))
        print("\n# QEMU args:")
        print(" ".join(profile.qemu_args()))
        print("\n# Host setup:")
        for cmd in profile.host_setup_commands():
            print(f"  {cmd}")
        print("\n# Proxmox conf:")
        for line in profile.proxmox_conf_lines():
            print(f"  {line}")
        return

    # Apply to Proxmox VM config
    conf_path = Path(f"/etc/pve/qemu-server/{args.vmid}.conf")
    if not conf_path.exists():
        print(f"VM {args.vmid} config not found", file=sys.stderr)
        sys.exit(1)

    # Read existing config, add ozma section
    lines = conf_path.read_text().splitlines()

    # Remove existing ozma lines
    lines = [l for l in lines if not l.startswith("# Ozma") and not l.startswith("#ozma:")]

    # Add profile lines
    lines.extend(profile.proxmox_conf_lines())

    # Add ozma metadata as a comment
    lines.append(f"#ozma: {json.dumps(profile.to_dict())}")

    conf_path.write_text("\n".join(lines) + "\n")
    print(f"Profile '{args.profile}' applied to VM {args.vmid}")

    # Run host setup commands
    for cmd in profile.host_setup_commands():
        if cmd.startswith("#"):
            continue
        print(f"  Running: {cmd}")
        import subprocess
        subprocess.run(cmd, shell=True, check=False)


if __name__ == "__main__":
    main()
