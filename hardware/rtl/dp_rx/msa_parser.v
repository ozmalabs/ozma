// Ozma Dock — Main Stream Attribute (MSA) Parser
// SPDX-License-Identifier: Apache-2.0
//
// Extracts video timing and format parameters from the MSA header that the
// source transmits at the start of each frame on the main link.
//
// MSA fields of interest:
//   Mvid / Nvid      fractional pixel clock recovery
//   H_TOTAL          total horizontal pixels (active + blanking)
//   V_TOTAL          total vertical lines
//   H_START          horizontal active start
//   V_START          vertical active start
//   H_WIDTH          active pixel width
//   V_HEIGHT         active pixel height
//   MISC0            colour depth, YCbCr/RGB, sync polarity
//   MISC1            interlace, stereo, VSC-SDP present
//
// Spec reference: VESA DisplayPort Standard v1.4, Section 2.2.3 (MSA)
//
// Status: STUB

`default_nettype none

module msa_parser (
    input  wire        clk,
    input  wire        rst_n,

    // From decoder_8b10b (lane 0 carries MSA in BS/BE packet boundaries)
    input  wire  [7:0] lane0_data,
    input  wire        lane0_is_control,
    input  wire        lane0_valid,

    // Parsed timing outputs (registered, stable after BS→BE window)
    output reg  [15:0] h_total,
    output reg  [15:0] v_total,
    output reg  [15:0] h_width,
    output reg  [15:0] v_height,
    output reg   [4:0] bit_depth,   // 6, 8, 10, 12, 16
    output reg   [1:0] colorspace,  // 0=RGB, 1=YCbCr422, 2=YCbCr444
    output reg         msa_valid    // pulses high when a complete MSA is parsed
);

// TODO: implement MSA extraction state machine
//
//  DP main link packetization:
//    K28.0 (BS) marks blank start
//    K28.5 (SR) is the sync reset during blank
//    K28.2 (BE) marks blank end → active video follows
//    MSA bytes are transmitted in the horizontal blanking period
//    after BS, before BE, on lane 0 (and mirrored on other lanes)
//
//  State machine: WAIT_BS → READ_MSA_BYTES (34 bytes) → WAIT_BE → ACTIVE
//  Parse the 34-byte MSA header into the output registers.

assign msa_valid = 1'b0;  // stub

endmodule

`default_nettype wire
