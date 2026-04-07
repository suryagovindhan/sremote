/**
 * sRemote Technician Console — app.js
 *
 * Flow:
 *  1. Connect to Session Broker via WSS.
 *  2. Receive "config" → build RTCPeerConnection with TURN credentials.
 *  3. Wait for "offer" from daemon → set remote desc → create answer → send.
 *  4. Exchange ICE candidates with daemon via broker relay.
 *  5. Render remote video.  Capture mouse/keyboard events → DataChannel.
 *  6. Handle iceconnectionstatechange → show overlays as needed.
 */

'use strict';

// ─── DOM refs ────────────────────────────────────────────────────────────────
const $ = id => document.getElementById(id);

const elVideo           = $('remote-video');
const elConnectBtn      = $('btn-connect');
const elDisconnectBtn   = $('btn-disconnect');
const elStatusText      = $('status-text');
const elWsDot           = $('ws-dot');
const elIceDot          = $('ice-dot');
const elToolbar         = $('toolbar');
const elQualityVal      = $('quality-val');
const elLatencyVal      = $('latency-val');

const elOverlayIdle       = $('overlay-idle');
const elOverlayConnecting = $('overlay-connecting');
const elOverlayRelay      = $('overlay-relay');
const elOverlayError      = $('overlay-error');
const elOverlayErrorTitle = $('overlay-error-title');
const elOverlaySub        = $('overlay-connecting-sub');
const elOverlayErrorSub   = $('overlay-error-sub');

// ─── State ────────────────────────────────────────────────────────────────────
let ws            = null;  // WebSocket to broker
let pc            = null;  // RTCPeerConnection
let dataChannel   = null;  // Control channel (received from daemon)
let statsTimer    = null;
let iceServers    = [];    // Populated from broker "config" message

// Remote video absolute bounds (native host resolution, for coordinate mapping)
let remoteNativeWidth  = null;
let remoteNativeHeight = null;

// Stats tracking for deltas
let lastReport = {
  timestamp: 0,
  bytesReceived: 0,
  framesDecoded: 0,
  framesDropped: 0
};

// ─── Overlay helpers ──────────────────────────────────────────────────────────
function showOverlay(name, subtitle = '') {
  [elOverlayIdle, elOverlayConnecting, elOverlayRelay, elOverlayError]
    .forEach(el => el.classList.add('hidden'));

  if (name === 'idle')       { elOverlayIdle.classList.remove('hidden'); }
  if (name === 'connecting') {
    elOverlaySub.textContent = subtitle || 'Negotiating WebRTC session';
    elOverlayConnecting.classList.remove('hidden');
  }
  if (name === 'relay')  { elOverlayRelay.classList.remove('hidden'); }
  if (name === 'error')  {
    elOverlayErrorTitle.textContent = subtitle || 'Connection Error';
    elOverlayErrorSub.textContent   = '';
    elOverlayError.classList.remove('hidden');
  }
  if (name === 'none')   { /* all hidden */ }
}

function setStatus(text, wsDot = '', iceDot = '') {
  elStatusText.textContent = text;
  elWsDot.className  = 'status-dot ' + wsDot;
  elIceDot.className = 'status-dot ' + iceDot;
}

// ─── Connect ─────────────────────────────────────────────────────────────────
elConnectBtn.addEventListener('click', () => {
  const url   = $('inp-url').value.trim();
  const room  = $('inp-room').value.trim();
  const token = $('inp-token').value.trim();

  if (!url || !room || !token) {
    alert('Please fill in broker URL, room ID, and JWT token.');
    return;
  }
  startSession(url, room, token);
});

elDisconnectBtn.addEventListener('click', () => teardown('Disconnected'));

// ─── Session lifecycle ────────────────────────────────────────────────────────
function startSession(brokerUrl, room, token) {
  elConnectBtn.style.display    = 'none';
  elDisconnectBtn.style.display = '';
  showOverlay('connecting', 'Connecting to Session Broker…');
  setStatus('Connecting to broker…', 'warning');

  const wsUrl = `${brokerUrl}/ws/${room}?token=${encodeURIComponent(token)}`;
  ws = new WebSocket(wsUrl);

  ws.onopen = () => {
    setStatus('Broker connected — awaiting peer…', 'connected');
    showOverlay('connecting', 'Waiting for remote agent to connect…');
  };

  ws.onmessage = async ({ data }) => {
    let msg;
    try { msg = JSON.parse(data); } catch { return; }

    // ── Broker hands us TURN credentials ──────────────────────────────────
    if (msg.type === 'config') {
      iceServers = (msg.ice_servers || []).map(s => ({
        urls:       s.urls,
        username:   s.username   || undefined,
        credential: s.credential || undefined,
      }));
      console.log('[sRemote] Received ICE config:', iceServers);
      return;
    }

    // ── SDP offer from daemon ──────────────────────────────────────────────
    if (msg.type === 'offer') {
      showOverlay('connecting', 'Received offer — creating answer…');
      await handleOffer(msg.sdp);
      return;
    }

    // ── ICE candidate from daemon ──────────────────────────────────────────
    if (msg.type === 'ice' && pc) {
      try {
        await pc.addIceCandidate({
          candidate:     msg.candidate,
          sdpMLineIndex: msg.sdpMLineIndex,
          sdpMid:        msg.sdpMid,
        });
      } catch (e) {
        console.warn('[sRemote] addIceCandidate error:', e);
      }
    }
  };

  ws.onerror = err => {
    console.error('[sRemote] WS error', err);
    setStatus('WebSocket error', 'error');
    showOverlay('error', 'Broker Connection Failed');
  };

  ws.onclose = () => {
    setStatus('Broker disconnected', 'error');
    if (pc && pc.connectionState !== 'closed') {
      showOverlay('error', 'Broker Disconnected');
    }
  };
}

// ─── WebRTC negotiation ───────────────────────────────────────────────────────
async function handleOffer(sdpString) {
  if (pc) {
    console.warn('[sRemote] Closing existing PeerConnection before handling new offer...');
    pc.close();
    pc = null;
    clearInterval(statsTimer);
    elVideo.srcObject = null;
  }

  if (!iceServers.length) {
    console.warn('[sRemote] No ICE servers yet — broker config not received?');
  }

  pc = new RTCPeerConnection({ iceServers });

  // ── Track handler: attach remote video ──────────────────────────────────
  pc.ontrack = ({ track, streams }) => {
    console.log('[sRemote] ontrack fired — kind:', track.kind, 'streams:', streams.length);
    let stream;
    if (streams && streams[0]) {
      stream = streams[0];
    } else {
      // webrtc-rs add_track() does not attach a MediaStream — build one manually
      stream = new MediaStream([track]);
    }
    elVideo.srcObject = stream;
    
    // Explicitly unmute/mute for play() if browser blocks
    elVideo.muted = true; 
    elVideo.play()
      .then(() => console.log('[sRemote] Video playback started'))
      .catch(e => {
        console.warn('[sRemote] video.play() blocked/interrupted:', e);
        // Show a "Click to play" helper if needed? Overlays usually handle this.
      });

    console.log('[sRemote] Video stream attached. Track state:', track.readyState);

    elVideo.onloadedmetadata = () => {
      console.log('[sRemote] video loadedmetadata. src resolution:', elVideo.videoWidth, 'x', elVideo.videoHeight);
      if (!remoteNativeWidth) {
        remoteNativeWidth  = elVideo.videoWidth  || 1920;
        remoteNativeHeight = elVideo.videoHeight || 1080;
        console.log(`[sRemote] Cached native resolution: ${remoteNativeWidth}×${remoteNativeHeight}`);
      }
    };
    elVideo.onplaying = () => console.log('[sRemote] VIDEO PLAYING');
    elVideo.onwaiting = () => console.log('[sRemote] VIDEO WAITING (buffering)');
    elVideo.onerror = (e) => console.error('[sRemote] VIDEO ERROR:', e);
  };

  // ── DataChannel from daemon (control channel) ────────────────────────────
  pc.ondatachannel = ({ channel }) => {
    dataChannel = channel;
    dataChannel.onopen  = () => console.log('[sRemote] Control channel open');
    dataChannel.onclose = () => console.log('[sRemote] Control channel closed');
    // We send on this channel; receiving is for future server→tech messages.
    setupInputCapture();
    elToolbar.style.display = '';
    startStatsPolling();
    showOverlay('none');
    setStatus('Session active ✓', 'connected', 'connected');
  };

  // ── ICE candidate relay ──────────────────────────────────────────────────
  pc.onicecandidate = ({ candidate }) => {
    if (candidate && ws && ws.readyState === WebSocket.OPEN) {
      ws.send(JSON.stringify({
        type:          'ice',
        candidate:     candidate.candidate,
        sdpMLineIndex: candidate.sdpMLineIndex,
        sdpMid:        candidate.sdpMid,
      }));
    }
  };

  // ── ICE connection-state change → overlays ───────────────────────────────
  pc.oniceconnectionstatechange = () => {
    const s = pc.iceConnectionState;
    console.log('[sRemote] ICE state:', s);

    switch (s) {
      case 'checking':
        showOverlay('connecting', 'ICE negotiation in progress…');
        setStatus('ICE checking…', 'connected', 'warning');
        break;
      case 'connected':
      case 'completed':
        showOverlay('none');
        setStatus('Session active ✓', 'connected', 'connected');
        elIceDot.className = 'status-dot connected';
        break;
      case 'disconnected':
        // Transient interruption — may self-heal via TURN relay.
        showOverlay('relay');
        setStatus('Reconnecting to relay…', 'connected', 'warning');
        elIceDot.className = 'status-dot warning';
        break;
      case 'failed':
        showOverlay('error', 'WebRTC Connection Failed');
        setStatus('ICE failed', 'connected', 'error');
        elIceDot.className = 'status-dot error';
        break;
      case 'closed':
        showOverlay('idle');
        setStatus('Session ended', '', '');
        break;
    }
  };

  // ── Negotiate ────────────────────────────────────────────────────────────
  // Use the RTCSessionDescription constructor (spec-compliant, avoids plain-object lint).
  await pc.setRemoteDescription(new RTCSessionDescription({ type: 'offer', sdp: sdpString }));
  const answer = await pc.createAnswer();
  await pc.setLocalDescription(answer);

  ws.send(JSON.stringify({ type: 'answer', sdp: answer.sdp }));
  console.log('[sRemote] SDP answer sent');
  showOverlay('connecting', 'ICE negotiation in progress…');
}

// ─── Tear down ────────────────────────────────────────────────────────────────
function teardown(reason = 'Disconnected') {
  clearInterval(statsTimer);
  if (pc)  { pc.close();  pc  = null; }
  if (ws)  { ws.close();  ws  = null; }
  dataChannel = null;

  elVideo.srcObject = null;
  elToolbar.style.display    = 'none';
  elConnectBtn.style.display = '';
  elDisconnectBtn.style.display = 'none';
  removeInputCapture();
  showOverlay('idle');
  setStatus(reason, '', '');
}

function setupInputCapture() {
  elVideo.addEventListener('mousemove',  onMouseMove);
  elVideo.addEventListener('mousedown',  onMouseDown);
  elVideo.addEventListener('mouseup',    onMouseUp);
  elVideo.addEventListener('contextmenu', onContextMenu);
  elVideo.addEventListener('wheel',      onWheel, { passive: false });
  window.addEventListener('keydown',     onKeyDown);
  window.addEventListener('keyup',       onKeyUp);

  // Diagnostics
  elVideo.onwaiting = () => console.warn('[sRemote] Video element waiting (buffering/low data)');
  elVideo.onplaying = () => console.log('[sRemote] Video element playing');
  elVideo.onstalled = () => console.error('[sRemote] Video element stalled');
  elVideo.onresize  = () => console.log('[sRemote] Video element resize:', elVideo.videoWidth, 'x', elVideo.videoHeight);
}

function removeInputCapture() {
  elVideo.removeEventListener('mousemove',  onMouseMove);
  elVideo.removeEventListener('mousedown',  onMouseDown);
  elVideo.removeEventListener('mouseup',    onMouseUp);
  elVideo.removeEventListener('contextmenu', onContextMenu);
  elVideo.removeEventListener('wheel',      onWheel);
  window.removeEventListener('keydown',     onKeyDown);
  window.removeEventListener('keyup',       onKeyUp);
}

/** Convert a browser mouse event coordinate to the remote machine's
 *  absolute pixel coordinate, accounting for the video element scale. */
function scaleCoords(e) {
  const rect    = elVideo.getBoundingClientRect();
  
  // Bound the event to the actual video bounds to prevent out-of-bounds clicks
  const clientX = Math.max(rect.left, Math.min(rect.right, e.clientX));
  const clientY = Math.max(rect.top, Math.min(rect.bottom, e.clientY));

  const scaleX  = (remoteNativeWidth || 1920) / rect.width;
  const scaleY  = (remoteNativeHeight || 1080) / rect.height;
  
  return {
    x: Math.round((clientX - rect.left) * scaleX),
    y: Math.round((clientY - rect.top)  * scaleY),
  };
}

function sendCtrl(obj) {
  if (dataChannel && dataChannel.readyState === 'open') {
    dataChannel.send(JSON.stringify(obj));
  }
}

function mouseButtonName(btn) {
  return ['left', 'middle', 'right'][btn] || 'left';
}

function onMouseMove(e) {
  const { x, y } = scaleCoords(e);
  sendCtrl({ action: 'mousemove', x, y });
}

function onMouseDown(e) {
  const { x, y } = scaleCoords(e);
  sendCtrl({ action: 'mousedown', x, y, button: mouseButtonName(e.button) });
}

function onMouseUp(e) {
  const { x, y } = scaleCoords(e);
  sendCtrl({ action: 'mouseup', x, y, button: mouseButtonName(e.button) });
}

function onContextMenu(e) {
  e.preventDefault(); // block browser right-click; already sent as mousedown/up
}

function onWheel(e) {
  e.preventDefault();
  // Scale raw deltaY into a bounded [-10, 10] range rather than discarding the
  // magnitude with Math.sign — keeps trackpad momentum feel natural.
  const delta = Math.round(Math.max(-10, Math.min(10, e.deltaY / 40)));
  sendCtrl({ action: 'scroll', delta_y: delta || Math.sign(e.deltaY) });
}

function isInputFocused() {
  return ['INPUT', 'SELECT', 'TEXTAREA'].includes(document.activeElement.tagName);
}

function onKeyDown(e) {
  if (isInputFocused()) return;
  // Let the browser handle its own shortcuts:
  //   Ctrl+Shift+* (DevTools, inspector, etc.)
  //   Ctrl+Alt+*   (system combos on some platforms)
  //   Ctrl+[common navigation keys]
  if (e.ctrlKey && e.shiftKey) return;
  if (e.ctrlKey && e.altKey)   return;
  if (e.ctrlKey && ['t','w','r','l','n','f','p','u'].includes(e.key.toLowerCase())) return;
  e.preventDefault();
  sendCtrl({ action: 'keydown', key: e.key });
}

function onKeyUp(e) {
  if (isInputFocused()) return;
  if (e.ctrlKey && e.shiftKey) return;
  if (e.ctrlKey && e.altKey)   return;
  if (e.ctrlKey && ['t','w','r','l','n','f','p','u'].includes(e.key.toLowerCase())) return;
  e.preventDefault();
  sendCtrl({ action: 'keyup', key: e.key });
}

// ─── Stats polling ────────────────────────────────────────────────────────────
function startStatsPolling() {
  lastReport = { timestamp: performance.now(), bytesReceived: 0, framesDecoded: 0, framesDropped: 0 };
  
  statsTimer = setInterval(async () => {
    if (!pc) return;
    try {
      const stats = await pc.getStats();
      stats.forEach(report => {
        if (report.type === 'inbound-rtp' && report.kind === 'video') {
          const now = performance.now();
          const dt  = (now - lastReport.timestamp) / 1000; // seconds
          
          if (dt > 0) {
            const bytesReceived = report.bytesReceived || 0;
            const framesDecoded = report.framesDecoded || 0;
            const framesDropped = report.framesDropped || 0;
            
            const bps  = (bytesReceived - lastReport.bytesReceived) * 8 / dt;
            const kbps = Math.round(bps / 1000);
            const fps  = Math.round((framesDecoded - lastReport.framesDecoded) / dt);
            
            elQualityVal.textContent = `${kbps} kbps | ${fps} fps`;
            
            if (framesDecoded > lastReport.framesDecoded) {
              // We are successfully decoding!
              if (kbps > 0) {
                // Heartbeat log every few seconds to keep console clean but informative
                if (Math.round(now / 1000) % 5 === 0) {
                  console.log(`[sRemote] Stats: ${kbps} kbps, ${fps} fps, total decoded: ${framesDecoded}, dropped: ${framesDropped}`);
                }
              }
            } else if (bytesReceived > lastReport.bytesReceived) {
              console.warn(`[sRemote] Receiving data (${kbps} kbps) but NO frames are decoding. This usually indicates a codec/profile mismatch.`);
            } else {
              // Both bytes and frames are not advancing
              if (now - lastReport.timestamp > 5000) {
                 elStatusText.textContent = 'Session active ✓ (Waiting for video data...)';
                 elStatusText.classList.add('pulse');
              }
            }

            lastReport = { timestamp: now, bytesReceived, framesDecoded, framesDropped };
          }
        }
        if (report.type === 'candidate-pair' && report.state === 'succeeded') {
          elLatencyVal.textContent = `${Math.round(report.currentRoundTripTime * 1000)} ms`;
        }
      });
    } catch (e) { console.error('[sRemote] Stats error:', e); }
  }, 2000);
}

// ─── Fullscreen ───────────────────────────────────────────────────────────────
$('btn-fullscreen').addEventListener('click', () => {
  const vp = $('viewport');
  if (!document.fullscreenElement) {
    vp.requestFullscreen().catch(console.warn);
  } else {
    document.exitFullscreen();
  }
});

// ─── Quality Selector ─────────────────────────────────────────────────────────
$('quality-selector').addEventListener('change', (e) => {
  const mode = e.target.value;
  let width = null;
  let height = null;
  if (mode === 'fit') {
    width = window.innerWidth;
    height = window.innerHeight;
  }
  else if (mode === '1080p') { width = 1920; height = 1080; }
  else if (mode === '720p') { width = 1280; height = 720; }
  else if (mode === '480p') { width = 854;  height = 480; }
  
  sendCtrl({ action: 'resize', width, height });
});
