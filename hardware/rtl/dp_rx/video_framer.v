// Ozma Dock — Video Framer
// SPDX-License-Identifier: Apache-2.0
//
// Reassembles pixel data from the DisplayPort transport packet stream into
// a conventional pixel clock + HSync + VSync + DE + RGB/YCbCr output, suitable
// for feeding into a frame buffer or downstream video processor.
//
// Operates after link training is complete (link_training.link_ready == 1)
// and after the MSA has been parsed (msa_parser.msa_valid == 1).
//
// Spec reference: VESA DisplayPort Standard v1.4, Section 2.2 (Main Link)
//
// Status: STUB

`default_nettype none

module video_framer #(
    parameter LANES     = 4,
    parameter MAX_WIDTH = 3840,
    parameter MAX_HEIGHT = 2160
) (
    input  wire        clk,
    input  wire        rst_n,

    // Control
    input  wire        link_ready,     // from link_training
    input  wire        msa_valid,      // from msa_parser

    // Timing from MSA parser
    input  wire [15:0] h_total,
    input  wire [15:0] v_total,
    input  wire [15:0] h_width,
    input  wire [15:0] v_height,
    input  wire  [4:0] bit_depth,
    input  wire  [1:0] colorspace,

    // Pixel data from 8b/10b decoder (all lanes)
    input  wire [8*LANES-1:0] lane_data,
    input  wire [LANES-1:0]   lane_is_control,
    input  wire [LANES-1:0]   lane_valid,

    // Output: standard video timing interface
    output reg         pclk,          // recovered pixel clock
    output reg         hsync,
    output reg         vsync,
    output reg         de,            // display enable (active pixel)
    output reg [47:0]  pixel_rgb,     // 16 bits per channel (normalised from bit_depth)
    output reg         frame_start    // pulses at top-left of each frame
);

// TODO: implement pixel reassembly
//
//  DP packs pixels across lanes in pixel-interleaved order.
//  Pixel extraction depends on bit_depth and lane_count:
//    8bpc RGB, 4 lanes: R0 G0 B0 R1 G1 B1... distributed across lanes
//    Exact packing described in DP spec Section 2.2.4 (Pixel Data)
//
//  The framer must:
//    1. Strip control symbols (BS/SR/BE/FS/FE etc.) from the data stream
//    2. Track horizontal pixel count and vertical line count from MSA timing
//    3. Reassemble per-pixel RGB from the interleaved lane bytes
//    4. Generate hsync/vsync/de based on h_total/v_total/h_width/v_height
//    5. Recover pixel clock from the Mvid/Nvid ratio (or use link clock directly)

endmodule

`default_nettype wire
