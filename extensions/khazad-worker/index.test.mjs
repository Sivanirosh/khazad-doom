import assert from 'node:assert/strict';
import fs from 'node:fs/promises';
import net from 'node:net';
import os from 'node:os';
import path from 'node:path';
import test from 'node:test';
import { createRequire } from 'node:module';

const require = createRequire(import.meta.url);
const khazadWorkerExtension = require('./index.js');

function registeredTools() {
	const tools = new Map();
	khazadWorkerExtension({
		registerTool(tool) {
			tools.set(tool.name, tool);
		},
		registerCommand() {},
		on() {},
	});
	return tools;
}

async function withEnv(vars, fn) {
	const previous = new Map();
	for (const [key, value] of Object.entries(vars)) {
		previous.set(key, Object.prototype.hasOwnProperty.call(process.env, key) ? process.env[key] : undefined);
		if (value === undefined) delete process.env[key];
		else process.env[key] = value;
	}
	try {
		return await fn();
	} finally {
		for (const [key, value] of previous.entries()) {
			if (value === undefined) delete process.env[key];
			else process.env[key] = value;
		}
	}
}

function validWorkerResult(overrides = {}) {
	return {
		slice_id: 'TUI-PROOF-01',
		status: 'complete',
		summary: 'Native Pi TUI proof submitted through extension artifact channel.',
		changed_files: [],
		tests_run: ['node --test extensions/khazad-worker/index.test.mjs'],
		acceptance_status: [
			{
				criterion: 'Worker can submit authoritative result without terminal scraping.',
				status: 'satisfied',
				evidence: 'submit_worker_result wrote the artifact directly.',
			},
		],
		...overrides,
	};
}

test('worker extension registers ask_operator and submit_worker_result tools', () => {
	const tools = registeredTools();
	assert.ok(tools.has('ask_operator'));
	assert.ok(tools.has('submit_worker_result'));
});

test('submit_worker_result reports unavailable when result path is missing', async () => {
	const tool = registeredTools().get('submit_worker_result');
	await withEnv({ KHAZAD_WORKER_RESULT_PATH: undefined }, async () => {
		const result = await tool.execute('call-1', validWorkerResult());
		assert.equal(result.details.available, false);
		assert.equal(result.terminate, undefined);
		assert.match(result.content[0].text, /KHAZAD_WORKER_RESULT_PATH/);
	});
});

test('submit_worker_result writes a terminating artifact without reading terminal output', async () => {
	const tmp = await fs.mkdtemp(path.join(os.tmpdir(), 'khazad-worker-result-'));
	const resultPath = path.join(tmp, 'result.json');
	const tool = registeredTools().get('submit_worker_result');
	await withEnv(
		{
			KHAZAD_WORKER_RESULT_PATH: resultPath,
			KHAZAD_RUN_ID: 'kd-proof-run',
			KHAZAD_SLICE_ID: 'TUI-PROOF-01',
			KHAZAD_ATTEMPT: '2',
		},
		async () => {
			const result = await tool.execute('call-2', validWorkerResult());
			assert.equal(result.terminate, true);
			assert.equal(result.details.available, true);
			assert.equal(result.details.written, true);
			assert.equal(result.details.result_path, resultPath);

			const artifact = JSON.parse(await fs.readFile(resultPath, 'utf8'));
			assert.equal(artifact.schema_version, 1);
			assert.equal(artifact.source, 'khazad_worker_submit_worker_result_v1');
			assert.equal(artifact.run_id, 'kd-proof-run');
			assert.equal(artifact.slice_id, 'TUI-PROOF-01');
			assert.equal(artifact.attempt, 2);
			assert.deepEqual(artifact.result, validWorkerResult());
		},
	);
});

test('submit_worker_result rejects invalid worker JSON and slice mismatches without writing', async () => {
	const tmp = await fs.mkdtemp(path.join(os.tmpdir(), 'khazad-worker-invalid-'));
	const resultPath = path.join(tmp, 'result.json');
	const tool = registeredTools().get('submit_worker_result');
	await withEnv(
		{
			KHAZAD_WORKER_RESULT_PATH: resultPath,
			KHAZAD_SLICE_ID: 'EXPECTED',
		},
		async () => {
			const invalid = await tool.execute('call-3', validWorkerResult({ status: 'done' }));
			assert.equal(invalid.details.written, false);
			assert.equal(invalid.terminate, undefined);
			assert.match(invalid.details.error, /status/);

			const mismatch = await tool.execute('call-4', validWorkerResult({ slice_id: 'OTHER' }));
			assert.equal(mismatch.details.written, false);
			assert.equal(mismatch.terminate, undefined);
			assert.match(mismatch.details.error, /slice_id/);

			await assert.rejects(fs.access(resultPath));
		},
	);
});

async function withDaemonServer(handler, fn) {
	const tmp = await fs.mkdtemp(path.join(os.tmpdir(), 'khazad-worker-daemon-'));
	const socketPath = path.join(tmp, 'daemon.sock');
	const server = net.createServer((socket) => {
		let buffer = '';
		socket.setEncoding('utf8');
		socket.on('data', (chunk) => {
			buffer += chunk;
			const index = buffer.indexOf('\n');
			if (index < 0) return;
			const request = JSON.parse(buffer.slice(0, index));
			const result = handler(request);
			socket.end(`${JSON.stringify({ id: request.id, result })}\n`);
		});
	});
	await new Promise((resolve) => server.listen(socketPath, resolve));
	try {
		return await fn(socketPath);
	} finally {
		await new Promise((resolve) => server.close(resolve));
	}
}

test('ask_operator uses daemon workerAsk channel when KD worker env is present', async () => {
	const tool = registeredTools().get('ask_operator');
	await withDaemonServer(
		(request) => {
			assert.equal(request.method, 'workerAsk');
			assert.equal(request.params.run_id, 'kd-run');
			assert.equal(request.params.slice_id, 'TUI-PROOF-01');
			assert.equal(request.params.token, 'secret-token');
			assert.equal(request.params.attempt, 1);
			assert.equal(request.params.question, 'Choose?');
			assert.deepEqual(request.params.options, ['A', 'B']);
			return { answer: 'A', question_id: 'q-1' };
		},
		async (socketPath) => {
			await withEnv(
				{
					KHAZAD_DAEMON_SOCKET: socketPath,
					KHAZAD_RUN_ID: 'kd-run',
					KHAZAD_SLICE_ID: 'TUI-PROOF-01',
					KHAZAD_WORKER_TOKEN: 'secret-token',
					KHAZAD_ATTEMPT: '1',
				},
				async () => {
					const result = await tool.execute('call-5', {
						question: 'Choose?',
						options: ['A', 'B'],
						timeout_seconds: 3,
					});
					assert.equal(result.details.available, true);
					assert.equal(result.details.answer, 'A');
					assert.equal(result.details.question_id, 'q-1');
				},
			);
		},
	);
});

test('ask_operator preserves timeout as a blocked-contract signal', async () => {
	const tool = registeredTools().get('ask_operator');
	await withDaemonServer(
		() => ({ timed_out: true, question_id: 'q-timeout' }),
		async (socketPath) => {
			await withEnv(
				{
					KHAZAD_DAEMON_SOCKET: socketPath,
					KHAZAD_RUN_ID: 'kd-run',
					KHAZAD_SLICE_ID: 'TUI-PROOF-01',
					KHAZAD_WORKER_TOKEN: 'secret-token',
				},
				async () => {
					const result = await tool.execute('call-6', { question: 'Need input?' });
					assert.equal(result.details.available, true);
					assert.equal(result.details.timed_out, true);
					assert.equal(result.details.answer, '');
					assert.match(result.content[0].text, /No operator answer/);
				},
			);
		},
	);
});
