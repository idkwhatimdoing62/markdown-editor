import assert from 'node:assert/strict';
import { spawn } from 'node:child_process';
import { mkdtemp, readFile } from 'node:fs/promises';
import { tmpdir } from 'node:os';
import { join } from 'node:path';

// Exercise the production scroll bridge in an isolated Chromium DOM, including
// the markers that Markdown emits inside raw HTML <details> containers.
const source = await readFile(new URL('../src/web_preview.rs', import.meta.url), 'utf8');
const script = source.match(/const SCROLL_SYNC_SCRIPT: &str = r#"([\s\S]*?)"#;/)?.[1];
assert.ok(script, 'production scroll bridge exists');
const profile = await mkdtemp(join(tmpdir(), 'markdown-details-scroll-'));
const child = spawn('C:/Program Files (x86)/Microsoft/Edge/Application/msedge.exe', [
  '--headless=new', '--remote-debugging-port=0', `--user-data-dir=${profile}`,
  '--no-first-run', '--disable-gpu', '--window-size=1200,800', 'about:blank',
], { stdio: ['ignore', 'ignore', 'pipe'], windowsHide: true });
let browserErrors = '';
child.stderr.on('data', (data) => { browserErrors += data.toString(); });
const delay = (ms) => new Promise((resolve) => setTimeout(resolve, ms));
let socket;
try {
  let page;
  for (let attempt = 0; attempt < 100; attempt++) {
    try {
      const port = (await readFile(join(profile, 'DevToolsActivePort'), 'utf8')).split('\n')[0];
      const pages = await fetch(`http://127.0.0.1:${port}/json/list`).then((r) => r.json());
      page = pages.find((entry) => entry.type === 'page');
      if (page) break;
    } catch { /* Wait for this test browser's startup. */ }
    await delay(50);
  }
  assert.ok(page, 'isolated test browser starts');
  socket = new WebSocket(page.webSocketDebuggerUrl);
  await new Promise((resolve, reject) => {
    socket.addEventListener('open', resolve, { once: true });
    socket.addEventListener('error', reject, { once: true });
  });
  const result = await new Promise((resolve, reject) => {
    const timeout = setTimeout(() => reject(new Error('browser regression timed out')), 25000);
    socket.addEventListener('close', (event) => {
      clearTimeout(timeout);
      reject(new Error(`test browser disconnected: ${event.code} ${event.reason}\n${browserErrors.slice(-5000)}`));
    }, { once: true });
    socket.addEventListener('message', (event) => {
      const reply = JSON.parse(event.data);
      if (reply.id !== 1) return;
      clearTimeout(timeout);
      if (reply.error || reply.result.exceptionDetails) reject(reply.error || reply.result.exceptionDetails);
      else resolve(reply.result.result.value);
    });
    socket.send(JSON.stringify({ id: 1, method: 'Runtime.evaluate', params: {
      awaitPromise: true, returnByValue: true,
      expression: `(${browserTest})(${JSON.stringify(script)})`,
    } }));
  });
  console.log(JSON.stringify(result, null, 2));
  assert.deepEqual(result.failures, [], 'details scroll mapping must remain monotonic and skip hidden blocks');
} finally {
  socket?.close();
  child.kill();
}

async function browserTest(script) {
  const marker = (line, id) => `<!--md-editor-source:${line}--><!--md-editor-block:${id}-->`;
  document.body.innerHTML = `
    <style>body { margin: 0; } h1,p,summary,blockquote,hr { margin:0; }
    h1 { height:80px; } summary { height:40px; } p { height:200px; }
    blockquote { height:240px; } hr { height:20px; }</style>
    ${marker(0, '1')}<p style="height:900px">前文</p>
    ${marker(10, '2')}<h1>第 4 章</h1>
    ${marker(12, '3')}<blockquote>引用</blockquote>
    ${marker(20, '4')}<details id="notes"><summary>备词</summary>
      ${marker(24, '5')}<p>被折叠的多行列表</p>
      ${marker(40, '6')}<p>更多备词</p>
      ${marker(60, '7')}</details>
    ${marker(62, '8')}<hr>
    ${marker(64, '9')}<h1>第 5 章</h1>
    ${marker(66, '10')}<blockquote>引用</blockquote>
    ${marker(74, '11')}<details id="nested"><summary>第二段备词</summary>
      ${marker(76, '12')}<details><summary>嵌套备词</summary>
        ${marker(80, '13')}<p>嵌套内容</p>${marker(90, '14')}</details>
      ${marker(94, '15')}</details>
    ${marker(96, '16')}<hr>
    ${marker(98, '17')}<h1>第 6 章</h1>
    ${marker(100, '18')}<p style="height:1400px">后文</p>
    <!--md-editor-source:150-->`;
  const messages = [];
  window.ipc = { postMessage(message) { messages.push(message); } };
  eval(script);
  const settle = () => new Promise((resolve) => requestAnimationFrame(() => requestAnimationFrame(resolve)));
  await settle();
  const failures = [];
  const measurements = [];
  for (const open of [false, true, false]) {
    for (const details of document.querySelectorAll('details')) details.open = open;
    await settle();
    const positions = [];
    for (const line of [10, 12, 20, 24, 32, 40, 60, 62, 64, 66, 74, 80, 90, 94, 96, 98]) {
      window.__mdEditorSetSourcePosition(line, false);
      positions.push({ line, y: scrollY });
    }
    for (let index = 1; index < positions.length; index++) {
      if (positions[index].y < positions[index - 1].y - 1) {
        failures.push({ kind: 'source-to-preview-backwards', open, before: positions[index - 1], after: positions[index] });
      }
    }
    const endY = Math.max(0, document.documentElement.scrollHeight - window.innerHeight);
    for (const smooth of [false, true]) {
      window.scrollTo(0, 0);
      window.__mdEditorScrollToEnd(smooth);
      for (let frame = 0; smooth && frame < 60 && Math.abs(scrollY - endY) > 0.5; frame++) await settle();
      if (Math.abs(scrollY - endY) > 0.5) failures.push({ kind: 'end-position', open, smooth, y: scrollY, endY });
    }
    if (!open) {
      const notes = document.getElementById('notes');
      const top = notes.getBoundingClientRect().top + scrollY;
      const bottom = notes.getBoundingClientRect().bottom + scrollY;
      window.__mdEditorSetBlockAnchor('5', 0.5, 32, false);
      if (scrollY < top - 1 || scrollY > bottom + 1) failures.push({ kind: 'hidden-block-target', y: scrollY, top, bottom });
    }
    let previous = -1;
    for (let y = 900; y <= 1800; y += 75) {
      window.scrollTo(0, y);
      window.dispatchEvent(new Event('scroll'));
      await settle();
      const message = messages.filter((value) => /^md-(anchor|source):/.test(value)).at(-1);
      const line = Number(message?.split(':').at(-1));
      if (line < previous) failures.push({ kind: 'preview-to-source-backwards', open, y, previous, line });
      if (!open && /^md-anchor:/.test(message) && ['5', '6', '7', '12', '13', '14', '15'].includes(message.split(':')[2])) {
        failures.push({ kind: 'reported-hidden-block', y, message });
      }
      previous = line;
    }
    measurements.push({ open, positions });
  }
  return { failures, measurements };
}
