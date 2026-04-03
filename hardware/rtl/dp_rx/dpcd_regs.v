// Ozma Dock — DisplayPort Configuration Data (DPCD) Register File
// SPDX-License-Identifier: Apache-2.0
//
// The DPCD is the sink's "config space". The source (laptop) reads it via
// the AUX channel to discover sink capabilities and configure the link.
//
// Spec reference: VESA DisplayPort Standard v1.4, Section 2.9 (DPCD)
//   Key registers:
//   0x00000  DPCD_REV              (report 0x14 = DP 1.4)
//   0x00001  MAX_LINK_RATE         (0x0A = HBR2 5.4 Gbps, 0x06 = HBR 2.7 Gbps)
//   0x00002  MAX_LANE_COUNT        (bit[4:0] = lane count, bit[7] = enhanced framing)
//   0x00100  LINK_BW_SET           written by source during link training
//   0x00101  LANE_COUNT_SET        written by source during link training
//   0x00102  TRAINING_PATTERN_SET  TPS1/TPS2/TPS3/TPS4 or NORMAL
//   0x00202  LANE0_1_STATUS        lane lock/align status (written by us)
//   0x00204  LANE_ALIGN_STATUS_UPDATED
//   0x0020C  ADJUST_REQUEST_LANE0_1  voltage swing / pre-emphasis to apply
//
// Status: STUB — register map outlined, read/write logic not implemented

`default_nettype none

module dpcd_regs (
    input  wire        clk,
    input  wire        rst_n,

    // AUX channel read/write port
    input  wire [19:0] addr,
    input  wire  [7:0] wdata,
    input  wire        wen,
    input  wire        ren,
    output reg   [7:0] rdata,

    // Decoded outputs consumed by link_training and msa_parser
    output reg   [7:0] link_bw_set,
    output reg   [4:0] lane_count_set,
    output reg   [3:0] training_pattern,
    output reg   [1:0] voltage_swing_lane0,
    output reg   [1:0] pre_emphasis_lane0
);

// TODO: implement register file
//   - Initialize capability registers (DPCD_REV, MAX_LINK_RATE, MAX_LANE_COUNT)
//   - Handle source writes to training/config registers
//   - Drive decoded outputs from register values

endmodule

`default_nettype wire
