#version 450

// Downsamples the selection mask into a smaller R8 buffer (typically 4x
// smaller in each dim). The result is read back to host memory and traced on
// the CPU into the marching-ants polylines - the live outline during a mask
// brush stroke, where tracing the full-resolution mask costs too much.
//
// The pass writes the filtered mask value straight through; finding the
// boundary is the CPU tracer's job.

layout(location = 0) in vec2 v_uv;
layout(location = 0) out vec4 out_color;

layout(set = 0, binding = 0) uniform sampler2D u_mask;

// Box-average the whole 4x4 block. One linear tap only averages the middle
// 2x2, which drops 12 of every 16 texels - a brush thinner than the block
// could miss every sampled texel and vanish from the outline entirely. Four
// taps, each landing on a quadrant's shared corner, cover all 16 exactly.
void main() {
    vec2 texel = 1.0 / vec2(textureSize(u_mask, 0));
    float sum = texture(u_mask, v_uv + texel * vec2(-1.0, -1.0)).r
              + texture(u_mask, v_uv + texel * vec2( 1.0, -1.0)).r
              + texture(u_mask, v_uv + texel * vec2(-1.0,  1.0)).r
              + texture(u_mask, v_uv + texel * vec2( 1.0,  1.0)).r;
    out_color = vec4(sum * 0.25, 0.0, 0.0, 0.0);
}
