'use strict';

const DEFAULT_INTERVAL_MS = 1000;
const DEFAULT_EVENTS_LIMIT = 8;
const STATUS_TIMEOUT_MS = 8000;
const OUTPUT_TAIL_LINES = 10;
const EVENT_TAIL_LINES = 6;
const TERMINAL_STATUSES = new Set(['completed', 'failed', 'blocked', 'cancelled', 'interrupted']);

function khazadMonitorExtension(pi) {
	pi.registerCommand('khazad-monitor', {
		description:
			'Open an optional Khazad-Doom progress overlay. Usage: /khazad-monitor [--latest|--run <run-id>|<run-id>]',
		getArgumentCompletions(prefix) {
			const options = ['--latest', '--run ', '--repo ', '--events-limit ', '--interval-ms '];
			const filtered = options.filter((option) => option.startsWith(prefix));
			return filtered.length > 0 ? filtered.map((value) => ({ value, label: value.trim() })) : null;
		},
		handler: async (args, ctx) => {
			let config;
			try {
				config = parseCommandArgs(args || '', ctx.cwd || process.cwd());
			} catch (error) {
				showMessage(ctx, errorMessage(error), 'error');
				return;
			}

			if (config.help) {
				showMessage(ctx, helpText(config), 'info');
				return;
			}

			if (ctx.mode !== 'tui' || !ctx.ui || typeof ctx.ui.custom !== 'function') {
				showMessage(ctx, nonTuiText(config), 'info');
				return;
			}

			let component;
			try {
				await ctx.ui.custom(
					(tui, theme, _keybindings, done) => {
						component = new KhazadMonitorOverlay({
							pi,
							config,
							theme,
							done,
							requestRender: () => tui.requestRender(),
						});
						component.start();
						return component;
					},
					{
						overlay: true,
						overlayOptions: {
							width: '85%',
							minWidth: 64,
							maxHeight: '90%',
							anchor: 'center',
							margin: 1,
						},
					},
				);
			} catch (error) {
				showMessage(
					ctx,
					`Khazad-Doom monitor overlay is unavailable: ${errorMessage(error)}\n${nonTuiText(config)}`,
					'error',
				);
			} finally {
				if (component) component.stop();
			}
		},
	});
}

class KhazadMonitorOverlay {
	constructor({ pi, config, theme, done, requestRender }) {
		this.pi = pi;
		this.config = config;
		this.theme = theme;
		this.done = done;
		this.requestRender = requestRender;
		this.timer = undefined;
		this.closed = false;
		this.inFlight = false;
		this.abortController = undefined;
		this.attachedRunId = undefined;
		this.state = {
			loading: true,
			details: undefined,
			waitingRepo: config.mode === 'latest' ? config.repo : undefined,
			error: '',
			lastUpdated: undefined,
			lastCommand: statusCommandText(config),
		};
	}

	start() {
		this.poll();
		this.timer = setInterval(() => this.poll(), this.config.intervalMs);
	}

	stop() {
		this.closed = true;
		if (this.timer) clearInterval(this.timer);
		this.timer = undefined;
		if (this.abortController) this.abortController.abort();
	}

	dispose() {
		this.stop();
	}

	handleInput(data) {
		if (data === 'q' || data === 'Q' || data === 'escape' || data === '\x1b') {
			this.stop();
			this.done(undefined);
			return;
		}
		if (data === 'r' || data === 'R') {
			this.poll();
		}
	}

	async poll() {
		if (this.closed) return;
		if (this.inFlight) return;
		this.inFlight = true;
		this.abortController = typeof AbortController === 'function' ? new AbortController() : undefined;
		const command = statusCommandFor(this.config, this.attachedRunId);
		this.state.lastCommand = commandToText(command);
		try {
			const result = await this.pi.exec(command.bin, command.args, {
				timeout: STATUS_TIMEOUT_MS,
				signal: this.abortController && this.abortController.signal,
			});
			if (this.closed) return;
			if (result.code !== 0) {
				throw new Error(compactCommandError(result));
			}
			const stdout = String(result.stdout || '').trim();
			const details = stdout ? JSON.parse(stdout) : null;
			if (this.config.mode === 'latest' && details && details.run && !this.attachedRunId) {
				this.attachedRunId = details.run.id;
			}
			this.state = {
				loading: false,
				details: details || undefined,
				waitingRepo: this.config.mode === 'latest' && !details ? this.config.repo : undefined,
				error: '',
				lastUpdated: new Date(),
				lastCommand: commandToText(command),
			};
			if (this.config.mode === 'latest' && details && details.run && isTerminalStatus(details.run.status)) {
				this.attachedRunId = undefined;
			}
		} catch (error) {
			if (this.closed) return;
			this.state = {
				...this.state,
				loading: false,
				error: statusErrorText(error),
				lastUpdated: new Date(),
				lastCommand: commandToText(command),
			};
		} finally {
			this.inFlight = false;
			this.abortController = undefined;
			if (!this.closed) this.requestRender();
		}
	}

	render(width) {
		const safeWidth = Math.max(2, width || 80);
		const innerWidth = Math.max(1, safeWidth - 2);
		const lines = [];
		const border = (text) => this.theme.fg('border', text);
		const row = (content = '') => {
			const truncated = truncatePlain(content, innerWidth);
			return border('│') + padRight(truncated, innerWidth) + border('│');
		};

		lines.push(border(`╭${'─'.repeat(innerWidth)}╮`));
		lines.push(row(' Khazad-Doom Monitor'));
		lines.push(row(' q/Esc closes this overlay only; daemon-owned runs keep running.'));
		lines.push(row(' r refreshes now.'));
		lines.push(border(`├${'─'.repeat(innerWidth)}┤`));

		for (const line of this.snapshotLines()) {
			lines.push(row(line));
		}

		lines.push(border(`╰${'─'.repeat(innerWidth)}╯`));
		return lines;
	}

	invalidate() {}

	snapshotLines() {
		if (this.state.loading) {
			return [
				`Mode: ${modeLabel(this.config)}`,
				`Status: loading`,
				`Command: ${this.state.lastCommand}`,
			];
		}

		const lines = [`Mode: ${modeLabel(this.config)}`];
		if (this.state.error) {
			lines.push('Status: unavailable');
			lines.push(`Message: ${this.state.error}`);
			lines.push(`Command: ${this.state.lastCommand}`);
			lines.push('');
		}

		if (this.state.details) {
			lines.push(...runDetailsLines(this.state.details));
		} else if (!this.state.error || this.config.mode === 'latest') {
			lines.push(...waitingLines(this.state.waitingRepo || this.config.repo));
		}

		if (this.state.lastUpdated) {
			lines.push('');
			lines.push(`Overlay updated: ${this.state.lastUpdated.toLocaleTimeString()}`);
		}
		return lines;
	}
}

function parseCommandArgs(rawArgs, cwd) {
	const tokens = shellSplit(rawArgs.trim());
	const config = {
		mode: undefined,
		runId: '',
		repo: cwd || process.cwd(),
		intervalMs: DEFAULT_INTERVAL_MS,
		eventsLimit: DEFAULT_EVENTS_LIMIT,
		bin: process.env.KHAZAD_DOOM_BIN || 'khazad-doom',
		help: false,
	};

	for (let i = 0; i < tokens.length; i++) {
		const token = tokens[i];
		if (token === '-h' || token === '--help' || token === 'help') {
			config.help = true;
			continue;
		}
		if (token === '--latest' || token === 'latest') {
			if (config.mode === 'run' || config.runId) {
				throw new Error('/khazad-monitor cannot combine --latest with a run id.');
			}
			config.mode = 'latest';
			continue;
		}
		if (token === '--run' || token === '-r') {
			if (config.mode === 'latest') {
				throw new Error('/khazad-monitor cannot combine --latest with --run.');
			}
			config.runId = requireValue(tokens, ++i, token);
			config.mode = 'run';
			continue;
		}
		if (token.startsWith('--run=')) {
			if (config.mode === 'latest') {
				throw new Error('/khazad-monitor cannot combine --latest with --run.');
			}
			config.runId = token.slice('--run='.length);
			config.mode = 'run';
			continue;
		}
		if (token === '--repo') {
			config.repo = requireValue(tokens, ++i, token);
			continue;
		}
		if (token.startsWith('--repo=')) {
			config.repo = token.slice('--repo='.length);
			continue;
		}
		if (token === '--events-limit') {
			config.eventsLimit = parsePositiveInteger(requireValue(tokens, ++i, token), token);
			continue;
		}
		if (token.startsWith('--events-limit=')) {
			config.eventsLimit = parsePositiveInteger(token.slice('--events-limit='.length), '--events-limit');
			continue;
		}
		if (token === '--interval-ms') {
			config.intervalMs = parsePositiveInteger(requireValue(tokens, ++i, token), token);
			continue;
		}
		if (token.startsWith('--interval-ms=')) {
			config.intervalMs = parsePositiveInteger(token.slice('--interval-ms='.length), '--interval-ms');
			continue;
		}
		if (token === '--bin') {
			config.bin = requireValue(tokens, ++i, token);
			continue;
		}
		if (token.startsWith('--bin=')) {
			config.bin = token.slice('--bin='.length);
			continue;
		}
		if (token.startsWith('-')) {
			throw new Error(`Unknown /khazad-monitor option: ${token}`);
		}
		if (config.mode === 'latest') {
			throw new Error('/khazad-monitor cannot combine --latest with a run id.');
		}
		if (config.runId) {
			throw new Error(`Unexpected extra /khazad-monitor argument: ${token}`);
		}
		config.runId = token;
		config.mode = 'run';
	}

	if (!config.help && !config.mode) {
		config.mode = 'latest';
	}
	if (config.mode === 'run' && !config.runId) {
		throw new Error('/khazad-monitor --run requires a run id.');
	}
	if (config.mode === 'run' && config.repo !== (cwd || process.cwd())) {
		throw new Error('/khazad-monitor --repo can only be used with --latest.');
	}
	config.intervalMs = Math.max(250, config.intervalMs);
	return config;
}

function shellSplit(input) {
	if (!input) return [];
	const tokens = [];
	let current = '';
	let quote = '';
	let escaped = false;
	for (const ch of input) {
		if (escaped) {
			current += ch;
			escaped = false;
			continue;
		}
		if (ch === '\\') {
			escaped = true;
			continue;
		}
		if (quote) {
			if (ch === quote) {
				quote = '';
			} else {
				current += ch;
			}
			continue;
		}
		if (ch === '"' || ch === "'") {
			quote = ch;
			continue;
		}
		if (/\s/.test(ch)) {
			if (current) {
				tokens.push(current);
				current = '';
			}
			continue;
		}
		current += ch;
	}
	if (escaped) current += '\\';
	if (quote) throw new Error(`Unclosed ${quote} quote in /khazad-monitor arguments.`);
	if (current) tokens.push(current);
	return tokens;
}

function requireValue(tokens, index, flag) {
	const value = tokens[index];
	if (!value || value.startsWith('--')) {
		throw new Error(`${flag} requires a value.`);
	}
	return value;
}

function parsePositiveInteger(value, flag) {
	const parsed = Number.parseInt(value, 10);
	if (!Number.isFinite(parsed) || parsed <= 0) {
		throw new Error(`${flag} requires a positive integer.`);
	}
	return parsed;
}

function statusCommandFor(config, attachedRunId) {
	if (config.mode === 'run' || attachedRunId) {
		return {
			bin: config.bin,
			args: ['status', '--run', attachedRunId || config.runId, '--events-limit', String(config.eventsLimit)],
		};
	}
	return {
		bin: config.bin,
		args: ['status', '--repo', config.repo, '--latest', '--events-limit', String(config.eventsLimit)],
	};
}

function statusCommandText(config) {
	return commandToText(statusCommandFor(config));
}

function commandToText(command) {
	return [command.bin, ...command.args].map(shellQuote).join(' ');
}

function monitorCommandText(config) {
	if (config.mode === 'run') {
		return [config.bin, 'monitor', '--run', config.runId].map(shellQuote).join(' ');
	}
	return [config.bin, 'monitor', '--repo', config.repo, '--latest'].map(shellQuote).join(' ');
}

function watchCommandText(config) {
	if (config.mode === 'run') {
		return [config.bin, 'watch', '--run', config.runId].map(shellQuote).join(' ');
	}
	return [config.bin, 'status', '--repo', config.repo, '--latest'].map(shellQuote).join(' ');
}

function modeLabel(config) {
	if (config.mode === 'run') return `run ${config.runId}`;
	return `latest active run in ${config.repo}`;
}

function nonTuiText(config) {
	return [
		'Pi TUI mode is required for the /khazad-monitor overlay.',
		`Dashboard command: ${monitorCommandText(config)}`,
		`Plain fallback: ${watchCommandText(config)}`,
	].join('\n');
}

function helpText(config) {
	return [
		'Usage:',
		'  /khazad-monitor --latest [--repo <path>]',
		'  /khazad-monitor --run <run-id>',
		'  /khazad-monitor <run-id>',
		'',
		'Keys: q or Esc closes only the Pi overlay; r refreshes. The daemon-owned run is never cancelled.',
		`Current fallback command: ${monitorCommandText(config.mode ? config : { ...config, mode: 'latest' })}`,
	].join('\n');
}

function showMessage(ctx, message, level) {
	if (ctx && ctx.hasUI && ctx.ui && typeof ctx.ui.notify === 'function') {
		ctx.ui.notify(message, level || 'info');
		return;
	}
	console.log(message);
}

function compactCommandError(result) {
	const output = [result.stderr, result.stdout]
		.map((part) => String(part || '').trim())
		.filter(Boolean)
		.join('\n');
	const code = result.killed ? 'killed' : `exit ${result.code}`;
	return output ? `${code}: ${truncatePlain(output.replace(/\s+/g, ' '), 300)}` : code;
}

function statusErrorText(error) {
	const text = errorMessage(error);
	if (/ENOENT|not found|No such file/i.test(text)) {
		return `${text}. Verify khazad-doom is installed and on PATH, or set KHAZAD_DOOM_BIN.`;
	}
	return text;
}

function errorMessage(error) {
	return error && error.message ? String(error.message) : String(error);
}

function runDetailsLines(details) {
	const run = details.run || {};
	const progress = details.progress || undefined;
	const phase = progress && progress.phase ? progress.phase : isTerminalStatus(run.status) ? run.status : 'unknown';
	const command = progress && progress.command ? progress.command : '-';
	const message = monitorMessage(details);
	const updated = progress && progress.updated_at ? progress.updated_at : run.updated_at || '-';
	const elapsedStart = progress && progress.phase_started_at ? progress.phase_started_at : run.started_at;
	const lines = [
		`Run: ${valueOrDash(run.id)}`,
		`Repo: ${valueOrDash(run.repo_path)}`,
		`Status: ${valueOrDash(run.status)}`,
		`Integration branch: ${valueOrDash(run.integration_branch)}`,
		'',
		'Progress:',
		`  Phase: ${valueOrDash(phase)}`,
		`  Slice: ${monitorSliceLabel(details)}`,
		`  Attempt: ${progress && progress.attempt ? progress.attempt : '-'}`,
		`  Command: ${valueOrDash(command)}`,
		`  Elapsed: ${formatElapsed(elapsedStart)}`,
		`  Updated: ${updated}`,
		`  Message: ${valueOrDash(message)}`,
		'',
		'Recent events:',
		...eventLines(details.events || []),
		'',
		'Output tail:',
		...outputTailLines(progress && progress.output_tail ? progress.output_tail : ''),
	];
	return lines;
}

function waitingLines(repo) {
	return [
		'Run: -',
		`Repo: ${valueOrDash(repo)}`,
		'Status: waiting',
		'Progress:',
		'  Phase: waiting',
		'  Slice: -',
		'  Command: -',
		'  Message: waiting for latest active run',
		'',
		'Recent events:',
		'  -',
		'',
		'Output tail:',
		'  -',
	];
}

function eventLines(events) {
	if (!events.length) return ['  -'];
	return events.slice(-EVENT_TAIL_LINES).map((event) => `  ${eventTime(event.created_at)} ${valueOrDash(event.type)} ${eventSummary(event)}`.trimEnd());
}

function eventSummary(event) {
	const payload = event && event.payload;
	if (!payload || typeof payload !== 'object' || Array.isArray(payload)) {
		return truncatePlain(payload === undefined ? '' : JSON.stringify(payload), 120);
	}
	const parts = [];
	for (const key of ['slice_id', 'phase', 'status', 'message', 'summary', 'error', 'command']) {
		const value = payload[key];
		if (value === undefined || value === null || String(value).trim() === '') continue;
		parts.push(`${key}=${truncatePlain(typeof value === 'string' ? value : JSON.stringify(value), 80)}`);
	}
	return truncatePlain(parts.length ? parts.join(' ') : JSON.stringify(payload), 160);
}

function outputTailLines(outputTail) {
	const trimmed = String(outputTail || '').trimEnd();
	if (!trimmed) return ['  -'];
	return trimmed
		.split(/\r?\n/)
		.slice(-OUTPUT_TAIL_LINES)
		.map((line) => `  ${truncatePlain(line, 160)}`);
}

function monitorMessage(details) {
	if (details.progress && details.progress.message && details.progress.message.trim()) {
		return details.progress.message;
	}
	if (details.run && details.run.error && details.run.error.trim()) {
		return details.run.error;
	}
	return details.run && details.run.status ? `run is ${details.run.status}` : '';
}

function monitorSliceLabel(details) {
	if (details.progress && details.progress.slice_id && details.progress.slice_id.trim()) {
		return details.progress.slice_id;
	}
	const sliceRuns = details.slice_runs || [];
	for (const status of ['running', 'repair_needed', 'ready_to_merge', 'pending']) {
		const match = sliceRuns.find((sliceRun) => sliceRun.status === status);
		if (match) return `${match.slice_id} (${match.status})`;
	}
	if (sliceRuns.length === 1) return `${sliceRuns[0].slice_id} (${sliceRuns[0].status})`;
	return valueOrDash(details.run && details.run.selected_slice_id);
}

function isTerminalStatus(status) {
	return TERMINAL_STATUSES.has(String(status || ''));
}

function formatElapsed(start) {
	if (!start) return '-';
	const startedAt = Date.parse(start);
	if (!Number.isFinite(startedAt)) return '-';
	const seconds = Math.max(0, Math.floor((Date.now() - startedAt) / 1000));
	const hours = Math.floor(seconds / 3600);
	const minutes = Math.floor((seconds % 3600) / 60);
	const rest = seconds % 60;
	if (hours > 0) return `${hours}h ${minutes}m ${rest}s`;
	if (minutes > 0) return `${minutes}m ${rest}s`;
	return `${rest}s`;
}

function eventTime(value) {
	const date = Date.parse(value || '');
	if (!Number.isFinite(date)) return '--:--:--';
	return new Date(date).toLocaleTimeString();
}

function valueOrDash(value) {
	if (value === undefined || value === null) return '-';
	const text = String(value).trim();
	return text ? text : '-';
}

function shellQuote(value) {
	const text = String(value);
	if (/^[A-Za-z0-9_@%+=:,./-]+$/.test(text)) return text;
	return `'${text.replace(/'/g, `'\\''`)}'`;
}

function stripAnsi(value) {
	return String(value).replace(/\x1b\[[0-?]*[ -/]*[@-~]/g, '');
}

function visibleWidth(value) {
	return Array.from(stripAnsi(value)).length;
}

function truncatePlain(value, maxWidth) {
	const text = String(value || '').replace(/[\r\n\t]+/g, ' ');
	if (visibleWidth(text) <= maxWidth) return text;
	const chars = Array.from(stripAnsi(text));
	return `${chars.slice(0, Math.max(0, maxWidth - 1)).join('')}…`;
}

function padRight(value, width) {
	const text = String(value || '');
	return text + ' '.repeat(Math.max(0, width - visibleWidth(text)));
}

module.exports = khazadMonitorExtension;
module.exports.default = khazadMonitorExtension;
