struct Uniforms {
    viewport: vec2<f32>,
    approach_progress: f32,
    dissolve_progress: f32,
    red: u32,
    dissolving: u32,
    _padding: vec2<u32>,
};

struct VertexOutput {
    @builtin(position) position: vec4<f32>,
    @location(0) uv: vec2<f32>,
};

@group(0) @binding(0) var creeper_sampler: sampler;
@group(0) @binding(1) var creeper_texture: texture_2d<f32>;
@group(0) @binding(2) var<uniform> uniforms: Uniforms;

fn hash2u_to_1f(value: vec2<u32>) -> f32 {
    var state = value.x * 1597u + value.y * 3571u;
    state = state * 747796405u + 2891336453u;
    state = ((state >> ((state >> 28u) + 4u)) ^ state) * 277803737u;
    state = (state >> 22u) ^ state;
    return f32(state) / 4294967296.0;
}

fn gradient_at(cell: vec2<u32>) -> vec2<f32> {
    // Kept intentionally identical to the supplied pixel-dissolve demo: omitting TAU here is part
    // of the visual, even though the hash therefore covers only a one-radian angular interval.
    let angle = hash2u_to_1f(cell);
    return vec2<f32>(cos(angle), sin(angle));
}

fn perlin_at(position: vec2<u32>) -> f32 {
    let top_left = gradient_at(position);
    let top_right = gradient_at(position + vec2<u32>(0u, 1u));
    let bottom_left = gradient_at(position + vec2<u32>(1u, 0u));
    let bottom_right = gradient_at(position + vec2<u32>(1u, 1u));
    let uv = vec2<f32>(0.5, 0.5);
    return mix(
        mix(
            dot(top_left, uv - vec2<f32>(0.0, 0.0)),
            dot(top_right, uv - vec2<f32>(1.0, 0.0)),
            uv.x,
        ),
        mix(
            dot(bottom_left, uv - vec2<f32>(0.0, 1.0)),
            dot(bottom_right, uv - vec2<f32>(1.0, 1.0)),
            uv.x,
        ),
        uv.y,
    );
}

@vertex
fn vs_main(@builtin(vertex_index) vertex_index: u32) -> VertexOutput {
    let coordinates = array<vec2<f32>, 6>(
        vec2<f32>(0.0, 0.0),
        vec2<f32>(0.0, 1.0),
        vec2<f32>(1.0, 0.0),
        vec2<f32>(0.0, 1.0),
        vec2<f32>(1.0, 1.0),
        vec2<f32>(1.0, 0.0),
    );
    let uv = coordinates[vertex_index];
    let viewport = max(uniforms.viewport, vec2<f32>(1.0));
    let short_side = min(viewport.x, viewport.y);
    // Module 12 maps linear time through scale^3 as the creepers approach the camera.
    let side = short_side * 0.5 * pow(clamp(uniforms.approach_progress, 0.0, 1.0), 3.0);
    let clip_size = vec2<f32>(side * 2.0 / viewport.x, side * 2.0 / viewport.y);

    var output: VertexOutput;
    output.position = vec4<f32>(
        (uv.x - 0.5) * clip_size.x,
        (0.5 - uv.y) * clip_size.y,
        0.0,
        1.0,
    );
    output.uv = uv;
    return output;
}

fn saturate(value: f32) -> f32 {
    return clamp(value, 0.0, 1.0);
}

fn animated_color(input: VertexOutput) -> vec4<f32> {
    let sampled = textureSample(creeper_texture, creeper_sampler, input.uv);
    var color = vec4<f32>(sampled.rgb, 1.0);
    if uniforms.red != 0u {
        // Swapping red and green turns the green texture red while preserving black facial pixels
        // and neutral gray highlights.
        color = vec4<f32>(sampled.g, sampled.r, sampled.b, 1.0);
    }
    if uniforms.dissolving == 0u || uniforms.dissolve_progress <= 0.0 {
        return color;
    }
    if uniforms.dissolve_progress >= 1.0 {
        return vec4<f32>(0.0, 0.0, 0.0, 1.0);
    }

    let block_count = 8.0;
    let max_cell = vec2<u32>(7u, 7u);
    let cell = min(vec2<u32>(floor(input.uv * block_count)), max_cell);
    let shifted_cell = min(
        vec2<u32>(floor((input.uv + vec2<f32>(0.1)) * block_count)),
        max_cell,
    );
    let noise = perlin_at(cell);
    // The supplied demo dissolves top-down. Inverting its y slope preserves the same mask while
    // applying the product decision that the creeper must dissolve bottom-up.
    let y_slope = 1.0 - input.uv.y;
    let eased = smoothstep(0.0, 1.0, uniforms.dissolve_progress);
    // The demo's sine supplied a cyclic threshold in [-2.5, 0.5]. For this deterministic 8x8
    // mask, exhaustively evaluating every cell and shifted-cell combination gives a maximum base
    // mask of about 0.222. Start just above it so the first dissolve frame exactly continues the
    // intact head instead of traversing an oversized negative-mask interval. Keep the demo's -2.5
    // endpoint, which places every cell safely beyond the disappearing edge.
    let threshold = mix(0.25, -2.5, eased);
    let mask = noise - floor(y_slope * block_count) / block_count - threshold;
    let mask_shift = noise - perlin_at(shifted_cell);
    let mask_3d = mask + 0.7 * mask_shift;

    // The demo used vec3(0.34), matching its gray clear color. This lock scene is black, so black
    // is the equivalent disappearing color; the fake-3D mask and white boundary remain unchanged.
    let new_color = mix(color, vec4<f32>(0.0, 0.0, 0.0, 1.0), pow(saturate(mask_3d), 10.0));
    // The boundary exists only on the positive, disappearing side. Feeding a negative mask into
    // the demo's even power creates a second edge around -1 on native WGSL backends, making pixels
    // brighten as if they reappeared before the real dissolve edge reaches them.
    let light = saturate(1.0 - abs(pow(max(mask_3d, 0.0), 40.0) - 1.0));
    return vec4<f32>(new_color.rgb + vec3<f32>(light), 1.0);
}

fn srgb_to_linear(channel: f32) -> f32 {
    if channel <= 0.04045 {
        return channel / 12.92;
    }
    return pow((channel + 0.055) / 1.055, 2.4);
}

@fragment
fn fs_srgb(input: VertexOutput) -> @location(0) vec4<f32> {
    let color = animated_color(input);
    return vec4<f32>(
        srgb_to_linear(color.r),
        srgb_to_linear(color.g),
        srgb_to_linear(color.b),
        1.0,
    );
}

@fragment
fn fs_unorm(input: VertexOutput) -> @location(0) vec4<f32> {
    return animated_color(input);
}
