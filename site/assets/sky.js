// The hero: your machines as a small constellation, drawn with WebGL.
//
// Two passes. A full-screen shader paints a slow aurora with film grain. Then
// points and lines in 3D: "here" (the computer in front of you), the hub, the
// machines over there, the arcs between them, packets running along the arcs,
// and dust for depth. The camera orbits a little, leans toward the pointer and
// pulls back as the page scrolls. HTML labels follow the projected points so
// they stay sharp. With reduced motion it draws one frame and stops; without
// WebGL the page keeps its CSS background.

(function () {
  var canvas = document.getElementById("sky");
  if (!canvas) return;
  var gl = canvas.getContext("webgl", { antialias: true, alpha: false, preserveDrawingBuffer: false });
  if (!gl) {
    document.documentElement.classList.add("no-webgl");
    return;
  }
  document.documentElement.classList.add("webgl");
  var reduce = window.matchMedia && matchMedia("(prefers-reduced-motion: reduce)").matches;
  var hero = canvas.closest(".hero");
  var labelLayer = document.getElementById("sky-labels");

  /* ---------- shaders ---------- */
  function compile(type, source) {
    var shader = gl.createShader(type);
    gl.shaderSource(shader, source);
    gl.compileShader(shader);
    if (!gl.getShaderParameter(shader, gl.COMPILE_STATUS)) throw new Error(gl.getShaderInfoLog(shader));
    return shader;
  }
  function program(vertex, fragment) {
    var built = gl.createProgram();
    gl.attachShader(built, compile(gl.VERTEX_SHADER, vertex));
    gl.attachShader(built, compile(gl.FRAGMENT_SHADER, fragment));
    gl.linkProgram(built);
    if (!gl.getProgramParameter(built, gl.LINK_STATUS)) throw new Error(gl.getProgramInfoLog(built));
    return built;
  }

  var sky = program(
    "attribute vec2 p; varying vec2 uv; void main(){ uv = p * 0.5 + 0.5; gl_Position = vec4(p, 0.0, 1.0); }",
    [
      "precision mediump float;",
      "varying vec2 uv; uniform float t; uniform vec2 res; uniform vec2 lean; uniform float drop;",
      "float h(vec2 p){ return fract(sin(dot(p, vec2(127.1, 311.7))) * 43758.5453); }",
      "float n(vec2 p){ vec2 i = floor(p), f = fract(p); f = f * f * (3.0 - 2.0 * f);",
      "  return mix(mix(h(i), h(i + vec2(1.0, 0.0)), f.x), mix(h(i + vec2(0.0, 1.0)), h(i + vec2(1.0, 1.0)), f.x), f.y); }",
      "float fbm(vec2 p){ float v = 0.0, a = 0.5; for (int i = 0; i < 5; i++){ v += a * n(p); p = p * 2.03 + 3.1; a *= 0.5; } return v; }",
      "void main(){",
      "  vec2 p = (uv - 0.5) * vec2(res.x / res.y, 1.0);",
      "  vec2 q = p * 1.35 + vec2(t * 0.018, drop * 0.9) + lean * 0.06;",
      "  float w = fbm(q + 1.7 * fbm(q * 1.3 + vec2(t * 0.025, -t * 0.013)));",
      "  vec3 night = vec3(0.031, 0.043, 0.082);",
      "  vec3 deep = vec3(0.075, 0.105, 0.235);",
      "  vec3 c = mix(night, deep, smoothstep(0.35, 0.95, w));",
      // Amber haze low and to the left, where "here" sits.
      "  float haze = smoothstep(1.1, 0.0, length(p - vec2(-0.55, -0.42)));",
      "  c += vec3(0.95, 0.63, 0.2) * 0.13 * haze * smoothstep(0.45, 0.9, w);",
      "  c *= clamp(1.15 - dot(p, p) * 0.55, 0.0, 1.0);",
      "  c += (h(uv * res + fract(t) * 97.0) - 0.5) * 0.03;",
      "  gl_FragColor = vec4(c, 1.0);",
      "}"
    ].join("\n")
  );

  var marks = program(
    [
      "attribute vec3 pos; attribute vec4 col; attribute float size;",
      "uniform mat4 mvp; uniform float scale; varying vec4 vc;",
      "void main(){ gl_Position = mvp * vec4(pos, 1.0); gl_PointSize = size * scale / gl_Position.w; vc = col; }"
    ].join("\n"),
    [
      "precision mediump float; varying vec4 vc; uniform float points;",
      "void main(){",
      "  float a = vc.a;",
      "  if (points > 0.5) { float r = length(gl_PointCoord - 0.5); a *= pow(smoothstep(0.5, 0.0, r), 1.8); }",
      "  gl_FragColor = vec4(vc.rgb * a, a);",
      "}"
    ].join("\n")
  );

  /* ---------- the scene ---------- */
  var here = [-2.5, -0.75, 1.3];
  var hub = [0.15, 1.45, -0.6];
  var machines = [
    { name: "build-box", at: [2.35, 1.0, -1.5] },
    { name: "gpu-a", at: [2.85, -0.15, -0.15] },
    { name: "mac-mini", at: [2.05, -1.3, 1.0] },
    { name: "ubuntu-desk", at: [0.9, -1.75, -1.9] },
    { name: "lab-box", at: [0.75, 0.35, -2.9] }
  ];
  var amber = [0.95, 0.71, 0.27];
  var blue = [0.55, 0.67, 1.0];
  var cream = [0.94, 0.92, 0.87];

  function arc(from, to, lift, steps) {
    var mid = [(from[0] + to[0]) / 2, (from[1] + to[1]) / 2 + lift, (from[2] + to[2]) / 2];
    var points = [];
    for (var i = 0; i <= steps; i++) {
      var s = i / steps, a = (1 - s) * (1 - s), b = 2 * (1 - s) * s, c = s * s;
      points.push([a * from[0] + b * mid[0] + c * to[0], a * from[1] + b * mid[1] + c * to[1], a * from[2] + b * mid[2] + c * to[2]]);
    }
    return points;
  }
  var arcs = [];
  machines.forEach(function (machine, index) {
    arcs.push({ points: arc(here, machine.at, 0.9 + index * 0.12, 48), color: amber, speed: 0.11 + index * 0.017 });
    arcs.push({ points: arc(hub, machine.at, 0.35, 32), color: blue, speed: 0.15 + index * 0.02 });
  });

  // Static lines: every arc as segments.
  var lineData = [];
  arcs.forEach(function (path) {
    for (var i = 0; i < path.points.length - 1; i++) {
      [path.points[i], path.points[i + 1]].forEach(function (point) {
        lineData.push(point[0], point[1], point[2], path.color[0], path.color[1], path.color[2], 0.3, 1);
      });
    }
  });

  // Static points: dust, then the nodes.
  var pointData = [];
  var seed = 11;
  function random() { seed = (seed * 16807) % 2147483647; return seed / 2147483647; }
  for (var i = 0; i < 900; i++) {
    var radius = 2.5 + random() * 5, theta = random() * Math.PI * 2, phi = Math.acos(2 * random() - 1);
    pointData.push(
      radius * Math.sin(phi) * Math.cos(theta), radius * Math.cos(phi) * 0.6, radius * Math.sin(phi) * Math.sin(theta),
      cream[0], cream[1], cream[2], 0.18 + random() * 0.35, 1.2 + random() * 2.2
    );
  }
  function node(at, color, alpha, size) { pointData.push(at[0], at[1], at[2], color[0], color[1], color[2], alpha, size); }
  node(here, amber, 1, 70); node(here, amber, 0.4, 190);
  node(hub, blue, 1, 52); node(hub, blue, 0.35, 140);
  machines.forEach(function (machine) { node(machine.at, cream, 1, 40); node(machine.at, cream, 0.22, 110); });

  var quad = gl.createBuffer();
  gl.bindBuffer(gl.ARRAY_BUFFER, quad);
  gl.bufferData(gl.ARRAY_BUFFER, new Float32Array([-1, -1, 1, -1, -1, 1, 1, 1]), gl.STATIC_DRAW);
  var lines = gl.createBuffer();
  gl.bindBuffer(gl.ARRAY_BUFFER, lines);
  gl.bufferData(gl.ARRAY_BUFFER, new Float32Array(lineData), gl.STATIC_DRAW);
  var points = gl.createBuffer();
  gl.bindBuffer(gl.ARRAY_BUFFER, points);
  gl.bufferData(gl.ARRAY_BUFFER, new Float32Array(pointData), gl.STATIC_DRAW);
  var packets = gl.createBuffer();
  var packetData = new Float32Array(arcs.length * 2 * 8);

  /* ---------- labels ---------- */
  var labels = [];
  function label(text, at, kind) {
    if (!labelLayer) return;
    var element = document.createElement("span");
    element.className = "sky-label " + kind;
    element.textContent = text;
    labelLayer.appendChild(element);
    labels.push({ element: element, at: at });
  }
  label("here", here, "here");
  label("hub", hub, "hub");
  machines.forEach(function (machine) { label(machine.name, machine.at, "machine"); });

  /* ---------- math ---------- */
  function perspective(fov, aspect, near, far) {
    var f = 1 / Math.tan(fov / 2), r = 1 / (near - far);
    return [f / aspect, 0, 0, 0, 0, f, 0, 0, 0, 0, (far + near) * r, -1, 0, 0, 2 * far * near * r, 0];
  }
  function lookAt(eye, target) {
    var z = normalize([eye[0] - target[0], eye[1] - target[1], eye[2] - target[2]]);
    var x = normalize(cross([0, 1, 0], z));
    var y = cross(z, x);
    return [x[0], y[0], z[0], 0, x[1], y[1], z[1], 0, x[2], y[2], z[2], 0,
      -dot(x, eye), -dot(y, eye), -dot(z, eye), 1];
  }
  function multiply(a, b) {
    var out = new Array(16);
    for (var col = 0; col < 4; col++) for (var row = 0; row < 4; row++) {
      out[col * 4 + row] = a[row] * b[col * 4] + a[4 + row] * b[col * 4 + 1] + a[8 + row] * b[col * 4 + 2] + a[12 + row] * b[col * 4 + 3];
    }
    return out;
  }
  function normalize(v) { var l = Math.hypot(v[0], v[1], v[2]) || 1; return [v[0] / l, v[1] / l, v[2] / l]; }
  function cross(a, b) { return [a[1] * b[2] - a[2] * b[1], a[2] * b[0] - a[0] * b[2], a[0] * b[1] - a[1] * b[0]]; }
  function dot(a, b) { return a[0] * b[0] + a[1] * b[1] + a[2] * b[2]; }

  /* ---------- input ---------- */
  var pointer = [0, 0], lean = [0, 0];
  window.addEventListener("pointermove", function (event) {
    pointer = [event.clientX / window.innerWidth * 2 - 1, event.clientY / window.innerHeight * 2 - 1];
  }, { passive: true });

  var width = 0, height = 0, ratio = 1;
  function resize() {
    ratio = Math.min(window.devicePixelRatio || 1, 2);
    width = canvas.clientWidth; height = canvas.clientHeight;
    canvas.width = Math.round(width * ratio); canvas.height = Math.round(height * ratio);
    gl.viewport(0, 0, canvas.width, canvas.height);
  }
  window.addEventListener("resize", resize);
  resize();

  function bind(buffer, location, size, stride, offset) {
    gl.bindBuffer(gl.ARRAY_BUFFER, buffer);
    gl.enableVertexAttribArray(location);
    gl.vertexAttribPointer(location, size, gl.FLOAT, false, stride, offset);
  }
  var sky_p = gl.getAttribLocation(sky, "p");
  var marks_pos = gl.getAttribLocation(marks, "pos");
  var marks_col = gl.getAttribLocation(marks, "col");
  var marks_size = gl.getAttribLocation(marks, "size");

  function drawMarks(buffer, mode, count, isPoints) {
    var stride = 8 * 4;
    bind(buffer, marks_pos, 3, stride, 0);
    bind(buffer, marks_col, 4, stride, 12);
    bind(buffer, marks_size, 1, stride, 28);
    gl.uniform1f(gl.getUniformLocation(marks, "points"), isPoints ? 1 : 0);
    gl.drawArrays(mode, 0, count);
  }

  var start = performance.now();
  var running = false;

  function frame(now) {
    // A frame can be timestamped just before the script started; never go below zero.
    var t = reduce ? 14 : Math.max(0, now - start) / 1000;
    var drop = hero ? Math.min(Math.max(-hero.getBoundingClientRect().top / Math.max(hero.offsetHeight, 1), 0), 1.2) : 0;
    lean[0] += (pointer[0] - lean[0]) * 0.04;
    lean[1] += (pointer[1] - lean[1]) * 0.04;

    gl.disable(gl.BLEND);
    gl.useProgram(sky);
    gl.uniform1f(gl.getUniformLocation(sky, "t"), t);
    gl.uniform2f(gl.getUniformLocation(sky, "res"), canvas.width, canvas.height);
    gl.uniform2f(gl.getUniformLocation(sky, "lean"), lean[0], lean[1]);
    gl.uniform1f(gl.getUniformLocation(sky, "drop"), drop);
    bind(quad, sky_p, 2, 0, 0);
    gl.drawArrays(gl.TRIANGLE_STRIP, 0, 4);
    gl.disableVertexAttribArray(sky_p);

    // The camera: a slow orbit, leaning toward the pointer, backing off with scroll.
    var yaw = Math.sin(t * 0.09) * 0.32 + lean[0] * 0.22;
    var pitch = 0.18 + Math.sin(t * 0.07) * 0.05 - lean[1] * 0.12;
    var narrow = width < 700;
    var distance = (narrow ? 16 : 11.5) + drop * 4.5;
    var target = [0.2, -0.1, -0.3];
    var eye = [
      target[0] + Math.sin(yaw) * Math.cos(pitch) * distance,
      target[1] + Math.sin(pitch) * distance,
      target[2] + Math.cos(yaw) * Math.cos(pitch) * distance
    ];
    var mvp = multiply(perspective(narrow ? 0.95 : 0.72, width / height, 0.1, 60), lookAt(eye, target));
    // Keep the constellation clear of the headline: the right half of a wide
    // screen, the upper half of a narrow one.
    var shiftX = narrow ? -0.02 : 0.5, shiftY = narrow ? 0.5 : 0.06;
    for (var column = 0; column < 4; column++) {
      mvp[column * 4] += shiftX * mvp[column * 4 + 3];
      mvp[column * 4 + 1] += shiftY * mvp[column * 4 + 3];
    }

    // Packets along the arcs.
    arcs.forEach(function (path, index) {
      for (var k = 0; k < 2; k++) {
        var s = (((t * path.speed + k * 0.5 + index * 0.137) % 1) + 1) % 1;
        var at = s * (path.points.length - 1), i0 = Math.floor(at), f = at - i0;
        var a = path.points[i0], b = path.points[Math.min(i0 + 1, path.points.length - 1)];
        var o = (index * 2 + k) * 8;
        packetData[o] = a[0] + (b[0] - a[0]) * f; packetData[o + 1] = a[1] + (b[1] - a[1]) * f; packetData[o + 2] = a[2] + (b[2] - a[2]) * f;
        packetData[o + 3] = path.color[0]; packetData[o + 4] = path.color[1]; packetData[o + 5] = path.color[2];
        packetData[o + 6] = Math.sin(s * Math.PI); packetData[o + 7] = 26;
      }
    });
    gl.bindBuffer(gl.ARRAY_BUFFER, packets);
    gl.bufferData(gl.ARRAY_BUFFER, packetData, gl.DYNAMIC_DRAW);

    gl.enable(gl.BLEND);
    gl.blendFunc(gl.ONE, gl.ONE);
    gl.useProgram(marks);
    gl.uniformMatrix4fv(gl.getUniformLocation(marks, "mvp"), false, new Float32Array(mvp));
    gl.uniform1f(gl.getUniformLocation(marks, "scale"), ratio * Math.min(width, 1400) / 520);
    drawMarks(lines, gl.LINES, lineData.length / 8, false);
    drawMarks(points, gl.POINTS, pointData.length / 8, true);
    drawMarks(packets, gl.POINTS, arcs.length * 2, true);

    // Labels follow their points.
    labels.forEach(function (item) {
      var p = item.at;
      var x = mvp[0] * p[0] + mvp[4] * p[1] + mvp[8] * p[2] + mvp[12];
      var y = mvp[1] * p[0] + mvp[5] * p[1] + mvp[9] * p[2] + mvp[13];
      var w = mvp[3] * p[0] + mvp[7] * p[1] + mvp[11] * p[2] + mvp[15];
      item.element.style.transform = "translate(" + ((x / w * 0.5 + 0.5) * width).toFixed(1) + "px," + ((0.5 - y / w * 0.5) * height).toFixed(1) + "px)";
    });

    if (!reduce && running) requestAnimationFrame(frame);
  }

  function setRunning(next) {
    if (next === running) return;
    running = next;
    if (running) requestAnimationFrame(frame);
  }

  if (reduce) {
    requestAnimationFrame(frame);
    return;
  }
  // Draw only while the hero is on screen or the tab is visible.
  if ("IntersectionObserver" in window && hero) {
    new IntersectionObserver(function (entries) {
      setRunning(entries[0].isIntersecting && !document.hidden);
    }).observe(hero);
    document.addEventListener("visibilitychange", function () {
      var rect = hero.getBoundingClientRect();
      setRunning(!document.hidden && rect.bottom > 0);
    });
  } else {
    setRunning(true);
  }
})();
