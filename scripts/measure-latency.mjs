import { readFile, writeFile } from 'node:fs/promises';
import { performance } from 'node:perf_hooks';
import { randomUUID } from 'node:crypto';
import os from 'node:os';

// Measures full responses, including image transfer/JSON parsing, without LLM or UI rendering.
const [url, patFile, output, mode = 'http'] = process.argv.slice(2);
if (!url || !patFile || !output) throw Error('Usage: node measure-latency.mjs URL PAT_FILE OUTPUT [stdio]');
const n = 50, warmup = 5;
let call, cleanup, session, current;
const started = performance.now();
if (mode === 'stdio') {
  const { Client } = await import('@modelcontextprotocol/sdk/client/index.js');
  const { StdioClientTransport } = await import('@modelcontextprotocol/sdk/client/stdio.js');
  const c = new Client({ name: 'latency-probe', version: '1' });
  await c.connect(new StdioClientTransport({ command: process.execPath, args: [new URL('../dist/server/cli.js', import.meta.url).pathname.replace(/^\/(\w:)/, '$1'), 'mcp', '--url', url, '--pat-file', patFile], stderr: 'pipe' }));
  call = (name, args) => c.callTool({ name, arguments: args });
  cleanup = () => c.close();
} else {
  const token = (await readFile(patFile, 'utf8')).trim();
  let transportId, id = 0;
  async function rpc(method, params, notification = false) {
    const r = await fetch(new URL('/mcp', url), { method: 'POST', headers: {
      Authorization: `Bearer ${token}`, 'Content-Type': 'application/json', Accept: 'application/json, text/event-stream',
      'MCP-Protocol-Version': '2025-03-26', ...(transportId ? { 'Mcp-Session-Id': transportId } : {}),
    }, body: JSON.stringify({ jsonrpc: '2.0', ...(notification ? {} : { id: ++id }), method, params }), signal: AbortSignal.timeout(15000) });
    transportId = r.headers.get('mcp-session-id') ?? transportId;
    const text = await r.text();
    if (!r.ok) throw Error(`HTTP ${r.status}: ${text.slice(0,200)}`);
    if (notification) return;
    const body = r.headers.get('content-type')?.includes('text/event-stream')
      ? text.split('\n').filter(l => l.startsWith('data:')).map(l => JSON.parse(l.slice(5))).find(v => v.id === id)
      : JSON.parse(text);
    if (!body || body.error) throw Error(JSON.stringify(body?.error ?? 'Missing response'));
    return body.result;
  }
  await rpc('initialize', { protocolVersion: '2025-03-26', capabilities: {}, clientInfo: { name: 'latency-probe', version: '1' } });
  await rpc('notifications/initialized', {}, true);
  call = (name, args) => rpc('tools/call', { name, arguments: args });
  cleanup = async () => { if (transportId) await fetch(new URL('/mcp', url), { method: 'DELETE', headers: { Authorization: `Bearer ${token}`, 'Mcp-Session-Id': transportId } }); };
}
const report = { date: new Date().toISOString(), host: os.hostname(), url, mode, samples: n, warmup, initialize_ms: performance.now() - started, measurements: {} };
function checked(r) {
  if (r.isError || !r.structuredContent?.ok) throw Error(JSON.stringify(r.structuredContent));
  if (r.structuredContent.frames?.length) current = r.structuredContent.frames.at(-1);
  return r;
}
const ref = () => ({ frame_id: current.frame_id, view_id: current.view_id, input_revision: current.input_revision });
async function measure(name, fn) {
  const times = [], sizes = [];
  for (let i = -warmup; i < n; i++) {
    const t = performance.now(); const r = checked(await fn()); const ms = performance.now() - t;
    if (i >= 0) { times.push(ms); sizes.push(r.content.filter(c => c.type === 'image').reduce((sum,c) => sum + Buffer.byteLength(c.data,'base64'),0)); }
  }
  const sorted = [...times].sort((a,b) => a-b), pct = p => sorted[Math.ceil(p*n)-1];
  report.measurements[name] = { p50_ms: pct(.5), p95_ms: pct(.95), p99_ms: pct(.99), min_ms: sorted[0], max_ms: sorted.at(-1), mean_ms: times.reduce((a,b)=>a+b)/n, mean_image_bytes: sizes.reduce((a,b)=>a+b)/n, samples_ms: times };
}
try {
  const devices = checked(await call('list_computers', {})).structuredContent.computers;
  const d = devices.find(d => d.driver_id === 'simulator' && !d.lease && !d.control_suspended);
  if (!d) throw Error('No free simulator; refusing hardware');
  const t = performance.now();
  const opened = checked(await call('open_computer', { computer_id: d.device_id, mode: 'control', request_id: randomUUID() }));
  session = opened.structuredContent.session.session_id;
  report.open_with_image_ms = performance.now() - t;
  report.device_id = d.device_id;
  await measure('list', () => call('list_computers', {}));
  await measure('fresh_screenshot', () => call('computer_screenshot', { session_id: session, max_age_ms: 0 }));
  await measure('cached_screenshot', () => call('computer_screenshot', { session_id: session, max_age_ms: 5000 }));
  await measure('click_and_image', () => call('computer', { session_id: session, request_id: randomUUID(), reference: ref(), actions: [{ type: 'click', x: 120, y: 160 }] }));
  await measure('five_moves_and_image', () => call('computer', { session_id: session, request_id: randomUUID(), reference: ref(), actions: Array.from({length:5},(_,i)=>({type:'move',x:150+i,y:180+i})) }));
} finally {
  if (session) checked(await call('close_computer', { session_id: session }));
  await cleanup();
}
await writeFile(output, JSON.stringify(report,null,2));
console.log(JSON.stringify({...report,measurements:Object.fromEntries(Object.entries(report.measurements).map(([k,{samples_ms,...v}])=>[k,v]))}));
