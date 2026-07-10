import test from 'node:test';
import assert from 'node:assert/strict';
import net from 'node:net';
import os from 'node:os';
import path from 'node:path';
import { mkdtemp, readFile, rm } from 'node:fs/promises';
import khazadWorkerExtension from '../extensions/khazad-worker/index.js';

function registerExtension() {
	const tools = new Map();
	const commands = new Map();
	const events = new Map();
	khazadWorkerExtension({
		registerTool(tool) {
			tools.set(tool.name, tool);
		},
		registerCommand(name, command) {
			commands.set(name, command);
		},
		on(name, handler) {
			events.set(name, handler);
		},
	});
	return { tools, commands, events };
}

async function withEnv(overrides, callback) {
	const previous = {};
	for (const [key, value] of Object.entries(overrides)) {
		previous[key] = process.env[key];
		if (value === undefined) delete process.env[key];
		else process.env[key] = value;
	}
	try {
		return await callback();
	} finally {
		for (const [key, value] of Object.entries(previous)) {
			if (value === undefined) delete process.env[key];
			else process.env[key] = value;
		}
	}
}

async function withDaemonServer(handler, fn) {
	const tempDir = await mkdtemp(path.join(os.tmpdir(), 'khazad-worker-extension-test-'));
	const socketPath = path.join(tempDir, 'daemon.sock');
	const server = net.createServer((socket) => {
		let buffer = '';
		socket.setEncoding('utf8');
		socket.on('data', (chunk) => {
			buffer += chunk;
			const index = buffer.indexOf('\n');
			if (index < 0) return;
			const request = JSON.parse(buffer.slice(0, index));
			try {
				const result = handler(request);
				socket.end(`${JSON.stringify({ id: request.id, result })}\n`);
			} catch (error) {
				socket.end(`${JSON.stringify({ id: request.id, error: error.message })}\n`);
			}
		});
	});
	await new Promise((resolve, reject) => {
		server.once('error', reject);
		server.listen(socketPath, () => {
			server.off('error', reject);
			resolve();
		});
	});
	try {
		return await fn(socketPath);
	} finally {
		await new Promise((resolve) => server.close(resolve));
		await rm(tempDir, { recursive: true, force: true });
	}
}

test('package keeps the worker extension per-attempt instead of globally registered', async () => {
	const pkg = JSON.parse(await readFile(new URL('../package.json', import.meta.url), 'utf8'));

	assert.deepEqual(pkg.pi.extensions, ['./extensions/khazad-monitor']);
	assert.ok(!pkg.pi.extensions.includes('./extensions/khazad-worker'));
});

test('khazad worker extension registers ask_operator and submit_worker_result tools', () => {
	const { tools, commands, events } = registerExtension();

	assert.ok(tools.has('ask_operator'));
	assert.ok(tools.has('submit_worker_result'));
	assert.ok(commands.has('khazad-attach'));
	assert.ok(commands.has('khazad-detach'));
	assert.ok(events.has('session_shutdown'));
});

test('ask_operator degrades cleanly outside a daemon worker environment', async () => {
	const tool = registerExtension().tools.get('ask_operator');

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

test('ask_operator uses same-pane Pi UI when available', async () => {
	const tool = registerExtension().tools.get('ask_operator');
	const requests = [];

	await withDaemonServer(
		(request) => {
			requests.push(request);
			if (request.method === 'workerAskOpen') return { question_id: 'q-1', state: 'pending', timeout_seconds: 30 };
			if (request.method === 'answerQuestion') return { question: { id: 'q-1', state: 'answered', answer: 'yes' } };
			throw new Error(`unexpected method ${request.method}`);
		},
		async (socketPath) => {
			const result = await withEnv(
				{
					KHAZAD_DAEMON_SOCKET: socketPath,
					KHAZAD_RUN_ID: 'kd-run',
					KHAZAD_SLICE_ID: 'slice-001',
					KHAZAD_WORKER_TOKEN: 'secret-token',
					KHAZAD_ATTEMPT: '2',
				},
				() =>
					tool.execute(
						'tool-call',
						{ question: 'Proceed?', options: ['yes', 'no'], timeout_seconds: 30 },
						undefined,
						undefined,
						{ ui: { async select() { return 'yes'; } } },
					),
			);

			assert.equal(result.details.available, true);
			assert.equal(result.details.answer, 'yes');
			assert.equal(result.details.question_id, 'q-1');
			assert.equal(result.details.answered_via, 'worker_pane');
		},
	);

	assert.deepEqual(requests.map((request) => request.method), ['workerAskOpen', 'answerQuestion']);
	assert.deepEqual(requests[0].params, {
		run_id: 'kd-run',
		slice_id: 'slice-001',
		token: 'secret-token',
		attempt: 2,
		question: 'Proceed?',
		options: ['yes', 'no'],
		timeout_seconds: 30,
	});
	assert.deepEqual(requests[1].params, { run_id: 'kd-run', question_id: 'q-1', answer: 'yes' });
});

test('ask_operator same-pane cancellation preserves an auto-applied recommendation', async () => {
	const tool = registerExtension().tools.get('ask_operator');

	await withDaemonServer(
		(request) => {
			if (request.method === 'workerAskOpen') {
				return {
					question_id: 'q-auto',
					state: 'pending',
					timeout_seconds: 60,
					deadline_at: '2026-07-10T00:01:00+00:00',
					recommended_answer: 'yes',
					recommendation_rationale: 'yes stays inside the slice',
					fallback_eligible: true,
				};
			}
			if (request.method === 'workerQuestionTimeout') {
				return {
					question_id: 'q-auto',
					state: 'answered',
					answer: 'yes',
					answer_source: 'llm_recommendation_timeout',
				};
			}
			throw new Error(`unexpected method ${request.method}`);
		},
		async (socketPath) => {
			const result = await withEnv(
				{
					KHAZAD_DAEMON_SOCKET: socketPath,
					KHAZAD_RUN_ID: 'kd-run',
					KHAZAD_SLICE_ID: 'slice-001',
					KHAZAD_WORKER_TOKEN: 'secret-token',
				},
				() =>
					tool.execute(
						'tool-call',
						{
							question: 'Proceed?',
							options: ['yes', 'no'],
							recommended_answer: 'yes',
							rationale: 'yes stays inside the slice',
							bounded_within_current_slice_or_mission_authority: true,
							reversible: true,
						},
						undefined,
						undefined,
						{ ui: { async select() { return undefined; } } },
					),
			);

			assert.equal(result.details.available, true);
			assert.equal(result.details.answer, 'yes');
			assert.equal(result.details.timed_out, undefined);
			assert.equal(result.details.answered_via, 'llm_recommendation_timeout');
		},
	);
});

test('ask_operator falls back to daemon workerAsk when worker-pane UI is unavailable', async () => {
	const tool = registerExtension().tools.get('ask_operator');
	let seenRequest;

	await withDaemonServer(
		(request) => {
			seenRequest = request;
			return { question_id: 'q-1', answer: 'yes' };
		},
		async (socketPath) => {
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
		},
	);

	assert.equal(seenRequest.method, 'workerAsk');
});
