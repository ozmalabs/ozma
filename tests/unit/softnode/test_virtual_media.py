"""Unit tests for FAT32 synthesiser."""

import pytest

pytestmark = pytest.mark.unit


class TestFileEntry:
    def test_defaults(self):
        from virtual_media import FileEntry
        from pathlib import Path
        e = FileEntry(name="test.txt", path=Path("/tmp/test.txt"), is_dir=False)
        assert e.size == 0
        assert e.cluster == 0
        assert e.children == []


class TestShortNameGeneration:
    def _synth(self):
        from virtual_media import FATSynthesiser
        s = FATSynthesiser("/nonexistent", label="TEST", watch=False)
        s._short_names = set()
        return s

    def test_simple_name(self):
        s = self._synth()
        assert s._make_short_name("readme.txt") == "README  TXT"

    def test_no_extension(self):
        s = self._synth()
        result = s._make_short_name("Makefile")
        assert result == "MAKEFILE   "

    def test_long_name_truncated(self):
        s = self._synth()
        result = s._make_short_name("install-offline.bat")
        assert "~" in result
        assert result.endswith("BAT")

    def test_collision_avoidance(self):
        s = self._synth()
        name1 = s._make_short_name("install-offline.bat")
        name2 = s._make_short_name("install-offline.ps1")
        assert name1 != name2

    def test_multiple_collisions(self):
        s = self._synth()
        names = set()
        for i in range(10):
            name = s._make_short_name(f"very-long-filename-{i}.txt")
            names.add(name)
        assert len(names) == 10  # all unique

    def test_invalid_chars_replaced(self):
        s = self._synth()
        result = s._make_short_name("my file (1).txt")
        assert " " not in result  # spaces should be replaced
        assert "(" not in result


class TestFATSynthesiser:
    def test_scan(self, tmp_path):
        from virtual_media import FATSynthesiser
        (tmp_path / "test.txt").write_text("hello")
        (tmp_path / "subdir").mkdir()
        (tmp_path / "subdir" / "nested.txt").write_text("world")

        s = FATSynthesiser(str(tmp_path), label="TEST", watch=False)
        s.scan()

        assert s._scanned
        assert len(s._files) >= 3  # test.txt, subdir, nested.txt

    def test_total_bytes_positive(self, tmp_path):
        from virtual_media import FATSynthesiser
        (tmp_path / "file.txt").write_text("x" * 1000)
        s = FATSynthesiser(str(tmp_path), label="TEST", watch=False)
        s.scan()
        assert s.total_bytes > 0

    def test_boot_sector(self, tmp_path):
        from virtual_media import FATSynthesiser
        (tmp_path / "x.txt").write_text("x")
        s = FATSynthesiser(str(tmp_path), label="TEST", watch=False)
        s.scan()
        bs = s.read_sectors(0, 512)
        assert len(bs) == 512
        assert bs[0:3] == b"\xEB\x58\x90"    # jump
        assert bs[510:512] == b"\x55\xAA"     # boot signature
        assert bs[82:90] == b"FAT32   "       # FS type

    def test_label_in_boot_sector(self, tmp_path):
        from virtual_media import FATSynthesiser
        (tmp_path / "x.txt").write_text("x")
        s = FATSynthesiser(str(tmp_path), label="OZMA", watch=False)
        s.scan()
        bs = s.read_sectors(0, 512)
        assert b"OZMA" in bs[71:82]

    def test_read_beyond_end(self, tmp_path):
        from virtual_media import FATSynthesiser
        (tmp_path / "x.txt").write_text("x")
        s = FATSynthesiser(str(tmp_path), label="T", watch=False)
        s.scan()
        # Reading way past the end should return zeros
        data = s.read_sectors(s.total_bytes + 1000, 512)
        assert data == b"\x00" * 512

    def test_rescan_preserves_geometry(self, tmp_path):
        from virtual_media import FATSynthesiser
        (tmp_path / "a.txt").write_text("a")
        s = FATSynthesiser(str(tmp_path), label="T", watch=False)
        s.scan()
        old_total = s._total_sectors

        (tmp_path / "b.txt").write_text("b")
        s.rescan()

        assert s._total_sectors == old_total  # geometry stable
        assert len(s._files) >= 2

    def test_hidden_files_skipped(self, tmp_path):
        from virtual_media import FATSynthesiser
        (tmp_path / ".hidden").write_text("hidden")
        (tmp_path / "visible.txt").write_text("visible")
        s = FATSynthesiser(str(tmp_path), label="T", watch=False)
        s.scan()
        names = [f.name for f in s._files]
        assert ".hidden" not in names
        assert "visible.txt" in names

    def test_file_watcher_detects_add(self, tmp_path):
        """rescan() picks up files added after the initial scan."""
        from virtual_media import FATSynthesiser
        (tmp_path / "a.txt").write_text("a")
        s = FATSynthesiser(str(tmp_path), label="T", watch=False)
        s.scan()
        initial_count = len(s._files)

        (tmp_path / "b.txt").write_text("b")
        s.rescan()

        assert len(s._files) > initial_count

    def test_fsinfo_sector(self, tmp_path):
        from virtual_media import FATSynthesiser
        (tmp_path / "x.txt").write_text("x")
        s = FATSynthesiser(str(tmp_path), label="T", watch=False)
        s.scan()
        fs = s.read_sectors(512, 512)  # sector 1
        import struct
        lead_sig = struct.unpack_from("<I", fs, 0)[0]
        assert lead_sig == 0x41615252
