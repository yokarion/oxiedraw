#version 450

// Downsamples the selection mask into a smaller R8 buffer (typically 4x
// smaller in each dim) and tags pixels that lie near a selection
// boundary. The output of this pass is read back to host memory and fed
// to a CPU marching-squares contour tracer to produce the marching-ants
// polylines.
//
// We sample the centre value and four cardinal neighbours offset by one
// pixel of the *full-resolution* mask. If the centre is inside (> 0.5)
// and any neighbour is outside (or vice versa), this is a boundary cell.

layout(location = 0) in vec2 v_uv;
layout(location = 0) out vec4 out_color;

layout(set = 0, binding = 0) uniform sampler2D u_mask;

layout(push_constant) uniform Push {
    // 1.0 / mask_dim. Used to step one full-res pixel.
    vec2 inv_size;
} push;

void main() {
    float c = texture(u_mask, v_uv).r;
    float l = texture(u_mask, v_uv + vec2(-push.inv_size.x, 0.0)).r;
    float r = texture(u_mask, v_uv + vec2(push.inv_size.x, 0.0)).r;
    float t = texture(u_mask, v_uv + vec2(0.0, -push.inv_size.y)).r;
    float b = texture(u_mask, v_uv + vec2(0.0, push.inv_size.y)).r;

    float ci = step(0.5, c);
    float li = step(0.5, l);
    float ri = step(0.5, r);
    float ti = step(0.5, t);
    float bi = step(0.5, b);

    // We want the *value* of the mask at this downsampled location so
    // CPU marching squares can do bilinear-style iso extraction. Output
    // the centre value directly — boundary detection happens on the
    // CPU side once we have the small buffer. The shader's only job is
    // to give us a cheap, properly-filtered downsample.
    out_color = vec4(c, 0.0, 0.0, 0.0);
}
