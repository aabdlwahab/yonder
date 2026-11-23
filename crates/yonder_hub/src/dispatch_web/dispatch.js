const byId = (id) => document.getElementById(id);
const status = byId('status');
const pair = byId('pair');
const talk = byId('talk');
const speaker = byId('speaker');
const play = byId('play');
const transcript = byId('transcript');
const activity = byId('activity');
const sessions = byId('sessions');
const history = byId('history');
const catalogStatus = byId('catalog-status');
const toolActivity = {
  tell_session: 'Passed your message to the thread',
  ask_session: 'Asked the thread’s agent',
  answer_approval: 'Answered the approval',
};
let call;
let historyRequest;

function showStatus(element, message, error = false) {
  element.textContent = message;
  element.hidden = !message;
  element.toggleAttribute('data-error', error);
}

function showPairing(message, error = false) {
  endCall(message, error);
  byId('dispatch').hidden = true;
  pair.hidden = false;
  sessions.replaceChildren();
  transcript.replaceChildren();
  showSessions();
}

pair.addEventListener('submit', async (event) => {
  event.preventDefault();
  const button = pair.querySelector('button');
  button.disabled = true;
  try {
    const response = await fetch('/dispatch/pair', { method: 'POST', body: byId('code').value });
    if (!response.ok) throw Error(await response.text());
    byId('code').value = '';
    await loadSessions();
  } catch (error) {
    showStatus(status, error.message || 'The hub could not be reached.', true);
  } finally {
    button.disabled = false;
  }
});

async function loadSessions() {
  const refresh = byId('refresh');
  refresh.disabled = true;
  try {
    const response = await fetch('/dispatch/sessions');
    if (response.status === 401) {
      showPairing('Enter the pairing code from your hub.');
      return;
    }
    if (!response.ok) throw Error('Could not load threads from the hub.');
    const catalog = await response.json();
    const rows = catalog.sessions.map((thread) => {
      const item = document.createElement('li');
      const button = document.createElement('button');
      const details = document.createElement('small');
      button.type = 'button';
      button.textContent = thread.title || 'Untitled thread';
      details.textContent = [thread.machine, thread.agent, thread.state].filter(Boolean).join(' · ');
      button.append(details);
      item.append(button);
      button.addEventListener('click', () => openThread(thread.id));
      return item;
    });
    sessions.replaceChildren(...rows);
    showStatus(catalogStatus, catalog.total ? `Showing ${rows.length} of ${catalog.total} threads` : 'No threads yet.');
    pair.hidden = true;
    byId('dispatch').hidden = false;
    if (!call) showStatus(status, 'Ready to talk');
  } catch (error) {
    showStatus(byId('dispatch').hidden ? status : catalogStatus, error.message, true);
    if (byId('dispatch').hidden && pair.hidden) {
      pair.hidden = false;
    }
  } finally {
    refresh.disabled = false;
  }
}

function addMessage(parent, role, text) {
  if (typeof text !== 'string' || !text.trim()) return;
  const message = document.createElement('p');
  const label = document.createElement('span');
  message.className = 'entry';
  message.dataset.role = role;
  label.className = 'speaker';
  label.textContent = role === 'user' ? 'You' : parent === transcript ? 'Dispatch' : 'Agent';
  message.append(label, text);
  parent.append(message);
}

async function openThread(id) {
  historyRequest?.abort();
  const request = new AbortController();
  historyRequest = request;
  const messages = byId('messages');
  const heading = byId('thread-title');
  sessions.hidden = true;
  history.hidden = false;
  heading.textContent = 'Loading thread…';
  messages.replaceChildren();
  heading.focus();
  try {
    const response = await fetch(`/dispatch/sessions/${encodeURIComponent(id)}`, { signal: request.signal });
    if (response.status === 401) {
      showPairing('Pair this browser to open threads.');
      return;
    }
    if (!response.ok) throw Error('Thread unavailable');
    const thread = await response.json();
    if (request.signal.aborted) return;
    heading.textContent = thread.title || 'Untitled thread';
    for (const message of thread.messages.reverse()) addMessage(messages, message.role, message.text);
    if (!messages.childElementCount) messages.textContent = 'No messages are available for this thread.';
  } catch (error) {
    if (!request.signal.aborted) heading.textContent = error.message;
  }
}

function showSessions() {
  historyRequest?.abort();
  history.hidden = true;
  byId('messages').replaceChildren();
  sessions.hidden = false;
}

function endCall(message, error = false) {
  const active = call;
  call = undefined;
  active?.socket?.close();
  active?.peer?.close();
  active?.mic?.getTracks().forEach((track) => track.stop());
  speaker.srcObject = null;
  play.hidden = true;
  activity.hidden = true;
  talk.textContent = 'Talk to Dispatch';
  talk.removeAttribute('data-active');
  showStatus(status, message, error);
}

function waitForCandidates(peer) {
  if (peer.iceGatheringState === 'complete') return Promise.resolve();
  return new Promise((resolve) => {
    function done() {
      clearTimeout(timer);
      peer.removeEventListener('icegatheringstatechange', changed);
      resolve();
    }
    function changed() {
      if (peer.iceGatheringState === 'complete') done();
    }
    const timer = setTimeout(done, 2000);
    peer.addEventListener('icegatheringstatechange', changed);
  });
}

async function startCall() {
  const active = {};
  call = active;
  transcript.replaceChildren();
  talk.textContent = 'Hang up';
  talk.setAttribute('data-active', '');
  showStatus(status, 'Waiting for microphone permission…');
  try {
    if (!navigator.mediaDevices?.getUserMedia) throw Error('Open Dispatch at its trusted HTTPS address to use the microphone.');
    const mic = await navigator.mediaDevices.getUserMedia({ audio: { echoCancellation: true } });
    if (call !== active) {
      mic.getTracks().forEach((track) => track.stop());
      return;
    }
    active.mic = mic;
    showStatus(status, 'Connecting to Dispatch…');
    const peer = new RTCPeerConnection();
    active.peer = peer;
    for (const track of mic.getAudioTracks()) {
      track.enabled = false;
      peer.addTrack(track, mic);
    }
    peer.createDataChannel('oai-events');
    peer.addEventListener('track', (event) => {
      if (call !== active || event.track.kind !== 'audio') return;
      speaker.srcObject = event.streams[0] || new MediaStream([event.track]);
      speaker.play().catch(() => { if (call === active) play.hidden = false; });
    });
    peer.addEventListener('connectionstatechange', () => {
      if (call !== active) return;
      if (peer.connectionState === 'connected') {
        mic.getAudioTracks().forEach((track) => { track.enabled = true; });
        showStatus(status, 'Listening. Try “What’s running?”');
      } else if (peer.connectionState === 'disconnected') {
        showStatus(status, 'Reconnecting…');
      } else if (peer.connectionState === 'failed') {
        endCall('The voice connection failed. Tap to try again.', true);
      }
    });
    await peer.setLocalDescription(await peer.createOffer());
    await waitForCandidates(peer);
    if (call !== active) return;
    const socket = new WebSocket(`wss://${location.host}/dispatch/call`);
    active.socket = socket;
    socket.addEventListener('open', () => {
      if (call === active) socket.send(JSON.stringify({ sdp: peer.localDescription.sdp }));
    });
    socket.addEventListener('message', async (event) => {
      if (call !== active) return;
      try {
        const update = JSON.parse(event.data);
        switch (update.type) {
          case 'answer':
            await peer.setRemoteDescription({ type: 'answer', sdp: update.sdp });
            break;
          case 'transcript':
            addMessage(transcript, update.role, update.text);
            transcript.scrollTop = transcript.scrollHeight;
            break;
          case 'tool':
            showStatus(activity, toolActivity[update.name] || 'Checked the hub’s threads');
            break;
          case 'openThread':
            await openThread(update.id);
            break;
          case 'idle':
            endCall('Call ended after inactivity.');
            break;
          case 'error':
            endCall(update.message, true);
            break;
        }
      } catch (error) {
        if (call === active) endCall(`Could not connect the call. ${error.message}`, true);
      }
    });
    socket.addEventListener('close', (event) => {
      if (call !== active) return;
      endCall(event.wasClean ? 'Call ended.' : 'The hub connection closed. Tap to try again.', !event.wasClean);
    });
  } catch (error) {
    if (call !== active) return;
    const message = error.name === 'NotAllowedError'
      ? 'Allow microphone access in your browser and try again.'
      : error.message;
    endCall(message, true);
  }
}

talk.addEventListener('click', () => call ? endCall('Call ended.') : startCall());
play.addEventListener('click', async () => {
  await speaker.play().then(() => { play.hidden = true; }).catch(() => {
    showStatus(status, 'The browser could not play audio. Try enabling the speaker again.', true);
  });
});
byId('refresh').addEventListener('click', loadSessions);
byId('back').addEventListener('click', showSessions);
window.addEventListener('pagehide', () => endCall('Ready to talk'));
loadSessions();
