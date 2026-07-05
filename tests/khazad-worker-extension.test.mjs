import test from 'node:test';
import assert from 'node:assert/strict';
import net from 'node:net';
import os from 'node:os';
import path from 'node:path';
import { mkdtemp, readFile, rm } from 'node:fs/promises';
import khazadWorkerExtension from '../extensions/khazad-worker/index.js';

function registerExtension() {
	let tool;
	const commands = new Map();
	const events = new Map();
	khazadWorkerExtension({
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

test('package ships only the worker extension', async () => {
	const pkg = JSON.parse(await readFile(new URL('../package.json', import.meta.url), 'utf8'));

	const removedMonitorExtension = ['khazad', 'monitor'].join('-');

	assert.deepEqual(pkg.pi.extensions, ['./extensions/khazad-worker']);
	assert.ok(!pkg.pi.extensions.some((entry) => entry.includes(removedMonitorExtension)));
	assert.ok(!(pkg.scripts['check:extension'] || '').includes(removedMonitorExtension));
	assert.ok(!(pkg.scripts['test:extension'] || '').includes(removedMonitorExtension));
});

test('khazad worker extension registers the ask_operator tool', () => {
	const tool = registerTool();

	assert.equal(tool.name, 'ask_operator');
	assert.equal(tool.parameters.required[0], 'question');
	assert.match(tool.description, /operator/i);
});

function fakeCtx() {
	const calls = [];
	return {
		calls,
		ctx: {
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

async function waitFor(predicate) {
	for (let attempt = 0; attempt < 50; attempt += 1) {
		if (predicate()) return;
		await new Promise((resolve) => setTimeout(resolve, 5));
	}
	throw new Error('timed out waiting for condition');
}

test('khazad feedback adapter registers explicit attach commands', () => {
	const { commands, events } = registerExtension();

	assert.ok(commands.has('khazad-attach'));
	assert.ok(commands.has('khazad-detach'));
	assert.ok(events.has('session_shutdown'));
});

test('khazad attach renders daemon feed and shutdown clears it', async () => {
	const { commands, events } = registerExtension();
	const tempDir = await mkdtemp(path.join(os.tmpdir(), 'khazad-feedback-test-'));
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
		assert.deepEqual(calls.at(-2), { type: 'widget', key: 'khazad-doom', lines: undefined });
		assert.deepEqual(calls.at(-1), { type: 'status', key: 'khazad-doom', text: undefined });
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

test('ask_operator degrades cleanly outside a daemon worker environment', async () => {
	const tool = registerTool();

	const result = await withEnv(
		{
			KHAZAD_DAEMON_SOCKET: undefined,
			KHAZAD_RUN_ID: undefined,
			KHAZAD_SLICE_ID: undefined,
			KHAZAD_WORKER_TOKEN: undefined,
		},
		() => tool.execute('tool-call', { question: 'Proceed?' }),
	);

	assert.equal(result.details.available, false);
	assert.equal(result.details.answer, '');
	assert.match(result.content[0].text, /channel unavailable/);
});

test('ask_operator forwards bounded questions to the daemon socket', async () => {
	const tool = registerTool();
	const tempDir = await mkdtemp(path.join(os.tmpdir(), 'khazad-worker-test-'));
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
		assert.equal(result.details.question_id, 'q-1');
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
