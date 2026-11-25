'use strict';

const DISPATCH_PREFIX = 'Dispatch, for you: ';
const HOME_HINT = 'Ask what’s running, pass a message to a thread, or approve a command.';
const EARLIER_LIMIT = 5;
const CALLS_LIMIT = 3;
const REFRESH_MS = 20000;
// A thread that's working is checked this often while it's open.
const THREAD_REFRESH_MS = 5000;
// Idle threads updated this recently are shown as just finished.
const RECENT_SECONDS = 3600;
const DEFAULT_KINDS = { approval: true, done: true, stopped: true };
// Speaker loudness (RMS) above which Dispatch counts as speaking, held through
// the gaps between words so the orb doesn't flicker.
const SPEAKING_RMS = 0.012;
const SPEAKING_HOLD_MS = 600;

const byId = (id) => document.getElementById(id);
const page = {
  body: document.body,
  hub: byId('hub'),
  hubName: byId('hub-name'),
  timer: byId('timer'),
  notice: byId('notice'),
  pairForm: byId('pair-form'),
  code: byId('code'),
  pairHost: byId('pair-host'),
  talk: byId('talk'),
  status: byId('status'),
  caption: byId('caption'),
  play: byId('play'),
  waiting: byId('group-waiting'),
  running: byId('group-running'),
  earlier: byId('group-earlier'),
  calls: byId('group-calls'),
  showCalls: byId('show-calls'),
  showAll: byId('show-all'),
  threadsCount: byId('threads-count'),
  threadsEmpty: byId('threads-empty'),
  log: byId('log'),
  mute: byId('mute'),
  end: byId('end'),
  back: byId('back'),
  threadTitle: byId('thread-title'),
  threadMeta: byId('thread-meta'),
  messages: byId('messages'),
  threadTalk: byId('thread-talk'),
  threadTalkLabel: byId('thread-talk-label'),
  speaker: byId('speaker'),
  alertsOpen: byId('alerts-open'),
  notifyBanner: byId('notify-banner'),
  notifyOn: byId('notify-on'),
  notifyLater: byId('notify-later'),
  done: byId('group-done'),
  approval: byId('approval'),
  approvalTitle: byId('approval-title'),
  approvalCommand: byId('approval-command'),
  approvalWhere: byId('approval-where'),
  approvalError: byId('approval-error'),
  approvalReject: byId('approval-reject'),
  approvalApprove: byId('approval-approve'),
  replyForm: byId('reply-form'),
  replyLabel: byId('reply-label'),
  replyText: byId('reply-text'),
  replySend: byId('reply-send'),
  replyNote: byId('reply-note'),
  alertsBack: byId('alerts-back'),
  alertsTitle: byId('alerts-title'),
  alertsStatus: byId('alerts-status'),
  alertsSwitches: [...document.querySelectorAll('#alerts .switch')],
  alertsOn: byId('alerts-on'),
  alertsTest: byId('alerts-test'),
  alertsOff: byId('alerts-off'),
  alertsNote: byId('alerts-note'),
};
const reducedMotion = window.matchMedia('(prefers-reduced-motion: reduce)');
const state = {
  view: 'loading',
  threads: [],
  catalog: new Map(),
  total: 0,
  showAll: false,
  conversations: [],
  conversationTotal: 0,
  conversationIds: new Map(),
  showAllCalls: false,
  call: null,
  historyRequest: null,
  thread: null,
  threadData: null,
  threadRefresh: null,
  push: { started: false, supported: false, registration: null, subscription: null, key: null },
};
// A thread a notification asked to open, kept until the phone is paired.
let pendingThread = new URLSearchParams(location.search).get('thread');
const idleLevel = (seconds) => 0.15 + 0.05 * Math.sin(seconds * 0.8);
const orb = createOrb(byId('orb-canvas'), page.talk);

function element(tag, className, text) {
  const node = document.createElement(tag);
  if (className) node.className = className;
  if (text !== undefined) node.textContent = text;
  return node;
}

function setView(view) {
  state.view = view;
  page.body.dataset.view = view;
  // During a call the orb only shows the voice; it isn't a control.
  page.talk.tabIndex = view === 'call' ? -1 : 0;
  page.talk.setAttribute('aria-label', view === 'call' ? 'Dispatch' : 'Talk to Dispatch');
  orb.setVisible(view === 'home' || view === 'call');
}

function showNotice(message, error = false) {
  page.notice.textContent = message || '';
  page.notice.hidden = !message;
  page.notice.toggleAttribute('data-error', Boolean(message) && error);
}

function setStatus(text) {
  page.status.textContent = text;
}

function setCaption(text, role) {
  page.caption.textContent = text;
  if (role) page.caption.dataset.role = role;
  else delete page.caption.dataset.role;
}

function agentName(agent = '') {
  const [base, account] = agent.split('@');
  const name = { 'claude-acp': 'Claude', 'codex-acp': 'Codex' }[base] || base;
  return account ? `${name} (${account})` : name;
}

function describeThread(thread) {
  return [thread.machine, agentName(thread.agent)].filter(Boolean).join(' · ');
}

function stateLabel(value) {
  return { waiting: 'Needs approval', running: 'Running', idle: 'Idle', inactive: 'Idle' }[value] || value;
}

function age(updatedAt) {
  const seconds = Math.max(0, Date.now() / 1000 - updatedAt);
  if (seconds < 60) return 'now';
  if (seconds < 3600) return `${Math.floor(seconds / 60)} min`;
  if (seconds < 86400) return `${Math.floor(seconds / 3600)} h`;
  return `${Math.floor(seconds / 86400)} d`;
}

function threadTitle(id) {
  return state.catalog.get(id)?.title || 'the thread';
}

function tail(text, chars) {
  return text.length <= chars ? text : `…${text.slice(-chars).trimStart()}`;
}

// Per-phone choices. Storage can be unavailable; the defaults then apply.
function readSetting(name, fallback) {
  try {
    const value = localStorage.getItem(`dispatch.${name}`);
    return value === null ? fallback : JSON.parse(value);
  } catch {
    return fallback;
  }
}

function writeSetting(name, value) {
  try {
    localStorage.setItem(`dispatch.${name}`, JSON.stringify(value));
  } catch (error) {
    console.debug('Dispatch settings', error);
  }
}

async function post(path, body) {
  const response = await fetch(path, {
    method: 'POST',
    headers: { 'Content-Type': 'application/json' },
    body: JSON.stringify(body),
  });
  if (response.status === 401) {
    showPairing();
    throw Error('Pair this phone with the hub again.');
  }
  if (!response.ok) throw Error((await response.text()) || 'The hub refused that.');
  return response.json();
}

function icon(parts) {
  const svg = document.createElementNS('http://www.w3.org/2000/svg', 'svg');
  svg.setAttribute('viewBox', '0 0 24 24');
  svg.setAttribute('aria-hidden', 'true');
  for (const [tag, attributes] of parts) {
    const part = document.createElementNS('http://www.w3.org/2000/svg', tag);
    for (const [name, value] of Object.entries(attributes)) part.setAttribute(name, value);
    svg.append(part);
  }
  return svg;
}

// The orb: a WebGL sphere of flowing light, recolored and sped up by the
// voice it shows. Without WebGL the CSS gradient behind the canvas stays.
function createOrb(canvas, button) {
  const palettes = {
    idle: { deep: '#0e1838', mid: '#2f4aa8', accent: '#5b78d6', light: '#c9d6ff' },
    listening: { deep: '#13235a', mid: '#4a6fd6', accent: '#8fb0f2', light: '#f2efe6' },
    speaking: { deep: '#5c2c06', mid: '#f2a33c', accent: '#ffc46b', light: '#ffe7b8' },
  };
  const rgb = (hex) => [1, 3, 5].map((index) => parseInt(hex.slice(index, index + 2), 16) / 255);
  const toColors = (palette) => Object.fromEntries(Object.entries(palette).map(([key, hex]) => [key, rgb(hex)]));
  const paletteFor = (mode) => {
    if (mode === 'speaking') return palettes.speaking;
    if (mode === 'listening' || mode === 'muted') return palettes.listening;
    return palettes.idle;
  };
  const coolHalo = button.querySelector('.orb-halo.cool');
  const warmHalo = button.querySelector('.orb-halo.warm');
  const body = button.querySelector('.orb-body');
  const dot = button.querySelector('.orb-dot');
  const colors = toColors(palettes.idle);
  let target = toColors(palettes.idle);
  let mode = 'idle';
  let levelSource = idleLevel;
  let level = 0;
  let warmth = 0;
  let phase = 7;
  let previous = 0;
  let frame = 0;
  let visible = false;
  const renderer = createRenderer(canvas, () => {
    if (reducedMotion.matches && visible) frame = requestAnimationFrame(render);
  });
  // The gradient behind the canvas is only a fallback; at the canvas's soft
  // edge it would show as a ring.
  if (renderer) body.dataset.webgl = '';

  function render(now) {
    const seconds = previous ? Math.min(0.1, (now - previous) / 1000) : 0;
    previous = now;
    const wanted = Math.min(1, Math.max(0, levelSource(now / 1000) || 0));
    // Rise quickly with the voice, settle slowly, so the orb moves smoothly.
    level += (wanted - level) * (wanted > level ? 0.25 : 0.08);
    warmth += ((mode === 'speaking' ? 1 : 0) - warmth) * 0.06;
    for (const key of Object.keys(colors)) {
      for (let channel = 0; channel < 3; channel += 1) {
        colors[key][channel] += (target[key][channel] - colors[key][channel]) * 0.06;
      }
    }
    phase += seconds * (0.1 + 0.25 * level);
    if (renderer) renderer.draw(phase, level, colors);
    if (!reducedMotion.matches) {
      body.style.transform = `scale(${(1 + 0.05 * level).toFixed(4)})`;
      coolHalo.style.opacity = ((1 - warmth) * (0.4 + 0.6 * level)).toFixed(3);
      warmHalo.style.opacity = (warmth * (0.5 + 0.5 * level)).toFixed(3);
      dot.style.transform = mode === 'speaking' ? `scale(${(1 + 0.35 * level).toFixed(3)})` : '';
    }
  }

  function loop(now) {
    render(now);
    frame = requestAnimationFrame(loop);
  }

  function schedule() {
    cancelAnimationFrame(frame);
    previous = 0;
    if (!visible || document.hidden) return;
    if (reducedMotion.matches) {
      for (const key of Object.keys(colors)) colors[key] = [...target[key]];
      warmth = mode === 'speaking' ? 1 : 0;
      level = 0.3;
      body.style.transform = '';
      dot.style.transform = '';
      coolHalo.style.opacity = String((1 - warmth) * 0.6);
      warmHalo.style.opacity = String(warmth * 0.8);
      frame = requestAnimationFrame(render);
    } else {
      frame = requestAnimationFrame(loop);
    }
  }

  document.addEventListener('visibilitychange', schedule);
  reducedMotion.addEventListener('change', schedule);
  return {
    setMode(next, source) {
      mode = next;
      button.dataset.mode = next;
      target = toColors(paletteFor(next));
      levelSource = source;
      if (reducedMotion.matches) schedule();
    },
    setVisible(next) {
      if (next === visible) return;
      visible = next;
      schedule();
    },
  };
}

function createRenderer(canvas, onResize) {
  const gl = canvas.getContext('webgl', { alpha: true, premultipliedAlpha: true, antialias: true });
  if (!gl) return null;
  const fragmentSource = `
    #ifdef GL_FRAGMENT_PRECISION_HIGH
    precision highp float;
    #else
    precision mediump float;
    #endif
    uniform vec2 uRes;
    uniform float uTime;
    uniform float uLevel;
    uniform vec3 uDeep;
    uniform vec3 uMid;
    uniform vec3 uAccent;
    uniform vec3 uLight;
    float hash(vec2 p) { p = fract(p * vec2(123.34, 456.21)); p += dot(p, p + 45.32); return fract(p.x * p.y); }
    float noise(vec2 p) {
      vec2 i = floor(p); vec2 f = fract(p); vec2 u = f * f * (3.0 - 2.0 * f);
      return mix(mix(hash(i), hash(i + vec2(1.0, 0.0)), u.x), mix(hash(i + vec2(0.0, 1.0)), hash(i + vec2(1.0, 1.0)), u.x), u.y);
    }
    float fbm(vec2 p) {
      float value = 0.0; float amplitude = 0.5; mat2 turn = mat2(1.6, 1.2, -1.2, 1.6);
      for (int i = 0; i < 4; i++) { value += amplitude * noise(p); p = turn * p; amplitude *= 0.5; }
      return value;
    }
    void main() {
      vec2 uv = (gl_FragCoord.xy * 2.0 - uRes) / uRes.y;
      float r = length(uv);
      float edge = 1.0 - smoothstep(0.975, 1.0, r);
      if (edge <= 0.0) { gl_FragColor = vec4(0.0); return; }
      float z = sqrt(max(0.0, 1.0 - r * r));
      vec2 p = uv * (0.95 / (0.45 + z));
      float t = uTime;
      vec2 q = vec2(fbm(p + vec2(0.0, t)), fbm(p + vec2(5.2, 1.3) - vec2(t * 0.7, 0.0)));
      vec2 w = vec2(fbm(p + 2.2 * q + vec2(1.7, 9.2) + vec2(t * 0.9, 0.0)), fbm(p + 2.2 * q + vec2(8.3, 2.8) - vec2(0.0, t * 0.8)));
      float f = fbm(p + 2.2 * w);
      vec3 color = mix(uDeep, uMid, smoothstep(0.15, 0.65, f));
      color = mix(color, uAccent, smoothstep(0.35, 0.85, q.x) * 0.6);
      color = mix(color, uLight, smoothstep(0.45, 0.9, f * 0.7 + length(w) * 0.45) * (0.65 + 0.35 * uLevel));
      color *= mix(0.72, 1.06, z) * (0.92 + 0.16 * uLevel);
      color += uMid * 0.12 * pow(1.0 - z, 2.5);
      float highlight = pow(max(0.0, 1.0 - length(uv - vec2(-0.38, 0.42)) * 1.7), 3.0);
      color += vec3(highlight * 0.22);
      gl_FragColor = vec4(color * edge, edge);
    }
  `;
  const compile = (type, source) => {
    const shader = gl.createShader(type);
    gl.shaderSource(shader, source);
    gl.compileShader(shader);
    return gl.getShaderParameter(shader, gl.COMPILE_STATUS) ? shader : null;
  };
  const vertex = compile(gl.VERTEX_SHADER, 'attribute vec2 aPos; void main() { gl_Position = vec4(aPos, 0.0, 1.0); }');
  const fragment = compile(gl.FRAGMENT_SHADER, fragmentSource);
  if (!vertex || !fragment) return null;
  const program = gl.createProgram();
  gl.attachShader(program, vertex);
  gl.attachShader(program, fragment);
  gl.linkProgram(program);
  if (!gl.getProgramParameter(program, gl.LINK_STATUS)) return null;
  gl.useProgram(program);
  gl.bindBuffer(gl.ARRAY_BUFFER, gl.createBuffer());
  gl.bufferData(gl.ARRAY_BUFFER, new Float32Array([-1, -1, 1, -1, -1, 1, -1, 1, 1, -1, 1, 1]), gl.STATIC_DRAW);
  const position = gl.getAttribLocation(program, 'aPos');
  gl.enableVertexAttribArray(position);
  gl.vertexAttribPointer(position, 2, gl.FLOAT, false, 0, 0);
  const uniform = (name) => gl.getUniformLocation(program, name);
  const uniforms = {
    resolution: uniform('uRes'),
    time: uniform('uTime'),
    level: uniform('uLevel'),
    deep: uniform('uDeep'),
    mid: uniform('uMid'),
    accent: uniform('uAccent'),
    light: uniform('uLight'),
  };
  let lost = false;
  canvas.addEventListener('webglcontextlost', (event) => {
    event.preventDefault();
    lost = true;
    canvas.hidden = true;
  });
  const resize = () => {
    const scale = Math.min(window.devicePixelRatio || 1, 2);
    const size = Math.max(1, Math.round(canvas.clientWidth * scale));
    if (canvas.width === size) return;
    canvas.width = size;
    canvas.height = size;
    gl.viewport(0, 0, size, size);
    gl.uniform2f(uniforms.resolution, size, size);
    onResize();
  };
  resize();
  if (window.ResizeObserver) new ResizeObserver(resize).observe(canvas);
  return {
    draw(time, level, colors) {
      if (lost) return;
      gl.uniform1f(uniforms.time, time);
      gl.uniform1f(uniforms.level, level);
      gl.uniform3fv(uniforms.deep, colors.deep);
      gl.uniform3fv(uniforms.mid, colors.mid);
      gl.uniform3fv(uniforms.accent, colors.accent);
      gl.uniform3fv(uniforms.light, colors.light);
      gl.clearColor(0, 0, 0, 0);
      gl.clear(gl.COLOR_BUFFER_BIT);
      gl.drawArrays(gl.TRIANGLES, 0, 6);
    },
  };
}

function createMeter(context, stream) {
  try {
    const analyser = context.createAnalyser();
    analyser.fftSize = 512;
    context.createMediaStreamSource(stream).connect(analyser);
    const samples = new Float32Array(analyser.fftSize);
    return () => {
      analyser.getFloatTimeDomainData(samples);
      let sum = 0;
      for (let index = 0; index < samples.length; index += 1) sum += samples[index] * samples[index];
      return Math.sqrt(sum / samples.length);
    };
  } catch (error) {
    console.debug('Dispatch audio meter unavailable', error);
    return () => 0;
  }
}

// Speech RMS sits low on a linear scale; a square root spreads it out.
const levelFromRms = (rms) => Math.min(1, Math.sqrt(rms) * 1.6);

// Threads

async function loadSessions() {
  try {
    const response = await fetch('/dispatch/sessions', { cache: 'no-store' });
    if (response.status === 401) {
      showPairing();
      return;
    }
    if (!response.ok) throw Error('The hub didn’t send its threads. Try again in a moment.');
    const catalog = await response.json();
    state.threads = catalog.sessions;
    state.total = catalog.total;
    state.catalog = new Map(catalog.sessions.map((thread) => [thread.id, thread]));
    state.conversations = catalog.conversations || [];
    state.conversationTotal = catalog.conversationTotal || state.conversations.length;
    state.conversationIds = new Map(state.conversations.map((conversation) => [conversation.id, conversation]));
    page.hub.dataset.state = 'ok';
    renderThreads();
    updateBadge(state.threads.filter((thread) => thread.state === 'waiting').length);
    if (state.view === 'loading' || state.view === 'pair') {
      setView('home');
      showNotice('');
      setupPush();
      openPendingThread();
    }
  } catch (error) {
    page.hub.dataset.state = 'error';
    if (state.view === 'loading') setView('home');
    const unreachable = error instanceof TypeError || !error.message;
    showNotice(unreachable ? 'The hub can’t be reached. Check that Tailscale is on.' : error.message, true);
  }
}

function renderThreads() {
  const waiting = [];
  const running = [];
  const done = [];
  const earlier = [];
  const now = Date.now() / 1000;
  for (const thread of state.threads) {
    if (thread.state === 'waiting') waiting.push(thread);
    else if (thread.state === 'running') running.push(thread);
    else if (now - thread.updatedAt < RECENT_SECONDS) done.push(thread);
    else earlier.push(thread);
  }
  fillGroup(page.waiting, waiting);
  fillGroup(page.running, running);
  fillGroup(page.done, done);
  const shown = state.showAll ? earlier : earlier.slice(0, EARLIER_LIMIT);
  fillGroup(page.earlier, shown);
  const more = shown.length < earlier.length || state.total > state.threads.length;
  page.showAll.hidden = state.showAll || !more;
  page.showAll.textContent = `Show all ${state.total} threads`;
  page.threadsCount.hidden = !(state.showAll && state.total > state.threads.length);
  page.threadsCount.textContent = `Showing the ${state.threads.length} most recent of ${state.total} threads`;
  page.threadsEmpty.hidden = state.threads.length > 0 || state.conversations.length > 0;
  const calls = state.showAllCalls ? state.conversations : state.conversations.slice(0, CALLS_LIMIT);
  page.calls.hidden = calls.length === 0;
  page.calls.querySelector('ul').replaceChildren(...calls.map(createCallRow));
  page.showCalls.hidden = state.showAllCalls || state.conversations.length <= CALLS_LIMIT;
  page.showCalls.textContent = `Show all ${state.conversationTotal} conversations`;
}

function duration(seconds) {
  const minutes = Math.round(seconds / 60);
  return minutes < 1 ? 'under a minute' : `${minutes} min`;
}

function startedAt(seconds) {
  const date = new Date(seconds * 1000);
  const time = date.toLocaleTimeString([], { hour: 'numeric', minute: '2-digit' });
  if (date.toDateString() === new Date().toDateString()) return `Today ${time}`;
  return `${date.toLocaleDateString([], { weekday: 'short', month: 'short', day: 'numeric' })} ${time}`;
}

function createCallRow(conversation) {
  const item = element('li');
  const button = element('button', 'row call-row');
  button.type = 'button';
  const voice = conversation.kind === 'voice';
  const text = element('span', 'row-text');
  const kind = voice ? `Call · ${duration(conversation.updatedAt - conversation.createdAt)}` : 'Typed question';
  text.append(
    element('span', 'row-title', conversation.title || (voice ? 'Voice call' : 'Typed question')),
    element('span', 'row-meta', [kind, conversation.summary].filter(Boolean).join(' · ')),
  );
  const symbol = voice
    ? icon([['rect', { x: 9, y: 3, width: 6, height: 11, rx: 3 }], ['path', { d: 'M5 11a7 7 0 0 0 14 0M12 18v3' }]])
    : icon([['path', { d: 'M4 5h16v11H8l-4 4z' }]]);
  symbol.classList.add('row-icon');
  button.append(symbol, text, element('span', 'row-age', age(conversation.createdAt)));
  button.addEventListener('click', () => showThread(conversation.id));
  item.append(button);
  return item;
}

function fillGroup(group, threads) {
  group.hidden = threads.length === 0;
  group.querySelector('ul').replaceChildren(...threads.map(createRow));
}

function createRow(thread) {
  const item = element('li');
  const button = element('button', 'row');
  button.type = 'button';
  button.dataset.state = thread.state;
  const text = element('span', 'row-text');
  text.append(
    element('span', 'row-title', thread.title || 'Untitled thread'),
    element('span', 'row-meta', describeThread(thread)),
  );
  const end = thread.state === 'waiting'
    ? element('span', 'pill', 'Approval')
    : element('span', 'row-age', age(thread.updatedAt));
  button.append(element('span', 'row-dot'), text, end);
  button.addEventListener('click', () => showThread(thread.id));
  item.append(button);
  return item;
}

async function showThread(id, { push = true } = {}) {
  leaveThread();
  if (push) history.pushState({ thread: id }, '');
  state.thread = id;
  state.threadData = null;
  setView('thread');
  const thread = state.catalog.get(id);
  const conversation = state.conversationIds.get(id);
  page.threadTitle.textContent = thread?.title || conversation?.title || 'Loading thread…';
  if (conversation) renderConversationMeta(conversation);
  else renderThreadMeta(thread);
  page.messages.replaceChildren();
  page.approval.hidden = true;
  page.replyForm.hidden = true;
  page.replyNote.textContent = '';
  updateThreadTalk();
  page.threadTitle.focus();
  await loadThread(id, true);
}

// Fetches an open thread and shows it, again every few seconds while it works.
async function loadThread(id, initial) {
  state.historyRequest?.abort();
  clearTimeout(state.threadRefresh);
  const request = new AbortController();
  state.historyRequest = request;
  const conversation = state.conversationIds.get(id);
  try {
    const response = await fetch(`/dispatch/sessions/${encodeURIComponent(id)}`, { signal: request.signal, cache: 'no-store' });
    if (response.status === 401) {
      showPairing();
      return;
    }
    if (!response.ok) throw Error('This thread isn’t available on the hub.');
    const data = await response.json();
    if (request.signal.aborted || state.thread !== id) return;
    state.threadData = data;
    const thread = state.catalog.get(id);
    const title = data.title || thread?.title || 'Untitled thread';
    page.threadTitle.textContent = title;
    if (!conversation) {
      renderThreadMeta({
        machine: data.machine || thread?.machine,
        agent: data.agent || thread?.agent,
        state: data.state || thread?.state,
      });
    }
    const list = page.messages;
    const nearBottom = list.scrollHeight - list.scrollTop - list.clientHeight < 80;
    const entries = data.entries || [];
    list.replaceChildren(...entries.map((entry) => createEntry(entry, Boolean(conversation))));
    if (!entries.length) list.append(element('p', 'muted', 'Nothing is saved for this thread yet.'));
    if (initial || nearBottom) requestAnimationFrame(() => { list.scrollTop = list.scrollHeight; });
    renderApproval(id, data);
    page.replyForm.hidden = Boolean(conversation);
    page.replyLabel.textContent = `Reply to ${title}`;
    page.replyText.placeholder = 'Reply to this thread…';
    if (data.state === 'running' || data.state === 'waiting') scheduleThreadRefresh(id, THREAD_REFRESH_MS);
  } catch (error) {
    if (!request.signal.aborted && initial) page.messages.replaceChildren(element('p', 'muted', error.message));
  }
}

function scheduleThreadRefresh(id, delay) {
  clearTimeout(state.threadRefresh);
  state.threadRefresh = setTimeout(() => {
    if (state.view === 'thread' && state.thread === id && !document.hidden) loadThread(id, false);
  }, delay);
}

function leaveThread() {
  state.historyRequest?.abort();
  clearTimeout(state.threadRefresh);
  state.thread = null;
}

function renderApproval(id, data) {
  const pending = data.pendingApproval;
  page.approval.hidden = !pending;
  if (!pending) return;
  const agent = agentName(data.agent || state.catalog.get(id)?.agent || '') || 'The agent';
  page.approvalTitle.textContent = pending.command ? `${agent} wants to run` : pending.title || 'Waiting for your approval';
  page.approvalCommand.textContent = pending.command || pending.question || '';
  const where = data.machine ? `On ${data.machine}` : '';
  page.approvalWhere.textContent = pending.commandTruncated ? `${where} · shortened here; the agent runs all of it` : where;
  page.approval.dataset.session = id;
  page.approval.dataset.approvalId = pending.approvalId;
  page.approvalError.hidden = true;
}

async function answerApproval(decision) {
  const id = page.approval.dataset.session;
  const approvalId = page.approval.dataset.approvalId;
  if (!id || !approvalId) return;
  page.approvalApprove.disabled = true;
  page.approvalReject.disabled = true;
  try {
    await post(`/dispatch/sessions/${encodeURIComponent(id)}/approval`, { approvalId, decision });
    page.approval.hidden = true;
    page.replyNote.textContent = decision === 'approve' ? 'Approved once. The agent carries on.' : 'Rejected. The agent won’t run it.';
    loadSessions();
    scheduleThreadRefresh(id, 1500);
  } catch (error) {
    page.approvalError.textContent = error.message;
    page.approvalError.hidden = false;
  } finally {
    page.approvalApprove.disabled = false;
    page.approvalReject.disabled = false;
  }
}

async function sendReply() {
  const id = state.thread;
  const text = page.replyText.value.trim();
  if (!id || !text) return;
  page.replySend.disabled = true;
  page.replyNote.textContent = 'Sending…';
  try {
    const result = await post(`/dispatch/sessions/${encodeURIComponent(id)}/reply`, { text });
    page.replyText.value = '';
    fitReplyBox();
    const sent = createMessage({ role: 'user', text });
    sent.dataset.pending = '';
    page.messages.append(sent);
    page.messages.scrollTop = page.messages.scrollHeight;
    page.replyNote.textContent = {
      queued: 'Queued. It goes out when the agent’s current turn ends.',
      sent: 'Sent. The agent is working on it.',
    }[result.status] || result.note || 'It goes out as soon as the agent is ready.';
    scheduleThreadRefresh(id, 3000);
  } catch (error) {
    page.replyNote.textContent = error.message;
  } finally {
    page.replySend.disabled = false;
  }
}

function fitReplyBox() {
  const box = page.replyText;
  box.style.height = 'auto';
  box.style.height = `${Math.min(box.scrollHeight, 140)}px`;
}

function openPendingThread() {
  if (!pendingThread) return;
  const id = pendingThread;
  pendingThread = null;
  history.replaceState(null, '', location.pathname);
  showThread(id);
}

// Notifications

function updateBadge(count) {
  if (!('setAppBadge' in navigator)) return;
  const update = count > 0 ? navigator.setAppBadge(count) : navigator.clearAppBadge();
  update.catch((error) => console.debug('Dispatch badge', error));
}

function bytesFromBase64Url(text) {
  const base64 = text.replace(/-/g, '+').replace(/_/g, '/').padEnd(Math.ceil(text.length / 4) * 4, '=');
  return Uint8Array.from(atob(base64), (char) => char.charCodeAt(0));
}

// Push works in the Home Screen app on iPhones, not in Safari tabs.
async function setupPush() {
  if (state.push.started) return;
  state.push.started = true;
  state.push.supported = 'serviceWorker' in navigator && 'PushManager' in window && 'Notification' in window;
  if (state.push.supported) {
    try {
      state.push.registration = await navigator.serviceWorker.register('/dispatch/sw.js', { scope: '/dispatch' });
      state.push.subscription = await state.push.registration.pushManager.getSubscription();
      // Tell the hub again, in case it lost this phone or its choices changed.
      if (state.push.subscription && Notification.permission === 'granted') await saveSubscription();
    } catch (error) {
      console.warn('Dispatch notifications', error);
    }
    try {
      const response = await fetch('/dispatch/push/key', { cache: 'no-store' });
      if (response.ok) state.push.key = (await response.json()).key;
    } catch (error) {
      console.debug('Dispatch push key', error);
    }
  }
  renderPush();
}

function saveSubscription() {
  return post('/dispatch/push/subscribe', {
    subscription: state.push.subscription.toJSON(),
    kinds: readSetting('kinds', DEFAULT_KINDS),
  });
}

function notificationsOn() {
  return Boolean(state.push.subscription) && state.push.supported && Notification.permission === 'granted';
}

async function turnOnNotifications(button) {
  if (!state.push.supported) {
    showAlerts();
    return;
  }
  button.disabled = true;
  try {
    // iPhones only ask from a tap, so nothing may be awaited before this.
    const permission = await Notification.requestPermission();
    if (permission !== 'granted') throw Error('Notifications are off for Dispatch. Turn them on in Settings → Notifications → Dispatch.');
    if (!state.push.registration) {
      state.push.registration = await navigator.serviceWorker.register('/dispatch/sw.js', { scope: '/dispatch' });
    }
    const registration = await navigator.serviceWorker.ready;
    if (!state.push.key) {
      const response = await fetch('/dispatch/push/key', { cache: 'no-store' });
      if (!response.ok) throw Error((await response.text()) || 'The hub has no notification key.');
      state.push.key = (await response.json()).key;
    }
    state.push.subscription = await registration.pushManager.subscribe({
      userVisibleOnly: true,
      applicationServerKey: bytesFromBase64Url(state.push.key),
    });
    await saveSubscription();
    showNotice('Notifications are on.');
  } catch (error) {
    showNotice(error.message || 'Notifications couldn’t be turned on.', true);
  } finally {
    button.disabled = false;
    renderPush();
  }
}

async function turnOffNotifications() {
  const subscription = state.push.subscription;
  if (!subscription) return;
  page.alertsOff.disabled = true;
  try {
    const endpoint = subscription.endpoint;
    await subscription.unsubscribe();
    state.push.subscription = null;
    await post('/dispatch/push/unsubscribe', { endpoint });
    showNotice('Notifications are off on this phone.');
  } catch (error) {
    showNotice(error.message, true);
  } finally {
    page.alertsOff.disabled = false;
    renderPush();
  }
}

async function sendTestNotification() {
  if (!state.push.subscription) return;
  page.alertsTest.disabled = true;
  try {
    await post('/dispatch/push/test', { endpoint: state.push.subscription.endpoint });
    showNotice('Sent. It should arrive in a few seconds.');
  } catch (error) {
    showNotice(error.message, true);
  } finally {
    page.alertsTest.disabled = false;
  }
}

async function toggleKind(button) {
  const kinds = { ...DEFAULT_KINDS, ...readSetting('kinds', DEFAULT_KINDS) };
  kinds[button.dataset.kind] = !kinds[button.dataset.kind];
  writeSetting('kinds', kinds);
  renderPush();
  if (!notificationsOn()) return;
  try {
    await saveSubscription();
  } catch (error) {
    showNotice(error.message, true);
  }
}

function renderPush() {
  const { supported } = state.push;
  const permission = supported ? Notification.permission : 'unsupported';
  const on = notificationsOn();
  page.notifyBanner.hidden = !state.push.started || !supported || on || permission === 'denied' || readSetting('bannerDismissed', false);
  page.alertsStatus.textContent = on
    ? 'On for this phone'
    : !supported ? 'Not available here'
    : permission === 'denied' ? 'Blocked in Settings'
    : 'Off';
  page.alertsStatus.toggleAttribute('data-on', on);
  page.alertsOn.hidden = on || !supported || permission === 'denied';
  page.alertsTest.hidden = !on;
  page.alertsOff.hidden = !on;
  const kinds = { ...DEFAULT_KINDS, ...readSetting('kinds', DEFAULT_KINDS) };
  for (const button of page.alertsSwitches) {
    button.setAttribute('aria-checked', String(Boolean(kinds[button.dataset.kind])));
    button.disabled = !supported;
  }
  page.alertsNote.textContent = !supported
    ? 'Notifications only work in the Home Screen app. In Safari, tap Share → Add to Home Screen, then open Dispatch from there.'
    : permission === 'denied'
      ? 'Notifications are blocked. Turn them on in Settings → Notifications → Dispatch.'
      : 'The hub sends these through Apple’s push service. They’re encrypted on the hub, so only this phone can read the thread names and lines in them.';
}

function showAlerts({ push = true } = {}) {
  leaveThread();
  if (push) history.pushState({ alerts: true }, '');
  setView('alerts');
  renderPush();
  page.alertsTitle.focus();
}

function renderThreadMeta(thread) {
  if (!thread) {
    page.threadMeta.replaceChildren();
    return;
  }
  const status = element('span', 'tag', stateLabel(thread.state));
  status.dataset.state = thread.state;
  page.threadMeta.replaceChildren(
    element('span', 'tag', thread.machine),
    element('span', 'tag', agentName(thread.agent)),
    status,
  );
}

function renderConversationMeta(conversation) {
  const voice = conversation.kind === 'voice';
  const tags = [element('span', 'tag', voice ? 'Voice call' : 'Typed question'), element('span', 'tag', startedAt(conversation.createdAt))];
  if (voice) tags.push(element('span', 'tag', duration(conversation.updatedAt - conversation.createdAt)));
  page.threadMeta.replaceChildren(...tags);
}

function createEntry(entry, conversation) {
  if (entry.type === 'tool') return createStep(entry);
  if (entry.role === 'thought') return createNotes(entry.text, conversation ? 'Codex’s notes' : 'Thinking');
  return createMessage(entry, conversation);
}

const stepIcons = {
  read: [['path', { d: 'M2 12s3.5-7 10-7 10 7 10 7-3.5 7-10 7S2 12 2 12z' }], ['circle', { cx: 12, cy: 12, r: 3 }]],
  search: [['circle', { cx: 11, cy: 11, r: 7 }], ['path', { d: 'M20 20l-3.5-3.5' }]],
  execute: [['path', { d: 'M4 17l6-5-6-5M12 19h8' }]],
  edit: [['path', { d: 'M12 20h9M16.5 3.5a2.1 2.1 0 0 1 3 3L7 19l-4 1 1-4z' }]],
  other: [['path', { d: 'M5 12h14M12 5l7 7-7 7' }]],
};

// A tool call, shown like an agent's step: what it did, and on tap what it
// read or printed.
function createStep({ title, kind, status, text }) {
  const step = element('details', 'step');
  step.dataset.kind = kind || 'other';
  if (status) step.dataset.status = status;
  const summary = element('summary');
  const symbol = icon(stepIcons[kind] || stepIcons.other);
  symbol.classList.add('step-icon');
  summary.append(symbol, element('span', 'step-title', title || 'Tool call'));
  if (status === 'failed') summary.append(element('span', 'step-status', 'Failed'));
  step.append(summary);
  if (text && text.trim()) step.append(element('pre', 'step-body', text.trim()));
  else step.dataset.empty = '';
  return step;
}

function createNotes(text, label) {
  const notes = element('details', 'notes');
  notes.append(element('summary', null, label), formatText(text));
  return notes;
}

function createMessage({ role, text, agent }, conversation = false) {
  const viaDispatch = role === 'user' && text.startsWith(DISPATCH_PREFIX);
  const message = element('div', 'message');
  message.dataset.role = role;
  if (viaDispatch) message.dataset.via = 'dispatch';
  const assistant = conversation ? 'Dispatch' : agentName(agent) || 'Agent';
  const who = role === 'user' ? (viaDispatch ? 'You, via Dispatch' : 'You') : assistant;
  message.append(
    element('span', 'message-who', who),
    formatText(viaDispatch ? text.slice(DISPATCH_PREFIX.length) : text),
  );
  return message;
}

// Agents write Markdown. Show its bold, code and bullets without inserting
// any of the text as HTML.
function formatText(text) {
  const block = element('div', 'message-text');
  for (const line of text.split('\n')) {
    const bullet = line.match(/^\s*[-*]\s+(.*)$/);
    const heading = line.match(/^\s*#{1,6}\s+(.*)$/);
    const paragraph = element('p', bullet ? 'bullet' : heading ? 'heading' : null);
    appendInline(paragraph, bullet ? bullet[1] : heading ? heading[1] : line);
    block.append(paragraph);
  }
  return block;
}

function appendInline(parent, text) {
  for (const part of text.split(/(\*\*[^*]+\*\*|`[^`]+`)/)) {
    if (!part) continue;
    if (part.startsWith('**') && part.endsWith('**') && part.length > 4) parent.append(element('strong', null, part.slice(2, -2)));
    else if (part.startsWith('`') && part.endsWith('`') && part.length > 2) parent.append(element('code', null, part.slice(1, -1)));
    else parent.append(document.createTextNode(part));
  }
}

function showPairing() {
  endCall(null);
  setView('pair');
}

// Calls

function waitForCandidates(peer) {
  if (peer.iceGatheringState === 'complete') return Promise.resolve();
  return new Promise((resolve) => {
    const done = () => {
      clearTimeout(timer);
      peer.removeEventListener('icegatheringstatechange', changed);
      resolve();
    };
    const changed = () => {
      if (peer.iceGatheringState === 'complete') done();
    };
    const timer = setTimeout(done, 2000);
    peer.addEventListener('icegatheringstatechange', changed);
  });
}

async function startCall() {
  if (state.call) return;
  const call = {
    approvals: new Set(),
    lastLoud: -Infinity,
    micLevel: 0,
    speakerLevel: 0,
    muted: false,
    speaking: false,
    connected: false,
    captionRole: null,
    captionText: '',
  };
  state.call = call;
  keepScreenOn(call);
  page.log.replaceChildren();
  page.body.removeAttribute('data-compact');
  showNotice('');
  setCaption('', null);
  setView('call');
  updateCallStatus(call);
  updateThreadTalk();
  try {
    if (!navigator.mediaDevices?.getUserMedia) {
      throw Error('Open Dispatch at its HTTPS address to use the microphone.');
    }
    // Created during the tap, so phones allow the audio meters to run.
    const AudioContext = window.AudioContext || window.webkitAudioContext;
    call.audio = new AudioContext();
    call.audio.resume().catch((error) => console.debug('Dispatch audio resume', error));
    const mic = await navigator.mediaDevices.getUserMedia({ audio: { echoCancellation: true, noiseSuppression: true } });
    if (state.call !== call) {
      mic.getTracks().forEach((track) => track.stop());
      return;
    }
    call.mic = mic;
    call.micMeter = createMeter(call.audio, mic);
    const peer = new RTCPeerConnection();
    call.peer = peer;
    for (const track of mic.getAudioTracks()) {
      track.enabled = false;
      peer.addTrack(track, mic);
    }
    peer.createDataChannel('oai-events');
    peer.addEventListener('track', (event) => {
      if (state.call !== call || event.track.kind !== 'audio') return;
      const stream = event.streams[0] || new MediaStream([event.track]);
      page.speaker.srcObject = stream;
      page.speaker.play().catch(() => {
        if (state.call === call) page.play.hidden = false;
      });
      call.speakerMeter = createMeter(call.audio, stream);
    });
    peer.addEventListener('connectionstatechange', () => {
      if (state.call !== call) return;
      if (peer.connectionState === 'connected') {
        call.connected = true;
        for (const track of mic.getAudioTracks()) track.enabled = !call.muted;
        if (!call.startedAt) {
          call.startedAt = Date.now();
          call.timer = setInterval(updateTimer, 1000);
          updateTimer();
        }
        updateCallStatus(call);
      } else if (peer.connectionState === 'disconnected') {
        setStatus('Reconnecting…');
      } else if (peer.connectionState === 'failed') {
        endCall('The voice connection failed. Tap the orb to try again.', true);
      }
    });
    await peer.setLocalDescription(await peer.createOffer());
    await waitForCandidates(peer);
    if (state.call !== call) return;
    const socket = new WebSocket(`wss://${location.host}/dispatch/call`);
    call.socket = socket;
    socket.addEventListener('open', () => {
      if (state.call === call) socket.send(JSON.stringify({ sdp: peer.localDescription.sdp }));
    });
    socket.addEventListener('message', (event) => {
      handleUpdate(call, event.data).catch((error) => {
        if (state.call === call) endCall(`The call couldn’t connect. ${error.message}`, true);
      });
    });
    socket.addEventListener('close', (event) => {
      if (state.call !== call) return;
      if (event.wasClean) endCall('Call ended.');
      else if (call.leftPage) endCall('The call ended because the screen locked or you left Safari. Tap the orb to call again.', true);
      else endCall('The connection to the hub dropped. Tap the orb to call again.', true);
    });
    call.sampler = setInterval(() => sampleLevels(call), 50);
  } catch (error) {
    if (state.call !== call) return;
    const message = error.name === 'NotAllowedError'
      ? 'Allow microphone access in your browser, then tap the orb again.'
      : error.message;
    endCall(message, true);
  }
}

// A web page doesn't keep the phone awake. When Auto-Lock turns the screen
// off, Safari suspends the page and its connection, ending the call.
function keepScreenOn(call) {
  if (!navigator.wakeLock || document.hidden || (call.wakeLock && !call.wakeLock.released)) return;
  navigator.wakeLock.request('screen')
    .then((lock) => {
      if (state.call === call) call.wakeLock = lock;
      else lock.release().catch((error) => console.debug('Dispatch screen lock release', error));
    })
    .catch((error) => console.debug('Dispatch could not keep the screen on', error));
}

function sampleLevels(call) {
  if (state.call !== call) return;
  const now = performance.now();
  const speakerRms = call.speakerMeter ? call.speakerMeter() : 0;
  if (speakerRms > SPEAKING_RMS) call.lastLoud = now;
  call.speakerLevel = levelFromRms(speakerRms);
  call.micLevel = call.muted || !call.micMeter ? 0 : levelFromRms(call.micMeter());
  const speaking = now - call.lastLoud < SPEAKING_HOLD_MS;
  if (speaking !== call.speaking) {
    call.speaking = speaking;
    updateCallStatus(call);
  }
}

function updateCallStatus(call) {
  if (state.call !== call) return;
  let mode = 'listening';
  let label = 'Listening';
  if (!call.connected) {
    mode = 'connecting';
    label = 'Connecting…';
  } else if (call.speaking) {
    mode = 'speaking';
    label = 'Speaking';
  } else if (call.muted) {
    mode = 'muted';
    label = 'Muted';
  }
  if (mode === call.mode) return;
  call.mode = mode;
  setStatus(label);
  const source = {
    connecting: (seconds) => 0.3 + 0.15 * Math.sin(seconds * 3),
    speaking: () => call.speakerLevel,
    muted: () => 0.05,
    listening: () => call.micLevel,
  }[mode];
  orb.setMode(mode, source);
}

async function handleUpdate(call, data) {
  if (state.call !== call) return;
  let update;
  try {
    update = JSON.parse(data);
  } catch (error) {
    console.debug('Dispatch sent an unreadable update', error);
    return;
  }
  switch (update.type) {
    case 'answer':
      await call.peer.setRemoteDescription({ type: 'answer', sdp: update.sdp });
      break;
    case 'caption':
      showLiveCaption(call, update.role, update.delta);
      break;
    case 'transcript':
      addTranscript(call, update.role, update.text);
      break;
    case 'tool':
      addToolChip(update);
      break;
    case 'approval':
      addApproval(call, update);
      break;
    case 'answered':
      if (update.answer === 'approved once') addChip('Approved once');
      else addChip('Rejected', 'rejected');
      break;
    case 'openThread':
      showThread(update.id);
      break;
    case 'idle':
      endCall('The call ended after two quiet minutes.');
      break;
    case 'closed':
      endCall(update.reason ? `The voice session closed: ${update.reason}` : 'The voice session closed. Tap the orb to call again.', true);
      break;
    case 'error':
      endCall(update.message || 'Dispatch ran into a problem.', true);
      break;
  }
}

function showLiveCaption(call, role, delta) {
  if (typeof delta !== 'string') return;
  if (call.captionRole !== role) {
    call.captionRole = role;
    call.captionText = '';
  }
  call.captionText += delta;
  setCaption(tail(call.captionText, 160), role);
}

function addTranscript(call, role, text) {
  if (typeof text !== 'string' || !text.trim()) return;
  if (call.captionRole === role) {
    call.captionRole = null;
    call.captionText = '';
    setCaption('', null);
  }
  appendLog(element('p', role === 'user' ? 'bubble' : 'said', text.trim()));
}

function addToolChip(update) {
  const title = threadTitle(update.sessionId);
  if (update.name === 'tell_session') {
    addChip(update.status === 'queued' ? `Queued for ${title}` : `Passed your message to ${title}`);
  } else if (update.name === 'ask_session') {
    addChip(update.status === 'answered' ? `${title} answered` : `Asked ${title}`);
  } else if (update.name === 'start_session' && update.status === 'started') {
    addChip(`Started a thread on ${update.machine}`);
  } else if (update.name === 'start_session' && update.status === 'starting') {
    addChip(`Starting a thread on ${update.machine}`);
  }
}

function addChip(text, kind) {
  const chip = element('p', 'chip', text);
  if (kind) chip.dataset.kind = kind;
  appendLog(chip);
}

function addApproval(call, update) {
  const command = update.command || update.question || 'a command';
  const key = `${update.sessionId}\n${command}`;
  if (call.approvals.has(key)) return;
  call.approvals.add(key);
  const thread = state.catalog.get(update.sessionId);
  const head = element('div', 'approval-head');
  head.append(
    element('span', 'pill', 'Waiting for approval'),
    element('span', 'approval-where', thread ? describeThread(thread) : ''),
  );
  const hint = element('p', 'hint');
  hint.append(
    icon([['rect', { x: 9, y: 3, width: 6, height: 11, rx: 3 }], ['path', { d: 'M5 11a7 7 0 0 0 14 0M12 18v3' }]]),
    document.createTextNode('Say “approve” to allow it once'),
  );
  const card = element('div', 'approval');
  card.append(head, element('p', 'approval-title', thread?.title || 'A thread'), element('code', null, command), hint);
  appendLog(card);
}

function appendLog(node) {
  page.log.append(node);
  // The orb stays large until the transcript needs the room.
  page.body.toggleAttribute('data-compact', page.log.childElementCount >= 3);
  requestAnimationFrame(() => { page.log.scrollTop = page.log.scrollHeight; });
}

function formatDuration(milliseconds) {
  const seconds = Math.floor(milliseconds / 1000);
  return `${Math.floor(seconds / 60)}:${String(seconds % 60).padStart(2, '0')}`;
}

function updateTimer() {
  const call = state.call;
  page.timer.textContent = call?.startedAt ? formatDuration(Date.now() - call.startedAt) : '0:00';
  updateThreadTalk();
}

function updateThreadTalk() {
  page.threadTalk.toggleAttribute('data-call', Boolean(state.call));
  page.threadTalkLabel.textContent = state.call ? `Back to call · ${page.timer.textContent}` : 'Talk to Dispatch';
}

function endCall(message, error = false) {
  const call = state.call;
  if (call) {
    state.call = null;
    clearInterval(call.timer);
    clearInterval(call.sampler);
    call.socket?.close();
    call.peer?.close();
    call.mic?.getTracks().forEach((track) => track.stop());
    call.audio?.close().catch((closeError) => console.debug('Dispatch audio close', closeError));
    call.wakeLock?.release().catch((releaseError) => console.debug('Dispatch screen lock release', releaseError));
    page.speaker.srcObject = null;
    page.play.hidden = true;
    page.mute.setAttribute('aria-pressed', 'false');
    page.mute.setAttribute('aria-label', 'Mute');
    page.log.replaceChildren();
    page.body.removeAttribute('data-compact');
    page.timer.textContent = '0:00';
    setStatus('Tap to talk');
    setCaption(HOME_HINT, null);
    orb.setMode('idle', idleLevel);
    updateThreadTalk();
    if (state.view === 'call') {
      setView('home');
      loadSessions();
    }
  }
  showNotice(message || '', error);
}

// Controls

page.pairForm.addEventListener('submit', async (event) => {
  event.preventDefault();
  const button = page.pairForm.querySelector('button');
  button.disabled = true;
  try {
    const response = await fetch('/dispatch/pair', { method: 'POST', body: page.code.value.trim() });
    if (!response.ok) throw Error((await response.text()) || 'Pairing failed.');
    page.code.value = '';
    showNotice('');
    await loadSessions();
  } catch (error) {
    const unreachable = error instanceof TypeError || !error.message;
    showNotice(unreachable ? 'The hub can’t be reached. Check that Tailscale is on.' : error.message, true);
  } finally {
    button.disabled = false;
  }
});

page.talk.addEventListener('click', () => {
  if (state.view === 'home' && !state.call) startCall();
});

page.mute.addEventListener('click', () => {
  const call = state.call;
  if (!call) return;
  call.muted = !call.muted;
  page.mute.setAttribute('aria-pressed', String(call.muted));
  page.mute.setAttribute('aria-label', call.muted ? 'Unmute' : 'Mute');
  if (call.connected) call.mic?.getAudioTracks().forEach((track) => { track.enabled = !call.muted; });
  updateCallStatus(call);
});

page.end.addEventListener('click', () => endCall('Call ended.'));

page.play.addEventListener('click', () => {
  page.speaker.play()
    .then(() => { page.play.hidden = true; })
    .catch(() => showNotice('The browser blocked the speaker. Try again.', true));
});

page.showAll.addEventListener('click', () => {
  state.showAll = true;
  renderThreads();
});

page.showCalls.addEventListener('click', () => {
  state.showAllCalls = true;
  renderThreads();
});

page.back.addEventListener('click', () => history.back());

page.threadTalk.addEventListener('click', () => {
  // Leaving the thread returns to the call, or starts one within this tap.
  history.back();
  if (!state.call) startCall();
});

window.addEventListener('popstate', (event) => {
  if (event.state?.thread) {
    showThread(event.state.thread, { push: false });
    return;
  }
  if (event.state?.alerts) {
    showAlerts({ push: false });
    return;
  }
  leaveThread();
  if (state.view === 'pair' || state.view === 'loading') return;
  setView(state.call ? 'call' : 'home');
  if (!state.call) loadSessions();
});

document.addEventListener('visibilitychange', () => {
  const call = state.call;
  if (call && document.hidden) call.leftPage = true;
  // The screen lock is released whenever the page is hidden; take it again.
  if (call && !document.hidden) keepScreenOn(call);
  if (!document.hidden && state.view === 'home') loadSessions();
  if (!document.hidden && state.view === 'thread' && state.thread) loadThread(state.thread, false);
});

window.addEventListener('pagehide', () => endCall(null));

page.alertsOpen.addEventListener('click', () => showAlerts());
page.alertsBack.addEventListener('click', () => history.back());
page.notifyOn.addEventListener('click', () => turnOnNotifications(page.notifyOn));
page.notifyLater.addEventListener('click', () => {
  writeSetting('bannerDismissed', true);
  renderPush();
});
page.alertsOn.addEventListener('click', () => turnOnNotifications(page.alertsOn));
page.alertsOff.addEventListener('click', turnOffNotifications);
page.alertsTest.addEventListener('click', sendTestNotification);
for (const button of page.alertsSwitches) button.addEventListener('click', () => toggleKind(button));
page.approvalApprove.addEventListener('click', () => answerApproval('approve'));
page.approvalReject.addEventListener('click', () => answerApproval('reject'));
page.replyForm.addEventListener('submit', (event) => {
  event.preventDefault();
  sendReply();
});
page.replyText.addEventListener('input', fitReplyBox);

// Tapping a notification while Dispatch is open lands here.
navigator.serviceWorker?.addEventListener('message', (event) => {
  if (event.data?.type !== 'open') return;
  const id = new URL(event.data.url, location.origin).searchParams.get('thread');
  if (!id) return;
  if (state.view === 'pair' || state.view === 'loading') pendingThread = id;
  else showThread(id);
});

setInterval(() => {
  if (state.view === 'home' && !document.hidden) loadSessions();
}, REFRESH_MS);

page.hubName.textContent = location.hostname.split('.')[0];
page.pairHost.textContent = location.hostname;
setCaption(HOME_HINT, null);
loadSessions();
