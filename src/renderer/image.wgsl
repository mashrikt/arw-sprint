struct Transform {
    placement: vec4<f32>,
    uv_row0: vec4<f32>,
    uv_row1: vec4<f32>,
};

@group(0) @binding(0) var image_texture: texture_2d<f32>;
@group(0) @binding(1) var image_sampler: sampler;
@group(1) @binding(0) var<uniform> transform: Transform;

struct VertexOutput {
    @builtin(position) position: vec4<f32>,
    @location(0) uv: vec2<f32>,
    @location(1) @interpolate(flat) brightness: f32,
};

@vertex
fn vs_main(@builtin(vertex_index) index: u32) -> VertexOutput {
    let coordinates = array<vec2<f32>, 6>(
        vec2<f32>(0.0, 0.0), vec2<f32>(0.0, 1.0), vec2<f32>(1.0, 1.0),
        vec2<f32>(0.0, 0.0), vec2<f32>(1.0, 1.0), vec2<f32>(1.0, 0.0)
    );
    let uv = coordinates[index];
    let position = vec2<f32>(uv.x * 2.0 - 1.0, 1.0 - uv.y * 2.0);
    var result: VertexOutput;
    result.position = vec4<f32>(
        position * transform.placement.xy + transform.placement.zw, 0.0, 1.0
    );
    let basis = vec3<f32>(uv, 1.0);
    result.uv = vec2<f32>(
        dot(transform.uv_row0.xyz, basis), dot(transform.uv_row1.xyz, basis)
    );
    // Reuse transform padding. The CPU computes 2^stops only on adjustment;
    // every pixel needs just a multiply after the sRGB texture is linearized.
    result.brightness = transform.uv_row0.w;
    return result;
}

@fragment
fn fs_main(input: VertexOutput) -> @location(0) vec4<f32> {
    let color = textureSample(image_texture, image_sampler, input.uv);
    return vec4<f32>(color.rgb * input.brightness, color.a);
}

@vertex
fn vs_background(@builtin(vertex_index) index: u32) -> @builtin(position) vec4<f32> {
    let corners = array<vec2<f32>, 3>(
        vec2<f32>(-1.0, -1.0), vec2<f32>(3.0, -1.0), vec2<f32>(-1.0, 3.0)
    );
    return vec4<f32>(corners[index], 0.0, 1.0);
}

@fragment
fn fs_background() -> @location(0) vec4<f32> {
    return vec4<f32>(0.006, 0.006, 0.006, 1.0);
}
