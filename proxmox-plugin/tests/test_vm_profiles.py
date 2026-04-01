#!/usr/bin/env python3
"""Unit tests for VM profile generation."""

import sys
import unittest
from pathlib import Path
from unittest.mock import patch, MagicMock

sys.path.insert(0, str(Path(__file__).parent.parent / "python"))
from vm_profiles import VMProfile, CPUTopology, GPUInfo


class TestCPUTopology(unittest.TestCase):
    """Test CPU topology detection and pinning."""

    def _mock_topo(self, cores=8, threads_per_core=2):
        """Create a mock topology: N cores, each with HT sibling."""
        topo = CPUTopology()
        topo.total_cores = cores
        topo.cores_per_socket = cores
        topo.threads_per_core = threads_per_core
        topo.total_threads = cores * threads_per_core
        topo.sockets = 1
        topo.numa_nodes = 1
        for c in range(cores):
            topo.core_threads[c] = [c, c + cores] if threads_per_core == 2 else [c]
        topo.numa_cores[0] = list(range(cores))
        return topo

    def test_pin_cores_basic(self):
        topo = self._mock_topo(cores=8, threads_per_core=2)
        pins = topo.pin_cores(4, reserve_host=2)
        # Should pick cores 2,3,4,5 (skipping 0,1 for host)
        # Each core has 2 threads: [core, core+8]
        self.assertEqual(len(pins), 8)  # 4 cores × 2 threads
        # First two cores (0,1) should NOT be in the list
        self.assertNotIn(0, pins)
        self.assertNotIn(1, pins)
        self.assertNotIn(8, pins)
        self.assertNotIn(9, pins)

    def test_pin_cores_no_ht(self):
        topo = self._mock_topo(cores=8, threads_per_core=1)
        pins = topo.pin_cores(4, reserve_host=2)
        self.assertEqual(len(pins), 4)
        self.assertEqual(pins, [2, 3, 4, 5])

    def test_pin_cores_too_many(self):
        topo = self._mock_topo(cores=4, threads_per_core=2)
        pins = topo.pin_cores(6, reserve_host=2)
        # Only 2 cores available after reserving 2
        self.assertEqual(len(pins), 4)  # 2 cores × 2 threads

    def test_pin_cores_zero_reserve(self):
        topo = self._mock_topo(cores=4, threads_per_core=1)
        pins = topo.pin_cores(2, reserve_host=0)
        self.assertEqual(pins, [0, 1])


class TestGPUInfo(unittest.TestCase):
    """Test GPU detection."""

    def test_audio_pci_detection(self):
        gpu = GPUInfo(pci_address="0000:01:00.0")
        # Audio should be at .1
        self.assertEqual(gpu.audio_pci, "")  # not detected without sysfs

    def test_iommu_group_default(self):
        gpu = GPUInfo()
        self.assertEqual(gpu.iommu_group, -1)
        self.assertEqual(gpu.iommu_group_devices, [])


class TestGamingProfile(unittest.TestCase):
    """Test gaming profile generation."""

    @patch.object(CPUTopology, 'detect')
    @patch.object(GPUInfo, 'detect')
    def test_gaming_basic(self, mock_gpu_detect, mock_cpu_detect):
        mock_cpu = CPUTopology()
        mock_cpu.total_cores = 12
        mock_cpu.cores_per_socket = 12
        mock_cpu.threads_per_core = 2
        mock_cpu.total_threads = 24
        mock_cpu.numa_nodes = 1
        for c in range(12):
            mock_cpu.core_threads[c] = [c, c + 12]
        mock_cpu_detect.return_value = mock_cpu

        mock_gpu = GPUInfo(
            pci_address="0000:01:00.0",
            vendor="nvidia",
            model="RTX 4090",
            rebar_supported=True,
            audio_pci="0000:01:00.1",
        )
        mock_gpu_detect.return_value = mock_gpu

        profile = VMProfile.gaming(vmid=100, gpu_pci="0000:01:00.0",
                                    cores=8, memory_mb=32768)

        self.assertEqual(profile.name, "gaming")
        self.assertEqual(profile.vmid, 100)
        self.assertEqual(profile.cores, 8)
        self.assertEqual(profile.memory_mb, 32768)
        self.assertTrue(profile.gpu_passthrough)
        self.assertEqual(profile.gpu_pci, "0000:01:00.0")
        self.assertTrue(profile.rebar)
        self.assertTrue(profile.hugepages)
        self.assertTrue(profile.looking_glass)
        self.assertEqual(profile.display_type, "kvmfr")
        self.assertEqual(profile.audio_channels, 6)
        self.assertEqual(profile.ivshmem_size_mb, 128)
        self.assertEqual(len(profile.cpu_pinning), 16)  # 8 cores × 2 threads

    @patch.object(CPUTopology, 'detect')
    def test_gaming_no_gpu(self, mock_cpu_detect):
        mock_cpu_detect.return_value = CPUTopology()
        profile = VMProfile.gaming(vmid=100)
        self.assertFalse(profile.gpu_passthrough)
        self.assertEqual(profile.gpu_pci, "")

    @patch.object(CPUTopology, 'detect')
    @patch.object(GPUInfo, 'detect')
    def test_gaming_qemu_args(self, mock_gpu, mock_cpu):
        mock_cpu.return_value = CPUTopology()
        gpu = GPUInfo(pci_address="0000:01:00.0", audio_pci="0000:01:00.1",
                      rebar_supported=True)
        mock_gpu.return_value = gpu

        profile = VMProfile.gaming(vmid=100, gpu_pci="0000:01:00.0")
        args = profile.qemu_args()

        self.assertIn("-cpu", args)
        self.assertIn("host", args)
        self.assertIn("-mem-prealloc", args)
        self.assertIn("-mem-path", args)
        self.assertIn("-display", args)
        self.assertIn("none", args)
        # GPU passthrough
        self.assertTrue(any("vfio-pci" in a and "01:00.0" in a for a in args))
        # Audio passthrough
        self.assertTrue(any("01:00.1" in a for a in args))
        # IVSHMEM
        self.assertTrue(any("ivshmem-plain" in a for a in args))
        # Single multi-channel audio
        self.assertTrue(any("out.channels=6" in a for a in args))
        # Should NOT have multiple audio devices
        audio_count = sum(1 for a in args if "audiodev" in a and a.startswith("-audiodev"))
        self.assertEqual(audio_count, 1)


class TestWorkstationProfile(unittest.TestCase):

    def test_workstation_defaults(self):
        profile = VMProfile.workstation(vmid=101)
        self.assertEqual(profile.name, "workstation")
        self.assertEqual(profile.display_heads, 2)
        self.assertEqual(profile.display_type, "dbus")
        self.assertFalse(profile.gpu_passthrough)
        self.assertFalse(profile.hugepages)
        self.assertEqual(profile.audio_channels, 2)
        self.assertTrue(profile.looking_glass)

    def test_workstation_qemu_args(self):
        profile = VMProfile.workstation(vmid=101, displays=3)
        args = profile.qemu_args()

        self.assertIn("dbus", args)
        self.assertTrue(any("virtio-gpu-pci" in a and "max_outputs=3" in a for a in args))
        self.assertFalse(any("vfio-pci" in a for a in args))
        self.assertTrue(any("out.channels=2" in a for a in args))


class TestServerProfile(unittest.TestCase):

    def test_server_minimal(self):
        profile = VMProfile.server(vmid=102)
        self.assertEqual(profile.cores, 2)
        self.assertEqual(profile.memory_mb, 4096)
        self.assertEqual(profile.audio_channels, 0)
        self.assertFalse(profile.looking_glass)
        self.assertEqual(profile.display_heads, 1)

    def test_server_no_ivshmem(self):
        profile = VMProfile.server(vmid=102)
        args = profile.qemu_args()
        self.assertFalse(any("ivshmem" in a for a in args))

    def test_server_no_audio(self):
        profile = VMProfile.server(vmid=102)
        args = profile.qemu_args()
        self.assertFalse(any("audiodev" in a for a in args))


class TestMediaProfile(unittest.TestCase):

    def test_media_surround(self):
        profile = VMProfile.media(vmid=103)
        self.assertEqual(profile.audio_channels, 8)
        self.assertTrue(profile.looking_glass)

    def test_media_qemu_args(self):
        profile = VMProfile.media(vmid=103)
        args = profile.qemu_args()
        self.assertTrue(any("out.channels=8" in a for a in args))


class TestProfileHostSetup(unittest.TestCase):

    @patch.object(CPUTopology, 'detect')
    @patch.object(GPUInfo, 'detect')
    def test_gaming_host_setup(self, mock_gpu, mock_cpu):
        mock_cpu.return_value = CPUTopology()
        mock_gpu.return_value = GPUInfo(pci_address="0000:01:00.0")

        profile = VMProfile.gaming(vmid=100, gpu_pci="0000:01:00.0",
                                    memory_mb=16384)
        cmds = profile.host_setup_commands()

        # Should have hugepages
        self.assertTrue(any("hugepages" in c for c in cmds))
        # Should have SHM creation
        self.assertTrue(any("ozma-vm100" in c for c in cmds))
        # Should have VFIO binding
        self.assertTrue(any("vfio-pci" in c for c in cmds))

    def test_server_no_host_setup(self):
        profile = VMProfile.server(vmid=102)
        cmds = profile.host_setup_commands()
        self.assertEqual(len(cmds), 0)


class TestProfileProxmoxConf(unittest.TestCase):

    @patch.object(CPUTopology, 'detect')
    @patch.object(GPUInfo, 'detect')
    def test_gaming_conf(self, mock_gpu, mock_cpu):
        mock_cpu.return_value = CPUTopology()
        gpu = GPUInfo(pci_address="0000:01:00.0", audio_pci="0000:01:00.1")
        mock_gpu.return_value = gpu

        profile = VMProfile.gaming(vmid=100, gpu_pci="0000:01:00.0")
        lines = profile.proxmox_conf_lines()
        text = "\n".join(lines)

        self.assertIn("hostpci0:", text)
        self.assertIn("01:00.0", text)
        self.assertIn("hostpci1:", text)
        self.assertIn("01:00.1", text)
        self.assertIn("hugepages:", text)
        self.assertIn("cpu: host", text)

    def test_workstation_conf(self):
        profile = VMProfile.workstation(vmid=101)
        lines = profile.proxmox_conf_lines()
        text = "\n".join(lines)

        self.assertNotIn("hostpci", text)
        self.assertNotIn("hugepages", text)
        self.assertIn("cores: 4", text)


class TestProfileDict(unittest.TestCase):

    def test_to_dict_roundtrip(self):
        profile = VMProfile.workstation(vmid=101)
        d = profile.to_dict()

        self.assertEqual(d["name"], "workstation")
        self.assertEqual(d["vmid"], 101)
        self.assertEqual(d["cores"], 4)
        self.assertEqual(d["display_heads"], 2)
        self.assertFalse(d["gpu_passthrough"])
        self.assertIsInstance(d["cpu_pinning"], list)


if __name__ == "__main__":
    unittest.main()
