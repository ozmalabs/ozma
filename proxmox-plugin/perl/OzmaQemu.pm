package PVE::QemuServer::Ozma;

# Ozma Proxmox Plugin — QEMU VM lifecycle hooks
#
# Hooks into PVE's QEMU server to:
#   - Configure D-Bus display, KVMFR shared memory, multi-head GPU
#   - Start/stop display listeners when VMs start/stop
#   - Register VMs as ozma nodes with the controller
#   - Handle live migration (transfer display listener)

use strict;
use warnings;

use PVE::Tools;
use PVE::QemuServer;

# Plugin version
our $VERSION = '1.0.0';

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

    # D-Bus display (for non-passthrough VMs)
    push @args, '-display', 'dbus';

    # KVMFR shared memory (Looking Glass compatible)
    push @args, '-object', "memory-backend-file,id=ozma-shm,share=on,mem-path=$shm_path,size=${shm_size}M";
    push @args, '-device', 'ivshmem-plain,memdev=ozma-shm';

    # Multi-head virtio-gpu
    if ($displays > 1) {
        push @args, '-device', "virtio-gpu-pci,id=ozma-vga,max_outputs=$displays";
    }

    # Audio — single multi-channel PipeWire sink
    my $channels = $ozma->{audio_channels} || 2;
    push @args, '-audiodev', "pipewire,id=ozma-audio,out.name=ozma-$name,out.channels=$channels";
    push @args, '-device', 'intel-hda', '-device', 'hda-duplex,audiodev=ozma-audio';

    # QMP control socket (for power/status)
    push @args, '-qmp', "unix:/var/run/ozma/vm$vmid-ctrl.qmp,server,nowait";

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

    # Create shared memory file
    PVE::Tools::file_set_contents($shm_path, "\0" x 0);  # create empty
    truncate($shm_path, $shm_size) if -f $shm_path;

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

    # Deregister from controller
    _deregister_node($vmid);

    PVE::Tools::log_msg('info', 'ozma', "VM $vmid ozma display stopped");
}

sub on_vm_migrate {
    my ($vmid, $conf, $target_node) = @_;

    # The shared memory doesn't migrate — it needs to be recreated
    # on the target. The display listener restarts via the hook on the
    # destination node.

    PVE::Tools::log_msg('info', 'ozma', "VM $vmid migrating to $target_node");
}

# ── Internal helpers ──────────────────────────────────────────────────────

sub _register_node {
    my ($vmid, $name, $conf) = @_;

    my $controller = $conf->{ozma}{controller_url} || _find_controller();
    return unless $controller;

    # HTTP POST to controller's registration endpoint
    eval {
        my $data = {
            name => $name,
            host => PVE::Tools::get_host_ip(),
            port => 7340 + $vmid,
            hw => 'soft',
            role => 'compute',
            capabilities => 'qmp,power,display',
            api_port => 7390 + $vmid,
            display_type => 'kvmfr',
            shm_path => "/dev/shm/ozma-vm$vmid",
        };
        # Use curl for simplicity
        my $json = encode_json($data);
        system("curl -s -X POST '$controller/api/v1/nodes/register' -H 'Content-Type: application/json' -d '$json' &");
    };
}

sub _deregister_node {
    my ($vmid) = @_;
    # Controller will detect node offline via heartbeat timeout
}

sub _find_controller {
    # Check config file
    my $conf = eval { PVE::Tools::file_get_contents('/etc/ozma/proxmox-plugin.conf') } // '';
    if ($conf =~ /controller_url\s*=\s*(.+)/) {
        return $1;
    }
    return 'http://localhost:7380';  # default
}

1;
