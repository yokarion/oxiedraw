#version 450

// Composite one premultiplied-BGRA layer (src) over an accumulator (dst)
// using a selectable separable blend mode and a layer opacity. The shader
// fully computes the src-over-dst result and writes it (the pipeline blend
// state is disabled / replace), so callers ping-pong through a scratch copy
// of the accumulator rather than relying on fixed-function blending.
//
// Both inputs are premultiplied; the output is premultiplied too. The blend
// math follows the W3C Compositing spec: the source colour mixed towards the
// blended colour by the backdrop alpha, then plain src-over.

layout(location = 0) in vec2 v_uv;
layout(location = 0) out vec4 out_color;

layout(set = 0, binding = 0) uniform sampler2D u_src;
layout(set = 1, binding = 0) uniform sampler2D u_dst;

layout(push_constant) uniform Push {
    uint mode;
    float opacity;
} pc;

vec3 blend_overlay(vec3 cb, vec3 cs) {
    return mix(2.0 * cb * cs, 1.0 - 2.0 * (1.0 - cb) * (1.0 - cs), step(0.5, cb));
}

void main() {
    vec4 s = texture(u_src, v_uv) * pc.opacity;
    vec4 d = texture(u_dst, v_uv);

    // Unpremultiply for the blend math; guard against divide-by-zero.
    vec3 sc = s.a > 0.0 ? s.rgb / s.a : vec3(0.0);
    vec3 dc = d.a > 0.0 ? d.rgb / d.a : vec3(0.0);

    vec3 blended;
    switch (pc.mode) {
        case 1u: blended = sc * dc; break;                 // Multiply
        case 2u: blended = min(sc + dc, vec3(1.0)); break;  // Addition
        case 3u: blended = min(sc, dc); break;              // Darken
        case 4u: blended = sc + dc - sc * dc; break;        // Screen
        case 5u: blended = blend_overlay(dc, sc); break;    // Overlay
        default: blended = sc; break;                       // Normal
    }

    // Source colour mixed towards the blended colour by the backdrop alpha,
    // then standard src-over. Output stays premultiplied.
    vec3 src_color = mix(sc, blended, d.a);
    float out_a = s.a + d.a * (1.0 - s.a);
    vec3 out_rgb = src_color * s.a + dc * d.a * (1.0 - s.a);
    out_color = vec4(out_rgb, out_a);
}
