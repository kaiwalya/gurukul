// Fragment shader for the GPU pitch-trace polyline mesh.
//
// Clips the mesh to the pitch-lane rect in world space and applies
// per-vertex colour (written by `build_trace_mesh`) with a right-edge fade
// so the leading edge of the trace dissolves rather than hard-cuts.

#import bevy_sprite::mesh2d_vertex_output::VertexOutput

struct ClipRect {
    rect: vec4<f32>,
};

@group(2) @binding(0)
var<uniform> clip: ClipRect;

@fragment
fn fragment(in: VertexOutput) -> @location(0) vec4<f32> {
    let wx = in.world_position.x;
    let wy = in.world_position.y;
    // Discard fragments outside the lane rect.
    if wx < clip.rect.x || wx > clip.rect.z || wy < clip.rect.y || wy > clip.rect.w {
        discard;
    }
    // Fade the right edge over 1 world-unit so the trace head dissolves.
    let right_fade = clamp(clip.rect.z - wx, 0.0, 1.0);
#ifdef VERTEX_COLORS
    return vec4<f32>(in.color.rgb, in.color.a * right_fade);
#else
    return vec4<f32>(1.0, 1.0, 1.0, right_fade);
#endif
}
