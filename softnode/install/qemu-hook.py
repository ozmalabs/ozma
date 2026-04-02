#!/usr/bin/env python3
"""
Libvirt QEMU hook — Ozma integration.

Automatically injects a dedicated QMP monitor socket into every VM at
launch time. This gives ozma's soft node a direct, low-latency (1-2ms)
control channel for input injection and screen capture — without
modifying any VM's permanent configuration.

Install: sudo cp softnode/install/qemu-hook.py /etc/libvirt/hooks/qemu
         sudo chmod +x /etc/libvirt/hooks/qemu

How it works:
  - At prepare/begin, libvirt pipes the domain XML to stdin
  - We inject a qemu:commandline block with a chardev socket + monitor
  - The modified XML goes to stdout; libvirt uses it for this launch only
  - The VM's saved config is never touched

The QMP socket appears at /run/ozma/qmp/<vm-name>.sock and is owned by
root:kvm so the ozma soft node can connect directly.
"""

import os
import sys
import syslog

VM_NAME = sys.argv[1] if len(sys.argv) > 1 else ""
OPERATION = sys.argv[2] if len(sys.argv) > 2 else ""
SUB_OP = sys.argv[3] if len(sys.argv) > 3 else ""

QMP_DIR = "/run/ozma/qmp"


def log(msg: str) -> None:
    syslog.syslog(syslog.LOG_INFO, f"ozma-hook: {msg}")


def inject_qmp_socket() -> None:
    """Read domain XML from stdin, inject QMP socket, write to stdout."""
    os.makedirs(QMP_DIR, mode=0o775, exist_ok=True)
    try:
        os.chown(QMP_DIR, 0, _kvm_gid())
    except Exception:
        pass

    sock_path = f"{QMP_DIR}/{VM_NAME}.sock"
    xml = sys.stdin.read()

    if "ozma-mon" in xml:
        # Already has our QMP socket (e.g. manually configured)
        sys.stdout.write(xml)
        return

    ns = "xmlns:qemu='http://libvirt.org/schemas/domain/qemu/1.0'"
    qemu_args = f"""  <qemu:commandline>
    <qemu:arg value='-chardev'/>
    <qemu:arg value='socket,id=ozma-mon,path={sock_path},server=on,wait=off'/>
    <qemu:arg value='-mon'/>
    <qemu:arg value='chardev=ozma-mon,mode=control'/>
  </qemu:commandline>"""

    # Add namespace declaration if not present
    if ns not in xml:
        xml = xml.replace("type='kvm'", f"type='kvm' {ns}", 1)

    # Inject before </domain>
    xml = xml.replace("</domain>", f"{qemu_args}\n</domain>")

    sys.stdout.write(xml)
    sys.stdout.flush()
    log(f"injected QMP socket {sock_path} for {VM_NAME}")


def rebar_fix() -> None:
    """Resize RX 6600 XT BAR0 for VFIO passthrough."""
    if VM_NAME != "ozma-game-test":
        return
    resize = "/sys/bus/pci/devices/0000:31:00.0/resource0_resize"
    if os.path.exists(resize):
        try:
            with open(resize, "w") as f:
                f.write("8")
            log(f"resized BAR0 to 256MB for {VM_NAME}")
        except Exception:
            pass


def cleanup() -> None:
    """Remove QMP socket on VM stop."""
    sock = f"{QMP_DIR}/{VM_NAME}.sock"
    try:
        os.unlink(sock)
    except FileNotFoundError:
        pass
    log(f"cleaned up QMP socket for {VM_NAME}")


def _kvm_gid() -> int:
    import grp
    try:
        return grp.getgrnam("kvm").gr_gid
    except KeyError:
        return 0


if __name__ == "__main__":
    if OPERATION == "prepare" and SUB_OP == "begin":
        inject_qmp_socket()
    elif OPERATION == "start" and SUB_OP == "begin":
        rebar_fix()
    elif OPERATION == "stopped" and SUB_OP == "end":
        cleanup()
