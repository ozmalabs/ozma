# Open DisplayPort Receiver RTL

Open-source DisplayPort receiver for the Ozma Dock. Implements the DP sink
(receive) side of the DisplayPort protocol in synthesisable Verilog, targeting
Lattice ECP5 (V1, HBR1) and CrossLink-NX (V2, HBR2).

The VESA DisplayPort specification is publicly available at vesa.org (free
registration). No NDA, no license fee. All RTL in this directory is Apache-2.0.

**Ozma does not pay for protocol IP licences.** The DP receiver stack is
implemented from the public specification because that is the correct approach
for an open hardware project — and because a from-scratch implementation can
be audited, forked, and improved by the community in a way that licensed black-box
IP cannot. The only unavoidably closed component is the FPGA vendor's hard SerDes
primitive, which handles physical signalling at the wire level.

---

## Layer map

```
Physical (SerDes)     Hard FPGA IP — unavoidable. Handles CDR, EQ, lane bonding.
                      Vendor-specific instantiation in hardware/rtl/top/dp_rx_top.v

AUX channel           aux_channel.v     1 Mbps Manchester-encoded control channel
DPCD registers        dpcd_regs.v       Sink capability + training config register file
Link training         link_training.v   TPS1/TPS2/TPS3 state machine, lane status reporting
8b/10b decode         decoder_8b10b.v   4-lane parallel symbol decode
MSA parsing           msa_parser.v      Extract resolution/depth/colorspace from header
Video framing         video_framer.v    Pixel reassembly → hsync/vsync/de/RGB output
Frame buffer          frame_buffer.v    Dual-port BRAM store, async read for SoC DMA
MST branch            mst_branch.v      Multi-stream topology (V2 target, not started)
```

---

## Build order

Implement and test in this sequence — each module depends on the one above it
being stable before the next is useful to build:

1. **`aux_channel.v`** — everything else is unreachable without this. The source
   (laptop) will not start sending video until it has successfully read DPCD via
   AUX and completed link training.

2. **`dpcd_regs.v`** — the register file that AUX channel reads/writes. Initialise
   capability registers (DPCD_REV=0x14, MAX_LINK_RATE=0x0A, MAX_LANE_COUNT=0x04)
   so the source knows what to negotiate.

3. **`link_training.v`** — respond to TPS1/TPS2 patterns with lane status. CDR lock
   comes from the hard SerDes; report it back via DPCD_LANE_STATUS so the source
   advances to the next training phase.

4. **`decoder_8b10b.v`** — once training is complete the source sends normal video
   packets encoded in 8b/10b. Port an existing open implementation rather than
   writing from scratch.

5. **`msa_parser.v`** — extract resolution and color format from the MSA header
   sent during horizontal blanking.

6. **`video_framer.v`** — reassemble pixel data from the 4-lane interleaved stream
   into a conventional video timing output.

7. **`frame_buffer.v`** — store completed frames; expose a DMA read port to the
   RK3588S for H.265 encoding.

---

## FPGA targets

### V1 — Lattice ECP5 (fully open toolchain)

- Toolchain: yosys + nextpnr-ecp5 + prjtrellis
- SerDes: ECP5 DCUA primitive, up to 3.2 Gbps per lane
- DP speed: HBR1 (2.7 Gbps per lane) → 1080p30 max on 4 lanes
- Synthesis: `make synth TARGET=ecp5`

### V2 — Lattice CrossLink-NX

- Toolchain: Lattice Radiant (primary) / nextpnr-nexus (improving)
- SerDes: LSCC SERDES, up to 6.25 Gbps per lane
- DP speed: HBR2 (5.4 Gbps per lane) → 1080p60 / 4K30 on 4 lanes
- Synthesis: `make synth TARGET=crosslink-nx`

---

## Simulation

Each module has a corresponding testbench in `hardware/sim/dp_rx/`:

```bash
make sim MODULE=aux_channel    # iverilog + vvp
make sim MODULE=decoder_8b10b
make sim MODULE=msa_parser
make sim MODULE=video_framer
```

Waveform output: `hardware/sim/dp_rx/<module>.vcd` (view with GTKWave).

---

## References

- VESA DisplayPort Standard v1.4 (register at vesa.org, free)
- enjoy-digital/litedp — LiteX-based DP TX/RX, Python/Migen
- ANSI X3.230-1994 — 8b/10b encoding specification
- Lattice ECP5 SerDes Usage Guide (FPGA-TN-02083)
- Lattice CrossLink-NX Hardware User Guide (FPGA-UG-02033)
