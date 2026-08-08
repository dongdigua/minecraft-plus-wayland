struct Uniforms {
  camera_dir: mat2x3f, // x and y, then cross to get z
  fov: f32,        // 垂直视场角（弧度）
  camera_pos: vec3f,
  resolution: vec2f,
  state_sample_index: u32,
  mask: u32,
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
  enabled: u32,
  texvof: mat2x3f, // row0: +x +y +z, row1: -x -y -z
}

struct VSOutput {
  @builtin(position) position: vec4f,
  @location(0) coord: vec2f, // [-1, 1]，在 fs 里重建光线方向
};

@group(0) @binding(0) var torchTextures: texture_2d_array<f32>;
// layer 0 = redstone, 1 = copper, 2 = soul, 3 = torch, 4 = smooth stone
@group(0) @binding(1) var<uniform> uniforms: Uniforms;

const NUM_BOXES = 16;
@group(0) @binding(2) var<storage, read> boxes: array<Box, NUM_BOXES>;

const PI = acos(-1.0);

const INF = 1e10;

fn hitAABB(ray: Ray, aabb: mat2x3f) -> Hit {
  let invD = 1.0 / ray.rd;
  let t0 = (aabb[0] - ray.ro) * invD;
  let t1 = (aabb[1] - ray.ro) * invD;

  let tmin = min(t0, t1);
  let tmax = max(t0, t1);

  let tNear = max(max(tmin.x, tmin.y), tmin.z);
  let tFar = min(min(tmax.x, tmax.y), tmax.z);

  if (tFar + 1e-5 < max(tNear, 0.0)) {
    return Hit(1e10, vec3f(0.0), vec3f(0.0), vec4f(0.0), Material(0, 0));
  }

  let eps = 1e-4;

  let isNear = tNear >= 0.0;
  let t = select(tFar, tNear, isNear);
  let p = ray.ro + ray.rd * (t - eps);
  //let p = ray.ro + ray.rd * t;

  // 用命中点位置反推法线，对零厚度薄片也健壮
  let onMin = abs(p - aabb[0]) < vec3f(eps);
  let onMax = abs(p - aabb[1]) < vec3f(eps);
  // 零厚度轴：p 同时落在 min 和 max 面上
  let thin = onMin & onMax;
  // 对零厚度轴，法线朝向光线来向（入口）或去向（出口）
  let thinNormal = select(sign(ray.rd), -sign(ray.rd), isNear);
  var normal = vec3f(0.0);
  normal = select(normal, thinNormal, thin);
  // 对有厚度的轴，min 面法线为 -1，max 面法线为 +1
  normal = select(normal, vec3f(-1.0), onMin & !thin);
  normal = select(normal, vec3f(1.0), onMax & !thin);

  return Hit(t, p, normal, vec4f(0.0), Material(0, 0));
}

fn calculateCollision(ray: Ray, directLight: bool) -> Hit {
  var nearestHit = Hit(1e10, vec3f(0), vec3f(0), vec4f(0), Material(0, 0));
  var color = vec4f(0);

  for (var i = 0u; i < NUM_BOXES; i = i + 1u) {
    if boxes[i].enabled == 0u {
      continue;
    }
    let hit = hitAABB(ray, boxes[i].aabb);
    let a = abs(hit.normal);
    let u_axis = vec3f(a.y + a.z, a.x, 0.0);
    let v_axis = vec3f(0.0, a.z, a.x + a.y);

    let axis_idx = u32(dot(abs(hit.normal), vec3f(0.0, 1.0, 2.0)));
    let sign_idx = u32((1.0 - dot(hit.normal, vec3f(1.0))) * 0.5);

    let off = vec2f(0.0, boxes[i].texvof[sign_idx][axis_idx]);
    let uv = hit.p * mat2x3f(u_axis, v_axis) + off;

    let texUV = vec2u((floor(fract(uv) * 16)));

    var lit = textureLoad(torchTextures, texUV, boxes[i].texidx, 0u);

    // 红石火把在直接光采样下的特判，侧面薄片不遮挡火把芯，统一火把芯的直接光照逻辑
    if directLight {
      if 1 <= i && i <= 4 && texUV.y > 7 {
        lit.a = 0;
      } else if i == 5 {
        lit += vec4f(1, 0, 0, 0);
      }
    }

    if hit.t < nearestHit.t && lit.a > 0.5 {
      nearestHit = hit;
      nearestHit.color = lit;
      nearestHit.material = boxes[i].material;
    }
  }
  return nearestHit;
}

fn rand(state: ptr<function, u32>) -> f32 {
  *state = *state * 747796405 + 2891336453;
  var result = ((*state >> ((*state >> 28) + 4)) ^ *state) * 277803737;
  result = (result >> 22) ^ result;
  return f32(result) / 4294967296.0;
}

fn randNorm(state: ptr<function, u32>) -> f32 {
  let theta = 2.0 * PI * rand(state);
  let u = max(rand(state), 1e-10);  // 防止 log(0) = -inf
  let rho = sqrt(-2.0 * log(u));
  return rho * cos(theta);
}

fn randDir(norm: vec3f, state: ptr<function, u32>) -> vec3f {
  let v = normalize(vec3f(randNorm(state), randNorm(state), randNorm(state)));
  return normalize(select(v + norm, norm, length(v) < 1e-3));
}

fn ambientLight(ray: Ray) -> vec4f {
  let skyZenith = vec3f(0.47, 0.65, 1);
  let skyHorizon = vec3f(1);
  let groundColor = vec3f(0);
  let skyGradient = pow(smoothstep(0, 0.4, dot(ray.rd, vec3f(0, 0, 1))), 0.35);
  let sky = mix(skyHorizon, skyZenith, skyGradient);
  return vec4f(select(groundColor, sky, ray.rd.z > 0), 1);
}

fn sampleDirectLight(rngState: ptr<function, u32>) -> LightSample {
  var selectedIdx = u32(rand(rngState) * 4) * 2 + 5;
  let selected = boxes[selectedIdx];

  let faceNormals = array<vec3f, 6>(
    vec3f(-1,  0,  0), vec3f( 1,  0,  0),
    vec3f( 0, -1,  0), vec3f( 0,  1,  0),
    vec3f( 0,  0, -1), vec3f( 0,  0,  1),
  );
  let faceIdx5 = u32(rand(rngState) * 5);
  let faceIdx = select(faceIdx5, faceIdx5 + 1u, faceIdx5 >= 4u); // skip bottom face (index 4)
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
  var pdf = 0.25 / 5;
  pdf /= area;
  return LightSample(point, faceNormals[faceIdx], pdf);
}

fn sampleSkyLight(hit: Hit, rngState: ptr<function, u32>) -> vec3f {
    let envRay = Ray(hit.p + hit.normal * 1e-5, randDir(hit.normal, rngState));
    let envHit = calculateCollision(envRay, false);

    if envHit.t < 1e5 {
      return vec3f(0.0);
    }

    let environmentRadiance = ambientLight(envRay).rgb;
    // randDir 是 cosine-weighted：
    // 所以不需要再乘 cosine 或除 PI。
    return hit.color.rgb * environmentRadiance;
}

fn trace(rray: Ray, rngState: ptr<function, u32>) -> vec4f {
  var ray = rray;
  var light = vec4f(0);

  var hit = calculateCollision(ray, false);

  if hit.t > 1e5 {
    return vec4f(0, 0, 0, 1);
  }

  if hit.material.light_level > 0.5 {
    light += hit.material.light_level * hit.color * 10;
    return light;
  }

  ray.ro = hit.p;

  let directLight = sampleDirectLight(rngState);

  ray.rd = normalize(directLight.pos - ray.ro);
  let shadowHit = calculateCollision(ray, true);

  if length(shadowHit.p - directLight.pos) < 1e-3 {
    let emittedLight = shadowHit.material.light_level * hit.color * 10;
    let lightStrength = max(dot(hit.normal, ray.rd), 0.0);
    let G = max(dot(directLight.normal, -ray.rd), 0.0) / (length(hit.p - shadowHit.p) * length(hit.p - shadowHit.p));
    light += emittedLight * shadowHit.color * lightStrength * G / PI / directLight.pdf;
  } else {
    light += vec4f(sampleSkyLight(hit, rngState), 0);
  }
  return vec4f(light.rgb, 1);
}

@vertex
fn vs(
  @builtin(vertex_index) vertexIndex : u32
) -> VSOutput {
  let pos = array(
    vec2f(-1.0, -1.0),
    vec2f(3.0,  -1.0),
    vec2f(-1.0, 3.0),
  );

  let xy = pos[vertexIndex];
  var vsOutput: VSOutput;
  vsOutput.position = vec4f(xy, 0.0, 1.0); // [-1,1]
  vsOutput.coord = xy;
  return vsOutput;
}

const NUM_RAYS = 4;

@fragment
fn fs(fsIn: VSOutput) -> @location(0) vec4f {
  let aspect = uniforms.resolution.x / uniforms.resolution.y;
  let d = 1.0 / tan(uniforms.fov / 2.0);
  let v = normalize(vec3f(fsIn.coord.x * aspect, fsIn.coord.y, d));
  let camera_z = -normalize(cross(uniforms.camera_dir[0], uniforms.camera_dir[1]));
  let rd = v.x * uniforms.camera_dir[0] + v.y * uniforms.camera_dir[1] + v.z * camera_z;

  let pixel = u32(fsIn.position.y * uniforms.resolution.x + fsIn.position.x);
  var rngState = pixel ^ (uniforms.mask * 2246822519u) ^ (uniforms.state_sample_index * 3266489917u);
  var light = vec4f(0);

  for (var i = 0u; i < NUM_RAYS; i = i + 1u) {
    light += trace(Ray(uniforms.camera_pos, rd), &rngState);
  }

  return light / f32(NUM_RAYS);
}

@group(0) @binding(3) var accumulated_hdr: texture_2d<f32>;

fn tonemapped_color(position: vec4f) -> vec3f {
  let color = textureLoad(accumulated_hdr, vec2i(position.xy), 0).rgb;
  let lum = dot(color, vec3f(0.2126, 0.7152, 0.0722));
  return color / (1.0 + lum);
}

fn srgbToLinear(channel: f32) -> f32 {
  if channel <= 0.04045 {
    return channel / 12.92;
  }
  return pow((channel + 0.055) / 1.055, 2.4);
}

// The Web demo writes tone-mapped numeric RGB to an UNORM canvas. Compensate only when the
// surface itself performs an automatic sRGB encode, matching the other Web-derived modules.
@fragment
fn fs_present_srgb(@builtin(position) position: vec4f) -> @location(0) vec4f {
  let color = tonemapped_color(position);
  return vec4f(
    srgbToLinear(color.r),
    srgbToLinear(color.g),
    srgbToLinear(color.b),
    1.0,
  );
}

@fragment
fn fs_present_unorm(@builtin(position) position: vec4f) -> @location(0) vec4f {
  return vec4f(tonemapped_color(position), 1.0);
}
