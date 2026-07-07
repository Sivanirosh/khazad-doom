'use strict';

const { spawn } = require('node:child_process');
const net = require('node:net');
const os = require('node:os');
const path = require('node:path');

const FEEDBACK_WIDGET_ID = 'khazad-doom';
const FEEDBACK_POLL_MS = 2000;
const TERMINAL_RUN_STATUSES = new Set(['blocked', 'completed', 'failed', 'cancelled', 'interrupted']);
const KHAZAD_COMMAND_TIMEOUT_MS = 10000;

function khazadMonitorExtension(pi) {
	const feedback = createFeedbackAdapter();

	pi.registerTool({
		name: 'ask_operator',
		label: 'Ask Operator',
		description: 'Ask the Khazad-Doom operator a bounded question when a must_ask_if rule is hit.',
		promptSnippet: 'Ask the Khazad-Doom operator a bounded question and wait for the answer.',
		promptGuidelines: [
			'Use ask_operator when a Khazad-Doom JSON Issue Slice must_ask_if rule requires operator input before proceeding.',
			'If ask_operator is unavailable or times out, return blocked JSON with an ask-user finding instead of inventing intent.',
		],
		parameters: {
			type: 'object',
			properties: {
				question: { type: 'string', description: 'Question to ask the operator.' },
				options: { type: 'array', items: { type: 'string' }, description: 'Candidate answers or choices.' },
				timeout_seconds: { type: 'number', description: 'Optional wait timeout in seconds.' },
			},
			required: ['question'],
			additionalProperties: false,
		},
		async execute(_toolCallId, input) {
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
			const result = await daemonCall(socket, 'workerAsk', {
				run_id: runId,
				slice_id: sliceId,
				token,
				attempt: Number(process.env.KHAZAD_ATTEMPT || '0'),
				question: String(input.question || ''),
				options: Array.isArray(input.options) ? input.options.map(String) : [],
				timeout_seconds: Number(input.timeout_seconds || 0),
			});
			if (result.timed_out) {
				return toolResult('No operator answer before timeout; proceed per the blocked contract.', {
					available: true,
					answer: '',
					timed_out: true,
					question_id: result.question_id,
				});
			}
			return toolResult(`Operator answered: ${result.answer || ''}`, {
				available: true,
				answer: result.answer || '',
				question_id: result.question_id,
			});
		},
	});

	if (typeof pi.registerCommand === 'function') {
		pi.registerCommand('khazad-attach', {
			description: 'Attach a compact read-only Khazad-Doom daemon feed widget by run id.',
			handler: async (args, ctx) => feedback.attach(String(args || '').trim(), ctx),
		});
		pi.registerCommand('khazad-detach', {
			description: 'Detach the Khazad-Doom daemon feed widget.',
			handler: async (_args, ctx) => feedback.detach(ctx, { notify: true }),
		});
		pi.registerCommand('khazad-explain', {
			description: 'Explain one run from the daemon-owned feed projection.',
			handler: async (args, ctx) => explainRun(String(args || '').trim(), ctx),
		});
		pi.registerCommand('khazad-open', {
			description: 'Open or focus the Herdr cockpit for a run or the latest repo run.',
			handler: async (args, ctx) => openHerdrCockpit(String(args || '').trim(), ctx),
		});
		pi.registerCommand('khazad-handoff', {
			description: 'Summarize daemon handoff data for a completed run.',
			handler: async (args, ctx) => summarizeHandoff(String(args || '').trim(), ctx),
		});
		pi.registerCommand('khazad-answer', {
			description: 'Answer a pending worker question through the daemon.',
			handler: async (args, ctx) => answerQuestion(String(args || '').trim(), ctx),
		});
	}

	if (typeof pi.on === 'function') {
		pi.on('session_shutdown', async (_event, ctx) => {
			feedback.shutdown(ctx);
		});
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
		safeNotify(ctx, `Attached Khazad-Doom daemon feed for ${runId}`, 'info');
	}

	function detach(ctx, options = {}) {
		if (active?.timer) clearInterval(active.timer);
		active = null;
		safeSetWidget(ctx, undefined);
		safeSetStatus(ctx, undefined);
		if (options.notify) safeNotify(ctx, 'Detached Khazad-Doom daemon feed', 'info');
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
			const summary = details?.feed?.summary_line ? truncateLine(details.feed.summary_line, 80) : 'daemon feed unavailable';
			safeSetStatus(current.ctx, `Khazad: ${summary}`);
			const status = String(details?.run?.status || '').trim();
			if (TERMINAL_RUN_STATUSES.has(status)) {
				if (current.timer) clearInterval(current.timer);
				current.timer = undefined;
			}
		} catch (error) {
			if (!isActive(token)) return;
			const message = error?.message || String(error);
			safeSetWidget(current.ctx, [`Khazad-Doom ${current.runId}`, `daemon feed unavailable: ${message}`]);
			safeSetStatus(current.ctx, 'Khazad: daemon feed unavailable');
			if (current.lastError !== message) {
				current.lastError = message;
				safeNotify(current.ctx, `Khazad-Doom daemon feed unavailable: ${message}`, 'warning');
			}
		}
	}

	function isActive(token) {
		return Boolean(active && active.token === token);
	}

	return { attach, detach, shutdown };
}

async function explainRun(input, ctx) {
	const tokens = splitArgs(input);
	const runId = tokens[0] && !tokens[0].startsWith('--') ? tokens[0] : '';
	const wantsLatest = tokens.includes('--latest');
	if (!runId && !wantsLatest) {
		safeNotify(ctx, 'Usage: /khazad-explain <run-id> or /khazad-explain --latest [--repo <path>]', 'error');
		return;
	}
	const params = runId
		? { run_id: runId, events_limit: 20 }
		: { repo_path: path.resolve(repoFromArgs(tokens, ctx)), latest: true, active_only: false, events_limit: 20 };
	try {
		const details = await daemonCall(daemonSocketPath(), 'status', params);
		if (!details || !details.run) {
			safeNotify(ctx, 'No Khazad-Doom run found for this repository.', 'warning');
			return;
		}
		const lines = renderRunFeed(details);
		safeSetWidget(ctx, lines);
		safeNotify(ctx, lines.slice(0, 2).join(' — '), 'info');
	} catch (error) {
		safeNotify(ctx, `Khazad-Doom explain failed: ${error?.message || error}`, 'error');
	}
}

async function openHerdrCockpit(input, ctx) {
	try {
		const tokens = splitArgs(input);
		if (tokens.length === 0) {
			safeNotify(ctx, 'Usage: /khazad-open <run-id> or /khazad-open --latest [--repo <path>]', 'error');
			return undefined;
		}
		const result = await runKhazad(cockpitOpenArgs(tokens, ctx));
		const parsed = JSON.parse(result.stdout || '{}');
		const summary = summarizeCockpitOpen(parsed);
		safeNotify(ctx, summary, parsed.opened ? 'info' : 'warning');
		return parsed;
	} catch (error) {
		safeNotify(ctx, `Khazad-Doom Herdr open failed: ${error?.message || error}`, 'error');
		return undefined;
	}
}

async function summarizeHandoff(input, ctx) {
	const runId = splitArgs(input)[0] || '';
	if (!runId) {
		safeNotify(ctx, 'Usage: /khazad-handoff <run-id>', 'error');
		return;
	}
	try {
		const handoff = await daemonCall(daemonSocketPath(), 'handoffRun', {
			run_id: runId,
			push: false,
			create_pr: false,
			dry_run: true,
		});
		const summary = summarizeHandoffData(handoff);
		safeSetWidget(ctx, summary);
		safeNotify(ctx, summary[0], 'info');
		return handoff;
	} catch (error) {
		safeNotify(ctx, `Khazad-Doom handoff summary failed: ${error?.message || error}`, 'error');
		return undefined;
	}
}

async function answerQuestion(input, ctx) {
	const tokens = splitArgs(input);
	const [runId, questionId, ...answerParts] = tokens;
	const answer = answerParts.join(' ').trim();
	if (!runId || !questionId || !answer) {
		safeNotify(ctx, 'Usage: /khazad-answer <run-id> <question-id> <answer>', 'error');
		return;
	}
	try {
		const result = await daemonCall(daemonSocketPath(), 'answerQuestion', {
			run_id: runId,
			question_id: questionId,
			answer,
		});
		safeNotify(ctx, `Answered ${questionId} through Khazad-Doom daemon state.`, 'info');
		return result;
	} catch (error) {
		safeNotify(ctx, `Khazad-Doom answer failed: ${error?.message || error}`, 'error');
		return undefined;
	}
}

function cockpitOpenArgs(tokens, ctx) {
	const args = ['cockpit', 'open'];
	const latest = tokens.includes('--latest');
	if (!latest) {
		args.push('--run', tokens[0]);
		return args;
	}
	args.push('--latest');
	args.push('--repo', repoFromArgs(tokens, ctx));
	return args;
}

function repoFromArgs(tokens, ctx) {
	const repoIndex = tokens.indexOf('--repo');
	if (repoIndex >= 0 && tokens[repoIndex + 1]) return tokens[repoIndex + 1];
	return ctx?.cwd || ctx?.workspace?.path || process.cwd();
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

function summarizeCockpitOpen(result) {
	if (result?.opened) {
		return `${result.message || 'Herdr cockpit ready'}: ${result.workspace_label || result.run_id}`;
	}
	const fallback = result?.fallback ? ` Fallback: ${result.fallback}` : '';
	const remediation = result?.remediation ? ` Remediation: ${result.remediation}` : '';
	return `Herdr cockpit unavailable for ${result?.run_id || 'run'}: ${result?.message || 'unknown error'}.${fallback}${remediation}`;
}

function summarizeHandoffData(handoff) {
	const completed = Array.isArray(handoff?.completed_slices) ? handoff.completed_slices.join(', ') : '';
	return [
		`Handoff ${handoff?.run_id || 'run'}: ${handoff?.status || 'unknown'} / ${handoff?.exit_states?.handoff || 'handoff state unknown'}`,
		`Branch: ${handoff?.integration_branch || '-'}`,
		`Evidence: ${handoff?.evidence_attestation?.status || '-'}`,
		`Completed slices: ${completed || '-'}`,
		`Push: ${handoff?.push_command || '-'}`,
		`PR: ${handoff?.pr_command || '-'}`,
	].map((line) => truncateLine(line));
}

function truncateLine(line, max = 120) {
	const text = String(line || '');
	if (text.length <= max) return text;
	return `${text.slice(0, max - 1)}…`;
}

function splitArgs(input) {
	const tokens = [];
	const pattern = /"([^"]*)"|'([^']*)'|(\S+)/g;
	let match;
	while ((match = pattern.exec(String(input || ''))) !== null) {
		tokens.push(match[1] ?? match[2] ?? match[3]);
	}
	return tokens;
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
		const id = `${Date.now()}-${Math.random().toString(16).slice(2)}`;
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
				if (response.error) reject(new Error(String(response.error)));
				else resolve(response.result || {});
			} catch (error) {
				reject(error);
			}
		});
		client.on('error', reject);
	});
}

function runKhazad(args) {
	return new Promise((resolve, reject) => {
		const bin = process.env.KHAZAD_DOOM_BIN || 'khazad-doom';
		const child = spawn(bin, args, {
			stdio: ['ignore', 'pipe', 'pipe'],
			env: process.env,
		});
		let stdout = '';
		let stderr = '';
		const timer = setTimeout(() => {
			child.kill('SIGTERM');
			reject(new Error(`khazad-doom ${args.join(' ')} timed out`));
		}, KHAZAD_COMMAND_TIMEOUT_MS);
		child.stdout.setEncoding('utf8');
		child.stderr.setEncoding('utf8');
		child.stdout.on('data', (chunk) => {
			stdout += chunk;
		});
		child.stderr.on('data', (chunk) => {
			stderr += chunk;
		});
		child.on('error', (error) => {
			clearTimeout(timer);
			reject(error);
		});
		child.on('close', (code, signal) => {
			clearTimeout(timer);
			if (code === 0) {
				resolve({ stdout, stderr });
			} else {
				reject(new Error(`khazad-doom ${args.join(' ')} exited ${code ?? signal}: ${stderr || stdout}`));
			}
		});
	});
}

module.exports = khazadMonitorExtension;
