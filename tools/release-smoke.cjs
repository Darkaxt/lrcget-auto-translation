const assert = require('node:assert/strict');
const fs = require('node:fs');
const path = require('node:path');
const { spawn, execFileSync } = require('node:child_process');
const { readEndpoint } = require('./release-smoke-endpoint.cjs');

// Known-folder SQLite paths cannot be isolated with APPDATA overrides on Windows.
// Run only on disposable GitHub-hosted Windows runners, never on a user's machine.
assert.equal(process.env.GITHUB_ACTIONS, 'true');
assert.equal(process.env.RUNNER_ENVIRONMENT, 'github-hosted');
assert.equal(process.platform, 'win32');
const binary = path.resolve(process.argv[2]);
assert.ok(fs.existsSync(binary), `Missing executable: ${binary}`);
const output = path.join(process.env.RUNNER_TEMP, 'lrcget-startup-smoke');
const profile = path.join(output, 'webview');
const portFile = path.join(profile, 'EBWebView', 'DevToolsActivePort');
fs.mkdirSync(path.dirname(portFile), { recursive: true });
const watcher = fs.watch(path.dirname(portFile));
const log = fs.openSync(path.join(output, 'startup.log'), 'w');
// Elevated WebView2 150+ ignores environment-based debugging arguments.
// This app-specific policy exists only for the disposable hosted-runner probe.
const windowsHelper = path.join(__dirname, 'release-smoke-windows.ps1');
const windowsArgs = ['-NoProfile', '-NonInteractive', '-File', windowsHelper];
execFileSync('powershell.exe', [...windowsArgs, '-Action', 'Configure'], { windowsHide: true });
const child = spawn(binary, [], {
  windowsHide: true,
  stdio: ['ignore', log, log],
  env: {
    ...process.env,
    WEBVIEW2_USER_DATA_FOLDER: profile,
    WEBVIEW2_ADDITIONAL_BROWSER_ARGUMENTS: '--remote-debugging-port=0',
  },
});
console.log(`Started isolated native probe, PID ${child.pid}`);
const exited = new Promise(resolve => child.once('exit', resolve));
let socket;
let discoverySocket;
async function main() {
  const endpoint = await new Promise((resolve, reject) => {
    const check = () => {
      try {
        const value = readEndpoint(portFile);
        if (value) resolve(value);
      } catch (error) {
        reject(error);
      }
    };
    watcher.on('change', check);
    watcher.once('error', reject);
    child.once('error', reject);
    child.once('exit', code => reject(new Error(`Exited before WebView readiness: ${code}`)));
    check();
  });
  const { port, browserPath } = endpoint;
  watcher.close();
  console.log(`WebView debugging endpoint ready on port ${port}`);
  // The browser endpoint precedes page creation/navigation. Wait for CDP's
  // actual target event instead of racing a one-shot /json/list request.
  discoverySocket = new WebSocket(`ws://127.0.0.1:${port}${browserPath}`);
  await new Promise((resolve, reject) => {
    discoverySocket.addEventListener('open', resolve, { once: true });
    discoverySocket.addEventListener('error', reject, { once: true });
  });
  await new Promise((resolve, reject) => {
    const onExit = code => reject(new Error(`Exited before application page readiness: ${code}`));
    child.once('exit', onExit);
    discoverySocket.addEventListener('close', () => reject(new Error('Target discovery closed before readiness')), { once: true });
    discoverySocket.addEventListener('message', event => {
      const message = JSON.parse(event.data);
      if (message.error) reject(new Error(JSON.stringify(message.error)));
      const target = message.params?.targetInfo;
      if (target) console.log(`WebView target: ${target.type} ${target.url}`);
      if (target?.type === 'page' && target.url === 'https://tauri.localhost/') {
        child.removeListener('exit', onExit);
        resolve();
      }
    });
    discoverySocket.send(JSON.stringify({ id: 1, method: 'Target.setDiscoverTargets', params: { discover: true } }));
  });
  const response = await fetch(`http://127.0.0.1:${port}/json/list`);
  assert.ok(response.ok);
  const targets = await response.json();
  const target = targets.find(item => item.url === 'https://tauri.localhost/');
  assert.ok(target, 'Application WebView target missing');
  socket = new WebSocket(target.webSocketDebuggerUrl);
  await new Promise((resolve, reject) => {
    socket.addEventListener('open', resolve, { once: true });
    socket.addEventListener('error', reject, { once: true });
  });
  discoverySocket.close();
  let id = 0;
  const pending = new Map();
  const errors = [];
  socket.addEventListener('message', event => {
    const message = JSON.parse(event.data);
    if (message.method === 'Runtime.exceptionThrown' ||
        (message.method === 'Runtime.consoleAPICalled' && message.params.type === 'error')) {
      errors.push(message.params);
      for (const request of pending.values()) request.reject(new Error(JSON.stringify(message.params)));
      pending.clear();
    }
    const request = pending.get(message.id);
    if (request) {
      pending.delete(message.id);
      if (message.error) request.reject(new Error(JSON.stringify(message.error)));
      else request.resolve(message.result);
    }
  });
  socket.addEventListener('close', () => {
    for (const request of pending.values()) request.reject(new Error('WebView closed during verification'));
    pending.clear();
  });
  const send = (method, params = {}) => new Promise((resolve, reject) => {
    const next = ++id;
    pending.set(next, { resolve, reject });
    socket.send(JSON.stringify({ id: next, method, params }));
  });
  await send('Runtime.enable');
  console.log('Attached to application WebView; waiting for initial UI');
  const result = await send('Runtime.evaluate', {
    awaitPromise: true,
    returnByValue: true,
    expression: `new Promise(resolve => {
      const ready = () => {
        const buttons = [...document.querySelectorAll('button')].map(b => b.textContent.trim());
        if (!buttons.includes('Add new directory') || !buttons.includes('Continue')) return;
        observer.disconnect();
        resolve({ title: document.title, buttons, url: location.href });
      };
      const observer = new MutationObserver(ready);
      observer.observe(document, { childList: true, subtree: true });
      ready();
    })`,
  });
  assert.equal(result.exceptionDetails, undefined);
  assert.equal(result.result.value.title, 'LRCGET');
  assert.equal(errors.length, 0, JSON.stringify(errors));
  const screenshot = await send('Page.captureScreenshot');
  fs.writeFileSync(path.join(output, 'startup.png'), Buffer.from(screenshot.data, 'base64'));
  fs.writeFileSync(path.join(output, 'result.json'), JSON.stringify({ binary, ui: result.result.value, errors }, null, 2));
  console.log(JSON.stringify(result.result.value));
}
main().catch(error => {
  console.error(error);
  process.exitCode = 1;
}).finally(async () => {
  watcher.close();
  execFileSync('powershell.exe', [...windowsArgs, '-Action', 'Reset'], { windowsHide: true });
  if (socket) socket.close();
  if (discoverySocket) discoverySocket.close();
  if (child.exitCode === null) {
    execFileSync('powershell.exe', [...windowsArgs, '-Action', 'Close', '-OwnedProcessId', String(child.pid), '-BinaryPath', binary],
    { windowsHide: true });
    assert.equal(await exited, 0, 'Application exited unsuccessfully');
  }
  fs.closeSync(log);
});
