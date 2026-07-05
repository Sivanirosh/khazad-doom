import test from 'node:test';
import assert from 'node:assert/strict';
import net from 'node:net';
import os from 'node:os';
import path from 'node:path';
import { mkdtemp, readFile, rm } from 'node:fs/promises';
import khazadWorkerExtension from '../extensions/khazad-worker/index.js';

function registerTool() {
	let tool;
	khazadWorkerExtension({
		registerTool(registered) {
			tool = registered;
		},
	});
	assert.ok(tool, 'expected ask_operator tool registration');
	return tool;
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
