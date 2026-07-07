import test from 'node:test';
import assert from 'node:assert/strict';
import net from 'node:net';
import os from 'node:os';
import path from 'node:path';
import { chmod, mkdtemp, readFile, rm, writeFile } from 'node:fs/promises';
import khazadMonitorExtension from './index.js';

function registerExtension() {
	let tool;
	const commands = new Map();
	const events = new Map();
	khazadMonitorExtension({
		registerTool(registered) {
			tool = registered;
		},
		registerCommand(name, registered) {
			commands.set(name, registered);
		},
		on(name, handler) {
			events.set(name, handler);
		},
	});
	assert.ok(tool, 'expected ask_operator tool registration');
	return { tool, commands, events };
}

function registerTool() {
	return registerExtension().tool;
}

function fakeCtx(extra = {}) {
	const calls = [];
	return {
		calls,
		ctx: {
			...extra,
			ui: {
				notify(message, level) {
					calls.push({ type: 'notify', message, level });
				},
				setWidget(key, lines) {
					calls.push({ type: 'widget', key, lines });
				},
				setStatus(key, text) {
					calls.push({ type: 'status', key, text });
				},
			},
		},
	};
}

function withEnv(overrides, callback) {
	const previous = {};
	for (const [key, value] of Object.entries(overrides)) {
		previous[key] = process.env[key];
		if (value === undefined) delete process.env[key];
		else process.env[key] = value;
	}
	return Promise.resolve()
		.then(callback)
		.finally(() => {
			for (const [key, value] of Object.entries(previous)) {
				if (value === undefined) delete process.env[key];
				else process.env[key] = value;
			}
		});
}

async function waitFor(predicate) {
	for (let attempt = 0; attempt < 50; attempt += 1) {
		if (predicate()) return;
		await new Promise((resolve) => setTimeout(resolve, 5));
	}
	throw new Error('timed out waiting for condition');
}

test('package ships the thin monitor bridge extension', async () => {
	const pkg = JSON.parse(await readFile(new URL('../../package.json', import.meta.url), 'utf8'));

	assert.deepEqual(pkg.pi.extensions, ['./extensions/khazad-monitor']);
	assert.match(pkg.description, /daemon/i);
	assert.match(pkg.keywords.join(' '), /herdr/);
	assert.match(pkg.scripts['check:extension'], /extensions\/khazad-monitor\/index\.js/);
});

test('monitor bridge registers tools and explicit bridge commands', () => {
	const { tool, commands, events } = registerExtension();

	assert.equal(tool.name, 'ask_operator');
	assert.equal(tool.parameters.required[0], 'question');
	for (const command of ['khazad-attach', 'khazad-detach', 'khazad-explain', 'khazad-open', 'khazad-handoff', 'khazad-answer']) {
		assert.ok(commands.has(command), `missing ${command}`);
	}
	assert.ok(events.has('session_shutdown'));
});

test('khazad attach renders only daemon feed projection text', async () => {
	const { commands, events } = registerExtension();
	const tempDir = await mkdtemp(path.join(os.tmpdir(), 'khazad-monitor-test-'));
	const socketPath = path.join(tempDir, 'daemon.sock');
	let seenRequest;
	const server = net.createServer((socket) => {
		let buffer = '';
		socket.setEncoding('utf8');
		socket.on('data', (chunk) => {
			buffer += chunk;
			const idx = buffer.indexOf('\n');
			if (idx < 0) return;
			seenRequest = JSON.parse(buffer.slice(0, idx));
			socket.end(`${JSON.stringify({
				id: seenRequest.id,
				result: {
					run: { id: 'kd-run', status: 'running' },
					feed: {
						feed_version: 1,
						summary_line: 'Run running — worker active',
						attention: [{ text: 'answer required' }],
						blocks: [
							{ label: 'Run', meta: 'running', lines: [{ text: 'worker active' }] },
						],
					},
					events: [{ type: 'raw_event_text_that_must_not_render' }],
				},
			})}\n`);
		});
	});

	try {
		await new Promise((resolve, reject) => {
			server.once('error', reject);
			server.listen(socketPath, () => {
				server.off('error', reject);
				resolve();
			});
		});
		const { ctx, calls } = fakeCtx();

		await withEnv({ KHAZAD_DAEMON_SOCKET: socketPath }, async () => {
			await commands.get('khazad-attach').handler('kd-run', ctx);
			await events.get('session_shutdown')({ reason: 'reload' }, ctx);
		});

		assert.equal(seenRequest.method, 'status');
		assert.deepEqual(seenRequest.params, { run_id: 'kd-run', events_limit: 20 });
		const rendered = calls.find((call) => call.type === 'widget' && Array.isArray(call.lines));
		assert.ok(rendered.lines.some((line) => line.includes('Run running')));
		assert.ok(rendered.lines.some((line) => line.includes('worker active')));
		assert.ok(!rendered.lines.some((line) => line.includes('raw_event_text_that_must_not_render')));
		const status = calls.find((call) => call.type === 'status' && call.text);
		assert.match(status.text, /Run running/);
		assert.deepEqual(calls.at(-2), { type: 'widget', key: 'khazad-doom', lines: undefined });
		assert.deepEqual(calls.at(-1), { type: 'status', key: 'khazad-doom', text: undefined });
	} finally {
		await new Promise((resolve) => server.close(resolve));
		await rm(tempDir, { recursive: true, force: true });
	}
});

test('khazad open delegates Herdr focus to the daemon CLI command', async () => {
	const { commands } = registerExtension();
	const tempDir = await mkdtemp(path.join(os.tmpdir(), 'khazad-open-test-'));
	const logPath = path.join(tempDir, 'args.json');
	const binPath = path.join(tempDir, 'khazad-doom');
	await writeFile(
		binPath,
		`#!/usr/bin/env node\nconst fs = require('node:fs');\nfs.writeFileSync(${JSON.stringify(logPath)}, JSON.stringify(process.argv.slice(2)));\nprocess.stdout.write(JSON.stringify({opened:true, run_id:'kd-run', workspace_label:'Khazad-Doom kd-run', message:'focused existing Herdr cockpit workspace'}));\n`,
	);
	await chmod(binPath, 0o755);
	const { ctx, calls } = fakeCtx({ cwd: '/repo' });

	try {
		await withEnv({ KHAZAD_DOOM_BIN: binPath }, () => commands.get('khazad-open').handler('kd-run', ctx));

		assert.deepEqual(JSON.parse(await readFile(logPath, 'utf8')), ['cockpit', 'open', '--run', 'kd-run']);
		assert.ok(calls.some((call) => call.type === 'notify' && /Herdr cockpit/.test(call.message)));
	} finally {
		await rm(tempDir, { recursive: true, force: true });
	}
});

test('ask_operator forwards bounded questions to daemon state', async () => {
	const tool = registerTool();
	const tempDir = await mkdtemp(path.join(os.tmpdir(), 'khazad-monitor-worker-test-'));
	const socketPath = path.join(tempDir, 'daemon.sock');
	let seenRequest;
	const server = net.createServer((socket) => {
		let buffer = '';
		socket.setEncoding('utf8');
		socket.on('data', (chunk) => {
			buffer += chunk;
			const idx = buffer.indexOf('\n');
			if (idx < 0) return;
			seenRequest = JSON.parse(buffer.slice(0, idx));
			socket.end(`${JSON.stringify({ id: seenRequest.id, result: { question_id: 'q-1', answer: 'yes' } })}\n`);
		});
	});

	try {
		await new Promise((resolve, reject) => {
			server.once('error', reject);
			server.listen(socketPath, () => {
				server.off('error', reject);
				resolve();
			});
		});

		const result = await withEnv(
			{
				KHAZAD_DAEMON_SOCKET: socketPath,
				KHAZAD_RUN_ID: 'kd-run',
				KHAZAD_SLICE_ID: 'slice-001',
				KHAZAD_WORKER_TOKEN: 'secret-token',
				KHAZAD_ATTEMPT: '2',
			},
			() => tool.execute('tool-call', { question: 'Proceed?', options: ['yes', 'no'], timeout_seconds: 30 }),
		);

		assert.equal(result.details.available, true);
		assert.equal(result.details.answer, 'yes');
		assert.equal(seenRequest.method, 'workerAsk');
		assert.deepEqual(seenRequest.params, {
			run_id: 'kd-run',
			slice_id: 'slice-001',
			token: 'secret-token',
			attempt: 2,
			question: 'Proceed?',
			options: ['yes', 'no'],
			timeout_seconds: 30,
		});
	} finally {
		await new Promise((resolve) => server.close(resolve));
		await rm(tempDir, { recursive: true, force: true });
	}
});

test('khazad attach ignores delayed daemon responses after shutdown', async () => {
	const { commands, events } = registerExtension();
	const tempDir = await mkdtemp(path.join(os.tmpdir(), 'khazad-feedback-stale-test-'));
	const socketPath = path.join(tempDir, 'daemon.sock');
	let reply;
	const server = net.createServer((socket) => {
		let buffer = '';
		socket.setEncoding('utf8');
		socket.on('data', (chunk) => {
			buffer += chunk;
			const idx = buffer.indexOf('\n');
			if (idx < 0) return;
			const request = JSON.parse(buffer.slice(0, idx));
			reply = () => socket.end(`${JSON.stringify({
				id: request.id,
				result: {
					run: { id: 'kd-run', status: 'running' },
					feed: { feed_version: 1, summary_line: 'late feed', attention: [], blocks: [] },
				},
			})}\n`);
		});
	});

	try {
		await new Promise((resolve, reject) => {
			server.once('error', reject);
			server.listen(socketPath, () => {
				server.off('error', reject);
				resolve();
			});
		});
		const { ctx, calls } = fakeCtx();
		const attachPromise = withEnv({ KHAZAD_DAEMON_SOCKET: socketPath }, () =>
			commands.get('khazad-attach').handler('kd-run', ctx),
		);
		await waitFor(() => typeof reply === 'function');

		await events.get('session_shutdown')({ reason: 'reload' }, ctx);
		const callCountAfterShutdown = calls.length;
		reply();
		await attachPromise;

		assert.equal(calls.length, callCountAfterShutdown);
	} finally {
		await new Promise((resolve) => server.close(resolve));
		await rm(tempDir, { recursive: true, force: true });
	}
});
