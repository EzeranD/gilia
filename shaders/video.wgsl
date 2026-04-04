struct VertexOutput {
    @builtin(position) position: vec4<f32>,
    @location(0) uv: vec2<f32>,
}

struct FragmentInput {
    @builtin(position) position: vec4<f32>,
    @location(0) uv: vec2<f32>,
}

struct VideoUniform {
    multiplier: f32,
    is_packed: u32,
    scale: vec2<f32>,
    color_matrix: array<vec4<f32>, 3>,
}

@group(0) @binding(0) var y_texture: texture_2d<f32>;
@group(0) @binding(1) var u_texture: texture_2d<f32>;
@group(0) @binding(2) var v_texture: texture_2d<f32>;
@group(0) @binding(3) var<uniform> video_uniform: VideoUniform;

@vertex
fn vs_main(@builtin(vertex_index) vertex_index: u32) -> VertexOutput {
    const pos = array(
        vec2(-1.0, -1.0),
        vec2(-1.0, 1.0),
        vec2(1.0, 1.0),
        vec2(1.0, 1.0),
        vec2(1.0, -1.0),
        vec2(-1.0, -1.0)
    );

    const uv = array(
        vec2(0.0, 1.0),
        vec2(0.0, 0.0),
        vec2(1.0, 0.0),
        vec2(1.0, 0.0),
        vec2(1.0, 1.0),
        vec2(0.0, 1.0)
    );

    let pos_v4 = vec4(pos[vertex_index] * video_uniform.scale, 0.0, 1.0);
    let uv_v2 = uv[vertex_index];
    var out: VertexOutput;
    out.position = pos_v4;
    out.uv = uv_v2;
    return out;
}

fn bilinear(texture: texture_2d<f32>, uv: vec2<f32>) -> vec4<f32> {
    let dims = textureDimensions(texture);
    let coord = uv * vec2<f32>(dims);
    let floor_coord = vec2<i32>(floor(coord));
    let weight = fract(coord);
    let max_coord = vec2<i32>(dims) - vec2<i32>(1, 1);
    let tl_coord = clamp(floor_coord, vec2<i32>(0, 0), max_coord);
    let tr_coord = clamp(floor_coord + vec2<i32>(1, 0), vec2<i32>(0, 0), max_coord);
    let bl_coord = clamp(floor_coord + vec2<i32>(0, 1), vec2<i32>(0, 0), max_coord);
    let br_coord = clamp(floor_coord + vec2<i32>(1, 1), vec2<i32>(0, 0), max_coord);
    let tl = textureLoad(texture, tl_coord, 0) * video_uniform.multiplier;
    let tr = textureLoad(texture, tr_coord, 0) * video_uniform.multiplier;
    let br = textureLoad(texture, br_coord, 0) * video_uniform.multiplier;
    let bl = textureLoad(texture, bl_coord, 0) * video_uniform.multiplier;
    let t_lerp = mix(tl, tr, weight.x);
    let b_lerp = mix(bl, br, weight.x);
    return mix(t_lerp, b_lerp, weight.y);
}

@fragment
fn fs_main(input: FragmentInput) -> @location(0) vec4f {
    let y = bilinear(y_texture, input.uv).x;
    var u: f32;
    var v: f32;
    if video_uniform.is_packed > 0 {
        let uv = bilinear(u_texture, input.uv);
        u = uv.x;
        v = uv.y;
    } else {
        u = bilinear(u_texture, input.uv).x;
        v = bilinear(v_texture, input.uv).x;
    }
    let yuv = vec4<f32>(y, u, v, 1.0);

    let r = clamp(dot(yuv, video_uniform.color_matrix[0]), 0.0, 1.0);
    let g = clamp(dot(yuv, video_uniform.color_matrix[1]), 0.0, 1.0);
    let b = clamp(dot(yuv, video_uniform.color_matrix[2]), 0.0, 1.0);

    let px = u32(input.position.x) % 8u;
    let py = u32(input.position.y) % 8u;

    const bayer_matrix: array<u32, 64> = array<u32, 64>(
        0, 32, 8, 40, 2, 34, 10, 42,
        48, 16, 56, 24, 50, 18, 58, 26,
        12, 44, 4, 36, 14, 46, 6, 38,
        60, 28, 52, 20, 62, 30, 54, 22,
        3, 35, 11, 43, 1, 33, 9, 41,
        51, 19, 59, 27, 49, 17, 57, 25,
        15, 47, 7, 39, 13, 45, 5, 37,
        63, 31, 55, 23, 61, 29, 53, 21,
    );

    let threshold = bayer_matrix[py * 8 + px];
    let threshold_f = f32(threshold);
    let bias = ((threshold_f + 0.5) / 64) - 0.5;
    let scaled_r = r * 255.0;
    let scaled_g = g * 255.0;
    let scaled_b = b * 255.0;
    let quantized_r = floor(scaled_r + bias);
    let quantized_g = floor(scaled_g + bias);
    let quantized_b = floor(scaled_b + bias);
    return vec4<f32>(quantized_r / 255.0, quantized_g / 255.0, quantized_b / 255.0, 1.0);
}
