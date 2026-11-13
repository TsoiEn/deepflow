// Holds one Puppeteer page alive and renders mermaid in it on demand.
// Protocol: one JSON object per line, in and out.
//   in   {"src": "flowchart TD\n A-->B"}
//   out  {"ok": true, "png": "<base64>"}
//        {"ok": false, "error": "Parse error ...", "line": 3}
// Booting Chrome costs ~700ms once; each render after that is ~85ms.
import puppeteer from 'puppeteer-core';
import { createRequire } from 'node:module';
import { createInterface } from 'node:readline';

const MERMAID = createRequire(import.meta.url).resolve('mermaid/dist/mermaid.min.js');

const browser = await puppeteer.launch({ channel: 'chrome', headless: true });
const page = await browser.newPage();
await page.setViewport({ width: 1600, height: 1200, deviceScaleFactor: 2 });
await page.setContent('<body style="margin:0;background:#fff"><div id="out"></div></body>');
await page.addScriptTag({ path: MERMAID });
await page.evaluate(() =>
  window.mermaid.initialize({ startOnLoad: false, suppressErrorRendering: true }),
);

let seq = 0;

async function render(src) {
  const id = `d${seq++}`;
  const err = await page.evaluate(
    async (src, id) => {
      try {
        const { svg } = await window.mermaid.render(id, src);
        document.getElementById('out').innerHTML = svg;
        return null;
      } catch (e) {
        return String(e?.message ?? e);
      }
    },
    src,
    id,
  );
  if (err !== null) {
    // Mermaid says "Parse error on line 3:"; it is the only line info we get.
    const m = err.match(/on line (\d+)/) ?? err.match(/line (\d+)/);
    return { ok: false, error: err, ...(m ? { line: Number(m[1]) } : {}) };
  }
  const el = await page.$('#out svg');
  if (!el) return { ok: false, error: 'mermaid produced no svg' };
  const png = await el.screenshot({ type: 'png' });
  // screenshot() hands back a Uint8Array, whose toString('base64') is not Buffer's.
  return { ok: true, png: Buffer.from(png).toString('base64') };
}

// Renders are serialised: concurrent page.evaluate calls would fight over #out.
let chain = Promise.resolve();
const rl = createInterface({ input: process.stdin });

rl.on('line', (line) => {
  if (!line.trim()) return;
  chain = chain.then(async () => {
    let reply;
    try {
      reply = await render(JSON.parse(line).src ?? '');
    } catch (e) {
      reply = { ok: false, error: String(e?.message ?? e) };
    }
    process.stdout.write(JSON.stringify(reply) + '\n');
  });
});

rl.on('close', async () => {
  await chain;
  await browser.close();
  process.exit(0);
});
