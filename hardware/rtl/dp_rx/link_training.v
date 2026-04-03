// Ozma Dock — DisplayPort Link Training Controller
// SPDX-License-Identifier: Apache-2.0
//
// Manages the link training handshake between source (laptop) and sink (us).
// Training proceeds through clock recovery (TPS1), channel equalisation (TPS2),
// and optionally TPS3/TPS4 for DP 1.3+.
//
// The source writes TRAINING_PATTERN_SET via AUX, then sends training patterns
// on the main link lanes. We observe the patterns via the hard SerDes (CDR lock
// and EQ status come from FPGA primitive ports), then report status back via
// DPCD_LANE_STATUS registers, which the source reads via AUX.
//
// Spec reference: VESA DisplayPort Standard v1.4, Section 3.5 (Link Training)
//
// Status: STUB

`default_nettype none

module link_training (
    input  wire       clk,
    input  wire       rst_n,

    // From dpcd_regs
    input  wire [7:0] link_bw_set,
    input  wire [4:0] lane_count_set,
    input  wire [3:0] training_pattern,

    // From hard SerDes primitives (FPGA-specific, e.g. Lattice DPHY/SERDES)
    // These are connected to the SerDes lock/CDR status ports in the top-level.
    input  wire [3:0] serdes_cdr_lock,    // one bit per lane
    input  wire [3:0] serdes_symbol_lock,

    // To dpcd_regs — update lane status registers so source can read them
    output reg  [7:0] lane_status,        // written back to DPCD 0x202–0x205
    output reg        interlane_align_done,

    // To video_framer — signal that training is complete and video can start
    output reg        link_ready
);

// TODO: implement training state machine
//
//  States:
//    IDLE
//    CR_PHASE      (TPS1 active: wait for serdes_cdr_lock on all lanes)
//    EQ_PHASE      (TPS2 active: wait for serdes_symbol_lock + align)
//    LINK_READY    (training_pattern == NORMAL: signal video_framer)
//    FAILED        (timeout or too many retries: deassert link_ready)
//
//  On each state transition, write updated lane status to dpcd_regs so
//  the source's next AUX read reflects current lock status.

assign link_ready = 1'b0;  // stub

endmodule

`default_nettype wire
