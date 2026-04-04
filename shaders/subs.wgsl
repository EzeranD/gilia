struct VertexOutput {
    @builtin(position) position: vec4<f32>,
    @location(0) uv: vec2<f32>,
}

struct FragmentInput {
    @builtin(position) position: vec4<f32>,
    @location(0) uv: vec2<f32>,
}

@group(0) @binding(0) var sub_texture: texture_2d<u32>;

@vertex
fn vs_main(@builtin(vertex_index) vertex_index: u32) -> VertexOutput {
    const pos = array(
        vec2( -1.0, -1.0),
        vec2( -1.0, 1.0),
        vec2( 1.0 , 1.0),
        vec2( 1.0, 1.0),
        vec2( 1.0, -1.0),
        vec2( -1.0, -1.0)
    );

    const uv = array(
        vec2( 0.0, 1.0),
        vec2( 0.0, 0.0),
        vec2( 1.0 , 0.0),
        vec2( 1.0, 0.0),
        vec2( 1.0, 1.0),
        vec2( 0.0, 1.0)
    );

    let pos_v4 = vec4(pos[vertex_index], 0.0, 1.0);
    let uv_v2 = uv[vertex_index];
    var out: VertexOutput;
    out.position = pos_v4;
    out.uv = uv_v2;
    return out;
}

@fragment
fn fs_main(input: FragmentInput) -> @location(0) vec4f {
    let coord = vec2<i32>(input.uv * vec2<f32>(textureDimensions(sub_texture)));
    return vec4<f32>(textureLoad(sub_texture, coord, 0)) / 255.0;
}