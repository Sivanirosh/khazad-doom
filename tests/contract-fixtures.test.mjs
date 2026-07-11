import assert from 'node:assert/strict';
import fs from 'node:fs/promises';
import net from 'node:net';
import os from 'node:os';
import path from 'node:path';
import test from 'node:test';
import { createRequire } from 'node:module';

const require = createRequire(import.meta.url);
const monitorExtension = require('../extensions/khazad-monitor/index.js');
const workerExtension = require('../extensions/khazad-worker/index.js');
const fixtureUrl = new URL('./fixtures/contracts/v1.json', import.meta.url);

async function fixtures() {
	return JSON.parse(await fs.readFile(fixtureUrl, 'utf8'));
}

function assertExactKeys(value, expected, label) {
	assert.deepEqual(Object.keys(value).sort(), [...expected].sort(), `${label} schema drifted`);
}

function registered(extension) {
	const tools = new Map();
	const commands = new Map();
	const events = new Map();
	extension({
		registerTool(tool) { tools.set(tool.name, tool); },
		registerCommand(name, command) { commands.set(name, command); },
		on(name, handler) { events.set(name, handler); },
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

async function withStatusServer(statuses, callback) {
	const tempDir = await fs.mkdtemp(path.join(os.tmpdir(), 'khazad-contract-fixture-'));
	const socketPath = path.join(tempDir, 'daemon.sock');
	const server = net.createServer((socket) => {
		let buffer = '';
		socket.setEncoding('utf8');
		socket.on('data', (chunk) => {
			buffer += chunk;
			const index = buffer.indexOf('\n');
			if (index < 0) return;
			const request = JSON.parse(buffer.slice(0, index));
			const runId = request.params?.run_id;
			socket.end(`${JSON.stringify({ id: request.id, result: statuses.get(runId) })}\n`);
		});
	});
	await new Promise((resolve) => server.listen(socketPath, resolve));
	try {
		return await callback(socketPath);
	} finally {
		await new Promise((resolve) => server.close(resolve));
		await fs.rm(tempDir, { recursive: true, force: true });
	}
}

function fakeContext() {
	const calls = [];
	return {
		calls,
		ctx: {
			ui: {
				notify(message, level) { calls.push({ type: 'notify', message, level }); },
				setWidget(key, lines) { calls.push({ type: 'widget', key, lines }); },
				setStatus(key, text) { calls.push({ type: 'status', key, text }); },
			},
		},
	};
}

test('standard npm test discovers every shipped extension test', async () => {
	const pkg = JSON.parse(await fs.readFile(new URL('../package.json', import.meta.url), 'utf8'));
	assert.match(pkg.scripts.test, /tests\/\*\.test\.mjs/);
});

test('shared v1 fixtures cover the complete Rust/Node contract surface', async () => {
	const contract = await fixtures();
	assert.equal(contract.schema_version, 1);
	assertExactKeys(contract.status_read_model, [
		'events', 'feed', 'questions', 'replan', 'run', 'selected_slice_ids', 'slice_runs',
		'snapshot', 'terminalization',
	], 'status read model');
	assert.equal(contract.status_read_model.feed.feed_version, 2);
	assert.deepEqual(contract.operator_actions, contract.status_read_model.feed.actions);
	assert.ok(contract.operator_actions.length > 0);

	assertExactKeys(contract.worker_result, [
		'acceptance_status', 'changed_files', 'status', 'summary', 'tests_run',
	], 'worker result');
	assert.equal(contract.worker_result.status, 'complete');
	assert.equal(contract.worker_result.slice_id, undefined);

	assertExactKeys(contract.repair_result, [
		'changed_files', 'status', 'summary', 'tests_run',
	], 'repair result');
	assert.deepEqual(contract.repair_result, {
		changed_files: ['src/domain.rs'],
		status: 'complete',
		summary: 'Shared repair contract fixture completed.',
		tests_run: ['cargo test --all-targets'],
	});
	assert.equal(contract.repair_result.trigger, undefined);
	assert.equal(contract.repair_result.attempts, undefined);

	const eventKeys = ['created_at', 'id', 'payload', 'run_id', 'type'];
	assertExactKeys(contract.events.typed, eventKeys, 'typed event');
	assertExactKeys(contract.events.legacy, eventKeys, 'legacy event');
	assert.deepEqual(contract.events.typed, {
		created_at: '2026-07-11T12:00:00Z',
		id: 1,
		payload: { path: '.workflow/reports/contract-completed-implementation-summary.json' },
		run_id: 'contract-completed',
		type: 'terminal_summary_written',
	});
	assert.deepEqual(contract.events.legacy, {
		created_at: '2026-07-11T12:00:00Z',
		id: 2,
		payload: { message: 'legacy payload remains inspectable' },
		run_id: 'contract-completed',
		type: 'legacy_worker_note',
	});

	assertExactKeys(contract.terminal_summary, [
		'base_sha', 'checks', 'completed_slices', 'created_at', 'economics',
		'evidence_attestation', 'exit_states', 'final_sha', 'integration_branch',
		'integration_gate', 'integration_repair', 'plan_revisions', 'repo_path', 'run_id',
	], 'terminal summary');
	assertExactKeys(contract.terminal_summary.completed_slices[0], [
		'acceptance_status', 'attempt', 'changed_files', 'launch_id', 'slice_id', 'status',
		'summary', 'tests_run',
	], 'completed slice summary');
	assertExactKeys(contract.terminal_summary.integration_repair, [
		'attempts', 'changed_files', 'launch_id', 'status', 'summary', 'tests_run', 'trigger',
	], 'integration repair summary');
	assert.deepEqual(contract.terminal_summary.exit_states, {
		evidence: 'attested',
		handoff: 'ready',
		run: 'completed',
		slices: [{ daemon: 'merged', slice_id: 'CA-09', worker: 'complete' }],
	});
	assert.equal(contract.terminal_summary.completed_slices[0].slice_id, 'CA-09');
	assert.equal(contract.terminal_summary.integration_repair.trigger, 'integration_gate_failed');

	assert.deepEqual(contract.terminal_runs.map(({ run }) => run.status), [
		'blocked', 'failed', 'completed', 'cancelled',
	]);
	for (const terminal of contract.terminal_runs) {
		const expectedKeys = [
			'events', 'feed', 'replan', 'run', 'selected_slice_ids', 'slice_runs', 'snapshot',
			'terminalization',
		];
		if (terminal.run.status !== 'completed') expectedKeys.push('primary_terminal_reason');
		assertExactKeys(terminal, expectedKeys, `${terminal.run.status} run projection`);
		assert.equal(terminal.feed.lifecycle.terminal, true);
	}
});

test('monitor paints daemon-owned shared status and terminal fixtures', async () => {
	const contract = await fixtures();
	const statuses = new Map([
		[contract.status_read_model.run.id, contract.status_read_model],
		...contract.terminal_runs.map((status) => [status.run.id, status]),
	]);
	const { commands, events } = registered(monitorExtension);

	await withStatusServer(statuses, async (socketPath) => {
		await withEnv({ KHAZAD_DAEMON_SOCKET: socketPath }, async () => {
			for (const [runId, status] of statuses) {
				const { calls, ctx } = fakeContext();
				await commands.get('khazad-attach').handler(runId, ctx);
				const painted = calls.find((call) => call.type === 'widget' && Array.isArray(call.lines));
				assert.ok(painted, `missing painted feed for ${runId}`);
				assert.ok(painted.lines.some((line) => line.includes(status.feed.summary_line)));
				await events.get('session_shutdown')({ reason: 'fixture test' }, ctx);
			}
		});
	});
});

test('worker extension accepts the shared worker-authored result fixture', async () => {
	const contract = await fixtures();
	const resultDir = await fs.mkdtemp(path.join(os.tmpdir(), 'khazad-worker-contract-'));
	const resultPath = path.join(resultDir, 'result.json');
	const tool = registered(workerExtension).tools.get('submit_worker_result');
	try {
		await withEnv(
			{
				KHAZAD_WORKER_RESULT_PATH: resultPath,
				KHAZAD_RUN_ID: 'contract-run',
				KHAZAD_SLICE_ID: 'CA-09',
				KHAZAD_ATTEMPT: '2',
			},
			async () => {
				const result = await tool.execute('contract-call', contract.worker_result);
				assert.equal(result.details.written, true);
			},
		);
		const artifact = JSON.parse(await fs.readFile(resultPath, 'utf8'));
		assert.deepEqual(artifact.result, contract.worker_result);
		assert.equal(artifact.slice_id, 'CA-09');
		assert.equal(artifact.attempt, 2);
	} finally {
		await fs.rm(resultDir, { recursive: true, force: true });
	}
});
