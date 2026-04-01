#!/bin/bash
# Ozma Windows VM Provisioning — Full Dogfooding
#
# Everything goes through ozma:
#   - VM managed by soft node (QMP + VNC)
#   - Virtio drivers delivered via USB mass storage (QMP hotplug)
#   - Agent installer delivered via USB mass storage
#   - Windows install is unattended (autounattend.xml on floppy)
#   - Post-install agent setup triggered via ozma automation
#   - Agent registers back with the controller — full loop
#
# Usage:
#   bash dev/windows-vm/provision.sh create
#   bash dev/windows-vm/provision.sh start
#   bash dev/windows-vm/provision.sh stop
#   bash dev/windows-vm/provision.sh attach-media   # hotplug USB drives
#   bash dev/windows-vm/provision.sh install-agent   # trigger agent install via RPA

set -e

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
REPO_DIR="$(cd "$SCRIPT_DIR/../.." && pwd)"
IMAGES_DIR="$REPO_DIR/images"

VM_NAME="${VM_NAME:-win10}"
DISK_SIZE="${DISK_SIZE:-40G}"
RAM="${RAM:-4G}"
CPUS="${CPUS:-4}"
VNC_SOCKET="${VNC_SOCKET:-/tmp/ozma-vnc-${VM_NAME}.sock}"
V4L2_DEVICE="${V4L2_DEVICE:-/dev/video10}"
V4L2_BRIDGE_PID="/tmp/ozma-v4l2-${VM_NAME}.pid"
QMP_SOCKET="${QMP_SOCKET:-/tmp/ozma-${VM_NAME}.qmp}"
SOFTNODE_PORT="${SOFTNODE_PORT:-7340}"
CONTROLLER_URL="${CONTROLLER_URL:-https://ozma.hrdwrbob.net}"
WINDOWS_ISO="${WINDOWS_ISO:-$HOME/Win10_22H2_English_x64v1.iso}"

DISK_IMAGE="$IMAGES_DIR/${VM_NAME}.qcow2"
VIRTIO_ISO="$IMAGES_DIR/virtio-win.iso"
MEDIA_DIR="$IMAGES_DIR/ozma-media-${VM_NAME}"   # host directory → becomes USB drive
MEDIA_IMG="$IMAGES_DIR/ozma-media-${VM_NAME}.img"  # synthesised FAT32
PID_FILE="/tmp/ozma-${VM_NAME}.pid"
SOFTNODE_PID="/tmp/ozma-softnode-${VM_NAME}.pid"
SOFTNODE_LOG="/tmp/ozma-softnode-${VM_NAME}.log"
NO_NETWORK="${NO_NETWORK:-false}"

[ -f "$SCRIPT_DIR/vm.conf" ] && source "$SCRIPT_DIR/vm.conf"

# Python — use venv if available
PYTHON="${REPO_DIR}/.venv/bin/python3"
[ ! -x "$PYTHON" ] && PYTHON="python3"

# ── Build media images ────────────────────────────────────────────────────────

GADGET_PID=""

build_usb_media() {
    # If prepare-media.sh has been run, the media directory already has everything.
    # If not, run it now.
    if [ ! -d "$MEDIA_DIR/ozma" ] || [ ! -d "$MEDIA_DIR/python" ]; then
        echo "Media directory not prepared. Running prepare-media.sh..."
        MEDIA_DIR="$MEDIA_DIR" CONTROLLER_URL="$CONTROLLER_URL" \
            bash "$SCRIPT_DIR/prepare-media.sh"
    else
        echo "Media directory ready: $MEDIA_DIR"
        echo "$CONTROLLER_URL" > "$MEDIA_DIR/ozma/controller.txt"
    fi

    # Start the userspace USB mass storage gadget.
    # The Python process IS the USB drive — no image files, no NBD.
    # FATSynthesiser computes sectors on demand from the host directory.
    # FunctionFS + dummy_hcd presents it as a real USB device.
    echo "Starting USB mass storage gadget (FAT32 from directory, no files)..."
    sudo "$PYTHON" -c "
import sys, time, logging
sys.path.insert(0, '$REPO_DIR/softnode')
logging.basicConfig(level=logging.INFO, format='%(asctime)s %(levelname)s %(name)s: %(message)s', datefmt='%H:%M:%S')
from virtual_media import FATSynthesiser
from usb_mass_storage import MassStorageGadget

synth = FATSynthesiser('$MEDIA_DIR', label='OZMA')
synth.scan()
gadget = MassStorageGadget(synth, udc='dummy_udc.0', product_name='Ozma Media')
gadget.start()
# Run until killed
try:
    while True:
        time.sleep(1)
except KeyboardInterrupt:
    gadget.stop()
" > /tmp/ozma-gadget-${VM_NAME}.log 2>&1 &
    GADGET_PID=$!
    echo "USB gadget PID: $GADGET_PID"
    sleep 3

    if ! kill -0 $GADGET_PID 2>/dev/null; then
        echo "USB gadget failed. Log:"
        cat /tmp/ozma-gadget-${VM_NAME}.log
        echo ""
        echo "Ensure: sudo modprobe libcomposite && sudo insmod dev/dummy_hcd/dummy_hcd.ko num=1"
        GADGET_PID=""
        return 1
    fi

    # Wait for the USB device to enumerate on the host
    echo "Waiting for USB device to enumerate..."
    USB_BUS=""
    USB_DEV=""
    for i in $(seq 1 10); do
        USB_LINE=$(lsusb | grep "1d6b:0104" | head -1)
        if [ -n "$USB_LINE" ]; then
            USB_BUS=$(echo "$USB_LINE" | awk '{print int($2)}')
            USB_DEV=$(echo "$USB_LINE" | awk '{gsub(/:/, "", $4); print int($4)}')
            echo "USB device found: bus $USB_BUS device $USB_DEV"
            # Fix permissions so QEMU can open the device without root
            sudo chmod 666 "/dev/bus/usb/$(printf '%03d' $USB_BUS)/$(printf '%03d' $USB_DEV)"
            break
        fi
        sleep 1
    done
    if [ -z "$USB_BUS" ]; then
        echo "WARNING: USB gadget device not found in lsusb"
        echo "QEMU will start without USB passthrough"
    fi
}

build_unattend() {
    echo "Writing autounattend.xml to media directory..."
    echo "(Windows scans all USB drives for this file)"

    mkdir -p "$MEDIA_DIR"

    # autounattend.xml — Windows scans all removable USB drives for this.
    # No floppy needed. It's on the same USB drive as the virtio drivers.
    cat > "$MEDIA_DIR/autounattend.xml" << 'XMLEOF'
<?xml version="1.0" encoding="utf-8"?>
<unattend xmlns="urn:schemas-microsoft-com:unattend">
  <settings pass="windowsPE">
    <component name="Microsoft-Windows-International-Core-WinPE" processorArchitecture="amd64" publicKeyToken="31bf3856ad364e35" language="neutral" versionScope="nonSxS" xmlns:wcm="http://schemas.microsoft.com/WMIConfig/2002/State" xmlns:xsi="http://www.w3.org/2001/XMLSchema-instance">
      <SetupUILanguage>
        <UILanguage>en-US</UILanguage>
      </SetupUILanguage>
      <InputLocale>en-US</InputLocale>
      <SystemLocale>en-US</SystemLocale>
      <UILanguage>en-US</UILanguage>
      <UserLocale>en-US</UserLocale>
    </component>
    <component name="Microsoft-Windows-Setup" processorArchitecture="amd64" publicKeyToken="31bf3856ad364e35" language="neutral" versionScope="nonSxS" xmlns:wcm="http://schemas.microsoft.com/WMIConfig/2002/State" xmlns:xsi="http://www.w3.org/2001/XMLSchema-instance">
      <DiskConfiguration>
        <Disk wcm:action="add">
          <CreatePartitions>
            <CreatePartition wcm:action="add">
              <Order>1</Order>
              <Size>350</Size>
              <Type>Primary</Type>
            </CreatePartition>
            <CreatePartition wcm:action="add">
              <Order>2</Order>
              <Extend>true</Extend>
              <Type>Primary</Type>
            </CreatePartition>
          </CreatePartitions>
          <ModifyPartitions>
            <ModifyPartition wcm:action="add">
              <Order>1</Order>
              <PartitionID>1</PartitionID>
              <Label>System</Label>
              <Format>NTFS</Format>
              <Active>true</Active>
            </ModifyPartition>
            <ModifyPartition wcm:action="add">
              <Order>2</Order>
              <PartitionID>2</PartitionID>
              <Format>NTFS</Format>
              <Label>Windows</Label>
            </ModifyPartition>
          </ModifyPartitions>
          <DiskID>0</DiskID>
          <WillWipeDisk>true</WillWipeDisk>
        </Disk>
        <WillShowUI>OnError</WillShowUI>
      </DiskConfiguration>
      <ImageInstall>
        <OSImage>
          <InstallTo>
            <DiskID>0</DiskID>
            <PartitionID>2</PartitionID>
          </InstallTo>
        </OSImage>
      </ImageInstall>
      <UserData>
        <ProductKey>
          <Key>W269N-WFGWX-YVC9B-4J6C9-T83GX</Key>
        </ProductKey>
        <AcceptEula>true</AcceptEula>
      </UserData>
    </component>
  </settings>
  <settings pass="oobeSystem">
    <component name="Microsoft-Windows-Shell-Setup" processorArchitecture="amd64" publicKeyToken="31bf3856ad364e35" language="neutral" versionScope="nonSxS" xmlns:wcm="http://schemas.microsoft.com/WMIConfig/2002/State" xmlns:xsi="http://www.w3.org/2001/XMLSchema-instance">
      <OOBE>
        <HideEULAPage>true</HideEULAPage>
        <HideOnlineAccountScreens>true</HideOnlineAccountScreens>
        <HideWirelessSetupInOOBE>true</HideWirelessSetupInOOBE>
        <ProtectYourPC>3</ProtectYourPC>
      </OOBE>
      <UserAccounts>
        <LocalAccounts>
          <LocalAccount wcm:action="add">
            <Name>ozma</Name>
            <Group>Administrators</Group>
            <Password>
              <Value>ozma</Value>
              <PlainText>true</PlainText>
            </Password>
          </LocalAccount>
        </LocalAccounts>
      </UserAccounts>
      <AutoLogon>
        <Enabled>true</Enabled>
        <Username>ozma</Username>
        <Password>
          <Value>ozma</Value>
          <PlainText>true</PlainText>
        </Password>
        <LogonCount>3</LogonCount>
      </AutoLogon>
      <FirstLogonCommands>
        <SynchronousCommand wcm:action="add">
          <Order>1</Order>
          <CommandLine>cmd /c "for %%d in (D E F G) do if exist %%d:\ozma\install-offline.bat start /wait %%d:\ozma\install-offline.bat"</CommandLine>
          <Description>Install Ozma Agent</Description>
        </SynchronousCommand>
      </FirstLogonCommands>
    </component>
  </settings>
</unattend>
XMLEOF

    echo "autounattend.xml written to $MEDIA_DIR/"
}

download_virtio() {
    if [ ! -f "$VIRTIO_ISO" ]; then
        echo "Downloading virtio-win drivers..."
        curl -L -o "$VIRTIO_ISO" \
            "https://fedorapeople.org/groups/virt/virtio-win/direct-downloads/stable-virtio/virtio-win.iso"
    fi
}

start_pipewire() {
    # Each soft node runs its own isolated PipeWire instance.
    # This is the audio equivalent of having separate hardware —
    # the VM's audio goes to this PipeWire, not the host's.
    local pw_dir="/tmp/ozma-pw-${VM_NAME}"
    mkdir -p "$pw_dir"

    echo "Starting isolated PipeWire for $VM_NAME..."
    PIPEWIRE_RUNTIME_DIR="$pw_dir" \
    XDG_RUNTIME_DIR="$pw_dir" \
        pipewire -c pipewire.conf > "$pw_dir/pipewire.log" 2>&1 &
    echo $! > "$pw_dir/pipewire.pid"

    # Also start wireplumber for this instance
    sleep 1
    PIPEWIRE_RUNTIME_DIR="$pw_dir" \
    XDG_RUNTIME_DIR="$pw_dir" \
        wireplumber > "$pw_dir/wireplumber.log" 2>&1 &
    echo $! > "$pw_dir/wireplumber.pid"

    # Export for QEMU and soft node to use
    export PIPEWIRE_RUNTIME_DIR="$pw_dir"
    export XDG_RUNTIME_DIR_PW="$pw_dir"

    sleep 1
    echo "Isolated PipeWire running in $pw_dir"
}

stop_pipewire() {
    local pw_dir="/tmp/ozma-pw-${VM_NAME}"
    [ -f "$pw_dir/wireplumber.pid" ] && kill "$(cat $pw_dir/wireplumber.pid)" 2>/dev/null
    [ -f "$pw_dir/pipewire.pid" ] && kill "$(cat $pw_dir/pipewire.pid)" 2>/dev/null
    rm -rf "$pw_dir"
}

start_v4l2_bridge() {
    # Ensure v4l2loopback is loaded
    if [ ! -e "$V4L2_DEVICE" ]; then
        echo "Loading v4l2loopback..."
        sudo modprobe v4l2loopback devices=1 video_nr=${V4L2_DEVICE##*/dev/video} \
            card_label="Ozma-${VM_NAME}" exclusive_caps=1 2>/dev/null || true
        sleep 1
    fi

    if [ ! -e "$V4L2_DEVICE" ]; then
        echo "WARNING: v4l2loopback not available at $V4L2_DEVICE"
        echo "Install: sudo apt install v4l2loopback-dkms"
        echo "Falling back to VNC-based stream"
        return 1
    fi

    # Start the QEMU→v4l2 bridge
    BRIDGE="$REPO_DIR/dev/qemu-v4l2/ozma-qemu-v4l2"
    if [ ! -x "$BRIDGE" ]; then
        echo "Building v4l2 bridge..."
        bash "$REPO_DIR/dev/qemu-v4l2/build.sh"
    fi

    echo "Starting QEMU→v4l2 bridge: $VNC_SOCKET → $V4L2_DEVICE"
    "$BRIDGE" --vnc="$VNC_SOCKET" --device="$V4L2_DEVICE" \
        > /tmp/ozma-v4l2-${VM_NAME}.log 2>&1 &
    echo $! > "$V4L2_BRIDGE_PID"
    echo "v4l2 bridge PID: $(cat $V4L2_BRIDGE_PID)"
    sleep 1
    return 0
}

start_softnode() {
    echo "Starting soft node..."
    local pw_dir="/tmp/ozma-pw-${VM_NAME}"

    # Start v4l2 bridge (QEMU framebuffer → v4l2loopback)
    if start_v4l2_bridge; then
        echo "Display: QEMU → unix socket → v4l2 bridge → $V4L2_DEVICE"
    else
        echo "Display: no v4l2loopback — stream unavailable"
    fi

    # Soft node args depend on what's available
    local node_args=(
        --name "$VM_NAME"
        --port "$SOFTNODE_PORT"
        --qmp "$QMP_SOCKET"
        --audio-sink "ozma-${VM_NAME}"
    )

    # Use TCP VNC for the stream — the controller's StreamManager connects to it
    VNC_HOST=$(ip -4 route get 1.0.0.0 2>/dev/null | awk '{print $7; exit}')
    [ -z "$VNC_HOST" ] && VNC_HOST="127.0.0.1"
    node_args+=(--vnc-host "$VNC_HOST" --vnc-port 5931)

    PIPEWIRE_RUNTIME_DIR="$pw_dir" \
    XDG_RUNTIME_DIR="$pw_dir" \
    "$PYTHON" "$REPO_DIR/softnode/soft_node.py" \
        "${node_args[@]}" \
        > "$SOFTNODE_LOG" 2>&1 &
    echo $! > "$SOFTNODE_PID"
    echo "Soft node PID: $(cat $SOFTNODE_PID) (log: $SOFTNODE_LOG)"
}

# ── Commands ──────────────────────────────────────────────────────────────────

cmd_create() {
    echo "=== Creating Windows VM: $VM_NAME ==="

    # Clean up any stale state from previous runs
    [ -f "$PID_FILE" ] && kill "$(cat $PID_FILE 2>/dev/null)" 2>/dev/null || true
    [ -f "$SOFTNODE_PID" ] && kill "$(cat $SOFTNODE_PID 2>/dev/null)" 2>/dev/null || true
    [ -f "$V4L2_BRIDGE_PID" ] && kill "$(cat $V4L2_BRIDGE_PID 2>/dev/null)" 2>/dev/null || true
    pkill -f "usb_mass_storage.*MassStorageGadget" 2>/dev/null || true
    pkill -f "ozma-qemu-v4l2.*${VM_NAME}" 2>/dev/null || true
    # Clean stale ConfigFS gadgets
    sudo bash -c 'for g in /sys/kernel/config/usb_gadget/g_*; do
        [ -d "$g" ] || continue
        echo "" > "$g/UDC" 2>/dev/null
        for cfg in "$g"/configs/*/; do
            [ -d "$cfg" ] || continue
            for link in "$cfg"*; do [ -L "$link" ] && rm "$link" 2>/dev/null; done
            rmdir "$cfg/strings/0x409" 2>/dev/null; rmdir "$cfg" 2>/dev/null
        done
        for func in "$g"/functions/*/; do [ -d "$func" ] && rmdir "$func" 2>/dev/null; done
        rmdir "$g/strings/0x409" 2>/dev/null; rmdir "$g" 2>/dev/null
    done' 2>/dev/null || true
    stop_pipewire 2>/dev/null || true
    rm -f "$PID_FILE" "$SOFTNODE_PID" "$V4L2_BRIDGE_PID" "$QMP_SOCKET" "$VNC_SOCKET" \
        "$SOFTNODE_LOG" /tmp/ozma-nbd-${VM_NAME}.log /tmp/ozma-v4l2-${VM_NAME}.log
    sleep 1

    [ ! -f "$WINDOWS_ISO" ] && echo "Windows ISO not found: $WINDOWS_ISO" && exit 1

    mkdir -p "$IMAGES_DIR"
    download_virtio
    # Write autounattend.xml FIRST, then synthesise the FAT32 image
    # (the image must include the answer file)
    build_unattend
    build_usb_media

    [ ! -f "$DISK_IMAGE" ] && qemu-img create -f qcow2 "$DISK_IMAGE" "$DISK_SIZE"

    # Create a floppy image with autounattend.xml
    # Use mount instead of mcopy to avoid LFN encoding issues
    FLOPPY_IMG="$IMAGES_DIR/ozma-floppy-${VM_NAME}.img"
    dd if=/dev/zero of="$FLOPPY_IMG" bs=1k count=1440 status=none
    mkfs.vfat "$FLOPPY_IMG" >/dev/null
    FLOPPY_MNT=$(mktemp -d)
    sudo mount -o loop "$FLOPPY_IMG" "$FLOPPY_MNT"
    sudo cp "$MEDIA_DIR/autounattend.xml" "$FLOPPY_MNT/autounattend.xml"
    sudo umount "$FLOPPY_MNT"
    rmdir "$FLOPPY_MNT"
    echo "Floppy image with autounattend.xml"

    # Start isolated PipeWire BEFORE QEMU so QEMU can use it for audio
    start_pipewire
    local pw_dir="/tmp/ozma-pw-${VM_NAME}"

    echo ""
    echo "Launching VM (air-gapped, USB only)..."
    echo "  - CDROM: Windows ISO (boot)"
    echo "  - USB: Ozma media (drivers + agent + autounattend.xml, via NBD)"
    echo "  - Audio: isolated PipeWire ($pw_dir)"
    echo "  - Display: VNC unix socket → v4l2 bridge → capture device"
    echo "  - Network: $([ "$NO_NETWORK" = "true" ] && echo "NONE (air-gapped)" || echo "user net")"
    echo ""

    # The VM boots with USB only — no CDROM, no floppy, no network
    #   - QMP for soft node control
    #   - VNC for display
    # Put autounattend.xml on the media USB drive (Windows scans all drives)
    # Build QEMU args — everything is USB, nothing else
    #   - Disk: virtio (the only non-USB device — the target drive)
    #   - Windows ISO: USB mass storage (boot from it)
    #   - Media (drivers + agent + autounattend.xml): USB mass storage
    #   - Input: USB tablet
    #   - Audio: USB audio (via HDA, could be UAC2 via dummy_hcd)
    #   - Network: none (air-gapped) or user net
    #   - Display: VNC (soft node streams it)
    # Windows ISO must be CDROM (El Torito boot). The ozma media drive
    # is USB mass storage — this is the part that tests the node's USB
    # delivery path. The ISO is just the OS installer.
    QEMU_ARGS=(
        -name "$VM_NAME"
        -machine type=q35,accel=kvm
        -cpu host -smp "$CPUS" -m "$RAM"
        -drive file="$DISK_IMAGE",format=qcow2,if=ide
        -cdrom "$WINDOWS_ISO"
        -drive file="$FLOPPY_IMG",format=raw,if=floppy
        -device qemu-xhci,id=xhci
        $(if [ -n "$USB_BUS" ] && [ -n "$USB_DEV" ]; then
            # Pass through the real USB gadget — identical to hardware node
            echo "-device usb-host,bus=xhci.0,hostbus=$USB_BUS,hostaddr=$USB_DEV"
        elif [ -f "$MEDIA_IMG" ]; then
            echo "-drive file=$MEDIA_IMG,format=raw,if=none,id=ozma-usb"
            echo "-device usb-storage,bus=xhci.0,drive=ozma-usb,removable=on"
        fi)
        -device usb-tablet,bus=xhci.0
        -vnc :31
        -qmp "unix:$QMP_SOCKET,server,nowait"
        -audiodev pipewire,id=a0,out.name=ozma-${VM_NAME}
        -device intel-hda -device hda-duplex,audiodev=a0
        -boot order=d,menu=on
        -vga virtio
        -daemonize
        -pidfile "$PID_FILE"
    )

    # QEMU needs to use the isolated PipeWire
    export PIPEWIRE_RUNTIME_DIR="$pw_dir"
    export XDG_RUNTIME_DIR="$pw_dir"

    if [ "$NO_NETWORK" = "true" ] || [ "$NO_NETWORK" = "1" ]; then
        echo "*** AIR-GAPPED MODE — no network in VM ***"
        QEMU_ARGS+=(-nic none)
    else
        QEMU_ARGS+=(
            -device virtio-net-pci,netdev=net0
            -netdev user,id=net0,hostfwd=tcp::${SOFTNODE_PORT}-:7331,hostfwd=tcp::$((SOFTNODE_PORT+1))-:7382
        )
    fi

    qemu-system-x86_64 "${QEMU_ARGS[@]}" 2>&1

    echo "VM started (PID: $(cat $PID_FILE 2>/dev/null))"

    start_softnode

    echo ""
    echo "=== Windows is installing unattended ==="
    echo "Dashboard: $CONTROLLER_URL"
    echo "The VM '$VM_NAME' appears in the dashboard."
    echo "Watch the VNC stream — Windows installs automatically."
    echo ""
    echo "After install completes (~15-30 min):"
    echo "  - Windows boots to desktop (user: ozma, pass: ozma)"
    echo "  - FirstLogonCommand runs the agent installer automatically"
    echo "  - Agent registers back with the controller"
    echo ""
    echo "Monitor: bash dev/windows-vm/provision.sh wait-agent"
}

cmd_start() {
    echo "Starting $VM_NAME (boot from disk)..."
    [ ! -f "$DISK_IMAGE" ] && echo "No disk. Run: provision.sh create" && exit 1
    [ -f "$PID_FILE" ] && kill "$(cat $PID_FILE)" 2>/dev/null; sleep 1
    [ -f "$SOFTNODE_PID" ] && kill "$(cat $SOFTNODE_PID)" 2>/dev/null; sleep 1

    # Rebuild media (in case controller URL changed)
    build_usb_media 2>/dev/null

    # Boot from disk, USB media still attached (for agent re-install if needed)
    QEMU_ARGS=(
        -name "$VM_NAME"
        -machine type=q35,accel=kvm
        -cpu host -smp "$CPUS" -m "$RAM"
        -drive file="$DISK_IMAGE",format=qcow2,if=ide
        $(if [ -n "$USB_BUS" ] && [ -n "$USB_DEV" ]; then
            # Pass through the real USB gadget — identical to hardware node
            echo "-device usb-host,bus=xhci.0,hostbus=$USB_BUS,hostaddr=$USB_DEV"
        elif [ -f "$MEDIA_IMG" ]; then
            echo "-drive file=$MEDIA_IMG,format=raw,if=none,id=ozma-usb"
            echo "-device usb-storage,bus=xhci.0,drive=ozma-usb,removable=on"
        fi)
        -vnc :31
        -qmp "unix:$QMP_SOCKET,server,nowait"
        -device qemu-xhci -device usb-tablet
        -audiodev pipewire,id=a0,out.name=ozma-${VM_NAME}
        -device intel-hda -device hda-duplex,audiodev=a0
        -vga virtio
        -daemonize
        -pidfile "$PID_FILE"
    )

    if [ "$NO_NETWORK" = "true" ] || [ "$NO_NETWORK" = "1" ]; then
        QEMU_ARGS+=(-nic none)
    else
        QEMU_ARGS+=(
            -device virtio-net-pci,netdev=net0
            -netdev user,id=net0,hostfwd=tcp::${SOFTNODE_PORT}-:7331,hostfwd=tcp::$((SOFTNODE_PORT+1))-:7382
        )
    fi

    qemu-system-x86_64 "${QEMU_ARGS[@]}" 2>&1

    start_softnode
    echo "Running. Dashboard: $CONTROLLER_URL"
}

cmd_wait_agent() {
    echo "Waiting for Windows agent to connect..."
    echo "(Windows install takes ~15-30 min, then agent installs on first login)"
    echo ""

    local elapsed=0 timeout=2400  # 40 minutes
    while [ $elapsed -lt $timeout ]; do
        local status=$(curl -s "$CONTROLLER_URL/api/v1/nodes" 2>/dev/null | \
            python3 -c "
import sys, json
try:
    d = json.load(sys.stdin)
    for n in d.get('nodes', []):
        if 'desktop' in n.get('hw','').lower() or 'windows' in n.get('hw','').lower():
            print('connected')
            sys.exit(0)
except: pass
print('waiting')
" 2>/dev/null)

        if [ "$status" = "connected" ]; then
            echo ""
            echo "=== AGENT CONNECTED ==="
            echo "Windows VM agent registered with the controller!"
            echo "The full loop works: ozma → VM → Windows → agent → ozma"
            return 0
        fi
        sleep 15
        elapsed=$((elapsed + 15))
        printf "\r  %d/%ds — watching dashboard for agent registration..." "$elapsed" "$timeout"
    done
    echo ""
    echo "Timeout. Check the VM via dashboard VNC."
    return 1
}

cmd_stop() {
    echo "Stopping $VM_NAME..."
    [ -f "$V4L2_BRIDGE_PID" ] && kill "$(cat $V4L2_BRIDGE_PID)" 2>/dev/null
    [ -f "$SOFTNODE_PID" ] && kill "$(cat $SOFTNODE_PID)" 2>/dev/null
    [ -f "$PID_FILE" ] && kill "$(cat $PID_FILE)" 2>/dev/null
    pkill -f "usb_mass_storage.*MassStorageGadget" 2>/dev/null || true
    pkill -f "ozma-qemu-v4l2" 2>/dev/null || true
    stop_pipewire
    rm -f "$PID_FILE" "$SOFTNODE_PID" "$V4L2_BRIDGE_PID" "$VNC_SOCKET"
    echo "Stopped"
}

cmd_destroy() {
    cmd_stop
    rm -f "$DISK_IMAGE" "$MEDIA_IMG" "$SCRIPT_DIR/vm.conf"
    echo "Destroyed"
}

cmd_status() {
    echo "=== $VM_NAME ==="
    [ -f "$PID_FILE" ] && kill -0 "$(cat $PID_FILE)" 2>/dev/null && echo "VM: running" || echo "VM: stopped"
    [ -f "$SOFTNODE_PID" ] && kill -0 "$(cat $SOFTNODE_PID)" 2>/dev/null && echo "Softnode: running" || echo "Softnode: stopped"
    echo "Disk: $(du -sh "$DISK_IMAGE" 2>/dev/null | cut -f1 || echo 'none')"
    echo "Dashboard: $CONTROLLER_URL"
}

case "${1:-status}" in
    create)      cmd_create ;;
    start)       cmd_start ;;
    stop)        cmd_stop ;;
    destroy)     cmd_destroy ;;
    status)      cmd_status ;;
    wait-agent)  cmd_wait_agent ;;
    *)           echo "Usage: $0 {create|start|stop|destroy|status|wait-agent}" ;;
esac
