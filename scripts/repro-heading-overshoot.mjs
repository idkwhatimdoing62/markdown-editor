import { spawn } from "node:child_process";
import { readFileSync } from "node:fs";
import { join } from "node:path";

const source = readFileSync(new URL("../src/web_preview.rs", import.meta.url), "utf8");
const match = source.match(/const SCROLL_SYNC_SCRIPT: &str = r#"([\s\S]*?)"#;/);
if (!match) throw new Error("SCROLL_SYNC_SCRIPT not found");
const restoreRace = process.env.RESTORE_RACE === "1";
const disableContentVisibility = process.env.DISABLE_CONTENT_VISIBILITY === "1";
const actualHtml = process.env.ACTUAL_HTML
  ? readFileSync(process.env.ACTUAL_HTML, "utf8")
      .replace(/<meta http-equiv="Content-Security-Policy"[^>]*>/i, "")
      .replace(/<script defer src="mdfont:[\s\S]*?<\/script>/gi, "")
  : null;

const edge = "C:/Program Files (x86)/Microsoft/Edge/Application/msedge.exe";
const port = 9337;
const profile = join(process.env.TEMP, "markdown-editor-heading-repro");
const child = spawn(edge, [
  "--headless=new",
  `--remote-debugging-port=${port}`,
  `--user-data-dir=${profile}`,
  "--no-first-run",
  "--disable-gpu",
  "--window-size=1200,800",
  "about:blank",
], { stdio: "ignore" });

const delay = (ms) => new Promise((resolve) => setTimeout(resolve, ms));
let socket;
try {
  let page;
  for (let attempt = 0; attempt < 50; attempt += 1) {
    try {
      const pages = await fetch(`http://127.0.0.1:${port}/json/list`).then((response) => response.json());
      page = pages.find((candidate) => candidate.type === "page");
      if (page) break;
    } catch {}
    await delay(50);
  }
  if (!page) throw new Error("Edge CDP page not ready");

  socket = new WebSocket(page.webSocketDebuggerUrl);
  await new Promise((resolve, reject) => {
    socket.addEventListener("open", resolve, { once: true });
    socket.addEventListener("error", reject, { once: true });
  });
  let nextId = 0;
  const pending = new Map();
  socket.addEventListener("message", (event) => {
    const message = JSON.parse(event.data);
    const waiter = pending.get(message.id);
    if (waiter) {
      pending.delete(message.id);
      waiter(message);
    }
  });
  const evaluate = (expression, awaitPromise = false) => new Promise((resolve, reject) => {
    const id = ++nextId;
    pending.set(id, (message) => message.error ? reject(message.error) : resolve(message.result.result));
    socket.send(JSON.stringify({
      id,
      method: "Runtime.evaluate",
      params: { expression, awaitPromise, returnByValue: true },
    }));
  });

  const result = await evaluate(`(async () => {
    window.ipc = { postMessage() {} };
    if (${actualHtml ? "true" : "false"}) {
      document.open();
      document.write(${JSON.stringify(actualHtml || "")});
      document.close();
      await new Promise((resolve) => requestAnimationFrame(() => requestAnimationFrame(resolve)));
      window.ipc = { postMessage() {} };
      if (${disableContentVisibility ? "true" : "false"}) {
        const correction = document.createElement('style');
        correction.textContent = 'body > * { content-visibility: visible !important; contain-intrinsic-block-size: none !important; }';
        document.head.append(correction);
        await new Promise((resolve) => requestAnimationFrame(() => requestAnimationFrame(resolve)));
      }
    } else {
      document.body.style.margin = '0';
      for (let index = 0; index < 14; index += 1) {
        document.body.append(document.createComment('md-source:' + index));
        const heading = document.createElement('h2');
        heading.id = 'md-heading-' + index;
        heading.textContent = '第' + index + '章';
        heading.style.cssText = 'height:40px;margin:0';
        document.body.append(heading);
        const filler = document.createElement('div');
        filler.style.height = '700px';
        document.body.append(filler);
      }
    }
    eval(${JSON.stringify(match[1])});
    const allHeadings = Array.from(document.querySelectorAll('h1,h2,h3,h4,h5,h6'));
    const actualTarget = allHeadings.find((heading) => heading.textContent.trim().startsWith('10. 变更影响'));
    const actualNext = allHeadings.find((heading) => heading.textContent.trim().startsWith('11. 当前缺口'));
    const targetIndex = actualTarget ? Number(actualTarget.id.replace('md-heading-', '')) : 10;
    const nextIndex = actualNext ? Number(actualNext.id.replace('md-heading-', '')) : 11;
    if (${restoreRace ? "true" : "false"}) {
      window.name = 'md-source:' + nextIndex;
      window.dispatchEvent(new Event('load'));
    } else {
      window.scrollTo(0, 0);
    }
    await window.__mdEditorScrollHeading(targetIndex);
    await new Promise((resolve) => setTimeout(resolve, 1800));
    const target = document.getElementById('md-heading-' + targetIndex);
    const next = document.getElementById('md-heading-' + nextIndex);
    const targetTop = target.getBoundingClientRect().top;
    const nextTop = next.getBoundingClientRect().top;
    return {
      scrollY,
      targetIndex,
      nextIndex,
      targetText: target.textContent.trim(),
      nextText: next.textContent.trim(),
      targetTop,
      nextTop,
      overshot: Math.abs(nextTop) < Math.abs(targetTop),
      misaligned: Math.abs(targetTop) > 8
    };
  })()`, true);
  const value = result.value;
  console.log(JSON.stringify(value));
  if (value.misaligned) process.exitCode = 1;
} finally {
  socket?.close();
  child.kill();
}
