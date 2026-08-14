struct Uniforms {
  camera_dir: mat2x3f,
  fov: f32,
  camera_pos: vec3f,
  resolution: vec2f,
  sample_index: u32,
  component: u32,
};

struct PresentUniforms {
  torch_mask: u32,
};

struct Ray {
  ro: vec3f,
  rd: vec3f,
}

struct Material {
  light_level: f32,
  smoothness: f32,
}

struct Hit {
  t: f32,
  p: vec3f,
  normal: vec3f,
  color: vec4f,
  material: Material,
}

struct LightSample {
  pos: vec3f,
  normal: vec3f,
  pdf: f32,
}

struct Box {
  aabb: mat2x3f,
  material: Material,
  texidx: u32,
  flags: u32,
  texvof: mat2x3f,
}

struct VSOutput {
  @builtin(position) position: vec4f,
  @location(0) coord: vec2f,
};

struct ComponentSample {
  direct_and_emission: vec3f,
  ambient: vec3f,
}

@group(0) @binding(0) var torchTextures: texture_2d_array<f32>;
@group(0) @binding(1) var<uniform> uniforms: Uniforms;

const NUM_BOXES = 18;
@group(0) @binding(2) var<storage, read> boxes: array<Box, NUM_BOXES>;

@group(0) @binding(3) var accumulated_hdr: texture_2d_array<f32>;
@group(0) @binding(4) var<uniform> present_uniforms: PresentUniforms;

const PI = acos(-1.0);
const INF = 1e10;
const HIT_EPS = 1e-4;
const SHEET_EDGE_EPS = 1e-5;
const PARALLEL_EPS = 1e-8;
const NUM_RAYS_PER_COMPONENT = 4u;
const NO_LIGHT_OWNER = 4u;
const REDSTONE_ON_BANK = 0u;
const REDSTONE_OFF_BANK = 1u;

const OWNER_MASK = 7u;
const DIRECT_LIGHT_FLAG = 1u << 3u;
const REDSTONE_ON_ONLY_FLAG = 1u << 4u;
const REDSTONE_OFF_ONLY_FLAG = 1u << 5u;
const REDSTONE_SHEET_FLAG = 1u << 6u;

fn boxLightOwner(flags: u32) -> u32 {
  let encoded = flags & OWNER_MASK;
  if encoded == 0u {
    return NO_LIGHT_OWNER;
  }
  return encoded - 1u;
}

fn boxEnabledInBank(flags: u32, bank: u32) -> bool {
  if (flags & REDSTONE_ON_ONLY_FLAG) != 0u {
    return bank == REDSTONE_ON_BANK;
  }
  if (flags & REDSTONE_OFF_ONLY_FLAG) != 0u {
    return bank == REDSTONE_OFF_BANK;
  }
  return true;
}

fn hitAABB(ray: Ray, aabb: mat2x3f) -> Hit {
  // Parallel slabs need an explicit interval: multiplying a zero distance by an infinite
  // reciprocal produces NaN on some GPUs, especially for zero-thickness boxes.
  let parallel = abs(ray.rd) < vec3f(PARALLEL_EPS);
  let outsideParallelSlab = parallel & ((ray.ro < aabb[0]) | (ray.ro > aabb[1]));
  if any(outsideParallelSlab) {
    return Hit(INF, vec3f(0.0), vec3f(0.0), vec4f(0.0), Material(0.0, 0.0));
  }

  // Keep the slab calculation vectorized. Parallel axes are unconstrained after the bounds check.
  let safeDirection = select(ray.rd, vec3f(1.0), parallel);
  let invD = 1.0 / safeDirection;
  let t0 = (aabb[0] - ray.ro) * invD;
  let t1 = (aabb[1] - ray.ro) * invD;
  let entry = select(min(t0, t1), vec3f(-INF), parallel);
  let exit = select(max(t0, t1), vec3f(INF), parallel);
  let tNear = max(max(entry.x, entry.y), entry.z);
  let tFar = min(min(exit.x, exit.y), exit.z);

  if tFar + 1e-5 < max(tNear, 0.0) {
    return Hit(INF, vec3f(0.0), vec3f(0.0), vec4f(0.0), Material(0.0, 0.0));
  }

  let isNear = tNear >= 0.0;
  let t = select(tFar, tNear, isNear);

  // t is copied from one or more slab bounds. Prefer an exactly collapsed slab at a tie, then
  // pick the first remaining axis. This gives zero-thickness and edge hits one canonical face
  // normal without the tolerance scans or per-axis loop used by the slower fixes.
  let selectedBound = select(exit, entry, isNear);
  let candidates = selectedBound == vec3f(t);
  let collapsed = (aabb[0] == aabb[1]) & !parallel;
  let preferred = select(candidates, collapsed, any(collapsed));
  let previousAxis = vec3<bool>(false, preferred.x, preferred.x | preferred.y);
  let face = preferred & !previousAxis;
  let faceDirection = select(sign(ray.rd), -sign(ray.rd), isNear);
  let normal = select(vec3f(0.0), faceDirection, face);
  let p = ray.ro + ray.rd * (t - HIT_EPS);
  return Hit(t, p, normal, vec4f(0.0), Material(0.0, 0.0));
}

fn insideVisualSheet(ray: Ray, hit: Hit, aabb: mat2x3f) -> bool {
  // Classify the true intersection, not the point biased backward for texture/shadow behavior.
  let geometricP = ray.ro + ray.rd * hit.t;
  let finiteAxis = aabb[1] > aabb[0];
  let interior =
    (geometricP > aabb[0] + vec3f(SHEET_EDGE_EPS)) &
    (geometricP < aabb[1] - vec3f(SHEET_EDGE_EPS));
  return all(!finiteAxis | interior);
}

fn calculateCollision(ray: Ray, directLight: bool, bank: u32, component: u32) -> Hit {
  var nearestHit = Hit(INF, vec3f(0.0), vec3f(0.0), vec4f(0.0), Material(0.0, 0.0));

  for (var i = 0u; i < NUM_BOXES; i = i + 1u) {
    let box = boxes[i];
    if !boxEnabledInBank(box.flags, bank) {
      continue;
    }

    let hit = hitAABB(ray, box.aabb);
    // Misses and occluded boxes cannot replace nearestHit. Reject them before the UV work and
    // texture load; this also makes the robust slab handling cheaper than the old hot path.
    if !(hit.t < nearestHit.t) {
      continue;
    }
    // The lit redstone model uses zero-thickness alpha-cutout sheets. Treat their outer edges as
    // open so an exact edge hit cannot wrap through fract() to a bright texel row.
    if (box.flags & REDSTONE_SHEET_FLAG) != 0u && !insideVisualSheet(ray, hit, box.aabb) {
      continue;
    }

    let a = abs(hit.normal);
    let u_axis = vec3f(a.y + a.z, a.x, 0.0);
    let v_axis = vec3f(0.0, a.z, a.x + a.y);

    let axis_idx = u32(dot(abs(hit.normal), vec3f(0.0, 1.0, 2.0)));
    let sign_idx = u32((1.0 - dot(hit.normal, vec3f(1.0))) * 0.5);
    let off = vec2f(0.0, box.texvof[sign_idx][axis_idx]);
    let uv = hit.p * mat2x3f(u_axis, v_axis) + off;
    let texUV = vec2u(floor(fract(uv) * 16.0));

    var lit = textureLoad(torchTextures, texUV, box.texidx, 0);
    let owner = boxLightOwner(box.flags);
    let belongsToComponent = owner == component;

    // Preserve the hand-tuned redstone direct-light special case from torch/shader.wgsl:
    // visual sheets do not shadow the sampled head, and the head receives the red boost.
    if directLight && bank == REDSTONE_ON_BANK {
      if (box.flags & REDSTONE_SHEET_FLAG) != 0u && belongsToComponent && texUV.y > 7u {
        lit.a = 0.0;
      } else if (box.flags & DIRECT_LIGHT_FLAG) != 0u && belongsToComponent && owner == 0u {
        lit += vec4f(1.0, 0.0, 0.0, 0.0);
      }
    }

    if lit.a > 0.5 {
      nearestHit = hit;
      nearestHit.color = lit;
      let effectiveLight = select(0.0, box.material.light_level, belongsToComponent);
      nearestHit.material = Material(effectiveLight, box.material.smoothness);
    }
  }
  return nearestHit;
}

fn rand(state: ptr<function, u32>) -> f32 {
  *state = *state * 747796405u + 2891336453u;
  var result = ((*state >> ((*state >> 28u) + 4u)) ^ *state) * 277803737u;
  result = (result >> 22u) ^ result;
  return f32(result) / 4294967296.0;
}

fn randNorm(state: ptr<function, u32>) -> f32 {
  let theta = 2.0 * PI * rand(state);
  let u = max(rand(state), 1e-10);
  let rho = sqrt(-2.0 * log(u));
  return rho * cos(theta);
}

fn randDir(norm: vec3f, state: ptr<function, u32>) -> vec3f {
  let v = normalize(vec3f(randNorm(state), randNorm(state), randNorm(state)));
  return normalize(select(v + norm, norm, length(v) < 1e-3));
}

fn ambientLight(ray: Ray) -> vec4f {
  let skyZenith = vec3f(0.47, 0.65, 1.0);
  let skyHorizon = vec3f(1.0);
  let groundColor = vec3f(0.0);
  let skyGradient = pow(smoothstep(0.0, 0.4, dot(ray.rd, vec3f(0.0, 0.0, 1.0))), 0.35);
  let sky = mix(skyHorizon, skyZenith, skyGradient);
  return vec4f(select(groundColor, sky, ray.rd.z > 0.0), 1.0);
}

fn sampleDirectLight(component: u32, rngState: ptr<function, u32>) -> LightSample {
  let lightSourceIndices = array<u32, 4>(5u, 9u, 11u, 13u);
  let selected = boxes[lightSourceIndices[component]];

  let faceNormals = array<vec3f, 6>(
    vec3f(-1.0,  0.0,  0.0), vec3f( 1.0,  0.0,  0.0),
    vec3f( 0.0, -1.0,  0.0), vec3f( 0.0,  1.0,  0.0),
    vec3f( 0.0,  0.0, -1.0), vec3f( 0.0,  0.0,  1.0),
  );
  let faceIdx5 = u32(rand(rngState) * 5.0);
  let faceIdx = select(faceIdx5, faceIdx5 + 1u, faceIdx5 >= 4u);
  let collapseWhichDim = faceIdx / 2u;
  let chooseMsOrMx = faceIdx % 2u;

  let aabb = selected.aabb;
  let fixedCoord = aabb[chooseMsOrMx][collapseWhichDim];
  let dim0 = (collapseWhichDim + 1u) % 3u;
  let dim1 = (collapseWhichDim + 2u) % 3u;

  var point: vec3f;
  point[collapseWhichDim] = fixedCoord;
  point[dim0] = mix(aabb[0][dim0], aabb[1][dim0], rand(rngState));
  point[dim1] = mix(aabb[0][dim1], aabb[1][dim1], rand(rngState));

  let area = abs((aabb[1][dim0] - aabb[0][dim0]) * (aabb[1][dim1] - aabb[0][dim1]));
  var pdf = 1.0 / 5.0;
  pdf /= area;
  return LightSample(point, faceNormals[faceIdx], pdf);
}

fn sampleSkyLight(hit: Hit, rngState: ptr<function, u32>, bank: u32, component: u32) -> vec3f {
  let envRay = Ray(hit.p + hit.normal * 1e-5, randDir(hit.normal, rngState));
  let envHit = calculateCollision(envRay, false, bank, component);

  if envHit.t < 1e5 {
    return vec3f(0.0);
  }

  let environmentRadiance = ambientLight(envRay).rgb;
  return hit.color.rgb * environmentRadiance;
}

// This keeps the authoritative hit epsilon, direct ray origin, five-face sampling, geometric
// term, redstone visibility special case, and sky sampler. The cache decomposition deliberately
// stores ambient separately so four independently switchable direct/emissive components can share
// one all-off environment term at presentation time.
fn traceComponent(
  rray: Ray,
  component: u32,
  bank: u32,
  storeAmbient: bool,
  rngState: ptr<function, u32>,
) -> ComponentSample {
  var ray = rray;
  var directAndEmission = vec3f(0.0);

  let hit = calculateCollision(ray, false, bank, component);
  if hit.t > 1e5 {
    return ComponentSample(vec3f(0.0), vec3f(0.0));
  }

  let hitActiveEmitter = hit.material.light_level > 0.5;
  if hitActiveEmitter {
    directAndEmission += (hit.material.light_level * hit.color * 10.0).rgb;
  } else {
    ray.ro = hit.p;
    let directLight = sampleDirectLight(component, rngState);
    ray.rd = normalize(directLight.pos - ray.ro);
    let shadowHit = calculateCollision(ray, true, bank, component);

    if length(shadowHit.p - directLight.pos) < 1e-3 {
      let emittedLight = shadowHit.material.light_level * hit.color * 10.0;
      let lightStrength = max(dot(hit.normal, ray.rd), 0.0);
      let distance = length(hit.p - shadowHit.p);
      let G = max(dot(directLight.normal, -ray.rd), 0.0) / (distance * distance);
      directAndEmission +=
        (emittedLight * shadowHit.color * lightStrength * G / PI / directLight.pdf).rgb;
    }
  }

  var ambient = vec3f(0.0);
  if storeAmbient {
    ambient = sampleSkyLight(hit, rngState, bank, component);
  }
  return ComponentSample(directAndEmission, ambient);
}

@vertex
fn vs(@builtin(vertex_index) vertexIndex: u32) -> VSOutput {
  let pos = array(
    vec2f(-1.0, -1.0),
    vec2f(3.0, -1.0),
    vec2f(-1.0, 3.0),
  );
  let xy = pos[vertexIndex];
  var output: VSOutput;
  output.position = vec4f(xy, 0.0, 1.0);
  output.coord = xy;
  return output;
}

fn renderComponent(
  cameraRay: Ray,
  pixel: u32,
  component: u32,
  bank: u32,
  ambientChannel: u32,
) -> vec4f {
  var rngState = pixel ^
    (uniforms.sample_index * 3266489917u) ^
    ((component + 1u) * 2246822519u);
  var directAndEmission = vec3f(0.0);
  var ambient = vec3f(0.0);
  let storeAmbient = ambientChannel < 3u;

  for (var i = 0u; i < NUM_RAYS_PER_COMPONENT; i = i + 1u) {
    let sample = traceComponent(cameraRay, component, bank, storeAmbient, &rngState);
    directAndEmission += sample.direct_and_emission;
    ambient += sample.ambient;
  }

  directAndEmission /= f32(NUM_RAYS_PER_COMPONENT);
  ambient /= f32(NUM_RAYS_PER_COMPONENT);
  var packedAmbient = 0.0;
  if storeAmbient {
    packedAmbient = ambient[ambientChannel];
  }
  return vec4f(directAndEmission, packedAmbient);
}

@fragment
fn fs_accumulate(fsIn: VSOutput) -> @location(0) vec4f {
  let aspect = uniforms.resolution.x / uniforms.resolution.y;
  let d = 1.0 / tan(uniforms.fov / 2.0);
  let v = normalize(vec3f(fsIn.coord.x * aspect, fsIn.coord.y, d));
  let camera_z = -normalize(cross(uniforms.camera_dir[0], uniforms.camera_dir[1]));
  let rd = v.x * uniforms.camera_dir[0] + v.y * uniforms.camera_dir[1] + v.z * camera_z;
  let cameraRay = Ray(uniforms.camera_pos, rd);
  let pixel = u32(fsIn.position.y) * u32(uniforms.resolution.x) + u32(fsIn.position.x);

  // Exactly one component is updated per display frame. Redstone uses its lit cross-sheet bank;
  // the other three use the MC-style redstone-off bank and carry the shared ambient RGB in alpha.
  if uniforms.component == 0u {
    return renderComponent(cameraRay, pixel, 0u, REDSTONE_ON_BANK, 3u);
  }
  return renderComponent(
    cameraRay,
    pixel,
    uniforms.component,
    REDSTONE_OFF_BANK,
    uniforms.component - 1u,
  );
}

fn tonemappedColor(position: vec4f) -> vec3f {
  let coord = vec2i(position.xy);
  let redstone = textureLoad(accumulated_hdr, coord, 0, 0);
  let copper = textureLoad(accumulated_hdr, coord, 1, 0);
  let soul = textureLoad(accumulated_hdr, coord, 2, 0);
  let torch = textureLoad(accumulated_hdr, coord, 3, 0);
  let mask = present_uniforms.torch_mask;

  var color = vec3f(copper.a, soul.a, torch.a);
  if (mask & 1u) != 0u {
    color += redstone.rgb;
  }
  if (mask & 2u) != 0u {
    color += copper.rgb;
  }
  if (mask & 4u) != 0u {
    color += soul.rgb;
  }
  if (mask & 8u) != 0u {
    color += torch.rgb;
  }

  let lum = dot(color, vec3f(0.2126, 0.7152, 0.0722));
  return color / (1.0 + lum);
}

fn srgbToLinear(channel: f32) -> f32 {
  if channel <= 0.04045 {
    return channel / 12.92;
  }
  return pow((channel + 0.055) / 1.055, 2.4);
}

@fragment
fn fs_present_srgb(@builtin(position) position: vec4f) -> @location(0) vec4f {
  let color = tonemappedColor(position);
  return vec4f(
    srgbToLinear(color.r),
    srgbToLinear(color.g),
    srgbToLinear(color.b),
    1.0,
  );
}

@fragment
fn fs_present_unorm(@builtin(position) position: vec4f) -> @location(0) vec4f {
  return vec4f(tonemappedColor(position), 1.0);
}
