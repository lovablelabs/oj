// SPDX-License-Identifier: MIT
import assert from 'node:assert/strict';
import { spawn } from 'node:child_process';
import { existsSync, readFileSync } from 'node:fs';
import { join } from 'node:path';
import { once } from 'node:events';
import { repo, tmpProject } from './unit/harness.mjs';

const fx = tmpProject({ prefix: 'oj-config-adoption-', linkEsbuild: true });
let child;
let log = '';
const waitFor = async (predicate) => {
  for (let i = 0; i < 200; i++) {
    if (await predicate()) return;
    if (child.exitCode !== null) throw new Error(log);
    await new Promise(r => setTimeout(r, 50));
  }
  throw new Error(`timed out\n${log}`);
};
try {
  fx.write('index.html', '<html><head></head><body></body></html>');
  fx.write('src/warm.js', 'export const warmed = 1;');
  fx.write('src/skip.js', 'export const skipped = 1;');
  fx.write('secret.private', 'private-content');
  fx.write('vite.config.mjs', `
    import { appendFileSync } from 'node:fs';
    export default {
      server: { fs: { deny: ['**/*.private'] }, warmup: { clientFiles: ['./src/*.js', '!./src/skip.js'] } },
      plugins: [{ name: 'warmup-probe', transform(code, id) {
        if (id.endsWith('/warm.js') || id.endsWith('/skip.js')) appendFileSync(${JSON.stringify(join(fx.root, 'transformed'))}, id + '\\n');
        return null;
      } }]
    };
  `);
  child = spawn(join(repo, 'target/debug/oj'), ['dev', fx.root, '--port', '15391', '--lazy'], { stdio: ['ignore', 'pipe', 'pipe'] });
  child.stdout.on('data', d => { log += d; });
  child.stderr.on('data', d => { log += d; });
  await waitFor(async () => { try { return (await fetch('http://127.0.0.1:15391/')).ok; } catch { return false; } });
  assert.equal((await fetch('http://127.0.0.1:15391/secret.private')).status, 403);
  await waitFor(() => existsSync(join(fx.root, 'transformed')));
  const transformed = readFileSync(join(fx.root, 'transformed'), 'utf8');
  assert.match(transformed, /warm\.js/);
  assert.doesNotMatch(transformed, /skip\.js/);
  console.log('Vite config: deny rules and lazy warmup honored');
} finally {
  if (child && child.exitCode === null) { const exited = once(child, 'exit'); child.kill('SIGTERM'); await exited; }
  fx.cleanup();
}
