// Ozma Dock — DisplayPort AUX Channel Controller
// SPDX-License-Identifier: Apache-2.0
//
// Implements the DisplayPort AUX channel: a half-duplex, Manchester-encoded
// 1 Mbps channel used for DPCD register access and link training negotiation.
//
// Spec reference: VESA DisplayPort Standard v1.4, Section 2.7 (AUX channel)
//
// Status: STUB — not yet implemented
//
// Interface:
//   aux_p / aux_n   differential AUX pins (connect to FPGA IOB)
//   dpcd_addr       19-bit DPCD register address
//   dpcd_wdata      8-bit write data
//   dpcd_rdata      8-bit read data
//   dpcd_wen        write enable (pulse)
//   dpcd_ren        read enable (pulse)
//   dpcd_ack        transaction complete
//   dpcd_nack       transaction failed (retry or error)
//
// Build order:
//   1. aux_channel.v      ← you are here
//   2. dpcd_regs.v        sink register file that this module accesses locally
//   3. link_training.v    drives transactions on this module
//   4. decoder_8b10b.v
//   5. msa_parser.v
//   6. video_framer.v

`default_nettype none

module aux_channel (
    input  wire        clk,        // system clock (≥ 100 MHz recommended)
    input  wire        rst_n,

    // AUX differential pair (connect via IOBUF to pad)
    inout  wire        aux_p,
    inout  wire        aux_n,

    // DPCD access port (from link_training / msa_parser)
    input  wire [19:0] dpcd_addr,
    input  wire  [7:0] dpcd_wdata,
    output reg   [7:0] dpcd_rdata,
    input  wire        dpcd_wen,
    input  wire        dpcd_ren,
    output reg         dpcd_ack,
    output reg         dpcd_nack
);

// TODO: implement AUX channel state machine
//
// Required states:
//   IDLE → START_PATTERN → COMMAND → ADDRESS → LENGTH →
//   DATA (TX or RX) → STOP → ACK_WAIT → DONE / RETRY
//
// Manchester encoding: bit period = 1 µs (1 Mbps)
//   Clock is derived from system clock via counter.
//
// The source (laptop) drives AUX for commands; sink (us) drives AUX for replies.
// The IOBUF direction must be controlled per transaction phase.
//
// Reference implementations to study:
//   - enjoy-digital/litedp (Python/Migen, partial)
//   - hdmi2usb/HDMI2USB-firmware-nextgen (different protocol but AUX concepts)

assign dpcd_ack  = 1'b0;  // stub
assign dpcd_nack = 1'b0;  // stub

endmodule

`default_nettype wire
