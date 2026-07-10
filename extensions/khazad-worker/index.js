'use strict';

const fs = require('node:fs');
const net = require('node:net');
const os = require('node:os');
const path = require('node:path');

const FEEDBACK_WIDGET_ID = 'khazad-doom';
const FEEDBACK_POLL_MS = 2000;
const TERMINAL_RUN_STATUSES = new Set(['blocked', 'completed', 'failed', 'cancelled', 'interrupted']);
const SUBMIT_WORKER_RESULT_SOURCE = 'khazad_worker_submit_worker_result_v1';
const WORKER_RESULT_STATUSES = new Set(['complete', 'blocked', 'failed']);
const ACCEPTANCE_EVIDENCE_STATUSES = new Set(['satisfied', 'blocked', 'failed']);
const CUSTOM_ANSWER_CHOICE = 'Type a custom answer…';

function khazadWorkerExtension(pi) {
	const feedback = createFeedbackAdapter();

	pi.registerTool({
		name: 'ask_operator',
		label: 'Ask Operator',
		description: 'Ask the Khazad-Doom operator a bounded question when a must_ask_if rule is hit.',
		promptSnippet: 'Ask the Khazad-Doom operator a bounded question and wait for the answer.',
		promptGuidelines: [
			'Use ask_operator when a Khazad-Doom JSON Issue Slice must_ask_if rule requires operator input before proceeding.',
			'Always provide your original recommended_answer and rationale. Mark timeout fallback eligible only when that answer is one declared option, bounded within the current JSON Slice or mission envelope authority, and reversible.',
			'Never attest timeout fallback eligibility for scope expansion, destructive or irreversible actions, credentials or secrets, permission changes, release/push/handoff authorization, or any choice outside the current slice or mission envelope.',
			'If ask_operator is unavailable or times out without an eligible daemon-applied recommendation, return blocked JSON with an ask-user finding instead of inventing intent.',
		],
		parameters: {
			type: 'object',
			properties: {
				question: { type: 'string', description: 'Question to ask the operator.' },
				options: { type: 'array', items: { type: 'string' }, description: 'Candidate answers or choices.' },
				timeout_seconds: { type: 'number', description: 'Optional wait timeout in seconds.' },
				recommended_answer: { type: 'string', description: 'The asking LLM original recommendation; it must exactly match one declared non-empty option to be eligible at timeout.' },
				rationale: { type: 'string', description: 'Audit rationale for the original recommendation.' },
				bounded_within_current_slice_or_mission_authority: { type: 'boolean', description: 'Attestation that the recommendation stays inside existing slice or mission authority.' },
				reversible: { type: 'boolean', description: 'Attestation that applying the recommendation is reversible.' },
			},
			required: ['question'],
			additionalProperties: false,
		},
		async execute(_toolCallId, input, _signal, _onUpdate, ctx) {
			const socket = process.env.KHAZAD_DAEMON_SOCKET;
			const runId = process.env.KHAZAD_RUN_ID;
			const sliceId = process.env.KHAZAD_SLICE_ID;
			const token = process.env.KHAZAD_WORKER_TOKEN;
			if (!socket || !runId || !sliceId || !token) {
				return toolResult('ask_operator channel unavailable; return blocked JSON if the question is required.', {
					available: false,
					answer: '',
				});
			}
			const params = {
				run_id: runId,
				slice_id: sliceId,
				token,
				attempt: Number(process.env.KHAZAD_ATTEMPT || '0'),
				question: String(input.question || ''),
				options: Array.isArray(input.options) ? input.options.map(String) : [],
				timeout_seconds: Number(input.timeout_seconds || 0),
			};
			if (Object.prototype.hasOwnProperty.call(input, 'recommended_answer')) {
				params.recommended_answer = String(input.recommended_answer || '');
			}
			if (Object.prototype.hasOwnProperty.call(input, 'rationale')) {
				params.rationale = String(input.rationale || '');
			}
			if (Object.prototype.hasOwnProperty.call(input, 'bounded_within_current_slice_or_mission_authority')) {
				params.bounded_within_current_slice_or_mission_authority = input.bounded_within_current_slice_or_mission_authority === true;
			}
			if (Object.prototype.hasOwnProperty.call(input, 'reversible')) {
				params.reversible = input.reversible === true;
			}
			if (canPromptInWorkerPane(ctx?.ui, params.options)) {
				return askOperatorInWorkerPane(socket, params, ctx.ui);
			}
			try {
				return await askOperatorViaDaemonWait(socket, params);
			} catch (error) {
				return askOperatorUnavailable(`ask_operator channel unavailable: ${error?.message || error}`);
			}
		},
	});

	registerSubmitWorkerResultTool(pi);

	if (typeof pi.registerCommand === 'function') {
		pi.registerCommand('khazad-attach', {
			description: 'Attach a compact Khazad-Doom run feed widget by run id.',
			handler: async (args, ctx) => feedback.attach(String(args || '').trim(), ctx),
		});
		pi.registerCommand('khazad-detach', {
			description: 'Detach the Khazad-Doom run feed widget.',
			handler: async (_args, ctx) => feedback.detach(ctx, { notify: true }),
		});
	}

	if (typeof pi.on === 'function') {
		pi.on('session_shutdown', async (_event, ctx) => {
			feedback.shutdown(ctx);
		});
	}
}

async function askOperatorViaDaemonWait(socket, params) {
	const result = await daemonCall(socket, 'workerAsk', params);
	const state = String(result?.state || '');
	if (state === 'answered' || (!state && result?.answer)) {
		return answeredToolResult(result, result.question_id, 'daemon_wait');
	}
	if (result?.timed_out || state === 'timed_out') {
		return toolResult('No operator answer before timeout; proceed per the blocked contract.', {
			available: true,
			answer: '',
			timed_out: true,
			question_id: result.question_id,
		});
	}
	return askOperatorUnavailable(
		`ask_operator question ${result?.question_id || ''} ended as ${state || 'unknown'} before it was answered`,
	);
}

async function askOperatorInWorkerPane(socket, params, ui) {
	let opened;
	try {
		opened = await daemonCall(socket, 'workerAskOpen', params);
	} catch (error) {
		return askOperatorUnavailable(`ask_operator channel unavailable: ${error?.message || error}`);
	}
	const questionId = String(opened.question_id || '');
	if (!questionId) return askOperatorUnavailable('ask_operator channel unavailable: daemon did not return a question id');
	const question = {
		...params,
		...opened,
		id: questionId,
		timeout_seconds: Number(opened.timeout_seconds || params.timeout_seconds || 0),
	};
	if (questionDeadlineElapsed(question)) {
		return closeWorkerQuestionWithoutAnswer(
			socket,
			params,
			questionId,
			'Operator question deadline elapsed',
		);
	}
	let answer;
	try {
		answer = await promptWorkerPaneForAnswer(ui, question);
	} catch (error) {
		return closeWorkerQuestionWithoutAnswer(
			socket,
			params,
			questionId,
			`Pi operator prompt failed: ${error?.message || error}`,
			{ error: String(error?.message || error) },
		);
	}
	const trimmed = answer === undefined ? '' : String(answer).trim();
	if (!trimmed) {
		return closeWorkerQuestionWithoutAnswer(socket, params, questionId, 'No operator answer was submitted');
	}
	try {
		const recorded = await daemonCall(socket, 'answerQuestion', {
			run_id: params.run_id,
			question_id: questionId,
			answer: trimmed,
		});
		return completedQuestionToolResult(recorded?.question, questionId, 'worker_pane');
	} catch (error) {
		const durable = await readDurableWorkerQuestion(socket, params.run_id, questionId);
		if (durable) return completedQuestionToolResult(durable, questionId, 'worker_pane');
		return toolResult(`Operator answer could not be recorded: ${error?.message || error}; proceed per the blocked contract.`, {
			available: true,
			answer: '',
			question_id: questionId,
			error: String(error?.message || error),
		});
	}
}

async function closeWorkerQuestionWithoutAnswer(socket, params, questionId, message, details = {}) {
	let closeError = '';
	let durable;
	try {
		durable = await daemonCall(socket, 'workerQuestionTimeout', {
			run_id: params.run_id,
			question_id: questionId,
			token: params.token,
		});
	} catch (error) {
		closeError = String(error?.message || error);
		durable = await readDurableWorkerQuestion(socket, params.run_id, questionId);
	}
	const completionDetails = {
		...details,
		...(closeError ? { close_error: closeError } : {}),
	};
	const durableState = String(durable?.state || '');
	const durableAnswer = String(durable?.answer || '').trim();
	if (durable && (durableState === 'answered' || (!durableState && durableAnswer))) {
		return completedQuestionToolResult(durable, questionId, 'worker_pane', completionDetails);
	}
	if (durableState && durableState !== 'timed_out') {
		return completedQuestionToolResult(durable, questionId, 'worker_pane', completionDetails);
	}
	return toolResult(`${message}; proceed per the blocked contract.`, {
		available: true,
		answer: '',
		timed_out: true,
		question_id: questionId,
		...completionDetails,
	});
}

async function readDurableWorkerQuestion(socket, runId, questionId) {
	try {
		const result = await daemonCall(socket, 'listQuestions', { run_id: runId });
		return Array.isArray(result.questions)
			? result.questions.find((question) => String(question.id || '') === questionId)
			: undefined;
	} catch {
		return undefined;
	}
}

function completedQuestionToolResult(question, questionId, transport, details = {}) {
	const state = String(question?.state || '');
	if (state === 'answered' || (!state && String(question?.answer || '').trim())) {
		return answeredToolResult(question, questionId, transport, details);
	}
	if (state === 'timed_out') {
		return toolResult('No operator answer before timeout; proceed per the blocked contract.', {
			available: true,
			answer: '',
			timed_out: true,
			question_id: questionId,
			...details,
		});
	}
	return askOperatorUnavailable(
		`ask_operator question ${questionId} ended as ${state || 'unknown'} before it was answered`,
		{ question_id: questionId, ...details },
	);
}

function answeredToolResult(result, questionId, transport, details = {}) {
	const answer = String(result?.answer || '');
	const answerSource = String(result?.answer_source || '');
	const answeredVia = answerSource === 'llm_recommendation_timeout' ? answerSource : transport;
	const label = answeredVia === 'llm_recommendation_timeout' ? 'LLM timeout recommendation applied' : 'Operator answered';
	return toolResult(`${label}: ${answer}`, {
		available: true,
		answer,
		question_id: questionId,
		answered_via: answeredVia,
		...(answerSource ? { answer_source: answerSource } : {}),
		...details,
	});
}

function askOperatorUnavailable(message, details = {}) {
	return toolResult(`${message}; return blocked JSON if the question is required.`, {
		available: false,
		answer: '',
		...details,
	});
}

function canPromptInWorkerPane(ui, options) {
	if (!ui) return false;
	if (Array.isArray(options) && options.length > 0 && typeof ui.select === 'function') return true;
	return typeof ui.input === 'function';
}

async function promptWorkerPaneForAnswer(ui, question) {
	const options = Array.isArray(question.options) ? question.options.map(String).filter((option) => option.trim()) : [];
	const title = questionPromptTitle(question);
	if (options.length > 0 && typeof ui?.select === 'function') {
		const choices = [...options];
		const customChoice = customAnswerChoice(choices);
		if (typeof ui?.input === 'function') choices.push(customChoice);
		const selected = await ui.select(title, choices, dialogOptionsForQuestion(question));
		if (selected === undefined) return undefined;
		if (selected === customChoice) {
			if (questionDeadlineElapsed(question)) return undefined;
			return ui.input(
				`Answer ${question.id}`,
				options[0] || '',
				dialogOptionsForQuestion(question),
			);
		}
		return selected;
	}
	if (typeof ui?.input === 'function') {
		return ui.input(title, options[0] || '', dialogOptionsForQuestion(question));
	}
	return undefined;
}

function dialogOptionsForQuestion(question) {
	const deadline = Date.parse(String(question?.deadline_at || ''));
	if (Number.isFinite(deadline)) {
		return { timeout: Math.max(1, deadline - Date.now()) };
	}
	const seconds = Number(question?.timeout_seconds || 0);
	return seconds > 0 ? { timeout: seconds * 1000 } : undefined;
}

function questionDeadlineElapsed(question) {
	const deadline = Date.parse(String(question?.deadline_at || ''));
	return Number.isFinite(deadline) && Date.now() >= deadline;
}

function customAnswerChoice(options) {
	let label = CUSTOM_ANSWER_CHOICE;
	while (options.includes(label)) label = `${label}.`;
	return label;
}

function questionPromptTitle(question) {
	const slice = question.slice_id ? ` ${question.slice_id}` : '';
	const lines = [truncateLine(`Khazad-Doom asks${slice}: ${question.question || question.id}`, 180)];
	if (question.deadline_at) lines.push(`Deadline: ${question.deadline_at}`);
	if (question.fallback_eligible && question.recommended_answer) {
		lines.push(`Eligible fallback: ${question.recommended_answer}`);
		if (question.recommendation_rationale) lines.push(`Rationale: ${question.recommendation_rationale}`);
	}
	return lines.join('\n');
}

function registerSubmitWorkerResultTool(pi) {
	pi.registerTool({
		name: 'submit_worker_result',
		label: 'Submit Worker Result',
		description: 'Submit the final Khazad-Doom worker JSON result through a daemon-owned artifact channel.',
		promptSnippet: 'Use submit_worker_result as the final action for Khazad-Doom TUI worker sessions.',
		promptGuidelines: [
			'Use submit_worker_result exactly once, as the final action, when the slice implementation is complete, blocked, or failed.',
			'Do not paste JSON into the terminal as the final answer when submit_worker_result is available.',
			'Populate acceptance_status as worker evidence claims only; Khazad-Doom will validate and attest them separately.',
		],
		parameters: {
			type: 'object',
			properties: {
				slice_id: { type: 'string' },
				status: { type: 'string', enum: ['complete', 'blocked', 'failed'] },
				summary: { type: 'string' },
				commit_sha: { type: 'string' },
				commit_message: { type: 'string' },
				changed_files: { type: 'array', items: { type: 'string' } },
				public_interfaces_changed: { type: 'array', items: { type: 'string' } },
				tests_run: { type: 'array', items: { type: 'string' } },
				acceptance_status: {
					type: 'array',
					items: {
						type: 'object',
						properties: {
							criterion: { type: 'string' },
							status: { type: 'string', enum: ['satisfied', 'blocked', 'failed'] },
							evidence: { type: 'string' },
						},
						required: ['criterion', 'status', 'evidence'],
						additionalProperties: false,
					},
				},
				findings: { type: 'array', items: { type: 'object' } },
				finding_dispositions: { type: 'array', items: { type: 'object' } },
				assumptions: { type: 'array', items: { type: 'string' } },
			},
			required: ['slice_id', 'status', 'summary', 'acceptance_status'],
			additionalProperties: false,
		},
		async execute(_toolCallId, input) {
			const resultPath = process.env.KHAZAD_WORKER_RESULT_PATH;
			if (!resultPath) {
				return toolResult('submit_worker_result unavailable: KHAZAD_WORKER_RESULT_PATH is not set.', {
					available: false,
				});
			}

			const validationError = validateWorkerResult(input);
			if (validationError) {
				return toolResult(`submit_worker_result rejected invalid worker result: ${validationError}`, {
					available: true,
					written: false,
					error: validationError,
				});
			}

			const envSliceId = process.env.KHAZAD_SLICE_ID || '';
			if (envSliceId && input.slice_id !== envSliceId) {
				return toolResult(
					`submit_worker_result rejected worker result: slice_id ${JSON.stringify(input.slice_id)} does not match KHAZAD_SLICE_ID ${JSON.stringify(envSliceId)}.`,
					{
						available: true,
						written: false,
						error: 'slice_id does not match KHAZAD_SLICE_ID',
					},
				);
			}

			const attempt = Number.parseInt(process.env.KHAZAD_ATTEMPT || '0', 10);
			const artifact = {
				schema_version: 1,
				source: SUBMIT_WORKER_RESULT_SOURCE,
				submitted_at: new Date().toISOString(),
				run_id: process.env.KHAZAD_RUN_ID || '',
				slice_id: input.slice_id,
				attempt: Number.isFinite(attempt) ? attempt : 0,
				result: input,
			};
			writeJsonAtomic(resultPath, artifact);
			return {
				content: [{ type: 'text', text: `Submitted Khazad-Doom worker result for ${input.slice_id}.` }],
				details: {
					available: true,
					written: true,
					result_path: resultPath,
					source: SUBMIT_WORKER_RESULT_SOURCE,
				},
				terminate: true,
			};
		},
	});
}

function validateWorkerResult(result) {
	if (!result || typeof result !== 'object' || Array.isArray(result)) return 'result must be an object';
	for (const key of ['slice_id', 'status', 'summary']) {
		if (typeof result[key] !== 'string' || result[key].trim() === '') return `${key} must be a non-empty string`;
	}
	if (!WORKER_RESULT_STATUSES.has(result.status)) return 'status must be one of complete, blocked, failed';
	for (const key of ['commit_sha', 'commit_message']) {
		if (result[key] !== undefined && typeof result[key] !== 'string') return `${key} must be a string when present`;
	}
	for (const key of ['changed_files', 'public_interfaces_changed', 'tests_run', 'assumptions']) {
		const error = validateOptionalStringArray(result, key);
		if (error) return error;
	}
	if (!Array.isArray(result.acceptance_status)) return 'acceptance_status must be an array';
	for (let index = 0; index < result.acceptance_status.length; index += 1) {
		const item = result.acceptance_status[index];
		if (!item || typeof item !== 'object' || Array.isArray(item)) {
			return `acceptance_status[${index}] must be an object`;
		}
		for (const key of ['criterion', 'status', 'evidence']) {
			if (typeof item[key] !== 'string' || item[key].trim() === '') {
				return `acceptance_status[${index}].${key} must be a non-empty string`;
			}
		}
		if (!ACCEPTANCE_EVIDENCE_STATUSES.has(item.status)) {
			return `acceptance_status[${index}].status must be one of satisfied, blocked, failed`;
		}
	}
	for (const key of ['findings', 'finding_dispositions']) {
		if (result[key] !== undefined && !Array.isArray(result[key])) return `${key} must be an array when present`;
	}
	return '';
}

function validateOptionalStringArray(result, key) {
	if (result[key] === undefined) return '';
	if (!Array.isArray(result[key])) return `${key} must be an array when present`;
	for (let index = 0; index < result[key].length; index += 1) {
		if (typeof result[key][index] !== 'string') return `${key}[${index}] must be a string`;
	}
	return '';
}

function writeJsonAtomic(filePath, value) {
	fs.mkdirSync(path.dirname(filePath), { recursive: true });
	const tempPath = `${filePath}.${process.pid}.${Date.now()}.tmp`;
	try {
		fs.writeFileSync(tempPath, `${JSON.stringify(value, null, 2)}\n`, { mode: 0o600 });
		fs.renameSync(tempPath, filePath);
	} catch (error) {
		try {
			fs.rmSync(tempPath, { force: true });
		} catch (_cleanupError) {
			// Best-effort cleanup; preserve the original write error.
		}
		throw error;
	}
}

function createFeedbackAdapter() {
	let active = null;

	async function attach(runId, ctx) {
		if (!runId) {
			safeNotify(ctx, 'Usage: /khazad-attach <run-id>', 'error');
			return;
		}
		detach(ctx, { notify: false });
		const token = Symbol(runId);
		active = { runId, ctx, token, timer: undefined, lastError: '' };
		await poll(token);
		if (!isActive(token)) return;
		active.timer = setInterval(() => {
			poll(token).catch(() => undefined);
		}, FEEDBACK_POLL_MS);
		if (typeof active.timer.unref === 'function') active.timer.unref();
		safeNotify(ctx, `Attached Khazad-Doom feed for ${runId}`, 'info');
	}

	function detach(ctx, options = {}) {
		if (active?.timer) clearInterval(active.timer);
		active = null;
		safeSetWidget(ctx, undefined);
		safeSetStatus(ctx, undefined);
		if (options.notify) safeNotify(ctx, 'Detached Khazad-Doom feed', 'info');
	}

	function shutdown(ctx) {
		detach(ctx, { notify: false });
	}

	async function poll(token) {
		const current = active;
		if (!current || current.token !== token) return;
		try {
			const details = await daemonCall(daemonSocketPath(), 'status', {
				run_id: current.runId,
				events_limit: 20,
			});
			if (!isActive(token)) return;
			current.lastError = '';
			safeSetWidget(current.ctx, renderRunFeed(details));
			const status = String(details?.run?.status || '').trim();
			safeSetStatus(current.ctx, status ? `Khazad: ${status}` : 'Khazad: attached');
			if (TERMINAL_RUN_STATUSES.has(status)) {
				if (current.timer) clearInterval(current.timer);
				current.timer = undefined;
			}
		} catch (error) {
			if (!isActive(token)) return;
			const message = error?.message || String(error);
			safeSetWidget(current.ctx, [`Khazad-Doom ${current.runId}`, `status unavailable: ${message}`]);
			safeSetStatus(current.ctx, 'Khazad: status unavailable');
			if (current.lastError !== message) {
				current.lastError = message;
				safeNotify(current.ctx, `Khazad-Doom status unavailable: ${message}`, 'warning');
			}
		}
	}

	function isActive(token) {
		return Boolean(active && active.token === token);
	}

	return { attach, detach, shutdown };
}

function renderRunFeed(details) {
	const runId = String(details?.run?.id || 'unknown');
	const feed = details?.feed;
	if (!feed) {
		return [`Khazad-Doom ${runId}`, 'daemon status feed unavailable'];
	}
	const lines = [`Khazad-Doom ${runId}`, truncateLine(feed.summary_line || 'status feed')];
	for (const item of feed.attention || []) {
		lines.push(`! ${item.text || ''}`);
	}
	for (const block of feed.blocks || []) {
		if (lines.length >= 12) break;
		const label = block.meta ? `${block.label}: ${block.meta}` : block.label;
		lines.push(truncateLine(label));
		for (const line of block.lines || []) {
			if (lines.length >= 12) break;
			lines.push(truncateLine(`  ${line.text || ''}`));
		}
	}
	return lines;
}

function truncateLine(line, max = 120) {
	const text = String(line || '');
	if (text.length <= max) return text;
	return `${text.slice(0, max - 1)}…`;
}

function safeSetWidget(ctx, lines) {
	try {
		ctx?.ui?.setWidget?.(FEEDBACK_WIDGET_ID, lines);
	} catch (_error) {
		// Session replacement can stale old ctx objects; cleanup/poll paths must not crash Pi.
	}
}

function safeSetStatus(ctx, text) {
	try {
		ctx?.ui?.setStatus?.(FEEDBACK_WIDGET_ID, text);
	} catch (_error) {
		// See safeSetWidget.
	}
}

function safeNotify(ctx, message, level) {
	try {
		ctx?.ui?.notify?.(message, level);
	} catch (_error) {
		// See safeSetWidget.
	}
}

function daemonSocketPath() {
	if (process.env.KHAZAD_DAEMON_SOCKET) return process.env.KHAZAD_DAEMON_SOCKET;
	if (process.env.KHAZAD_HOME) return path.join(process.env.KHAZAD_HOME, 'socket');
	return path.join(os.homedir(), '.khazad-doom', 'socket');
}

function toolResult(text, details) {
	return { content: [{ type: 'text', text }], details };
}

function daemonCall(socketPath, method, params) {
	return new Promise((resolve, reject) => {
		const client = net.createConnection(socketPath);
		let buffer = '';
		let settled = false;
		const id = `${Date.now()}-${Math.random().toString(16).slice(2)}`;
		const finish = (callback, value) => {
			if (settled) return;
			settled = true;
			callback(value);
		};
		const rejectUnavailable = () => {
			finish(reject, new Error('daemon connection closed before a complete response'));
		};
		client.setEncoding('utf8');
		client.on('connect', () => {
			client.write(`${JSON.stringify({ id, method, params })}\n`);
		});
		client.on('data', (chunk) => {
			buffer += chunk;
			const idx = buffer.indexOf('\n');
			if (idx < 0) return;
			const line = buffer.slice(0, idx).trim();
			client.end();
			try {
				const response = JSON.parse(line);
				if (response.error) finish(reject, new Error(String(response.error)));
				else finish(resolve, response.result || {});
			} catch (error) {
				finish(reject, error);
			}
		});
		client.on('error', (error) => finish(reject, error));
		client.on('end', rejectUnavailable);
		client.on('close', rejectUnavailable);
	});
}

module.exports = khazadWorkerExtension;
