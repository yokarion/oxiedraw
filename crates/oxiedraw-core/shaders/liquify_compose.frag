#version 450

// Liquify displacement-field update.
//
// The field stores, per canvas pixel, the offset the warp pass adds before
// sampling the source: out(p) = source(p + D(p)). A visual push of `v` is
// therefore stored as `-v`, which is why every mode below negates.
//
// A batch of dabs is applied by *composition*, not addition:
//
//     D_new(p) = d(p) + D_old(p + d(p))
//
// i.e. W_new = W_old . W_delta for W(p) = p + D(p). Plain addition drifts
// visibly once displacements exceed a brush radius, so pushing already-pushed
// pixels has to go through the old field at the shifted position.
//
// Reconstruct is the other branch: it scales the existing field toward zero at
// `p` with no advection. Both are accumulated in the same pass, so a batch that
// mixes them still has a well-defined result.

layout(location = 0) in vec2 v_uv;
layout(location = 0) out vec2 out_field;

layout(set = 0, binding = 0) uniform sampler2D u_field;      // RG16F, current field
layout(set = 0, binding = 1) uniform sampler2D u_selection;  // R8, selection mask

struct Dab {
    vec4 center_drag; // xy = centre (canvas px), zw = drag (un-mirrored frame)
    vec4 linear;      // row-major 2x2 of this copy's symmetry element
    vec4 params;      // x = radius, y = density, z = strength, w = mode code
};

layout(std430, set = 0, binding = 2) readonly buffer Dabs {
    Dab dabs[];
} u_dabs;

layout(push_constant) uniform PC {
    // xy = canvas size in pixels, z = dab count, w = 1 when a selection is live
    vec4 info;
} pc;

const int MODE_FORWARD_WARP = 0;
const int MODE_TWIRL        = 1;
const int MODE_PUCKER       = 2;
const int MODE_BLOAT        = 3;
const int MODE_PUSH_LEFT    = 4;
const int MODE_RECONSTRUCT  = 5;

// Radians of twirl per dab at full strength.
const float TWIRL_RATE = 0.16;
// Fraction of the local radius pulled in per pucker/bloat dab at full strength.
const float PINCH_RATE = 0.12;
// Multiplier turning the drag vector into a push at full strength; 0.5 (the
// default strength) reproduces the raw drag one-to-one.
const float PUSH_RATE = 2.0;
// Fraction of the field removed per reconstruct dab at full strength.
const float RECONSTRUCT_RATE = 0.25;

// 1 at the dab centre, 0 at its rim. `density` hardens the profile: 0 is a
// smooth bump, 1 is a nearly flat top with a narrow shoulder.
//
// Written as `1 - smoothstep(inner, 1, t)` rather than `smoothstep(1, inner, t)`
// because GLSL leaves smoothstep undefined when edge0 >= edge1, and a driver is
// free to lower it with an early-out that would make this return 0 everywhere -
// silently disabling the whole tool. The two forms are algebraically identical
// for edge0 < edge1, and this matches `dab_textured.frag` / `mask_textured.frag`.
float falloff(float dist, float radius, float density) {
    float t = clamp(dist / radius, 0.0, 1.0);
    float inner = clamp(density, 0.0, 1.0) * 0.95;
    return 1.0 - smoothstep(inner, 1.0, t);
}

void main() {
    vec2 canvas = pc.info.xy;
    int count = int(pc.info.z + 0.5);
    // Fragment centres land on pixel centres, so this is canvas-pixel space.
    vec2 p = v_uv * canvas;

    vec2 delta = vec2(0.0);
    float erode = 0.0;

    for (int i = 0; i < count; ++i) {
        Dab dab = u_dabs.dabs[i];
        vec2 offset = p - dab.center_drag.xy;
        float radius = dab.params.x;
        float dist = length(offset);
        if (dist > radius) {
            continue;
        }
        float fall = falloff(dist, radius, dab.params.y);
        if (fall <= 0.0) {
            continue;
        }
        float strength = dab.params.z;
        int mode = int(dab.params.w + 0.5);

        if (mode == MODE_RECONSTRUCT) {
            erode += fall * abs(strength) * RECONSTRUCT_RATE;
            continue;
        }

        // GLSL fills mat2 column-major; `linear` is row-major.
        vec4 l = dab.linear;
        mat2 m = mat2(l.x, l.z, l.y, l.w);
        // Both symmetry matrices are orthogonal, so the transpose inverts them.
        // Evaluate the mode in the un-mirrored frame, then map the resulting
        // vector back - that is what makes a mirrored twirl spin the other way
        // and a mirrored push travel the other way, for free.
        mat2 m_inv = transpose(m);
        vec2 local = m_inv * offset;
        vec2 drag = dab.center_drag.zw;
        vec2 d_local = vec2(0.0);

        if (mode == MODE_FORWARD_WARP) {
            d_local = -drag * (fall * strength * PUSH_RATE);
        } else if (mode == MODE_TWIRL) {
            d_local = vec2(-local.y, local.x) * (fall * strength * TWIRL_RATE);
        } else if (mode == MODE_PUCKER) {
            d_local = local * (fall * strength * PINCH_RATE);
        } else if (mode == MODE_BLOAT) {
            d_local = -local * (fall * strength * PINCH_RATE);
        } else if (mode == MODE_PUSH_LEFT) {
            // Left of the direction of travel, in y-down canvas coordinates.
            d_local = -vec2(drag.y, -drag.x) * (fall * strength * PUSH_RATE);
        }

        delta += m * d_local;
    }

    // An active selection is the mask: everything outside it is protected from
    // every mode. Zeroing the delta is all composition needs to leave those
    // pixels alone, since d(p) == 0 makes D_new(p) == D_old(p). Content can
    // still be dragged *in* from outside, which is what Photoshop does too.
    float pass_through = 1.0;
    if (pc.info.w > 0.5) {
        pass_through = clamp(texture(u_selection, v_uv).r, 0.0, 1.0);
    }
    delta *= pass_through;
    erode = clamp(erode * pass_through, 0.0, 1.0);

    vec2 shifted_uv = (p + delta) / canvas;
    vec2 old = texture(u_field, shifted_uv).rg;
    out_field = (delta + old) * (1.0 - erode);
}
