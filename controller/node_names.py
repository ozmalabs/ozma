# SPDX-License-Identifier: AGPL-3.0-only WITH OzmaPluginException
"""
Friendly node name generation — adjective-noun names for unnamed nodes.

When a node doesn't have a hostname, or when multiple nodes share the
same hostname (common with "localhost" or default SBC hostnames like
"milkv-duos"), the controller assigns a unique friendly name.

Names are deterministic: the same node ID always gets the same name
(derived from a hash of the ID). This means names are stable across
controller restarts without needing to persist a mapping.

Examples:
  swift-falcon, amber-pixel, steady-beacon, crisp-summit,
  bright-compass, quiet-anchor, bold-horizon, calm-circuit
"""

from __future__ import annotations

import hashlib

# Word lists — short, memorable, professional (no silly or offensive words)
# ~100 adjectives × ~100 nouns = 10,000 unique combinations

ADJECTIVES = [
    "amber", "arctic", "azure", "bold", "bright", "brisk", "calm", "cedar",
    "clear", "cobalt", "coral", "crisp", "dusk", "ember", "fern", "flint",
    "frost", "gentle", "golden", "granite", "hazel", "hollow", "iron", "ivory",
    "jade", "keen", "lark", "lemon", "lunar", "maple", "mellow", "misty",
    "noble", "ocean", "olive", "onyx", "opal", "orchid", "pale", "pearl",
    "pine", "plum", "polar", "prime", "quiet", "rapid", "raven", "reed",
    "river", "robin", "royal", "ruby", "rustic", "sage", "sandy", "scarlet",
    "shadow", "sharp", "silver", "slate", "smooth", "solar", "solid", "spark",
    "spruce", "steady", "steel", "stone", "storm", "stout", "swift", "tawny",
    "teal", "timber", "topaz", "twin", "upper", "vast", "velvet", "verdant",
    "violet", "vivid", "warm", "west", "wild", "willow", "winter", "wise",
    "wren", "young", "zeal", "zen", "zinc", "alder", "birch", "brook",
    "chalk", "cliff", "cloud", "crest", "delta", "drift", "dune",
]

NOUNS = [
    "anchor", "apex", "arrow", "atlas", "badge", "basin", "beacon", "blade",
    "bloom", "bolt", "bridge", "cairn", "canyon", "carbon", "cask", "cedar",
    "chain", "cipher", "circuit", "cliff", "coil", "column", "compass",
    "conduit", "copper", "core", "crane", "crest", "crown", "crystal",
    "delta", "depot", "dome", "drift", "edge", "ember", "engine", "falcon",
    "ferry", "fiber", "field", "flame", "flare", "forge", "frame", "frost",
    "garden", "gate", "glyph", "grain", "grove", "guild", "harbor", "haven",
    "heron", "hollow", "horizon", "hub", "index", "inlet", "iris", "kernel",
    "lantern", "lattice", "ledge", "lever", "light", "link", "locus",
    "mantle", "mason", "matrix", "meadow", "mesa", "mirror", "nexus",
    "node", "orbit", "origin", "osprey", "outpost", "pane", "path",
    "peak", "perch", "pine", "pixel", "plinth", "point", "portal",
    "prism", "pulse", "quartz", "rail", "range", "reef", "relay",
    "ridge", "ring", "rover", "shard", "shelf", "signal", "socket",
    "spark", "spire", "spoke", "spring", "stack", "stone", "summit",
    "surge", "terrace", "thread", "tide", "timber", "tower", "trail",
    "truss", "vault", "vector", "vertex", "vista", "well", "zenith",
]


def generate_name(node_id: str) -> str:
    """Generate a deterministic adjective-noun name from a node ID."""
    h = hashlib.sha256(node_id.encode()).digest()
    adj_idx = int.from_bytes(h[0:2], "big") % len(ADJECTIVES)
    noun_idx = int.from_bytes(h[2:4], "big") % len(NOUNS)
    return f"{ADJECTIVES[adj_idx]}-{NOUNS[noun_idx]}"


def generate_unique_name(node_id: str, existing_names: set[str]) -> str:
    """Generate a unique name, appending a number if there's a collision."""
    base = generate_name(node_id)
    if base not in existing_names:
        return base
    # Collision — append incrementing number
    for i in range(2, 100):
        candidate = f"{base}-{i}"
        if candidate not in existing_names:
            return candidate
    # Extremely unlikely fallback
    return f"{base}-{node_id[:8]}"


def should_assign_name(hostname: str, existing_hostnames: dict[str, str]) -> bool:
    """
    Check if a node needs an assigned name.

    Returns True if:
      - hostname is empty or generic (localhost, milkv-duos, raspberrypi, etc.)
      - hostname is already used by another node (duplication)
    """
    if not hostname or hostname in _GENERIC_HOSTNAMES:
        return True
    # Check for duplication
    count = sum(1 for h in existing_hostnames.values() if h == hostname)
    return count > 1


_GENERIC_HOSTNAMES = {
    "localhost",
    "milkv-duos", "milkv-duo", "milkv",
    "lichee-rv", "licheerv",
    "raspberrypi", "raspberry",
    "orangepi", "orange-pi",
    "rock64", "rock-5b", "rock5b",
    "beaglebone", "beagle",
    "pine64", "pinebook",
    "nanopi",
    "tinkerboard",
    "odroid",
    "banana-pi", "bananapi",
    "unknown",
    "(none)",
    "",
}
