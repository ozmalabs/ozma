package PVE::API2::Ozma;

# Ozma Proxmox Plugin — REST API extension
#
# Adds /api2/json/ozma/* endpoints to the Proxmox API:
#   GET  /ozma/status         — plugin status + detected hardware
#   GET  /ozma/profiles       — list available VM profiles
#   POST /ozma/profiles/apply — apply a profile to a VM
#   GET  /ozma/vms            — list ozma-enabled VMs
#   GET  /ozma/displays       — list active display captures
#   GET  /ozma/gpu            — list GPUs available for passthrough

use strict;
use warnings;

use PVE::RESTHandler;
use PVE::JSONSchema qw(get_standard_option);
use PVE::Tools;

use base qw(PVE::RESTHandler);

our $VERSION = '1.0.0';

__PACKAGE__->register_method({
    name => 'index',
    path => '',
    method => 'GET',
    description => "Ozma plugin status",
    parameters => { additionalProperties => 0 },
    returns => { type => 'object' },
    code => sub {
        return {
            version => $VERSION,
            status => 'active',
            display_backend => 'kvmfr',
            controller => _get_controller_url(),
        };
    },
});

__PACKAGE__->register_method({
    name => 'profiles',
    path => 'profiles',
    method => 'GET',
    description => "List available VM profiles",
    parameters => { additionalProperties => 0 },
    returns => { type => 'array' },
    code => sub {
        return [
            {
                name => 'gaming',
                description => 'Maximum performance: GPU passthrough, ReBAR, CPU pinning, hugepages, 5.1 audio, Looking Glass',
                gpu_required => 1,
            },
            {
                name => 'workstation',
                description => 'Balanced multi-monitor: virtio-gpu, dual display, stereo audio',
                gpu_required => 0,
            },
            {
                name => 'server',
                description => 'Minimal headless: single display for console, no audio',
                gpu_required => 0,
            },
            {
                name => 'media',
                description => 'Media consumption: single display, 7.1 surround audio, hardware decode',
                gpu_required => 0,
            },
        ];
    },
});

__PACKAGE__->register_method({
    name => 'apply_profile',
    path => 'profiles/apply',
    method => 'POST',
    description => "Apply an ozma profile to a VM",
    parameters => {
        additionalProperties => 0,
        properties => {
            vmid => get_standard_option('pve-vmid'),
            profile => {
                type => 'string',
                enum => ['gaming', 'workstation', 'server', 'media'],
                description => 'Profile to apply',
            },
            gpu => {
                type => 'string',
                optional => 1,
                description => 'PCI address of GPU for passthrough (gaming profile)',
            },
            cores => {
                type => 'integer',
                optional => 1,
                minimum => 1,
                maximum => 128,
                description => 'Number of CPU cores',
            },
            memory => {
                type => 'integer',
                optional => 1,
                minimum => 512,
                description => 'Memory in MB',
            },
        },
    },
    returns => { type => 'object' },
    code => sub {
        my ($param) = @_;

        my $vmid = $param->{vmid};
        my $profile = $param->{profile};
        my $gpu = $param->{gpu} || '';
        my $cores = $param->{cores} || 0;
        my $memory = $param->{memory} || 0;

        # Call Python profile generator
        my $args = "--profile $profile --vmid $vmid";
        $args .= " --gpu $gpu" if $gpu;
        $args .= " --cores $cores" if $cores;
        $args .= " --memory $memory" if $memory;

        my $output = PVE::Tools::run_command(
            ['/usr/lib/ozma-proxmox/apply-profile.py', split(' ', $args)],
            outfunc => sub { },
        );

        return {
            vmid => $vmid,
            profile => $profile,
            status => 'applied',
        };
    },
});

__PACKAGE__->register_method({
    name => 'install_agent',
    path => 'agent/install',
    method => 'POST',
    description => "One-click install ozma agent inside a VM",
    parameters => {
        additionalProperties => 0,
        properties => {
            vmid => get_standard_option('pve-vmid'),
            controller => {
                type => 'string',
                optional => 1,
                description => 'Controller URL (default: from config)',
            },
        },
    },
    returns => { type => 'object' },
    code => sub {
        my ($param) = @_;
        my $vmid = $param->{vmid};
        my $controller = $param->{controller} || _get_controller_url();

        my $output = '';
        eval {
            PVE::Tools::run_command(
                ['/usr/lib/ozma-proxmox/agent-installer.py',
                 '--vmid', $vmid, '--controller', $controller],
                outfunc => sub { $output .= $_[0] . "\n"; },
                timeout => 600,
            );
        };

        if (my $err = $@) {
            return { ok => 0, error => $err, output => $output };
        }

        # Parse JSON result from installer
        my $result = eval { decode_json($output) } // { ok => 1, raw => $output };
        return $result;
    },
});

__PACKAGE__->register_method({
    name => 'gpu_list',
    path => 'gpu',
    method => 'GET',
    description => "List GPUs available for passthrough",
    parameters => { additionalProperties => 0 },
    returns => { type => 'array' },
    code => sub {
        my @gpus;
        my $output = `lspci -nn | grep -iE 'VGA|3D|Display'`;
        for my $line (split /\n/, $output) {
            if ($line =~ /^(\S+)\s+(.+?)\s+\[([0-9a-f]{4}):([0-9a-f]{4})\]/) {
                push @gpus, {
                    pci_address => "0000:$1",
                    description => $2,
                    vendor_id => $3,
                    device_id => $4,
                };
            }
        }
        return \@gpus;
    },
});

# ── Helpers ─────────────────────────────────────────────────────────────────

sub _get_controller_url {
    my $conf = eval { PVE::Tools::file_get_contents('/etc/ozma/proxmox-plugin.conf') } // '';
    if ($conf =~ /controller_url\s*=\s*(.+)/) {
        return $1;
    }
    return 'http://localhost:7380';
}

1;
