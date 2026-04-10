# SPDX-License-Identifier: AGPL-3.0-only WITH OzmaPluginException
"""
agent/chassis_db.py — Chassis/enclosure model database, physical location registry,
drive inventory, and field replacement guide generation.

This module answers the question: "A drive failed somewhere. Where exactly is it,
what do I need to replace it with, and do I have one in stock?"

Key concepts
────────────
ChassisModel     — Definition of a known chassis/enclosure type.  Ships from
                   agent/chassis_models/*.yaml (community-contributed, open DB).
                   User can add custom models via the API.

DriveBay         — One physical slot in a chassis model. Knows its label (e.g.
                   "Bay 3, third from left"), row/col for visual layout, supported
                   form factors, interfaces, max RPM.

ChassisInstance  — A registered chassis at a specific physical location:
                   rack → rack_unit → unit, with human description.
                   Maps each slot to the current drive serial (updated by
                   EnclosureManager when SES is available, else manual).

DriveInventoryItem — A spare drive in stock. Has all physical specs + location in
                     the stockroom (e.g. shelf B3).

ReplacementSpec  — What the replacement drive MUST satisfy: min capacity, interface,
                   min RPM, form factor. Derived from the failed drive + pool config.

FieldGuide       — Human-readable replacement instructions: location description
                   ("rack 3, third machine down, disk third from the left"),
                   exactly what to pull, what to put in ("pull and replace with at
                   least a 300 GB, 10K, SAS3 drive"), and which stock item to grab
                   ("get drive ST300MP0006 S/N WFN2XXXX from shelf B3").

The chassis model database is loaded from agent/chassis_models/*.yaml on startup
and merged with any custom models stored in /var/lib/ozma/chassis-custom.yaml.
Community contributions go to agent/chassis_models/ via PR.
"""

from __future__ import annotations

import dataclasses
import json
import logging
import time
from dataclasses import dataclass, field
from pathlib import Path
from typing import Any

log = logging.getLogger("ozma.chassis_db")

# ──────────────────────────── Data classes ─────────────────────────────────

@dataclass
class DriveBay:
    """One physical drive bay in a chassis model."""
    slot: int
    label: str                           # human label, e.g. "Bay 3 (third from left)"
    row: int = 0                         # 0-based row from top
    col: int = 0                         # 0-based column from left
    supported_form_factors: list[str] = field(default_factory=lambda: ["3.5in"])
    supported_interfaces: list[str] = field(default_factory=lambda: ["SATA3"])
    max_speed_rpm: int = 7200            # 0 = solid-state (NVMe/SSD)

    def accepts(self, spec: "ReplacementSpec") -> bool:
        """True if this bay can accept a drive matching the given spec."""
        if spec.form_factor and spec.form_factor not in self.supported_form_factors:
            # Allow 2.5in in 3.5in bay (with adapter), not the reverse
            if not (spec.form_factor == "2.5in" and "3.5in" in self.supported_form_factors):
                return False
        if spec.interface and spec.interface not in self.supported_interfaces:
            return False
        return True


@dataclass
class ChassisModel:
    """
    Definition of a chassis/enclosure type.
    Loaded from YAML files in agent/chassis_models/ or custom entries.
    """
    id: str                              # e.g. "supermicro-cse-826"
    manufacturer: str
    model: str
    description: str = ""
    form_factor: str = ""               # "1U", "2U", "4U", "tower", "desktop"
    is_jbod: bool = False
    bays: list[DriveBay] = field(default_factory=list)
    contributor_notes: str = ""

    def bay_by_slot(self, slot: int) -> DriveBay | None:
        for b in self.bays:
            if b.slot == slot:
                return b
        return None

    def bay_count(self) -> int:
        return len(self.bays)

    def label_for_slot(self, slot: int) -> str:
        b = self.bay_by_slot(slot)
        return b.label if b else f"Slot {slot}"


@dataclass
class PhysicalLocation:
    """
    Describes where a chassis sits in the physical world.
    Supports rack/row/position hierarchy or freeform description.
    """
    rack: str = ""                       # e.g. "rack-3", "south-wall-rack"
    rack_unit: int = 0                   # U position (1 = bottom)
    rack_units_tall: int = 1             # how many U does this chassis occupy
    row: str = ""                        # e.g. "row-B" for data-centre row
    room: str = ""                       # e.g. "server-room", "study"
    description: str = ""               # freeform, e.g. "third machine down on left"
    site: str = ""                       # e.g. "head-office", "home"

    def to_prose(self) -> str:
        """Return human-readable location string."""
        parts: list[str] = []
        if self.site:
            parts.append(self.site)
        if self.room:
            parts.append(self.room)
        if self.rack:
            parts.append(self.rack)
            if self.rack_unit:
                parts.append(f"U{self.rack_unit}")
        if self.description:
            parts.append(self.description)
        return ", ".join(parts) if parts else "unknown location"


@dataclass
class SlotState:
    """Current state of a single drive bay in a registered chassis."""
    slot: int
    drive_serial: str = ""              # empty = empty bay
    drive_model: str = ""
    drive_manufacturer: str = ""
    drive_capacity_bytes: int = 0
    drive_interface: str = ""
    drive_speed_rpm: int = 0
    drive_form_factor: str = ""
    drive_health: str = "unknown"       # "healthy" | "warning" | "failed" | "unknown"
    pool_or_array: str = ""             # which pool/array this drive belongs to
    last_seen_ts: float = 0.0

    def is_empty(self) -> bool:
        return not self.drive_serial


@dataclass
class ChassisInstance:
    """
    A registered chassis at a specific physical location.
    Ties a ChassisModel to real-world coordinates and live slot states.
    """
    chassis_id: str                      # user-assigned unique ID, e.g. "storage-1"
    model_id: str                        # references ChassisModel.id
    location: PhysicalLocation = field(default_factory=PhysicalLocation)
    label: str = ""                      # friendly name, e.g. "Main Storage Server"
    node_id: str = ""                    # ozma node_id if it's an ozma-managed node
    agent_host: str = ""                 # agent hostname/IP for remote management
    slots: dict[int, SlotState] = field(default_factory=dict)
    registered_ts: float = field(default_factory=time.time)
    notes: str = ""

    def get_slot(self, slot: int) -> SlotState:
        if slot not in self.slots:
            self.slots[slot] = SlotState(slot=slot)
        return self.slots[slot]

    def slot_for_serial(self, serial: str) -> SlotState | None:
        for s in self.slots.values():
            if s.drive_serial == serial:
                return s
        return None

    def failed_slots(self) -> list[SlotState]:
        return [s for s in self.slots.values() if s.drive_health == "failed"]

    def drive_count(self) -> int:
        return sum(1 for s in self.slots.values() if not s.is_empty())


@dataclass
class DriveSpec:
    """Physical specification of a drive — used for inventory and matching."""
    model: str = ""
    manufacturer: str = ""
    part_number: str = ""               # OEM/vendor part number (e.g. "ST300MP0006")
    capacity_bytes: int = 0
    interface: str = ""                 # "SAS3" | "SAS2" | "SATA3" | "SATA2" | "NVMe"
    speed_rpm: int = 0                  # 0 for SSDs
    form_factor: str = ""              # "3.5in" | "2.5in" | "M.2"
    cache_mb: int = 0
    sector_size: int = 512             # 512 or 4096 (4Kn / 512e)

    @property
    def capacity_gb(self) -> float:
        return self.capacity_bytes / (1000 ** 3)

    @property
    def capacity_gib(self) -> float:
        return self.capacity_bytes / (1024 ** 3)

    def short_desc(self) -> str:
        parts: list[str] = []
        if self.manufacturer:
            parts.append(self.manufacturer)
        if self.model:
            parts.append(self.model)
        if self.capacity_bytes:
            parts.append(f"{self.capacity_gb:.0f}GB")
        if self.interface:
            parts.append(self.interface)
        if self.speed_rpm:
            parts.append(f"{self.speed_rpm // 1000}K RPM")
        return " ".join(parts)


@dataclass
class DriveInventoryItem:
    """
    A spare drive available in the parts inventory/stockroom.
    """
    inventory_id: str                    # unique ID, e.g. "INV-0042"
    serial: str = ""
    spec: DriveSpec = field(default_factory=DriveSpec)
    condition: str = "new"              # "new" | "refurbished" | "used-tested"
    stock_location: str = ""            # physical location in stockroom, e.g. "shelf B3, bin 4"
    acquired_date: str = ""             # ISO date string
    notes: str = ""
    reserved_for: str = ""             # chassis_id:slot if reserved for a specific replacement
    added_ts: float = field(default_factory=time.time)

    def is_available(self) -> bool:
        return not self.reserved_for

    def matches_spec(self, spec: "ReplacementSpec") -> bool:
        """True if this drive satisfies the minimum replacement specification."""
        if spec.min_capacity_bytes and self.spec.capacity_bytes < spec.min_capacity_bytes:
            return False
        if spec.interface and self.spec.interface != spec.interface:
            # Allow SAS3 to replace SAS2 (backwards compatible)
            if not (spec.interface == "SAS2" and self.spec.interface == "SAS3"):
                if not (spec.interface == "SATA2" and self.spec.interface == "SATA3"):
                    return False
        if spec.min_speed_rpm and self.spec.speed_rpm < spec.min_speed_rpm:
            return False
        if spec.form_factor and self.spec.form_factor != spec.form_factor:
            if not (spec.form_factor == "3.5in" and self.spec.form_factor == "2.5in"):
                return False
        return True


@dataclass
class ReplacementSpec:
    """
    Minimum specification for a replacement drive.
    Generated from the failed drive's spec + pool/array config.
    """
    min_capacity_bytes: int = 0
    interface: str = ""
    min_speed_rpm: int = 0
    form_factor: str = ""

    # Context for the operator
    failed_drive_serial: str = ""
    failed_drive_desc: str = ""         # e.g. "Seagate ST300MP0006 300GB SAS3 10K"
    pool_or_array: str = ""
    slot_label: str = ""
    chassis_label: str = ""

    @property
    def min_capacity_gb(self) -> float:
        return self.min_capacity_bytes / (1000 ** 3)

    def to_prose(self) -> str:
        """Human-readable spec summary for operators."""
        parts: list[str] = []
        if self.min_capacity_bytes:
            parts.append(f"at least {self.min_capacity_gb:.0f} GB")
        if self.min_speed_rpm:
            parts.append(f"{self.min_speed_rpm // 1000}K RPM")
        if self.interface:
            parts.append(self.interface)
        return ", ".join(parts) if parts else "any drive"


@dataclass
class FieldGuide:
    """
    Complete field replacement instructions for an operator.
    Generated by ChassisDatabase.build_field_guide().
    """
    failed_serial: str
    chassis_label: str
    physical_location: str              # e.g. "rack 3, U14, third machine down"
    slot_label: str                     # e.g. "Bay 3 (third from left, bottom row)"
    failed_drive_desc: str             # e.g. "Seagate ST300MP0006 S/N WFN2XXXX 300GB SAS3 10K"
    replacement_spec: str              # e.g. "at least 300 GB, 10K, SAS3"
    suggested_stock_items: list[DriveInventoryItem]
    pool_or_array: str = ""
    led_status: str = ""               # "fault LED active" | "locate LED active" | ""
    estimated_resilver_hours: float = 0.0
    notes: str = ""

    def to_text(self) -> str:
        """Plain-text field replacement guide, ready for Slack/SMS/email."""
        lines = [
            "─── DRIVE REPLACEMENT GUIDE ───",
            f"Location:   {self.physical_location}",
            f"Chassis:    {self.chassis_label}",
            f"Slot:       {self.slot_label}",
        ]
        if self.led_status:
            lines.append(f"LED:        {self.led_status}")
        lines += [
            "",
            "WHAT TO PULL:",
            f"  {self.failed_drive_desc}",
            f"  Serial: {self.failed_serial}",
            "",
            "REPLACEMENT SPEC:",
            f"  Pull and replace with {self.replacement_spec}",
            f"  Outgoing drive type: {self.failed_drive_desc}",
        ]
        if self.suggested_stock_items:
            lines += ["", "FROM STOCK (best matches):"]
            for item in self.suggested_stock_items[:3]:
                lines.append(
                    f"  [{item.inventory_id}] {item.spec.short_desc()} "
                    f"S/N {item.serial or '(no serial)'} — {item.stock_location}"
                )
        else:
            lines += ["", "NO MATCHING DRIVES IN STOCK — order required."]
        if self.pool_or_array:
            lines.append(f"\nPool/Array: {self.pool_or_array}")
        if self.estimated_resilver_hours:
            lines.append(f"Est. resilver: {self.estimated_resilver_hours:.1f} hours")
        if self.notes:
            lines.append(f"\nNotes: {self.notes}")
        lines.append("────────────────────────────────")
        return "\n".join(lines)

    def to_dict(self) -> dict:
        d = dataclasses.asdict(self)
        d["suggested_stock_items"] = [dataclasses.asdict(i) for i in self.suggested_stock_items]
        return d


# ──────────────────────────── YAML model loading ───────────────────────────

def _load_yaml_models(path: Path) -> list[ChassisModel]:
    """Load chassis model definitions from a YAML file."""
    try:
        import yaml  # type: ignore
    except ImportError:
        log.warning("PyYAML not available — chassis model file %s skipped", path)
        return []

    try:
        raw = yaml.safe_load(path.read_text()) or []
    except Exception as e:
        log.warning("failed to parse chassis model file %s: %s", path, e)
        return []

    models: list[ChassisModel] = []
    for item in raw:
        try:
            bays = [DriveBay(**b) for b in item.get("bays", [])]
            model = ChassisModel(
                id=item["id"],
                manufacturer=item.get("manufacturer", ""),
                model=item.get("model", ""),
                description=item.get("description", ""),
                form_factor=item.get("form_factor", ""),
                is_jbod=item.get("is_jbod", False),
                bays=bays,
            )
            models.append(model)
        except Exception as e:
            log.warning("skipping malformed chassis model entry: %s — %s", item.get("id", "?"), e)
    return models


def load_all_chassis_models(
    builtin_dir: Path | None = None,
    custom_file: Path | None = None,
) -> dict[str, ChassisModel]:
    """
    Load the full chassis model database:
    1. Built-in seed models from agent/chassis_models/*.yaml
    2. User custom models from /var/lib/ozma/chassis-custom.yaml

    Returns dict keyed by model ID (custom overrides built-in with same ID).
    """
    if builtin_dir is None:
        builtin_dir = Path(__file__).parent / "chassis_models"
    if custom_file is None:
        custom_file = Path("/var/lib/ozma/chassis-custom.yaml")

    models: dict[str, ChassisModel] = {}

    # Built-in models
    if builtin_dir.exists():
        for yaml_file in sorted(builtin_dir.glob("*.yaml")):
            for m in _load_yaml_models(yaml_file):
                models[m.id] = m

    # Custom/user-defined models (override built-in)
    if custom_file.exists():
        for m in _load_yaml_models(custom_file):
            models[m.id] = m

    log.debug("loaded %d chassis models", len(models))
    return models


# ──────────────────────────── Main database class ──────────────────────────

class ChassisDatabase:
    """
    Central registry: chassis model definitions, physical instances,
    drive inventory, and replacement guide generation.

    Persists instance + inventory state to /var/lib/ozma/chassis-db.json.
    Chassis model definitions live in YAML files (source of truth).
    """

    def __init__(
        self,
        state_path: Path = Path("/var/lib/ozma/chassis-db.json"),
        custom_models_path: Path = Path("/var/lib/ozma/chassis-custom.yaml"),
    ):
        self._state_path = state_path
        self._custom_models_path = custom_models_path
        self._models: dict[str, ChassisModel] = {}
        self._instances: dict[str, ChassisInstance] = {}
        self._inventory: dict[str, DriveInventoryItem] = {}
        self._reload_models()
        self._load_state()

    def _reload_models(self) -> None:
        self._models = load_all_chassis_models(custom_file=self._custom_models_path)

    def _load_state(self) -> None:
        if not self._state_path.exists():
            return
        try:
            data = json.loads(self._state_path.read_text())
            for inst_data in data.get("instances", []):
                slots_raw = inst_data.pop("slots", {})
                loc_raw = inst_data.pop("location", {})
                location = PhysicalLocation(**loc_raw)
                inst = ChassisInstance(location=location, **inst_data)
                inst.slots = {int(k): SlotState(**v) for k, v in slots_raw.items()}
                self._instances[inst.chassis_id] = inst
            for inv_data in data.get("inventory", []):
                spec_raw = inv_data.pop("spec", {})
                spec = DriveSpec(**spec_raw)
                item = DriveInventoryItem(spec=spec, **inv_data)
                self._inventory[item.inventory_id] = item
        except Exception as e:
            log.warning("failed to load chassis DB state: %s", e)

    def _save_state(self) -> None:
        self._state_path.parent.mkdir(parents=True, exist_ok=True)
        payload: dict[str, Any] = {
            "instances": [],
            "inventory": [],
        }
        for inst in self._instances.values():
            d = dataclasses.asdict(inst)
            payload["instances"].append(d)
        for item in self._inventory.values():
            payload["inventory"].append(dataclasses.asdict(item))
        self._state_path.write_text(json.dumps(payload, indent=2))

    # ── Chassis model access ───────────────────────────────────────────────

    def list_models(self) -> list[ChassisModel]:
        return list(self._models.values())

    def get_model(self, model_id: str) -> ChassisModel | None:
        return self._models.get(model_id)

    def search_models(self, query: str) -> list[ChassisModel]:
        q = query.lower()
        return [
            m for m in self._models.values()
            if q in m.manufacturer.lower()
            or q in m.model.lower()
            or q in m.description.lower()
            or q in m.id.lower()
        ]

    def add_custom_model(self, model: ChassisModel) -> None:
        """Add or update a user-defined chassis model (saved to custom YAML)."""
        try:
            import yaml  # type: ignore
        except ImportError:
            raise RuntimeError("PyYAML required to save custom chassis models")

        existing: list[dict] = []
        if self._custom_models_path.exists():
            import yaml as _yaml
            existing = _yaml.safe_load(self._custom_models_path.read_text()) or []

        # Remove existing entry with same ID
        existing = [e for e in existing if e.get("id") != model.id]
        existing.append(dataclasses.asdict(model))
        self._custom_models_path.parent.mkdir(parents=True, exist_ok=True)
        import yaml as _yaml
        self._custom_models_path.write_text(_yaml.dump(existing, default_flow_style=False))
        self._models[model.id] = model

    # ── Chassis instances ──────────────────────────────────────────────────

    def register_chassis(self, instance: ChassisInstance) -> None:
        """Register a physical chassis instance."""
        if instance.model_id not in self._models:
            raise ValueError(
                f"unknown chassis model '{instance.model_id}' — "
                f"add it to agent/chassis_models/ or via add_custom_model()"
            )
        self._instances[instance.chassis_id] = instance
        # Pre-populate empty slot states from model
        model = self._models[instance.model_id]
        for bay in model.bays:
            if bay.slot not in instance.slots:
                instance.slots[bay.slot] = SlotState(slot=bay.slot)
        self._save_state()

    def unregister_chassis(self, chassis_id: str) -> bool:
        if chassis_id not in self._instances:
            return False
        del self._instances[chassis_id]
        self._save_state()
        return True

    def list_instances(self) -> list[ChassisInstance]:
        return list(self._instances.values())

    def get_instance(self, chassis_id: str) -> ChassisInstance | None:
        return self._instances.get(chassis_id)

    def update_slot(self, chassis_id: str, slot: int, state: SlotState) -> bool:
        inst = self._instances.get(chassis_id)
        if not inst:
            return False
        inst.slots[slot] = state
        self._save_state()
        return True

    def find_drive(self, serial: str) -> tuple[ChassisInstance, SlotState] | None:
        """Locate a drive by serial number across all registered chassis."""
        for inst in self._instances.values():
            slot = inst.slot_for_serial(serial)
            if slot:
                return inst, slot
        return None

    def sync_from_enclosure_manager(
        self, chassis_id: str, enclosure_data: list[dict]
    ) -> None:
        """
        Update slot states from EnclosureManager.list_slots() output.
        enclosure_data: list of dicts with keys matching SlotState fields.
        """
        inst = self._instances.get(chassis_id)
        if not inst:
            return
        for item in enclosure_data:
            slot_idx = item.get("slot_index", item.get("slot", -1))
            if slot_idx < 0:
                continue
            s = inst.get_slot(slot_idx)
            s.drive_serial = item.get("serial", "")
            s.drive_model = item.get("model", "")
            s.drive_capacity_bytes = item.get("capacity_bytes", 0)
            s.drive_interface = item.get("interface", "")
            s.drive_speed_rpm = item.get("speed_rpm", 0)
            s.drive_form_factor = item.get("form_factor", "")
            s.drive_health = item.get("health", "unknown")
            s.pool_or_array = item.get("pool_or_array", "")
            s.last_seen_ts = time.time()
        self._save_state()

    # ── Parts inventory ────────────────────────────────────────────────────

    def add_to_inventory(self, item: DriveInventoryItem) -> None:
        self._inventory[item.inventory_id] = item
        self._save_state()

    def remove_from_inventory(self, inventory_id: str) -> bool:
        if inventory_id not in self._inventory:
            return False
        del self._inventory[inventory_id]
        self._save_state()
        return True

    def list_inventory(
        self,
        available_only: bool = True,
        interface: str = "",
        min_capacity_gb: float = 0,
    ) -> list[DriveInventoryItem]:
        items = list(self._inventory.values())
        if available_only:
            items = [i for i in items if i.is_available()]
        if interface:
            items = [i for i in items if i.spec.interface == interface]
        if min_capacity_gb:
            threshold = min_capacity_gb * 1e9
            items = [i for i in items if i.spec.capacity_bytes >= threshold]
        return items

    def reserve_for_replacement(self, inventory_id: str, chassis_id: str, slot: int) -> bool:
        item = self._inventory.get(inventory_id)
        if not item or not item.is_available():
            return False
        item.reserved_for = f"{chassis_id}:{slot}"
        self._save_state()
        return True

    def unreserve(self, inventory_id: str) -> bool:
        item = self._inventory.get(inventory_id)
        if not item:
            return False
        item.reserved_for = ""
        self._save_state()
        return True

    def find_matching_inventory(
        self, spec: "ReplacementSpec", available_only: bool = True
    ) -> list[DriveInventoryItem]:
        """
        Find all stock items that satisfy the minimum replacement spec.
        Results sorted best-first: exact capacity match before oversized,
        new before refurbished/used.
        """
        candidates = [
            i for i in self._inventory.values()
            if (not available_only or i.is_available()) and i.matches_spec(spec)
        ]
        # Sort: condition ("new" first), then capacity (closest to minimum first)
        condition_order = {"new": 0, "refurbished": 1, "used-tested": 2}
        candidates.sort(key=lambda i: (
            condition_order.get(i.condition, 9),
            i.spec.capacity_bytes,
        ))
        return candidates

    # ── Replacement spec generation ────────────────────────────────────────

    def derive_replacement_spec(
        self,
        failed_serial: str,
        chassis_id: str | None = None,
    ) -> ReplacementSpec | None:
        """
        Build a ReplacementSpec from the failed drive's known attributes.

        Capacity: same-or-greater (for pool/array integrity).
        Interface: exact match (SAS2 → SAS2 or SAS3 ok; SATA2 → SATA2/3 ok).
        Speed: same-or-faster RPM class (7200 → 7200+; 10K → 10K+; 15K → 15K).
        Form factor: exact match (2.5" slot can't take 3.5" drive physically).
        """
        # Try to find from registered chassis
        found = self.find_drive(failed_serial)
        if not found and chassis_id:
            inst = self._instances.get(chassis_id)
            if inst:
                found = (inst, inst.slot_for_serial(failed_serial))

        if not found:
            return None

        inst, slot_state = found
        model = self._models.get(inst.model_id)
        bay = model.bay_by_slot(slot_state.slot) if model else None

        spec = ReplacementSpec(
            failed_drive_serial=failed_serial,
            min_capacity_bytes=slot_state.drive_capacity_bytes,
            interface=slot_state.drive_interface,
            min_speed_rpm=slot_state.drive_speed_rpm,
            form_factor=slot_state.drive_form_factor,
            pool_or_array=slot_state.pool_or_array,
            failed_drive_desc=DriveSpec(
                model=slot_state.drive_model,
                manufacturer=slot_state.drive_manufacturer,
                capacity_bytes=slot_state.drive_capacity_bytes,
                interface=slot_state.drive_interface,
                speed_rpm=slot_state.drive_speed_rpm,
                form_factor=slot_state.drive_form_factor,
            ).short_desc(),
            slot_label=bay.label if bay else f"Slot {slot_state.slot}",
            chassis_label=inst.label or inst.chassis_id,
        )

        # Constrain to what the bay supports
        if bay:
            if spec.interface and spec.interface not in bay.supported_interfaces:
                # Bay may support a newer standard; pick the fastest supported
                iface_rank = {"NVMe": 5, "SAS3": 4, "SAS2": 3, "SATA3": 2, "SATA2": 1}
                supported_ranked = sorted(
                    bay.supported_interfaces,
                    key=lambda x: iface_rank.get(x, 0),
                    reverse=True,
                )
                if supported_ranked:
                    spec.interface = supported_ranked[0]

        return spec

    # ── Field guide generation ─────────────────────────────────────────────

    def build_field_guide(
        self,
        failed_serial: str,
        chassis_id: str | None = None,
        led_status: str = "",
        estimated_resilver_hours: float = 0.0,
    ) -> FieldGuide | None:
        """
        Generate a complete field replacement guide for an operator.

        Returns a FieldGuide with:
        - Physical location prose ("rack 3, U14, third machine down")
        - Slot label ("third from left, bottom row")
        - Full description of what to pull (make, model, serial, capacity, interface)
        - Minimum spec of what to put in
        - Ranked list of matching drives from stock inventory
        """
        spec = self.derive_replacement_spec(failed_serial, chassis_id)
        if not spec:
            return None

        found = self.find_drive(failed_serial)
        if not found:
            return None

        inst, slot_state = found
        model = self._models.get(inst.model_id)
        bay = model.bay_by_slot(slot_state.slot) if model else None

        stock_matches = self.find_matching_inventory(spec)

        failed_drive_desc = DriveSpec(
            model=slot_state.drive_model,
            manufacturer=slot_state.drive_manufacturer,
            capacity_bytes=slot_state.drive_capacity_bytes,
            interface=slot_state.drive_interface,
            speed_rpm=slot_state.drive_speed_rpm,
            form_factor=slot_state.drive_form_factor,
        ).short_desc()
        if failed_serial:
            failed_drive_desc += f" S/N {failed_serial}"

        return FieldGuide(
            failed_serial=failed_serial,
            chassis_label=inst.label or inst.chassis_id,
            physical_location=inst.location.to_prose(),
            slot_label=bay.label if bay else f"Slot {slot_state.slot}",
            failed_drive_desc=failed_drive_desc,
            replacement_spec=spec.to_prose(),
            suggested_stock_items=stock_matches[:5],
            pool_or_array=slot_state.pool_or_array,
            led_status=led_status,
            estimated_resilver_hours=estimated_resilver_hours,
        )

    def build_all_field_guides(self) -> list[FieldGuide]:
        """Generate field guides for all currently-failed drives."""
        guides: list[FieldGuide] = []
        for inst in self._instances.values():
            for slot in inst.failed_slots():
                guide = self.build_field_guide(slot.drive_serial, inst.chassis_id)
                if guide:
                    guides.append(guide)
        return guides

    # ── Visual slot map ────────────────────────────────────────────────────

    def render_slot_map(self, chassis_id: str) -> str:
        """
        ASCII art slot map of a chassis showing drive health.

        Example (CSE-826, 12 bays in 2 rows of 6):

          CSE-826 "Main Storage" — rack 3, U14
          ┌──────┬──────┬──────┬──────┬──────┬──────┐  row A
          │  OK  │  OK  │ FAIL │  OK  │  OK  │  OK  │
          ├──────┼──────┼──────┼──────┼──────┼──────┤  row B
          │  OK  │  OK  │  OK  │  OK  │ MISS │  OK  │
          └──────┴──────┴──────┴──────┴──────┴──────┘
        """
        inst = self._instances.get(chassis_id)
        if not inst:
            return f"chassis '{chassis_id}' not found"

        model = self._models.get(inst.model_id)
        if not model:
            return f"model '{inst.model_id}' not in database"

        # Group bays by row
        rows: dict[int, list[DriveBay]] = {}
        for bay in sorted(model.bays, key=lambda b: (b.row, b.col)):
            rows.setdefault(bay.row, []).append(bay)

        header = f"  {model.manufacturer} {model.model} — {inst.label or inst.chassis_id}"
        header += f"\n  {inst.location.to_prose()}\n"

        # Health symbols
        STATUS_SYM = {
            "healthy": " OK ",
            "warning": "WARN",
            "failed":  "FAIL",
            "unknown": " ?? ",
        }

        lines = [header]
        row_keys = sorted(rows.keys())
        row_labels = "ABCDEFGHIJKLMNOPQRSTUVWXYZ"
        for i, row_key in enumerate(row_keys):
            bays = rows[row_key]
            top = "  ┌" + "┬".join("──────" for _ in bays) + "┐"
            mid_parts = []
            for bay in bays:
                slot_state = inst.get_slot(bay.slot)
                if slot_state.is_empty():
                    sym = "EMPTY"
                else:
                    sym = STATUS_SYM.get(slot_state.drive_health, " ?? ")
                mid_parts.append(f"  {sym} ")
            mid = "  │" + "│".join(mid_parts) + "│"
            row_lbl = row_labels[i] if i < len(row_labels) else str(i)
            if i < len(row_keys) - 1:
                bot = "  ├" + "┼".join("──────" for _ in bays) + f"┤  row {row_lbl}"
            else:
                bot = "  └" + "┴".join("──────" for _ in bays) + f"┘  row {row_lbl}"
            lines += [top, mid, bot]

        return "\n".join(lines)

    # ── Integration with storage_manager.py ───────────────────────────────

    def on_vdev_degraded(
        self,
        pool: str,
        vdev_serial: str,
        chassis_id: str | None = None,
    ) -> FieldGuide | None:
        """
        Called by StorageHealthManager when a vdev transitions to DEGRADED.
        Looks up the drive, marks it as failed in slot state, generates
        field guide. Returns the guide or None if chassis not registered.
        """
        found = self.find_drive(vdev_serial)
        if not found and chassis_id:
            inst = self._instances.get(chassis_id)
            if inst:
                for slot in inst.slots.values():
                    if slot.drive_serial == vdev_serial:
                        found = (inst, slot)
                        break

        if not found:
            return None

        inst, slot_state = found
        slot_state.drive_health = "failed"
        slot_state.pool_or_array = pool
        self._save_state()

        return self.build_field_guide(
            vdev_serial,
            chassis_id=inst.chassis_id,
            led_status="fault LED active (amber)",
        )


# ──────────────────────────── Convenience API ──────────────────────────────

_db: ChassisDatabase | None = None


def get_db() -> ChassisDatabase:
    """Module-level singleton for use by storage_manager.py integration."""
    global _db
    if _db is None:
        _db = ChassisDatabase()
    return _db
