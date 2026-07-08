let THREE = null;

const G = {
  bg: 0x1d2021,
  orange: 0xd65d0e,
  yellow: 0xfabd2f,
  blue: 0x83a598,
  aqua: 0x8ec07c,
  purple: 0xd3869b,
  fg: 0xebdbb2,
  bg2: 0x504945,
};

const FLAT_ATLAS_MIN_ZOOM = 0.35;
const FLAT_ATLAS_MAX_ZOOM = 6.0;

export function mountRyeOsAmbientScene(canvas, sceneModel = {}, options = {}) {
  let runtime = null;
  let latestSceneModel = sceneModel;
  let latestOptions = options;
  let disposed = false;

  import("https://cdn.jsdelivr.net/npm/three@0.128.0/build/three.module.js")
    .then((module) => {
      if (disposed || !canvas.isConnected) return;
      THREE = module;
      runtime = startRyeOsAmbientScene(canvas, latestSceneModel, latestOptions);
    })
    .catch((error) => {
      console.warn("RyeOS ambient scene disabled; Three.js failed to load", error);
      canvas.dataset.ambientUnavailable = "true";
    });

  return {
    update(nextSceneModel = {}, nextOptions = latestOptions) {
      latestSceneModel = nextSceneModel;
      latestOptions = nextOptions;
      runtime?.update(nextSceneModel, nextOptions);
    },
    dispose() {
      disposed = true;
      runtime?.dispose();
    },
  };
}

function startRyeOsAmbientScene(canvas, sceneModel = {}, options = {}) {
  const namespaceAtlas = options.mode === "namespace_atlas" || options.mode === "atlas_2d" || options.mode === "atlas_paper_3d";
  const atlasStyle = options.atlasStyle || (options.mode === "atlas_paper_3d" ? "paper_3d" : "flat_2d");
  const renderer = new THREE.WebGLRenderer({ canvas, antialias: true, alpha: false });
  renderer.setPixelRatio(Math.min(window.devicePixelRatio || 1, 2));
  renderer.setClearColor(G.bg, 1);

  const scene = new THREE.Scene();
  scene.fog = new THREE.FogExp2(G.bg, namespaceAtlas ? 0.0018 : 0.004);

  const camera = namespaceAtlas
    ? new THREE.OrthographicCamera(-18, 18, 18, -18, 0.1, 1000)
    : new THREE.PerspectiveCamera(55, 1, 0.1, 1000);
  const root = new THREE.Group();
  scene.add(root);

  const state = {
    theta: 0,
    phi: namespaceAtlas ? 0.18 : Math.PI / 2.2,
    radius: namespaceAtlas ? 38 : 45,
    homeTheta: 0,
    homePhi: namespaceAtlas ? 0.18 : Math.PI / 2.2,
    homeRadius: namespaceAtlas ? 38 : 45,
    flatPanX: 0,
    flatPanZ: 0,
    flatZoom: 1,
    homeFlatPanX: 0,
    homeFlatPanZ: 0,
    homeFlatZoom: 1,
    spinVelTheta: namespaceAtlas ? 0 : 0.0012,
    spinVelPhi: 0,
    zoomVel: 0,
    resetting: false,
    dragging: false,
    didDrag: false,
    lastX: 0,
    lastY: 0,
    remotes: [],
    semanticSignature: "",
    namespaceAtlas,
    atlasStyle,
    atlasInteractive: [],
    atlasAnimated: [],
    atlasFocusables: [],
    atlasHover: null,
    atlasFocus: options.atlasFocus || null,
    latestSceneModel: sceneModel,
    canvas,
    pointerNdc: new THREE.Vector2(2, 2),
    disposed: false,
  };

  const spinners = [];
  const shard = namespaceAtlas ? null : makeShard();
  if (shard) root.add(shard.group);

  if (!namespaceAtlas) addRingBands(root, spinners);
  const fragments = namespaceAtlas ? [] : makeFragments(root);
  const streams = namespaceAtlas ? null : makeStreams(root);
  const stars = namespaceAtlas ? null : makeStars(scene);
  const semanticLayer = new THREE.Group();
  root.add(semanticLayer);
  const raycaster = namespaceAtlas ? new THREE.Raycaster() : null;
  if (raycaster) raycaster.domElement = canvas;

  const resize = () => {
    const rect = canvas.getBoundingClientRect();
    const width = Math.max(1, Math.floor(rect.width || window.innerWidth));
    const height = Math.max(1, Math.floor(rect.height || window.innerHeight));
    renderer.setSize(width, height, false);
    camera.aspect = width / height;
    if (namespaceAtlas) {
      const aspect = width / height;
      const halfHeight = 18;
      camera.left = -halfHeight * aspect;
      camera.right = halfHeight * aspect;
      camera.top = halfHeight;
      camera.bottom = -halfHeight;
    }
    camera.updateProjectionMatrix();
  };

  const updateCamera = () => {
    if (isFlatAtlas(state)) {
      camera.position.set(state.flatPanX, 38, state.flatPanZ + 0.01);
      camera.up.set(0, 0, -1);
      camera.lookAt(state.flatPanX, 0, state.flatPanZ);
      camera.zoom = state.flatZoom;
      camera.updateProjectionMatrix();
      return;
    }
    if (namespaceAtlas) {
      camera.position.set(0, state.radius, 0.01);
      camera.up.set(0, 0, -1);
      camera.lookAt(0, 0, 0);
      camera.zoom = 38 / Math.max(12, state.radius);
      camera.updateProjectionMatrix();
      return;
    }
    const x = state.radius * Math.sin(state.phi) * Math.sin(state.theta);
    const y = state.radius * Math.cos(state.phi);
    const z = state.radius * Math.sin(state.phi) * Math.cos(state.theta);
    camera.position.set(x, y, z);
    camera.up.set(0, Math.sin(state.phi) >= 0 ? 1 : -1, 0);
    camera.lookAt(0, 0, 0);
  };

  const onPointerDown = (event) => {
    if (event.button !== 0) return;
    if (state.namespaceAtlas) updatePointerNdc(state, event);
    state.dragging = true;
    state.didDrag = false;
    state.resetting = false;
    state.lastX = event.clientX;
    state.lastY = event.clientY;
    if (isFlatAtlas(state)) {
      state.spinVelTheta = 0;
      state.spinVelPhi = 0;
      state.zoomVel = 0;
    }
    canvas.setPointerCapture?.(event.pointerId);
  };
  const onPointerMove = (event) => {
    if (state.namespaceAtlas) updatePointerNdc(state, event);
    if (!state.dragging) return;
    const dx = event.clientX - state.lastX;
    const dy = event.clientY - state.lastY;
    state.lastX = event.clientX;
    state.lastY = event.clientY;
    if (Math.abs(dx) + Math.abs(dy) > 2) state.didDrag = true;
    if (isFlatAtlas(state)) {
      dispatchAtlasNavigate(state);
      const units = atlasWorldUnitsPerPixel(camera, canvas);
      state.flatPanX -= dx * units;
      state.flatPanZ -= dy * units;
      return;
    }
    state.spinVelTheta = dx * 0.004;
    state.spinVelPhi = -dy * 0.004;
  };
  const onPointerUp = (event) => {
    if (state.namespaceAtlas && !state.didDrag && state.atlasHover) {
      dispatchAtlasSelect(state, state.atlasHover);
    }
    state.dragging = false;
    canvas.releasePointerCapture?.(event.pointerId);
  };
  const onPointerLeave = () => {
    if (!namespaceAtlas) return;
    state.pointerNdc.set(2, 2);
    setAtlasHover(state, null);
  };
  const onWheel = (event) => {
    event.preventDefault();
    state.resetting = false;
    if (isFlatAtlas(state)) {
      dispatchAtlasNavigate(state);
      const before = flatAtlasPointUnderCursor(state, camera, canvas, event);
      state.flatZoom = clamp(
        state.flatZoom * Math.exp(-event.deltaY * 0.0012),
        FLAT_ATLAS_MIN_ZOOM,
        FLAT_ATLAS_MAX_ZOOM,
      );
      const after = flatAtlasPointUnderCursor(state, camera, canvas, event);
      state.flatPanX += before.x - after.x;
      state.flatPanZ += before.z - after.z;
      return;
    }
    state.zoomVel += event.deltaY * 0.008;
    state.spinVelTheta += namespaceAtlas ? 0 : event.deltaX * 0.0004;
  };

  const onKeyDown = (event) => {
    if (event.defaultPrevented || event.altKey || event.ctrlKey || event.metaKey) return;
    if (event.key?.toLowerCase() !== "r") return;
    if (event.target?.closest?.("input, textarea, select, [contenteditable='true']")) return;
    state.resetting = true;
    state.dragging = false;
    state.spinVelTheta = 0;
    state.spinVelPhi = 0;
    state.zoomVel = 0;
  };

  canvas.addEventListener("pointerdown", onPointerDown);
  canvas.addEventListener("pointermove", onPointerMove);
  canvas.addEventListener("pointerup", onPointerUp);
  canvas.addEventListener("pointercancel", onPointerUp);
  canvas.addEventListener("pointerleave", onPointerLeave);
  canvas.addEventListener("wheel", onWheel, { passive: false });
  window.addEventListener("keydown", onKeyDown);
  window.addEventListener("resize", resize);

  let last = performance.now();
  const animate = (now) => {
    if (state.disposed) return;
    requestAnimationFrame(animate);
    resize();
    const dt = Math.min((now - last) / 1000, 0.05);
    last = now;
    const t = now / 1000;

    if (state.resetting) {
      const lerpSpeed = 0.045;
      if (isFlatAtlas(state)) {
        state.flatPanX += (state.homeFlatPanX - state.flatPanX) * lerpSpeed;
        state.flatPanZ += (state.homeFlatPanZ - state.flatPanZ) * lerpSpeed;
        state.flatZoom += (state.homeFlatZoom - state.flatZoom) * lerpSpeed;
        const delta = Math.abs(state.flatPanX - state.homeFlatPanX)
          + Math.abs(state.flatPanZ - state.homeFlatPanZ)
          + Math.abs(state.flatZoom - state.homeFlatZoom);
        if (delta < 0.005) state.resetting = false;
      } else {
        state.theta += (state.homeTheta - state.theta) * lerpSpeed;
        state.phi += (state.homePhi - state.phi) * lerpSpeed;
        state.radius += (state.homeRadius - state.radius) * lerpSpeed;
        const delta = Math.abs(state.theta - state.homeTheta) + Math.abs(state.phi - state.homePhi) + Math.abs(state.radius - state.homeRadius);
        if (delta < 0.01) state.resetting = false;
      }
    } else {
      if (!isFlatAtlas(state)) {
        state.theta += state.spinVelTheta;
        state.phi += state.spinVelPhi;
      }
    }
    if (!state.dragging && !state.resetting) {
      state.spinVelTheta = state.spinVelTheta * 0.965 + (namespaceAtlas ? 0 : 0.00004);
      state.spinVelPhi *= 0.965;
    }
    if (!state.resetting) {
      state.radius = Math.max(12, Math.min(120, state.radius + state.zoomVel));
      state.zoomVel *= 0.9;
    }

    stars?.update(t);
    streams?.update(t);
    if (shard) {
      shard.group.rotation.y = t * 0.065;
      shard.group.rotation.x = Math.sin(t * 0.045) * 0.08;
      shard.group.rotation.z = Math.cos(t * 0.035) * 0.05;
      shard.group.position.y = Math.sin(t * 0.48) * 0.7;
      shard.glow.material.uniforms.time.value = t;
    }

    for (const spinner of spinners) {
      spinner.group.rotation.y += spinner.spinY * 0.28;
      spinner.group.position.y = (shard?.group.position.y || 0) * spinner.bob;
    }
    for (const frag of fragments) {
      const a = frag.angle + t * frag.speed + frag.phase;
      frag.group.position.set(
        Math.cos(a) * frag.radius,
        frag.height + Math.sin(t * 0.06 + frag.phase) * 0.35,
        Math.sin(a) * frag.radius,
      );
      frag.group.rotation.x += frag.rx * 0.18;
      frag.group.rotation.y += frag.ry * 0.18;
      frag.group.rotation.z += frag.rz * 0.18;
    }
    root.position.y = namespaceAtlas ? 0 : Math.sin(t * 0.08) * 0.2;
    root.rotation.y = namespaceAtlas ? 0 : t * 0.0015;
    for (const remote of state.remotes) {
      remote.marker.rotation.y = -root.rotation.y;
      remote.marker.scale.setScalar(0.85 + Math.sin(t * 1.8 + remote.phase) * 0.08);
    }
    animateAtlasMarkers(state, t);
    updateCamera();
    updateAtlasHover(state, camera, raycaster);
    renderer.render(scene, camera);
  };

  const api = {
    update(nextSceneModel = {}, nextOptions = {}) {
      state.latestSceneModel = nextSceneModel;
      if (state.namespaceAtlas) state.atlasStyle = nextOptions.atlasStyle || state.atlasStyle;
      state.atlasFocus = nextOptions.atlasFocus || null;
      updateSemanticObjects(semanticLayer, state, nextSceneModel);
      applyAtlasFocus(state);
    },
    dispose() {
      state.disposed = true;
      canvas.removeEventListener("pointerdown", onPointerDown);
      canvas.removeEventListener("pointermove", onPointerMove);
      canvas.removeEventListener("pointerup", onPointerUp);
      canvas.removeEventListener("pointercancel", onPointerUp);
      canvas.removeEventListener("pointerleave", onPointerLeave);
      canvas.removeEventListener("wheel", onWheel);
      window.removeEventListener("keydown", onKeyDown);
      window.removeEventListener("resize", resize);
      disposeObject(scene);
      renderer.dispose();
    },
  };
  api.update(sceneModel);
  resize();
  updateCamera();
  requestAnimationFrame(animate);
  return api;
}

function makeShard() {
  const group = new THREE.Group();
  const vertices = [
    [0.0, 2.2, 0.1], [0.4, 1.3, 0.3], [-0.35, 1.5, -0.2], [0.7, 0.4, 0.5],
    [-0.65, 0.35, -0.4], [0.55, 0.2, -0.6], [-0.5, 0.5, 0.55], [0.2, -0.3, 0.7],
    [-0.3, -0.2, -0.6], [0.6, -0.6, 0.1], [-0.55, -0.7, -0.2], [0.15, -1.8, 0.15],
    [-0.25, -1.4, 0.3], [0.45, -1.2, -0.35],
  ].map(([x, y, z]) => new THREE.Vector3(x, y, z));
  const faces = [
    [0, 1, 2, 0], [0, 1, 3, 0], [0, 2, 6, 1], [0, 2, 4, 1], [1, 3, 5, 1], [2, 4, 8, 2],
    [1, 2, 6, 3], [3, 5, 9, 1], [4, 6, 10, 2], [5, 8, 13, 3], [6, 7, 12, 0], [7, 9, 11, 0],
    [3, 6, 7, 2], [4, 8, 10, 3], [5, 9, 13, 1], [8, 10, 12, 2], [9, 11, 13, 0], [10, 11, 12, 1],
    [7, 10, 12, 3], [9, 10, 11, 2], [3, 7, 9, 1], [4, 6, 12, 0], [1, 3, 6, 3], [2, 4, 6, 2],
  ];
  const colors = [new THREE.Color(G.orange), new THREE.Color(G.yellow), new THREE.Color(G.aqua), new THREE.Color(0x1a1714)];
  const positions = [];
  const vertexColors = [];
  for (const [a, b, c, ci] of faces) {
    const color = colors[ci];
    const brightness = ci === 3 ? 0.025 : 0.09;
    for (const v of [vertices[a], vertices[b], vertices[c]]) {
      positions.push(v.x, v.y, v.z);
      vertexColors.push(color.r * brightness, color.g * brightness, color.b * brightness);
    }
  }
  const geo = new THREE.BufferGeometry();
  geo.setAttribute("position", new THREE.Float32BufferAttribute(positions, 3));
  geo.setAttribute("color", new THREE.Float32BufferAttribute(vertexColors, 3));
  group.add(new THREE.Mesh(geo, new THREE.MeshBasicMaterial({ vertexColors: true, transparent: true, opacity: 0.92, side: THREE.DoubleSide })));

  const edges = new Map();
  for (const [a, b, c, ci] of faces) {
    for (const [i, j] of [[a, b], [b, c], [a, c]]) {
      const key = i < j ? `${i}_${j}` : `${j}_${i}`;
      const existing = edges.get(key);
      if (!existing || ci < existing.ci) edges.set(key, { i, j, ci });
    }
  }
  const edgeColors = [G.orange, G.yellow, G.aqua, G.purple];
  for (const { i, j, ci } of edges.values()) {
    group.add(new THREE.Line(new THREE.BufferGeometry().setFromPoints([vertices[i], vertices[j]]), lineMat(ci === 3 ? G.bg2 : edgeColors[ci], ci === 3 ? 0.2 : [1.0, 0.85, 0.7, 0.5][ci])));
  }

  const glow = new THREE.Mesh(
    new THREE.SphereGeometry(2.8, 24, 24),
    new THREE.ShaderMaterial({
      uniforms: { time: { value: 0 } },
      transparent: true,
      depthWrite: false,
      side: THREE.BackSide,
      vertexShader: `varying vec3 vN; varying vec3 vP; void main(){vN=normalize(normalMatrix*normal);vP=position;gl_Position=projectionMatrix*modelViewMatrix*vec4(position,1.);}`,
      fragmentShader: `uniform float time; varying vec3 vN; varying vec3 vP; void main(){float rim=pow(1.-abs(dot(vN,vec3(0,0,1))),1.8);float pulse=.5+.5*sin(time*.6+vP.y*1.2);float y=clamp((vP.y+2.5)/5.,0.,1.);vec3 c=mix(vec3(.996,.502,.098),vec3(.98,.741,.184),y);gl_FragColor=vec4(c,rim*pulse*.35);}`,
    }),
  );
  glow.scale.set(1, 1.2, 1);
  group.add(glow);
  return { group, glow };
}

function addRingBands(root, spinners) {
  const defs = [
    [0.4, 0.3, "circ", 4.5, 90, G.orange, 0.9, 0.024, 1.4], [1.1, 0.5, "circ", 5.2, 90, G.orange, 0.75, -0.019, 0.3],
    [0.785, 0.4, "poly", 5.8, 6, G.yellow, 0.7, 0.015, 0.9], [1.0, -0.8, "circ", 4.8, 90, G.yellow, 0.6, -0.013, 1.8],
    [0.15, 0.0, "circ", 12, 110, G.aqua, 0.65, 0.008, 0.5], [1.45, 0.3, "circ", 14, 110, G.aqua, 0.55, -0.007, 1.6],
    [0.7, -0.6, "poly", 11, 8, G.yellow, 0.5, 0.006, 0.2], [1.1, 1.0, "circ", 13, 110, G.blue, 0.4, -0.005, 1.1],
    [0.2, 0.0, "circ", 22, 140, G.blue, 0.3, 0.003, 0.7], [0.9, 0.5, "poly", 28, 8, G.purple, 0.22, -0.0025, 1.3],
    [1.4, -0.3, "dash", 25, 90, G.purple, 0.18, 0.002, 0.4], [0.4, 1.2, "circ", 35, 160, G.bg2, 0.15, -0.0015, 1.0],
    [1.2, 0.8, "poly", 42, 12, G.bg2, 0.1, 0.001, 0.15],
  ];
  for (const [rx, rz, shape, r, sides, color, opacity, spinY, bob] of defs) {
    const mesh = shape === "dash" ? dashedRing(r, sides, color, opacity) : ring(shape === "poly" ? polyPoints(sides, r) : circlePoints(r, sides), color, opacity);
    const group = new THREE.Group();
    group.rotation.set(rx, 0, rz);
    group.add(mesh);
    root.add(group);
    spinners.push({ group, spinY, bob });
  }
}

function makeFragments(root) {
  const orbits = [
    [5, 0.8, 3.14, 0.25, 0.018], [5.5, -0.4, 2.8, 0.18, 0.02], [4.8, 0.3, 4.2, 0.3, 0.016], [5.2, -0.7, 0.8, 0.2, 0.022],
    [9, 1.5, 2.5, 0.4, 0.012], [10.5, -1.2, 3.8, 0.35, 0.01], [8.5, 0.5, 3.0, 0.5, 0.011], [11, -2, 0.5, 0.3, 0.013],
    [18, 2.5, 0.7, 0.6, 0.005], [22, -1.8, 2.0, 0.55, 0.004], [16, 0.8, 3.5, 0.7, 0.006], [20, -3, 1.2, 0.45, 0.003],
  ];
  return orbits.map(([radius, height, phase, scale, speed], index) => {
    const group = miniShard(scale, radius > 14 ? 0.48 : 0.78);
    root.add(group);
    return { group, radius, height, phase, angle: index * 0.9, speed, rx: (Math.random() - 0.5) * 0.012, ry: (Math.random() - 0.5) * 0.016, rz: (Math.random() - 0.5) * 0.01 };
  });
}

function miniShard(scale, opacity) {
  const verts = [[0, 0.7, 0.1], [0.5, 0.1, 0.3], [-0.4, 0.15, -0.2], [0.25, -0.4, 0.35], [-0.3, -0.5, 0.1], [0.4, 0.2, -0.4]].map(([x, y, z]) => new THREE.Vector3(x, y, z));
  const faces = [[0, 1, 2, 0], [0, 2, 5, 1], [1, 3, 4, 2], [2, 3, 4, 3], [0, 1, 5, 1], [3, 4, 5, 0]];
  const group = new THREE.Group();
  const edgeColors = [G.orange, G.yellow, G.aqua, G.purple];
  for (const [a, b, c, ci] of faces) {
    group.add(new THREE.Line(new THREE.BufferGeometry().setFromPoints([verts[a], verts[b], verts[c], verts[a]]), lineMat(edgeColors[ci], opacity)));
  }
  group.scale.setScalar(scale);
  return group;
}

function makeStreams(root) {
  const count = 480;
  const positions = new Float32Array(count * 3);
  const sizes = new Float32Array(count);
  const colors = new Float32Array(count * 3);
  const phases = new Float32Array(count);
  const defs = [new THREE.Color(G.orange), new THREE.Color(G.aqua), new THREE.Color(G.yellow), new THREE.Color(G.purple)];
  for (let i = 0; i < count; i++) {
    phases[i] = Math.random();
    sizes[i] = 0.8 + Math.random() * 1.5;
    const c = defs[i % defs.length];
    const fade = 0.3 + Math.random() * 0.7;
    colors[i * 3] = c.r * fade; colors[i * 3 + 1] = c.g * fade; colors[i * 3 + 2] = c.b * fade;
  }
  const geo = new THREE.BufferGeometry();
  geo.setAttribute("position", new THREE.Float32BufferAttribute(positions, 3));
  geo.setAttribute("size", new THREE.Float32BufferAttribute(sizes, 1));
  geo.setAttribute("color", new THREE.Float32BufferAttribute(colors, 3));
  const mat = pointMaterial(0.6, 150);
  const points = new THREE.Points(geo, mat);
  root.add(points);
  return {
    update(t) {
      for (let i = 0; i < count; i++) {
        const stream = i % 4;
        const phase = (phases[i] + t * 0.08) % 1;
        const spin = [1, -0.8, 0.6, -1.1][stream];
        const tiltX = [0.3, 1.2, 0.7, -0.4][stream];
        const tiltZ = [0.2, -0.5, 0.9, 1.3][stream];
        const r = 1 + phase * 18;
        const angle = phase * Math.PI * 4 * spin + i * 0.5;
        const h = (phase - 0.5) * 6;
        let x = Math.cos(angle) * r, y = h, z = Math.sin(angle) * r;
        const cx = Math.cos(tiltX), sx = Math.sin(tiltX), cz = Math.cos(tiltZ), sz = Math.sin(tiltZ);
        const y2 = y * cx - z * sx, z2 = y * sx + z * cx;
        positions[i * 3] = x * cz - y2 * sz;
        positions[i * 3 + 1] = x * sz + y2 * cz;
        positions[i * 3 + 2] = z2;
        const life = phase < 0.1 ? phase / 0.1 : phase > 0.85 ? (1 - phase) / 0.15 : 1;
        sizes[i] = (0.5 + phase * 1.5) * life;
      }
      geo.attributes.position.needsUpdate = true;
      geo.attributes.size.needsUpdate = true;
    },
  };
}

function makeStars(scene) {
  const count = 2200;
  const positions = new Float32Array(count * 3);
  const sizes = new Float32Array(count);
  const colors = new Float32Array(count * 3);
  const baseSizes = new Float32Array(count);
  const freqs = new Float32Array(count);
  const phases = new Float32Array(count);
  for (let i = 0; i < count; i++) {
    const theta = Math.random() * Math.PI * 2;
    const phi = Math.acos(2 * Math.random() - 1);
    const r = 60 + Math.random() * 320;
    positions[i * 3] = r * Math.sin(phi) * Math.cos(theta);
    positions[i * 3 + 1] = r * Math.sin(phi) * Math.sin(theta);
    positions[i * 3 + 2] = r * Math.cos(phi);
    baseSizes[i] = sizes[i] = 0.4 + Math.random() * 2.2;
    freqs[i] = 0.3 + Math.random() * 2.5;
    phases[i] = Math.random() * Math.PI * 2;
    const tint = Math.random();
    if (tint < 0.72) {
      const b = 0.48 + Math.random() * 0.46;
      colors[i * 3] = b; colors[i * 3 + 1] = b * 0.92; colors[i * 3 + 2] = b * 0.74;
    } else if (tint < 0.84) {
      colors[i * 3] = 0.86; colors[i * 3 + 1] = 0.55; colors[i * 3 + 2] = 0.28;
    } else if (tint < 0.94) {
      colors[i * 3] = 0.70; colors[i * 3 + 1] = 0.52; colors[i * 3 + 2] = 0.34;
    } else {
      colors[i * 3] = 0.42; colors[i * 3 + 1] = 0.52; colors[i * 3 + 2] = 0.56;
    }
  }
  const geo = new THREE.BufferGeometry();
  geo.setAttribute("position", new THREE.Float32BufferAttribute(positions, 3));
  geo.setAttribute("size", new THREE.Float32BufferAttribute(sizes, 1));
  geo.setAttribute("color", new THREE.Float32BufferAttribute(colors, 3));
  const points = new THREE.Points(geo, pointMaterial(0.75, 200));
  scene.add(points);
  return {
    update(t) {
      for (let i = 0; i < count; i++) sizes[i] = baseSizes[i] * (0.55 + 0.45 * Math.sin(t * freqs[i] + phases[i]));
      geo.attributes.size.needsUpdate = true;
      points.rotation.y = t * 0.0003;
      points.rotation.x = t * 0.0001;
    },
  };
}

function updateSemanticObjects(layer, state, sceneModel) {
  const signature = semanticSignature(sceneModel, state.namespaceAtlas);
  if (signature === state.semanticSignature) return;
  state.semanticSignature = signature;
  disposeLayer(layer);
  state.remotes = [];
  state.atlasInteractive = [];
  state.atlasAnimated = [];
  state.atlasFocusables = [];
  state.atlasHover = null;
  if (state.namespaceAtlas && sceneModel?.atlas) {
    updateAtlasObjects(layer, sceneModel.atlas, state);
    return;
  }
  if (state.namespaceAtlas) return;
  const objects = (sceneModel?.objects || []).filter((object) => object.kind !== "local_node");
  objects.forEach((object, index) => {
    const marker = semanticMarker(object);
    const position = object.position || [0, 0, 0];
    marker.position.set(position[0] || 0, position[1] || 0, position[2] || 0);
    if (object.kind === "remote_node") {
      const angle = (index / Math.max(1, objects.length)) * Math.PI * 2;
      marker.position.set(Math.cos(angle) * 7.5, 0.8 + index * 0.08, Math.sin(angle) * 7.5);
      state.remotes.push({ marker, phase: angle });
    }
    layer.add(marker);
  });
}

function semanticSignature(sceneModel, namespaceAtlas = false) {
  if (namespaceAtlas && sceneModel?.atlas) {
    const nodes = sceneModel.atlas.nodes || [];
    const ui = sceneModel.atlas.ui || {};
    // The frame counter (`generation`) is intentionally excluded: this
    // signature gates a full Three.js scene rebuild and must reflect
    // STRUCTURAL change only (nodes, selection, layers, lens), never the
    // per-tick animation clock — otherwise the atlas rebuilds ~4x/second.
    return `atlas:${sceneModel.atlas.selected_ref || ""}:${(ui.visible_layers || []).join(",")}:${ui.active_lens || "none"}:` + nodes
      .map((node) => [node.id, node.stack?.length || 0, node.state?.selected ? "s" : "", node.state?.highlighted ? "h" : "", node.state?.dimmed ? "d" : ""].join("/"))
      .join(";") + `:${sceneModel.atlas.regions?.length || 0}:${sceneModel.atlas.links?.length || 0}`;
  }
  return (sceneModel?.objects || [])
    .map((object) => [object.id, object.kind, object.color, object.label, object.scale?.[0], object.position?.join(":")].join("|"))
    .join(";");
}

function updateAtlasObjects(layer, atlas, state = {}) {
  const group = new THREE.Group();
  group.userData.sceneObjectKind = "namespace_atlas";
  const nodes = atlas?.nodes || [];
  const nodeByKey = new Map(nodes.map((node) => [node.namespace_key || "", node]));
  const nodeById = new Map(nodes.map((node) => [node.id || "", node]));
  const radiusScale = atlas?.bounds?.radius_max ? Math.min(3.2, 14 / Math.max(1, atlas.bounds.radius_max)) : 1.6;

  for (const node of nodes) {
    if (!node.path?.length) continue;
    const anchor = atlasVec(node.position, radiusScale, 0.04);
    const marker = atlasFolderMarker(node);
    marker.position.copy(anchor);
    registerAtlasMarker(state, marker, node);
    registerAtlasFocusable(state, marker, { type: "folder", folderId: node.id, folderPath: node.namespace_key || "" });
    group.add(marker);
  }

  for (const node of nodes) {
    if (!node.path?.length) continue;
    const parentKey = node.path.slice(0, -1).join("/");
    const parent = nodeByKey.get(parentKey);
    if (!parent) continue;
    const line = new THREE.Line(
      new THREE.BufferGeometry().setFromPoints([
        atlasVec(parent.position, radiusScale, 0),
        atlasVec(node.position, radiusScale, 0),
      ]),
      lineMat(node.stack?.length ? G.yellow : G.bg2, node.stack?.length ? 0.28 : 0.16),
    );
    registerAtlasFocusable(state, line, {
      type: "branch",
      folderId: node.id,
      folderPath: node.namespace_key || "",
      parentPath: parent.namespace_key || "",
    });
    group.add(line);
  }

  const rootNode = nodeByKey.get("");
  const rootAnchor = atlasVec(rootNode?.position || [0, 0, 0], radiusScale, 0.03);
  const rootHub = new THREE.Group();
  rootHub.position.copy(rootAnchor);
  rootHub.add(new THREE.Mesh(
    new THREE.SphereGeometry(0.18, 20, 14),
    new THREE.MeshBasicMaterial({ color: G.orange, transparent: true, opacity: 0.82 }),
  ));
  const rootRing = new THREE.LineLoop(
    new THREE.BufferGeometry().setFromPoints(circlePoints(0.42, 44)),
    lineMat(G.yellow, 0.7),
  );
  rootRing.rotation.x = Math.PI / 2;
  rootHub.add(rootRing);
  rootHub.userData.baseScale = 1;
  rootHub.userData.pulse = 0.18;
  rootHub.userData.phase = 0;
  state.atlasAnimated?.push(rootHub);
  group.add(rootHub);

  for (const region of atlas?.regions || []) {
    const radiusMin = Math.max(0.2, (region.radius_min || 0) * radiusScale);
    const radiusMax = Math.max(radiusMin + 0.2, (region.radius_max || region.radius_min || 1) * radiusScale);
    const start = region.angle_start || 0;
    const end = region.angle_end || start;
    group.add(arcLine(radiusMax, start, end, G.purple, 0.34));
    group.add(new THREE.Line(new THREE.BufferGeometry().setFromPoints([
      new THREE.Vector3(Math.cos(start) * radiusMin, 0.02, Math.sin(start) * radiusMin),
      new THREE.Vector3(Math.cos(start) * radiusMax, 0.02, Math.sin(start) * radiusMax),
    ]), lineMat(G.purple, 0.18)));
    group.add(new THREE.Line(new THREE.BufferGeometry().setFromPoints([
      new THREE.Vector3(Math.cos(end) * radiusMin, 0.02, Math.sin(end) * radiusMin),
      new THREE.Vector3(Math.cos(end) * radiusMax, 0.02, Math.sin(end) * radiusMax),
    ]), lineMat(G.purple, 0.18)));
  }

  for (const link of atlas?.links || []) {
    const from = nodeById.get(link.from);
    const to = nodeById.get(link.to);
    if (!from || !to) continue;
    group.add(new THREE.Line(new THREE.BufferGeometry().setFromPoints([
      atlasVec(from.position, radiusScale, 0.42),
      atlasVec(to.position, radiusScale, 0.42),
    ]), lineMat(G.yellow, 0.62)));
  }

  for (const node of nodes) {
    if (!node.path?.length && !(node.stack || []).length) continue;
    const stack = node.stack || [];
    const anchor = atlasVec(node.position, radiusScale, 0);
    if (!stack.length && !node.state?.selected && !node.state?.highlighted) {
      continue;
    }
    if (!stack.length) {
      const bead = new THREE.Mesh(
        new THREE.SphereGeometry(0.05, 8, 6),
        new THREE.MeshBasicMaterial({ color: G.yellow, transparent: true, opacity: node.state?.selected ? 0.65 : 0.38 }),
      );
      bead.position.copy(anchor);
      group.add(bead);
      continue;
    }
    const haloOpacity = node.state?.selected ? 0.95 : node.state?.highlighted ? 0.7 : node.state?.dimmed ? 0.16 : 0.42;
    const halo = new THREE.LineLoop(
      new THREE.BufferGeometry().setFromPoints(circlePoints(0.18 + stack.length * 0.025, 28)),
      lineMat(node.state?.selected ? G.yellow : G.orange, haloOpacity),
    );
    halo.position.copy(anchor);
    halo.rotation.x = Math.PI / 2;
    group.add(halo);
    for (const item of stack) {
      const marker = atlasStackMarker(item, node.state || {});
      const offset = (item.y_offset || 0) * 0.55;
      marker.position.set(anchor.x, anchor.y + 0.12 + offset, anchor.z);
      registerAtlasMarker(state, marker, node);
      registerAtlasFocusable(state, marker, {
        type: "item",
        ref: atlasItemRef(item),
        kind: item.kind || "other",
        folderId: node.id,
        folderPath: node.namespace_key || "",
        interaction: item.interaction || null,
      });
      group.add(marker);
    }
  }

  layer.add(group);
}

function atlasVec(position = [0, 0, 0], scale = 1, yOffset = 0) {
  return new THREE.Vector3((position[0] || 0) * scale, (position[1] || 0) + yOffset, (position[2] || 0) * scale);
}

function atlasFolderMarker(node) {
  const isRootFolder = (node.path?.length || 0) === 1;
  const hasItems = (node.stack || []).length > 0;
  const branch = (node.angle_end || 0) - (node.angle_start || 0);
  const radius = isRootFolder ? 0.16 : hasItems ? 0.105 : 0.07;
  const color = node.state?.selected ? G.yellow : node.state?.highlighted ? G.orange : isRootFolder ? G.aqua : hasItems ? G.fg : G.bg2;
  const opacity = node.state?.dimmed ? 0.18 : isRootFolder ? 0.76 : hasItems ? 0.5 : 0.34;
  const marker = new THREE.Mesh(
    new THREE.SphereGeometry(radius, isRootFolder ? 18 : 12, isRootFolder ? 12 : 8),
    new THREE.MeshBasicMaterial({ color, transparent: true, opacity }),
  );
  marker.userData.sceneObjectId = node.id;
  marker.userData.sceneObjectKind = isRootFolder ? "atlas_root_folder" : "atlas_folder";
  marker.userData.sceneObjectLabel = node.namespace_key || node.label || "folder";
  marker.userData.atlasInteraction = node.interaction || null;
  marker.userData.baseScale = 1;
  marker.userData.hoverScale = isRootFolder ? 1.65 : 1.9;
  marker.userData.pulse = node.state?.selected || node.state?.highlighted ? 0.18 : isRootFolder ? 0.08 : 0;
  marker.userData.phase = branch * 3.0;
  return marker;
}

function registerAtlasMarker(state, marker, node) {
  if (!state) return;
  state.atlasInteractive?.push(marker);
  if (marker.userData.pulse || node.state?.selected || node.state?.highlighted) {
    state.atlasAnimated?.push(marker);
  }
}

function registerAtlasFocusable(state, object, meta = {}) {
  if (!state || !object) return;
  object.userData.atlasFocus = meta;
  if (meta.interaction) object.userData.atlasInteraction = meta.interaction;
  object.userData.baseOpacity ??= object.material?.opacity;
  object.traverse?.((child) => {
    child.userData.atlasFocus = meta;
    child.userData.baseOpacity ??= child.material?.opacity;
  });
  state.atlasFocusables?.push(object);
}

function isFlatAtlas(state) {
  return state.namespaceAtlas && state.atlasStyle === "flat_2d";
}

function atlasWorldUnitsPerPixel(camera, canvas) {
  const rect = canvas.getBoundingClientRect();
  const height = Math.max(1, rect.height);
  return (camera.top - camera.bottom) / height / Math.max(0.001, camera.zoom || 1);
}

function flatAtlasPointUnderCursor(state, camera, canvas, event) {
  const rect = canvas.getBoundingClientRect();
  const x01 = rect.width ? (event.clientX - rect.left) / rect.width : 0.5;
  const y01 = rect.height ? (event.clientY - rect.top) / rect.height : 0.5;
  const ndcX = x01 * 2 - 1;
  const ndcY = -(y01 * 2 - 1);
  const halfW = (camera.right - camera.left) / 2 / Math.max(0.001, state.flatZoom);
  const halfH = (camera.top - camera.bottom) / 2 / Math.max(0.001, state.flatZoom);
  return {
    x: state.flatPanX + ndcX * halfW,
    z: state.flatPanZ - ndcY * halfH,
  };
}

function updatePointerNdc(state, event) {
  const rect = event.currentTarget.getBoundingClientRect();
  const x = rect.width ? (event.clientX - rect.left) / rect.width : 0.5;
  const y = rect.height ? (event.clientY - rect.top) / rect.height : 0.5;
  state.pointerNdc.set(x * 2 - 1, -(y * 2 - 1));
}

function updateAtlasHover(state, camera, raycaster) {
  if (!state.namespaceAtlas || !raycaster || !state.atlasInteractive?.length) return;
  raycaster.setFromCamera(state.pointerNdc, camera);
  const hit = raycaster.intersectObjects(state.atlasInteractive, true)[0]?.object || null;
  setAtlasHover(state, hit);
}

function clamp(value, min, max) {
  return Math.min(max, Math.max(min, value));
}

function setAtlasHover(state, next) {
  if (state.atlasHover === next) return;
  if (state.atlasHover) {
    const base = state.atlasHover.userData.baseScale || 1;
    state.atlasHover.scale.setScalar(base);
    if (state.atlasHover.material?.opacity !== undefined && state.atlasHover.userData.baseOpacity !== undefined) {
      state.atlasHover.material.opacity = state.atlasHover.userData.baseOpacity;
    }
  }
  state.atlasHover = next;
  dispatchAtlasHover(state, next);
  if (next) {
    next.userData.baseOpacity ??= next.material?.opacity;
    next.scale.setScalar(next.userData.hoverScale || 1.5);
    if (next.material?.opacity !== undefined) next.material.opacity = Math.min(1, next.material.opacity + 0.22);
  }
  applyAtlasFocus(state);
}

function dispatchAtlasHover(state, object) {
  state.canvas?.dispatchEvent(new CustomEvent("ryeos-atlas-hover", {
    detail: object ? {
      id: object.userData.sceneObjectId,
      kind: object.userData.sceneObjectKind,
      label: object.userData.sceneObjectLabel,
      path: object.userData.atlasFocus?.folderPath || "",
      interaction: object.userData.atlasInteraction || object.userData.atlasFocus?.interaction || null,
    } : null,
  }));
}

function dispatchAtlasSelect(state, object) {
  state.canvas?.dispatchEvent(new CustomEvent("ryeos-atlas-select", {
    detail: object ? {
      id: object.userData.sceneObjectId,
      kind: object.userData.sceneObjectKind,
      label: object.userData.sceneObjectLabel,
      path: object.userData.atlasFocus?.folderPath || "",
      interaction: object.userData.atlasInteraction || object.userData.atlasFocus?.interaction || null,
      sceneModel: state.latestSceneModel,
    } : null,
  }));
}

function dispatchAtlasNavigate(state) {
  state.canvas?.dispatchEvent(new CustomEvent("ryeos-atlas-navigate"));
}

function applyAtlasFocus(state) {
  if (!state.namespaceAtlas || !state.atlasFocusables?.length) return;
  const focus = state.atlasFocus;
  for (const object of state.atlasFocusables) {
    const active = !focus || atlasFocusRelated(focus, object.userData.atlasFocus || {});
    object.traverse?.((child) => applyAtlasFocusMaterial(child, active, Boolean(focus)));
    applyAtlasFocusMaterial(object, active, Boolean(focus));
  }
}

function applyAtlasFocusMaterial(object, active, hasFocus) {
  const material = object.material;
  if (!material || material.opacity === undefined) return;
  object.userData.baseOpacity ??= material.opacity;
  if (!hasFocus) {
    material.opacity = object.userData.baseOpacity;
  } else {
    material.opacity = active ? Math.min(1, object.userData.baseOpacity + 0.18) : Math.max(0.08, object.userData.baseOpacity * 0.25);
  }
}

function atlasFocusRelated(focus, meta) {
  if (!focus) return true;
  if (focus.type === "kind") return meta.kind === focus.kind;
  if (focus.type === "item") {
    const ref = focus.ref || focus.id || "";
    return Boolean(ref) && meta.ref === ref;
  }
  if (focus.type === "folder") {
    const focusPath = focus.path || "";
    return meta.folderId === focus.id
      || meta.folderPath === focusPath
      || (focusPath && meta.folderPath?.startsWith(`${focusPath}/`))
      || (focusPath && meta.parentPath?.startsWith(focusPath));
  }
  return true;
}

function animateAtlasMarkers(state, t) {
  for (const marker of state.atlasAnimated || []) {
    if (marker === state.atlasHover) continue;
    const pulse = marker.userData.pulse || 0;
    if (!pulse) continue;
    const base = marker.userData.baseScale || 1;
    const phase = marker.userData.phase || 0;
    marker.scale.setScalar(base + Math.sin(t * 2.4 + phase) * pulse);
  }
}

function atlasStackMarker(item, state) {
  const kind = item.kind || "other";
  const color = atlasKindColor(kind);
  const scope = item.scope || "unknown";
  const scopeOpacity = scope === "system" ? 0.44 : scope === "user" ? 0.54 : 0.62;
  const opacity = state.selected ? 0.98 : state.highlighted ? 0.86 : state.dimmed ? 0.18 : scopeOpacity;
  let geometry;
  switch (kind) {
    case "directive":
      geometry = new THREE.OctahedronGeometry(0.16, 0);
      break;
    case "tool":
      geometry = new THREE.TorusGeometry(0.15, 0.018, 8, 28);
      break;
    case "knowledge":
      geometry = new THREE.DodecahedronGeometry(0.14, 0);
      break;
    case "config":
      geometry = new THREE.BoxGeometry(0.18, 0.035, 0.18);
      break;
    case "file":
      geometry = new THREE.BoxGeometry(0.13, 0.02, 0.18);
      break;
    default:
      geometry = new THREE.SphereGeometry(0.12, 10, 8);
      break;
  }
  const material = new THREE.MeshBasicMaterial({ color, transparent: true, opacity, wireframe: kind !== "config" && kind !== "file" });
  const marker = new THREE.Mesh(geometry, material);
  marker.userData.sceneObjectId = atlasItemRef(item);
  marker.userData.sceneObjectKind = kind;
  marker.userData.sceneObjectLabel = item.label;
  return marker;
}

function atlasItemRef(item = {}) {
  return item.canonical_ref || item.id || item.label || "";
}

function arcLine(radius, start, end, color, opacity) {
  const points = [];
  const segments = Math.max(8, Math.ceil(Math.abs(end - start) * 24));
  for (let i = 0; i <= segments; i++) {
    const angle = start + (end - start) * (i / segments);
    points.push(new THREE.Vector3(Math.cos(angle) * radius, 0.02, Math.sin(angle) * radius));
  }
  return new THREE.Line(new THREE.BufferGeometry().setFromPoints(points), lineMat(color, opacity));
}

function atlasKindColor(kind) {
  switch (kind) {
    case "directive": return G.purple;
    case "tool": return G.aqua;
    case "knowledge": return G.yellow;
    case "config": return G.blue;
    case "file": return G.fg;
    default: return G.fg;
  }
}

function semanticMarker(object) {
  const color = colorValue(object.color || "#fabd2f");
  const opacity = object.opacity ?? 0.82;
  const size = Math.max(0.35, object.scale?.[0] || 0.8);
  let geometry;
  let material = new THREE.MeshBasicMaterial({ color, wireframe: true, transparent: true, opacity });
  switch (object.kind) {
    case "project_core":
      geometry = new THREE.IcosahedronGeometry(0.5 * size, 1);
      material = new THREE.MeshBasicMaterial({ color, transparent: true, opacity: 0.18 + opacity * 0.22 });
      break;
    case "space_ring":
      geometry = new THREE.TorusGeometry(3.0 * size, 0.025, 8, 96);
      break;
    case "item_cluster":
      geometry = new THREE.DodecahedronGeometry(0.48 * size, 0);
      break;
    case "thread_flow":
      geometry = new THREE.TorusKnotGeometry(0.38 * size, 0.018, 80, 8);
      break;
    case "schedule_pulse":
      geometry = new THREE.OctahedronGeometry(0.42 * size, 0);
      break;
    case "service_beacon":
      geometry = new THREE.ConeGeometry(0.36 * size, 0.82 * size, 4);
      break;
    case "remote_node":
      geometry = new THREE.OctahedronGeometry(0.38 * size, 0);
      break;
    case "link":
      geometry = new THREE.BoxGeometry(0.92 * size, 0.025, 0.025);
      material = new THREE.MeshBasicMaterial({ color, transparent: true, opacity: Math.min(opacity, 0.44) });
      break;
    default:
      geometry = new THREE.SphereGeometry(0.32 * size, 12, 8);
      break;
  }
  const marker = new THREE.Mesh(geometry, material);
  marker.userData.sceneObjectId = object.id;
  marker.userData.sceneObjectKind = object.kind;
  marker.userData.sceneObjectLabel = object.label;
  return marker;
}

function disposeLayer(layer) {
  for (const child of layer.children) {
    disposeObject(child);
  }
  layer.clear();
}

function disposeObject(object) {
  object.traverse?.((child) => {
    child.geometry?.dispose?.();
    if (Array.isArray(child.material)) {
      child.material.forEach((material) => material.dispose?.());
    } else {
      child.material?.dispose?.();
    }
  });
}

function circlePoints(r, segs) {
  return Array.from({ length: segs + 1 }, (_, i) => {
    const a = (i / segs) * Math.PI * 2;
    return new THREE.Vector3(Math.cos(a) * r, 0, Math.sin(a) * r);
  });
}

function polyPoints(sides, r) {
  return circlePoints(r, sides);
}

function ring(points, color, opacity) {
  return new THREE.LineLoop(new THREE.BufferGeometry().setFromPoints(points), lineMat(color, opacity));
}

function dashedRing(r, segs, color, opacity) {
  const positions = [];
  for (let i = 0; i < segs; i++) {
    if (i % 2 === 0) continue;
    const a1 = (i / segs) * Math.PI * 2;
    const a2 = ((i + 1) / segs) * Math.PI * 2;
    for (let s = 0; s <= 4; s++) {
      const a = a1 + (a2 - a1) * (s / 4);
      positions.push(Math.cos(a) * r, 0, Math.sin(a) * r);
    }
  }
  const geo = new THREE.BufferGeometry();
  geo.setAttribute("position", new THREE.Float32BufferAttribute(positions, 3));
  return new THREE.LineSegments(geo, lineMat(color, opacity));
}

function lineMat(color, opacity) {
  return new THREE.LineBasicMaterial({ color, opacity, transparent: true });
}

function pointMaterial(alpha, scale) {
  return new THREE.ShaderMaterial({
    vertexShader: `attribute float size; varying vec3 vColor; void main(){vColor=color;vec4 mvPos=modelViewMatrix*vec4(position,1.0);gl_PointSize=size*(${scale.toFixed(1)}/-mvPos.z);gl_Position=projectionMatrix*mvPos;}`,
    fragmentShader: `varying vec3 vColor; void main(){float d=length(gl_PointCoord-.5)*2.;float a=(1.-smoothstep(0.,1.,d))*${alpha.toFixed(2)};gl_FragColor=vec4(vColor,a);}`,
    transparent: true,
    depthWrite: false,
    blending: THREE.AdditiveBlending,
    vertexColors: true,
  });
}

function colorValue(value) {
  if (typeof value === "string" && value.startsWith("#")) return parseInt(value.slice(1), 16);
  return G.aqua;
}
