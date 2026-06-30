#version 450

// One jump-flood step. For the current step size (in pixels) each pixel checks
// its 8 neighbours at that offset and adopts whichever neighbour's seed is
// closer than its own current best, independently for the inside and outside
// fields. Run with halving step sizes (start >= the band radius down to 1) the
// fields converge to the nearest-seed offset for every pixel in O(log radius)
// passes instead of the brute-force O(radius^2) disc scan.

layout(location = 0) in vec2 v_uv;
layout(location = 0) out vec4 out_coord;

layout(set = 0, binding = 0) uniform sampler2D u_coord; // RG = inside, BA = outside

layout(push_constant) uniform Push {
    vec4 p; // xy = 1/canvas_size, z = step size (px), w unused
} push;

void main() {
    vec2 texel = push.p.xy;
    float step = push.p.z;

    vec4 self = texture(u_coord, v_uv);
    vec2 best_in = self.xy;
    vec2 best_out = self.zw;
    float dist_in = dot(best_in, best_in);
    float dist_out = dot(best_out, best_out);

    for (int dy = -1; dy <= 1; ++dy) {
        for (int dx = -1; dx <= 1; ++dx) {
            if (dx == 0 && dy == 0) { continue; }
            vec2 s = vec2(float(dx), float(dy)) * step;
            vec2 nuv = v_uv + texel * s;
            // Skip out-of-canvas neighbours: clamp-to-edge would return the
            // border pixel's offset, and adding the (large) step to it fabricates
            // a phantom near-zero distance (a JFA border artifact).
            if (any(lessThan(nuv, vec2(0.0))) || any(greaterThan(nuv, vec2(1.0)))) {
                continue;
            }
            vec4 n = texture(u_coord, nuv);
            // The neighbour stores the offset from ITS pixel to its seed; from
            // here that seed is `s` further away.
            vec2 cand_in = n.xy + s;
            float d_in = dot(cand_in, cand_in);
            if (d_in < dist_in) { dist_in = d_in; best_in = cand_in; }
            vec2 cand_out = n.zw + s;
            float d_out = dot(cand_out, cand_out);
            if (d_out < dist_out) { dist_out = d_out; best_out = cand_out; }
        }
    }

    out_coord = vec4(best_in, best_out);
}
