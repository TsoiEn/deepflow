// Asserts both sidecar paths: a good diagram returns a PNG, a broken one
// returns an error carrying the line number the editor points at.
import { spawn } from 'node:child_process';
import { createInterface } from 'node:readline';
import assert from 'node:assert/strict';

const child = spawn('node', [new URL('render.mjs', import.meta.url).pathname], {
  stdio: ['pipe', 'pipe', 'inherit'],
});
const replies = createInterface({ input: child.stdout })[Symbol.asyncIterator]();
const ask = async (src) => {
  child.stdin.write(JSON.stringify({ src }) + '\n');
  return JSON.parse((await replies.next()).value);
};

const good = await ask('flowchart TD\n  A[Start] --> B[End]');
assert.equal(good.ok, true, `expected a render, got ${JSON.stringify(good)}`);
assert.ok(
  Buffer.from(good.png, 'base64').subarray(0, 8).equals(Buffer.from('89504e470d0a1a0a', 'hex')),
  'reply was not a PNG',
);

const bad = await ask('flowchart TD\n  A[Start] -->\n  %%%(((');
assert.equal(bad.ok, false, `expected a parse error, got ${JSON.stringify(bad)}`);
assert.equal(typeof bad.line, 'number', `no line number in ${JSON.stringify(bad)}`);

// A render after an error must still work: the page is reused, errors must not poison it.
assert.equal((await ask('flowchart LR\n  X --> Y')).ok, true, 'page broken after an error');

child.stdin.end();
console.log(`ok — png ${Buffer.from(good.png, 'base64').length}b, error on line ${bad.line}, recovered`);
