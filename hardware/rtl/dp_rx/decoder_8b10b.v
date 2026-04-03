// Ozma Dock — 8b/10b Decoder (4-lane, parallel)
// SPDX-License-Identifier: Apache-2.0
//
// Decodes the 8b/10b line encoding used in DisplayPort 1.0–1.4 (HBR1/HBR2/HBR3).
// Each lane produces one decoded byte + a control/data indicator per symbol period.
//
// Spec reference: VESA DisplayPort Standard v1.4, Section 1.4 (8b/10b encoding)
// Also: ANSI X3.230-1994, Fibre Channel Physical and Signaling Interface
//
// Status: STUB — well-understood algorithm, many open reference implementations.
//   Recommend porting from an existing open 8b/10b core rather than writing from
//   scratch. Good references:
//     - opencores.org/projects/8b10b_encdec
//     - github.com/alexforencich/verilog-ethernet (uses 8b/10b internally)
//
// Note: DP 2.0 uses 128b/132b encoding instead. Target that separately when
//   USB4 Gen 3 (40 Gbps) support is needed. This module is HBR1/HBR2/HBR3 only.

`default_nettype none

module decoder_8b10b #(
    parameter LANES = 4
) (
    input  wire              clk,
    input  wire              rst_n,

    // Raw 10-bit symbols from SerDes (one per lane per symbol period)
    input  wire [10*LANES-1:0] raw_symbols,
    input  wire [LANES-1:0]    symbol_valid,

    // Decoded output
    output reg  [8*LANES-1:0]  data_out,      // decoded bytes
    output reg  [LANES-1:0]    is_control,    // 1 = K-character (control), 0 = data
    output reg  [LANES-1:0]    decode_error   // running disparity or invalid symbol
);

// TODO: implement 8b/10b lookup table decode for each lane
//   Standard approach: 512-entry ROM indexed by {RD, 10b_symbol}
//   Output: {8b_data, RD_next, is_control, is_valid}

endmodule

`default_nettype wire
