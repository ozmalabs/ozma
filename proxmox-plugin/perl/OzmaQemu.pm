package PVE::QemuServer::Ozma;

# Ozma Proxmox Plugin — QEMU VM lifecycle hooks
#
# Hooks into PVE's QEMU server to:
#   - Configure D-Bus p2p display for framebuffer capture
#   - Set up KVMFR shared memory (Looking Glass compatible)
#   - Configure multi-head GPU and audio
#   - Start/stop per-VM display services
#   - Register VMs as ozma nodes with the controller

use strict;
use warnings;

use PVE::Tools;
use PVE::QemuServer;

our $VERSION = '1.1.0';

# ── QEMU argument generation ──────────────────────────────────────────────

sub ozma_qemu_args {
    my ($vmid, $conf) = @_;

    my $ozma = $conf->{ozma} || {};
    return () unless $ozma->{enabled};

    my @args;
    my $name = $ozma->{name} || "vm$vmid";
    my $displays = $ozma->{displays} || 1;
    my $shm_size = $ozma->{shm_size} || 64;  # MB
    my $shm_path = "/dev/shm/ozma-vm$vmid";

    # D-Bus p2p display — QEMU acts as D-Bus server, ozma connects via
    # QMP add_client. No bus daemon needed. Works on QEMU 8.2+ but
    # QEMU 10+ (PVE 9) has better p2p support.
    push @args, '-display', 'dbus,p2p=yes';

    # KVMFR shared memory (Looking Glass compatible)
    # Works for both emulated GPU and GPU passthrough
    push @args, '-object', "memory-backend-file,id=ozma-shm,share=on,mem-path=$shm_path,size=${shm_size}M";
    push @args, '-device', 'ivshmem-plain,memdev=ozma-shm';

    # Multi-head virtio-gpu
    if ($displays > 1) {
        push @args, '-device', "virtio-gpu-pci,id=ozma-vga,max_outputs=$displays";
    }

    # Audio — PulseAudio (works cross-user via socket)
    # PipeWire serves PulseAudio protocol so this works on both
    my $channels = $ozma->{audio_channels} || 2;
    push @args, '-audiodev', "pa,id=ozma-audio,server=unix:/run/user/0/pulse/native,out.name=ozma-$name";
    push @args, '-device', 'intel-hda', '-device', 'hda-duplex,audiodev=ozma-audio';

    # Dedicated QMP socket for ozma (the Proxmox-native one is exclusive)
    my $qmp_path = "/var/run/ozma/vm$vmid-ctrl.qmp";
    push @args, '-chardev', "socket,id=ozma-mon,path=$qmp_path,server=on,wait=off";
    push @args, '-mon', 'chardev=ozma-mon,mode=control';

    return @args;
}

# ── VM lifecycle hooks ────────────────────────────────────────────────────

sub on_vm_start {
    my ($vmid, $conf) = @_;

    my $ozma = $conf->{ozma} || {};
    return unless $ozma->{enabled};

    my $name = $ozma->{name} || "vm$vmid";
    my $shm_path = "/dev/shm/ozma-vm$vmid";
    my $shm_size = ($ozma->{shm_size} || 64) * 1024 * 1024;

    # Create run directory
    system("mkdir -p /var/run/ozma");

    # Create shared memory file
    system("truncate -s $shm_size $shm_path");
    system("chmod 666 $shm_path");

    # Start the display listener service
    system("systemctl start ozma-display\@$vmid.service");

    # Register with controller
    _register_node($vmid, $name, $conf);

    PVE::Tools::log_msg('info', 'ozma', "VM $vmid ($name) ozma display started");
}

sub on_vm_stop {
    my ($vmid, $conf) = @_;

    # Stop display listener
    system("systemctl stop ozma-display\@$vmid.service");

    # Clean up
    unlink("/var/run/ozma/vm$vmid-ctrl.qmp");

    PVE::Tools::log_msg('info', 'ozma', "VM $vmid ozma display stopped");
}

sub on_vm_migrate {
    my ($vmid, $conf, $target_node) = @_;

    # SHM and QMP sockets are local — they'll be recreated on the
    # target by on_vm_start. The controller sees the node go offline
    # briefly, then come back online from the new host.

    PVE::Tools::log_msg('info', 'ozma',
        "VM $vmid migrating to $target_node — display will restart on target");
}

# ── Internal helpers ──────────────────────────────────────────────────────

sub _register_node {
    my ($vmid, $name, $conf) = @_;

    my $controller = $conf->{ozma}{controller_url} || _find_controller();
    return unless $controller;

    eval {
        my $data = {
            id => "$name._ozma._udp.local.",
            host => PVE::Tools::get_host_ip(),
            port => 7340 + $vmid,
            hw => 'soft',
            role => 'compute',
            cap => 'qmp,power,display',
            api_port => 7390 + $vmid,
        };

        my $json = PVE::Tools::encode_json($data);
        my $cmd = "curl -s -X POST '$controller/api/v1/nodes/register' "
                . "-H 'Content-Type: application/json' "
                . "-d '$json' -m 5";
        system($cmd);
    };
    if ($@) {
        PVE::Tools::log_msg('warn', 'ozma', "Node registration failed: $@");
    }
}

sub _find_controller {
    # Check common locations for controller URL
    my $conf_file = "/etc/ozma/controller.conf";
    if (-f $conf_file) {
        my $url = PVE::Tools::file_get_contents($conf_file);
        chomp $url;
        return $url;
    }

    # Try mDNS discovery (requires avahi-browse)
    my $result = `avahi-browse -rtp _ozma._tcp.local. 2>/dev/null | head -1`;
    if ($result =~ /;(\d+\.\d+\.\d+\.\d+);(\d+);/) {
        return "http://$1:$2";
    }

    # Default
    return "http://localhost:7380";
}

sub _deregister_node {
    my ($vmid) = @_;
    # The controller will detect the node going offline via heartbeat timeout
    # No explicit deregistration needed
}

1;
