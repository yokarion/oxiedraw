#version 450

// Same attribute layout as `dab.vert`. Textured family uses rotation +
// aspect to transform the unit quad, and emits a per-fragment UV from
// the quad corner.
layout(location = 0) in vec2 a_quad;
layout(location = 1) in vec2 a_center;
layout(location = 2) in float a_radius;
layout(location = 3) in float a_rotation;
layout(location = 4) in float a_aspect;
layout(location = 5) in float a_flow;
layout(location = 6) in vec4 a_color;
layout(location = 7) in vec4 a_texture_uv;   // (u0,v0,u1,v1) into the atlas slice

layout(push_constant) uniform Push {
    vec2 inv_size;
    uint slice;
} push;

layout(location = 0) out vec2 v_uv;
layout(location = 1) out vec4 v_color;
layout(location = 2) out float v_flow;

void main() {
    // Apply aspect (squish on Y) then rotation, then radius scale.
    vec2 squished = vec2(a_quad.x, a_quad.y * a_aspect);
    float c = cos(a_rotation);
    float s = sin(a_rotation);
    vec2 rotated = vec2(c * squished.x - s * squished.y, s * squished.x + c * squished.y);
    vec2 local = rotated * a_radius;
    vec2 pixel = a_center + local;
    vec2 clip = pixel * push.inv_size - vec2(1.0);
    gl_Position = vec4(clip, 0.0, 1.0);

    // Map quad corner [-1,+1] → [u0..u1, v0..v1] of the atlas slice.
    vec2 t = (a_quad + vec2(1.0)) * 0.5;
    v_uv = mix(a_texture_uv.xy, a_texture_uv.zw, t);
    v_color = a_color;
    v_flow = a_flow;
}
