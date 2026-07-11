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

test('submit_worker_result contract keeps daemon-owned slice identity out of worker input', async () => {
	const tool = registeredTools().get('submit_worker_result');
	assert.deepEqual(tool.parameters.required, ['status', 'summary', 'acceptance_status']);
	assert.equal(tool.parameters.properties.slice_id, undefined);
	assert.ok(tool.parameters.properties.candidate_followup_slices);

	const candidate = {
		id: 'CA-08-FOLLOWUP',
		title: 'Follow-up',
		goal: 'Preserve a worker-authored follow-up proposal.',
		areas: ['src/domain.rs'],
		acceptance: ['Proposal remains explicit.'],
		verify: ['cargo test wire'],
		verify_profile: 'rust-unit',
		depends_on: ['CA-08'],
		must_ask_if: [],
		rationale: 'Discovered within the authorized intent.',
	};
	const tmp = await fs.mkdtemp(path.join(os.tmpdir(), 'khazad-worker-wire-'));
	const resultPath = path.join(tmp, 'result.json');
	await withEnv(
		{
			KHAZAD_WORKER_RESULT_PATH: resultPath,
			KHAZAD_RUN_ID: 'kd-wire-run',
			KHAZAD_SLICE_ID: 'CA-08',
			KHAZAD_ATTEMPT: '3',
		},
		async () => {
			const result = await tool.execute(
				'call-wire',
				validWorkerResult({ candidate_followup_slices: [candidate] }),
			);
			assert.equal(result.terminate, true);
			const artifact = JSON.parse(await fs.readFile(resultPath, 'utf8'));
			assert.equal(artifact.slice_id, 'CA-08');
			assert.equal(artifact.attempt, 3);
			assert.equal(artifact.result.slice_id, undefined);
			assert.deepEqual(artifact.result.candidate_followup_slices, [candidate]);
		},
	);
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

test('submit_worker_result rejects invalid and out-of-contract worker JSON without writing', async () => {
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

			const outOfContract = await tool.execute('call-4', validWorkerResult({ slice_id: 'OTHER' }));
			assert.equal(outOfContract.details.written, false);
			assert.equal(outOfContract.terminate, undefined);
			assert.match(outOfContract.details.error, /not part of the worker result contract/);

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

test('ask_operator reports unavailable when the daemon closes without a response', async () => {
	const tmp = await fs.mkdtemp(path.join(os.tmpdir(), 'khazad-worker-dropped-daemon-'));
	const socketPath = path.join(tmp, 'daemon.sock');
	const server = net.createServer((socket) => {
		socket.on('data', () => socket.end());
	});
	await new Promise((resolve) => server.listen(socketPath, resolve));
	try {
		const tool = registeredTools().get('ask_operator');
		await withEnv(
			{
				KHAZAD_DAEMON_SOCKET: socketPath,
				KHAZAD_RUN_ID: 'kd-run',
				KHAZAD_SLICE_ID: 'TUI-PROOF-01',
				KHAZAD_WORKER_TOKEN: 'secret-token',
			},
			async () => {
				const result = await tool.execute('call-dropped', { question: 'Choose?' });
				assert.equal(result.details.available, false);
				assert.equal(result.details.answer, '');
				assert.match(result.content[0].text, /channel unavailable/);
			},
		);
	} finally {
		await new Promise((resolve) => server.close(resolve));
	}
});

test('ask_operator prompts in the worker Pi pane and records the answer through daemon state', async () => {
	const tool = registeredTools().get('ask_operator');
	const requests = [];
	const calls = [];
	await withDaemonServer(
		(request) => {
			requests.push(request);
			if (request.method === 'workerAskOpen') {
				assert.equal(request.params.run_id, 'kd-run');
				assert.equal(request.params.slice_id, 'TUI-PROOF-01');
				assert.equal(request.params.token, 'secret-token');
				assert.equal(request.params.attempt, 1);
				assert.equal(request.params.question, 'Choose?');
				assert.deepEqual(request.params.options, ['A', 'B']);
				return { question_id: 'q-1', state: 'pending', timeout_seconds: 3 };
			}
			if (request.method === 'answerQuestion') {
				assert.deepEqual(request.params, { run_id: 'kd-run', question_id: 'q-1', answer: 'B' });
				return { question: { id: 'q-1', state: 'answered', answer: 'B' } };
			}
			throw new Error(`unexpected method ${request.method}`);
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
					const result = await tool.execute(
						'call-5',
						{ question: 'Choose?', options: ['A', 'B'], timeout_seconds: 3 },
						undefined,
						undefined,
						{
							ui: {
								async select(title, options, opts) {
									calls.push({ type: 'select', title, options, opts });
									return 'B';
								},
								async input() {
									throw new Error('input should not be used when a concrete option is selected');
								},
							},
						},
					);
					assert.equal(result.details.available, true);
					assert.equal(result.details.answer, 'B');
					assert.equal(result.details.question_id, 'q-1');
					assert.equal(result.details.answered_via, 'worker_pane');
				},
			);
		},
	);

	assert.deepEqual(requests.map((request) => request.method), ['workerAskOpen', 'answerQuestion']);
	assert.deepEqual(calls[0].options, ['A', 'B', 'Type a custom answer…']);
	assert.deepEqual(calls[0].opts, { timeout: 3000 });
});

test('ask_operator closes the daemon question when the worker-pane prompt is cancelled', async () => {
	const tool = registeredTools().get('ask_operator');
	const requests = [];
	await withDaemonServer(
		(request) => {
			requests.push(request);
			if (request.method === 'workerAskOpen') return { question_id: 'q-cancel', state: 'pending' };
			if (request.method === 'workerQuestionTimeout') {
				assert.deepEqual(request.params, {
					run_id: 'kd-run',
					question_id: 'q-cancel',
					token: 'secret-token',
					launch_id: 73,
				});
				return { question_id: 'q-cancel', state: 'timed_out', timed_out: true };
			}
			throw new Error(`unexpected method ${request.method}`);
		},
		async (socketPath) => {
			await withEnv(
				{
					KHAZAD_DAEMON_SOCKET: socketPath,
					KHAZAD_RUN_ID: 'kd-run',
					KHAZAD_SLICE_ID: 'TUI-PROOF-01',
					KHAZAD_WORKER_TOKEN: 'secret-token',
					KHAZAD_LAUNCH_ID: '73',
				},
				async () => {
					const result = await tool.execute(
						'call-cancel',
						{ question: 'Choose?', options: ['A', 'B'] },
						undefined,
						undefined,
						{ ui: { async select() { return undefined; } } },
					);
					assert.equal(result.details.available, true);
					assert.equal(result.details.timed_out, true);
					assert.equal(result.details.answer, '');
					assert.equal(result.details.question_id, 'q-cancel');
				},
			);
		},
	);

	assert.deepEqual(requests.map((request) => request.method), ['workerAskOpen', 'workerQuestionTimeout']);
});

test('ask_operator closes the daemon question when the worker-pane select prompt rejects', async () => {
	const tool = registeredTools().get('ask_operator');
	const requests = [];
	await withDaemonServer(
		(request) => {
			requests.push(request);
			if (request.method === 'workerAskOpen') return { question_id: 'q-select-error', state: 'pending' };
			if (request.method === 'workerQuestionTimeout') {
				assert.deepEqual(request.params, { run_id: 'kd-run', question_id: 'q-select-error', token: 'secret-token' });
				return { question_id: 'q-select-error', state: 'timed_out', timed_out: true };
			}
			throw new Error(`unexpected method ${request.method}`);
		},
		async (socketPath) => {
			await withEnv(
				{
					KHAZAD_DAEMON_SOCKET: socketPath,
					KHAZAD_RUN_ID: 'kd-run',
					KHAZAD_SLICE_ID: 'TUI-PROOF-01',
					KHAZAD_WORKER_TOKEN: 'secret-token',
				},
				async () => {
					const result = await tool.execute(
						'call-select-error',
						{ question: 'Choose?', options: ['A', 'B'] },
						undefined,
						undefined,
						{ ui: { async select() { throw new Error('select exploded'); } } },
					);
					assert.equal(result.details.available, true);
					assert.equal(result.details.timed_out, true);
					assert.equal(result.details.answer, '');
					assert.equal(result.details.question_id, 'q-select-error');
					assert.match(result.details.error, /select exploded/);
					assert.match(result.content[0].text, /Pi operator prompt failed/);
				},
			);
		},
	);

	assert.deepEqual(requests.map((request) => request.method), ['workerAskOpen', 'workerQuestionTimeout']);
});

test('ask_operator closes the daemon question when the custom-answer input rejects', async () => {
	const tool = registeredTools().get('ask_operator');
	const requests = [];
	await withDaemonServer(
		(request) => {
			requests.push(request);
			if (request.method === 'workerAskOpen') return { question_id: 'q-input-error', state: 'pending' };
			if (request.method === 'workerQuestionTimeout') {
				assert.deepEqual(request.params, { run_id: 'kd-run', question_id: 'q-input-error', token: 'secret-token' });
				return { question_id: 'q-input-error', state: 'timed_out', timed_out: true };
			}
			throw new Error(`unexpected method ${request.method}`);
		},
		async (socketPath) => {
			await withEnv(
				{
					KHAZAD_DAEMON_SOCKET: socketPath,
					KHAZAD_RUN_ID: 'kd-run',
					KHAZAD_SLICE_ID: 'TUI-PROOF-01',
					KHAZAD_WORKER_TOKEN: 'secret-token',
				},
				async () => {
					const result = await tool.execute(
						'call-input-error',
						{ question: 'Choose?', options: ['A', 'B'] },
						undefined,
						undefined,
						{
							ui: {
								async select() {
									return 'Type a custom answer…';
								},
								async input() {
									throw new Error('input exploded');
								},
							},
						},
					);
					assert.equal(result.details.available, true);
					assert.equal(result.details.timed_out, true);
					assert.equal(result.details.answer, '');
					assert.equal(result.details.question_id, 'q-input-error');
					assert.match(result.details.error, /input exploded/);
				},
			);
		},
	);

	assert.deepEqual(requests.map((request) => request.method), ['workerAskOpen', 'workerQuestionTimeout']);
});

test('ask_operator uses daemon workerAsk channel when worker-pane UI is unavailable', async () => {
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

test('ask_operator forwards bounded recommendation data and returns a headless timeout fallback as an answer', async () => {
	const tool = registeredTools().get('ask_operator');
	assert.equal(tool.parameters.properties.recommended_answer.type, 'string');
	assert.equal(tool.parameters.properties.rationale.type, 'string');
	assert.equal(tool.parameters.properties.bounded_within_current_slice_or_mission_authority.type, 'boolean');
	assert.equal(tool.parameters.properties.reversible.type, 'boolean');

	await withDaemonServer(
		(request) => {
			assert.equal(request.method, 'workerAsk');
			assert.equal(request.params.recommended_answer, 'A');
			assert.equal(request.params.rationale, 'A is the smallest reversible option');
			assert.equal(request.params.bounded_within_current_slice_or_mission_authority, true);
			assert.equal(request.params.reversible, true);
			return {
				question_id: 'q-fallback',
				state: 'answered',
				answer: 'A',
				answer_source: 'llm_recommendation_timeout',
			};
		},
		async (socketPath) => {
			await withEnv(
				{
					KHAZAD_DAEMON_SOCKET: socketPath,
					KHAZAD_RUN_ID: 'kd-run',
					KHAZAD_SLICE_ID: 'TUI-PROOF-01',
					KHAZAD_WORKER_TOKEN: 'secret-token',
				},
				async () => {
					const result = await tool.execute('call-fallback', {
						question: 'Choose?',
						options: ['A', 'B'],
						recommended_answer: 'A',
						rationale: 'A is the smallest reversible option',
						bounded_within_current_slice_or_mission_authority: true,
						reversible: true,
					});
					assert.equal(result.details.available, true);
					assert.equal(result.details.answer, 'A');
					assert.equal(result.details.timed_out, undefined);
					assert.equal(result.details.answered_via, 'llm_recommendation_timeout');
				},
			);
		},
	);
});

test('ask_operator never treats a headless interruption reason as an answer', async () => {
	const tool = registeredTools().get('ask_operator');
	await withDaemonServer(
		() => ({
			question_id: 'q-interrupted',
			state: 'interrupted',
			answer: 'superseded by worker attempt 2',
		}),
		async (socketPath) => {
			await withEnv(
				{
					KHAZAD_DAEMON_SOCKET: socketPath,
					KHAZAD_RUN_ID: 'kd-run',
					KHAZAD_SLICE_ID: 'TUI-PROOF-01',
					KHAZAD_WORKER_TOKEN: 'secret-token',
				},
				async () => {
					const result = await tool.execute('call-interrupted', { question: 'Choose?' });
					assert.equal(result.details.available, false);
					assert.equal(result.details.answer, '');
					assert.match(result.content[0].text, /ended as interrupted/);
					assert.doesNotMatch(result.content[0].text, /Operator answered/);
				},
			);
		},
	);
});

test('ask_operator same-pane expiry returns an already-applied daemon recommendation', async () => {
	const tool = registeredTools().get('ask_operator');
	const deadline = new Date(Date.now() + 2_000).toISOString();
	let promptTitle = '';
	let promptTimeout = 0;
	await withDaemonServer(
		(request) => {
			if (request.method === 'workerAskOpen') {
				return {
					question_id: 'q-expired',
					state: 'pending',
					timeout_seconds: 60,
					deadline_at: deadline,
					recommended_answer: 'A',
					recommendation_rationale: 'A is reversible',
					fallback_eligible: true,
				};
			}
			if (request.method === 'workerQuestionTimeout') {
				return {
					question_id: 'q-expired',
					state: 'answered',
					answer: 'A',
					answer_source: 'llm_recommendation_timeout',
				};
			}
			throw new Error(`unexpected method ${request.method}`);
		},
		async (socketPath) => {
			await withEnv(
				{
					KHAZAD_DAEMON_SOCKET: socketPath,
					KHAZAD_RUN_ID: 'kd-run',
					KHAZAD_SLICE_ID: 'TUI-PROOF-01',
					KHAZAD_WORKER_TOKEN: 'secret-token',
				},
				async () => {
					const result = await tool.execute(
						'call-expired',
						{
							question: 'Choose?',
							options: ['A', 'B'],
							recommended_answer: 'A',
							rationale: 'A is reversible',
							bounded_within_current_slice_or_mission_authority: true,
							reversible: true,
						},
						undefined,
						undefined,
						{ ui: { async select(title, _choices, options) { promptTitle = title; promptTimeout = options?.timeout || 0; return undefined; } } },
					);
					assert.match(promptTitle, new RegExp(deadline.replace(/[.*+?^${}()|[\]\\]/g, '\\$&')));
					assert.match(promptTitle, /Eligible fallback: A/);
					assert.ok(promptTimeout > 0 && promptTimeout <= 2_000, `expected remaining deadline timeout, got ${promptTimeout}`);
					assert.equal(result.details.answer, 'A');
					assert.equal(result.details.timed_out, undefined);
					assert.equal(result.details.answered_via, 'llm_recommendation_timeout');
				},
			);
		},
	);
});

test('ask_operator same-pane answer race returns the durable fallback winner', async () => {
	const tool = registeredTools().get('ask_operator');
	await withDaemonServer(
		(request) => {
			if (request.method === 'workerAskOpen') return { question_id: 'q-race', state: 'pending', timeout_seconds: 60 };
			if (request.method === 'answerQuestion') {
				return {
					question: {
						id: 'q-race',
						state: 'answered',
						answer: 'A',
						answer_source: 'llm_recommendation_timeout',
					},
					applied: false,
				};
			}
			throw new Error(`unexpected method ${request.method}`);
		},
		async (socketPath) => {
			await withEnv(
				{
					KHAZAD_DAEMON_SOCKET: socketPath,
					KHAZAD_RUN_ID: 'kd-run',
					KHAZAD_SLICE_ID: 'TUI-PROOF-01',
					KHAZAD_WORKER_TOKEN: 'secret-token',
				},
				async () => {
					const result = await tool.execute(
						'call-race',
						{ question: 'Choose?', options: ['A', 'B'] },
						undefined,
						undefined,
						{ ui: { async select() { return 'B'; } } },
					);
					assert.equal(result.details.answer, 'A');
					assert.equal(result.details.answered_via, 'llm_recommendation_timeout');
				},
			);
		},
	);
});

test('ask_operator same-pane accepts a legacy answer response without state', async () => {
	const tool = registeredTools().get('ask_operator');
	await withDaemonServer(
		(request) => {
			if (request.method === 'workerAskOpen') return { question_id: 'q-legacy-answer', state: 'pending', timeout_seconds: 60 };
			if (request.method === 'answerQuestion') {
				return { question: { id: 'q-legacy-answer', answer: 'B' }, applied: true };
			}
			throw new Error(`unexpected method ${request.method}`);
		},
		async (socketPath) => {
			await withEnv(
				{
					KHAZAD_DAEMON_SOCKET: socketPath,
					KHAZAD_RUN_ID: 'kd-run',
					KHAZAD_SLICE_ID: 'TUI-PROOF-01',
					KHAZAD_WORKER_TOKEN: 'secret-token',
				},
				async () => {
					const result = await tool.execute(
						'call-legacy-answer',
						{ question: 'Choose?', options: ['A', 'B'] },
						undefined,
						undefined,
						{ ui: { async select() { return 'B'; } } },
					);
					assert.equal(result.details.available, true);
					assert.equal(result.details.answer, 'B');
					assert.equal(result.details.answered_via, 'worker_pane');
				},
			);
		},
	);
});

test('ask_operator same-pane answer race never exposes an interruption reason as an answer', async () => {
	const tool = registeredTools().get('ask_operator');
	await withDaemonServer(
		(request) => {
			if (request.method === 'workerAskOpen') return { question_id: 'q-interrupted-race', state: 'pending', timeout_seconds: 60 };
			if (request.method === 'answerQuestion') {
				return {
					question: {
						id: 'q-interrupted-race',
						state: 'interrupted',
						answer: 'superseded by worker attempt 2',
					},
					applied: false,
				};
			}
			throw new Error(`unexpected method ${request.method}`);
		},
		async (socketPath) => {
			await withEnv(
				{
					KHAZAD_DAEMON_SOCKET: socketPath,
					KHAZAD_RUN_ID: 'kd-run',
					KHAZAD_SLICE_ID: 'TUI-PROOF-01',
					KHAZAD_WORKER_TOKEN: 'secret-token',
				},
				async () => {
					const result = await tool.execute(
						'call-interrupted-race',
						{ question: 'Choose?', options: ['A', 'B'] },
						undefined,
						undefined,
						{ ui: { async select() { return 'B'; } } },
					);
					assert.equal(result.details.available, false);
					assert.equal(result.details.answer, '');
					assert.equal(result.details.question_id, 'q-interrupted-race');
					assert.match(result.content[0].text, /ended as interrupted/);
					assert.doesNotMatch(result.content[0].text, /Operator answered/);
				},
			);
		},
	);
});

test('ask_operator same-pane answer race preserves a timed-out blocked outcome', async () => {
	const tool = registeredTools().get('ask_operator');
	await withDaemonServer(
		(request) => {
			if (request.method === 'workerAskOpen') return { question_id: 'q-timeout-race', state: 'pending', timeout_seconds: 60 };
			if (request.method === 'answerQuestion') {
				return {
					question: { id: 'q-timeout-race', state: 'timed_out', answer: '' },
					applied: false,
				};
			}
			throw new Error(`unexpected method ${request.method}`);
		},
		async (socketPath) => {
			await withEnv(
				{
					KHAZAD_DAEMON_SOCKET: socketPath,
					KHAZAD_RUN_ID: 'kd-run',
					KHAZAD_SLICE_ID: 'TUI-PROOF-01',
					KHAZAD_WORKER_TOKEN: 'secret-token',
				},
				async () => {
					const result = await tool.execute(
						'call-timeout-race',
						{ question: 'Choose?', options: ['A', 'B'] },
						undefined,
						undefined,
						{ ui: { async select() { return 'B'; } } },
					);
					assert.equal(result.details.available, true);
					assert.equal(result.details.answer, '');
					assert.equal(result.details.timed_out, true);
					assert.equal(result.details.question_id, 'q-timeout-race');
				},
			);
		},
	);
});

test('ask_operator same-pane timeout race preserves an interrupted outcome', async () => {
	const tool = registeredTools().get('ask_operator');
	await withDaemonServer(
		(request) => {
			if (request.method === 'workerAskOpen') return { question_id: 'q-timeout-interrupted', state: 'pending', timeout_seconds: 60 };
			if (request.method === 'workerQuestionTimeout') {
				return {
					id: 'q-timeout-interrupted',
					state: 'interrupted',
					answer: 'run reached a terminal state before the question was answered',
				};
			}
			throw new Error(`unexpected method ${request.method}`);
		},
		async (socketPath) => {
			await withEnv(
				{
					KHAZAD_DAEMON_SOCKET: socketPath,
					KHAZAD_RUN_ID: 'kd-run',
					KHAZAD_SLICE_ID: 'TUI-PROOF-01',
					KHAZAD_WORKER_TOKEN: 'secret-token',
				},
				async () => {
					const result = await tool.execute(
						'call-timeout-interrupted',
						{ question: 'Choose?', options: ['A', 'B'] },
						undefined,
						undefined,
						{ ui: { async select() { return undefined; } } },
					);
					assert.equal(result.details.available, false);
					assert.equal(result.details.answer, '');
					assert.equal(result.details.timed_out, undefined);
					assert.equal(result.details.question_id, 'q-timeout-interrupted');
					assert.match(result.content[0].text, /ended as interrupted/);
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
