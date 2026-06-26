'use strict';

const DEFAULT_INTERVAL_MS = 1000;
const DEFAULT_EVENTS_LIMIT = 50;
const STATUS_TIMEOUT_MS = 8000;
const OUTPUT_TAIL_LINES = 4;
const FEED_BLOCK_LIMIT = 7;
const EVENT_TAIL_LINES = 6;
const MAX_TODO_ITEMS = 8;
const OVERLAY_WIDTH = 96;
const OVERLAY_MIN_WIDTH = 64;
const OVERLAY_MAX_HEIGHT = '86%';
const OVERLAY_MAX_HEIGHT_PERCENT = 86;
const OVERLAY_MARGIN = 1;
const TERMINAL_STATUSES = new Set(['completed', 'failed', 'blocked', 'cancelled', 'interrupted']);

function khazadMonitorExtension(pi) {
	pi.registerCommand('khazad-monitor', {
		description:
			'Open an optional Khazad-Doom activity-feed overlay. Usage: /khazad-monitor [--latest|--run <run-id>|<run-id>]',
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
							tui,
							done,
							requestRender: () => tui.requestRender(),
						});
						component.start();
						return component;
					},
					{
						overlay: true,
						overlayOptions: {
							width: OVERLAY_WIDTH,
							minWidth: OVERLAY_MIN_WIDTH,
							maxHeight: OVERLAY_MAX_HEIGHT,
							anchor: 'center',
							margin: OVERLAY_MARGIN,
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
	constructor({ pi, config, theme, tui, done, requestRender }) {
		this.pi = pi;
		this.config = config;
		this.theme = theme;
		this.tui = tui;
		this.done = done;
		this.requestRender = requestRender;
		this.timer = undefined;
		this.closed = false;
		this.inFlight = false;
		this.abortController = undefined;
		this.attachedRunId = undefined;
		this.feedRunId = undefined;
		this.feedEvents = [];
		this.feedEventKeys = new Set();
		this.scrollOffset = 0;
		this.maxScrollOffset = 0;
		this.scrollPageSize = 1;
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
			return;
		}
		const scroll = scrollInputAction(data, this.scrollPageSize);
		if (scroll) {
			this.applyScroll(scroll);
		}
	}

	applyScroll(action) {
		const previous = this.scrollOffset;
		if (action === 'top') {
			this.scrollOffset = 0;
		} else if (action === 'bottom') {
			this.scrollOffset = this.maxScrollOffset;
		} else {
			this.scrollOffset = clamp(this.scrollOffset + action, 0, this.maxScrollOffset);
		}
		if (this.scrollOffset !== previous) this.requestRender();
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
			const previousDetails = this.state.details;
			const keepLastTerminal = this.config.mode === 'latest'
				&& !details
				&& previousDetails
				&& previousDetails.run
				&& isTerminalStatus(previousDetails.run.status);
			const visibleDetails = details || (keepLastTerminal ? previousDetails : null);
			if (this.config.mode === 'latest' && details && details.run && !this.attachedRunId) {
				this.attachedRunId = details.run.id;
			}
			if (details && details.run) {
				this.rememberEvents(details.run.id, details.events || []);
			}
			this.state = {
				loading: false,
				details: visibleDetails || undefined,
				waitingRepo: this.config.mode === 'latest' && !visibleDetails ? this.config.repo : undefined,
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

	rememberEvents(runId, events) {
		if (!runId) return;
		if (this.feedRunId !== runId) {
			this.feedRunId = runId;
			this.feedEvents = [];
			this.feedEventKeys = new Set();
		}
		const ordered = [...(events || [])].sort(compareEvents);
		for (const event of ordered) {
			const key = eventKey(event);
			if (this.feedEventKeys.has(key)) continue;
			this.feedEventKeys.add(key);
			this.feedEvents.push(event);
		}
		if (this.feedEvents.length > 200) {
			this.feedEvents = this.feedEvents.slice(-200);
			this.feedEventKeys = new Set(this.feedEvents.map(eventKey));
		}
	}

	render(width) {
		const safeWidth = Math.max(28, width || 80);
		const innerWidth = Math.max(1, safeWidth - 2);
		const maxLines = this.overlayMaxLines();
		const top = overlayTopBorder(this.theme, innerWidth, ' Khazad-Doom Monitor ');
		if (maxLines <= 1) return [top];

		const bottom = overlayBottomBorder(this.theme, innerWidth);
		const availableRows = Math.max(0, maxLines - 2);
		const snapshot = this.snapshotLines();
		const { body, footer } = splitFixedFooter(snapshot, availableRows);
		const footerRows = footer.length;
		const bodyRows = Math.max(0, availableRows - footerRows);
		const needsScroll = bodyRows > 0 && body.length > bodyRows;
		this.maxScrollOffset = needsScroll ? body.length - bodyRows : 0;
		this.scrollOffset = clamp(this.scrollOffset, 0, this.maxScrollOffset);
		this.scrollPageSize = Math.max(1, bodyRows - 1);

		const visibleBody = bodyRows > 0 ? body.slice(this.scrollOffset, this.scrollOffset + bodyRows) : [];
		const blankRows = needsScroll ? Math.max(0, availableRows - visibleBody.length - footer.length) : 0;
		const visibleContent = footer.length
			? [...visibleBody, ...Array(blankRows).fill(''), ...footer]
			: [...visibleBody, ...Array(blankRows).fill('')];
		const lines = [top];
		for (let index = 0; index < visibleContent.length; index++) {
			const scrollChar = needsScroll ? scrollbarChar(index, bodyRows, body.length, this.scrollOffset) : undefined;
			lines.push(overlayLine(this.theme, visibleContent[index], innerWidth, scrollChar));
		}
		lines.push(bottom);
		return lines;
	}

	overlayMaxLines() {
		const terminalRows = Number(this.tui && this.tui.terminal && this.tui.terminal.rows) || 24;
		const available = Math.max(1, terminalRows - OVERLAY_MARGIN * 2);
		const percentageMax = Math.max(1, Math.floor((terminalRows * OVERLAY_MAX_HEIGHT_PERCENT) / 100));
		return Math.max(1, Math.min(available, percentageMax));
	}

	invalidate() {}

	snapshotLines() {
		const lines = [];

		if (this.state.loading) {
			lines.push(sectionHeading(this.theme, 'Run', 'loading'));
			lines.push(treeLine(this.theme, `command ${this.state.lastCommand}`, 'dim'));
			lines.push('', footerLine(this.theme, this.config, this.state.lastUpdated));
			return lines;
		}

		if (this.state.error) {
			lines.push(sectionHeading(this.theme, 'Monitor', 'unavailable'));
			lines.push(treeLine(this.theme, this.state.error, 'error'));
			lines.push(treeLine(this.theme, `command ${this.state.lastCommand}`, 'dim'));
			lines.push('');
		}

		if (this.state.details) {
			lines.push(...runDetailsLines(this.state.details, this.theme, this.feedEvents));
		} else if (!this.state.error || this.config.mode === 'latest') {
			lines.push(...waitingLines(this.state.waitingRepo || this.config.repo, this.theme));
		}

		lines.push('', footerLine(this.theme, this.config, this.state.lastUpdated));
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
		args: ['status', '--repo', config.repo, '--latest', '--include-terminal', '--events-limit', String(config.eventsLimit)],
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

function runDetailsLines(details, theme, rememberedEvents) {
	const lines = [];
	const todos = todoLines(details, theme);
	if (todos.length) {
		lines.push(...todos);
	}
	lines.push('', ...runSummaryLines(details, theme));

	const current = currentProgressBlock(details);
	if (current) {
		lines.push('', ...renderFeedBlock(current, theme));
		const warning = currentWorkerWarning(details);
		if (warning) {
			lines.push('', ...renderFeedBlock({ label: 'Warn', lines: [{ text: warning, role: 'warning' }, { text: 'wait, inspect, or cancel explicitly', role: 'dim' }] }, theme));
		}
	}

	const incidents = incidentLines(details, theme);
	if (incidents.length) {
		lines.push('', sectionHeading(theme, 'Incidents', `(${incidents.length})`), ...incidents);
	}

	const feedBlocks = buildFeedBlocks(details, rememberedEvents && rememberedEvents.length ? rememberedEvents : details.events || []);
	const visibleBlocks = feedBlocks
		.filter((block) => !current || (block.key !== current.key && block.semanticKey !== current.semanticKey))
		.slice(-FEED_BLOCK_LIMIT);
	const activity = feedBlocksToActivityLines(visibleBlocks, theme);
	if (activity.length) {
		lines.push('', sectionHeading(theme, 'Activity', `(${activity.length} recent)`), ...activity);
	}

	const outputTail = details.progress && details.progress.output_tail ? details.progress.output_tail : '';
	if (String(outputTail || '').trim()) {
		lines.push('', ...renderFeedBlock({ label: 'Tail', lines: outputTailBlockLines(outputTail) }, theme));
	}
	return lines;
}

function runSummaryLines(details, theme) {
	const run = details.run || {};
	const progress = details.progress || undefined;
	const phase = progress && progress.phase ? progress.phase : isTerminalStatus(run.status) ? run.status : 'unknown';
	const elapsedStart = progress && progress.phase_started_at ? progress.phase_started_at : run.started_at;
	const message = monitorMessage(details);
	const lines = [sectionHeading(theme, 'Run', `${statusIcon(run.status)} ${valueOrDash(run.status)} • ${shortRunId(run.id)}`)];
	lines.push(treeLine(theme, `phase ${valueOrDash(phase)} • elapsed ${formatElapsed(elapsedStart)}`));
	lines.push(treeLine(theme, `repo ${shortPath(valueOrDash(run.repo_path))}`, 'dim'));
	if (message) {
		lines.push(treeLine(theme, message, statusRole(run.status)));
	}
	return lines;
}

function todoLines(details, theme) {
	const items = selectedSliceItems(details);
	const lines = [sectionHeading(theme, 'Todos', `(${items.length} ${items.length === 1 ? 'item' : 'items'})`)];
	if (!items.length) {
		lines.push(treeLine(theme, 'no selected slices recorded', 'dim'));
		return lines;
	}
	for (const item of items.slice(0, MAX_TODO_ITEMS)) {
		lines.push(todoLine(item, theme));
	}
	if (items.length > MAX_TODO_ITEMS) {
		lines.push(fg(theme, 'dim', `  … ${items.length - MAX_TODO_ITEMS} more`));
	}
	return lines;
}

function selectedSliceItems(details) {
	const sliceRuns = details.slice_runs || [];
	if (sliceRuns.length) return sliceRuns;
	const selected = valueOrDash(details.run && details.run.selected_slice_id);
	if (selected === '-') return [];
	return selected
		.split(',')
		.map((sliceId) => sliceId.trim())
		.filter(Boolean)
		.map((slice_id) => ({ slice_id, status: 'selected' }));
}

function todoLine(sliceRun, theme) {
	const status = valueOrDash(sliceRun.status);
	const role = statusRole(status);
	const icon = fg(theme, role, sliceCheckbox(status));
	let label = valueOrDash(sliceRun.slice_id);
	if (isDoneSliceStatus(status)) label = strike(theme, label);
	label = fg(theme, role, label);
	const meta = [];
	if (status !== '-') meta.push(status);
	if (sliceRun.attempts) meta.push(`${sliceRun.attempts} ${Number(sliceRun.attempts) === 1 ? 'attempt' : 'attempts'}`);
	if (sliceRun.commit_sha) meta.push(shortSha(sliceRun.commit_sha));
	return `${icon} ${label}${meta.length ? fg(theme, 'dim', `  ${meta.join(' • ')}`) : ''}`;
}

function currentProgressBlock(details) {
	const progress = details.progress || undefined;
	const run = details.run || {};
	if (!progress) return undefined;
	if (isTerminalStatus(run.status) && isTerminalStatus(progress.phase)) return undefined;
	if (progress.worker) {
		return workerBlock(progress.worker, progress, details, { current: true });
	}
	if (progress.command) {
		return commandProgressBlock(progress, details, { current: true });
	}
	const phase = valueOrDash(progress.phase || (details.run && details.run.status));
	const block = {
		key: `current:${phase}:${progress.slice_id || ''}`,
		semanticKey: progressSemanticKey(progress),
		label: phaseLabel(phase),
		meta: '(now)',
		lines: [],
	};
	if (progress.slice_id) block.lines.push({ text: `slice ${progress.slice_id}` });
	if (progress.message) block.lines.push({ text: progress.message, role: statusRole(details.run && details.run.status) });
	if (progress.updated_at) block.lines.push({ text: `updated ${ageLabel(progress.updated_at)}`, role: 'dim' });
	return block.lines.length ? block : undefined;
}

function currentWorkerWarning(details) {
	const worker = details.progress && details.progress.worker;
	return worker ? workerQuietWarning(worker) : '';
}

function buildFeedBlocks(details, events) {
	return [...(events || [])]
		.sort(compareEvents)
		.map((event) => eventToBlock(event, details))
		.filter(Boolean);
}

function eventToBlock(event, details) {
	const type = valueOrDash(event && event.type);
	const payload = event && event.payload && typeof event.payload === 'object' && !Array.isArray(event.payload) ? event.payload : {};
	if (type === 'run_started') {
		const selected = Array.isArray(payload.selected_slices) ? payload.selected_slices : selectedSliceItems(details).map((item) => item.slice_id);
		return {
			key: `event:${eventKey(event)}`,
			label: 'Run',
			meta: '(started)',
			lines: [{ text: `${selected.length} selected ${selected.length === 1 ? 'slice' : 'slices'}`, role: 'dim' }],
		};
	}
	if (type === 'slice_started') {
		return {
			key: `event:${eventKey(event)}`,
			label: 'Worker',
			meta: payload.slice_id ? `(${payload.slice_id})` : '',
			lines: [{ text: 'slice worker started' }],
		};
	}
	if (type === 'slice_merged') {
		return {
			key: `event:${eventKey(event)}`,
			label: 'Todos',
			meta: payload.slice_id ? `(${payload.slice_id})` : '',
			lines: [{ raw: `☒ ${payload.slice_id || 'slice'}${payload.commit_sha ? fglessMeta(`merged • ${shortSha(payload.commit_sha)}`) : fglessMeta('merged')}` }],
		};
	}
	if (type === 'integration_repair_completed') {
		return {
			key: `event:${eventKey(event)}`,
			label: 'Repair',
			meta: payload.status ? `(${payload.status})` : '',
			lines: [{ text: payload.summary || 'integration repair completed', role: payload.status === 'failed' ? 'error' : 'dim' }],
		};
	}
	if (type === 'implementation_summary') {
		const gate = payload.integration_gate || {};
		const completed = Array.isArray(payload.completed_slices) ? payload.completed_slices.length : undefined;
		const lines = [];
		if (completed !== undefined) lines.push({ text: `${completed} completed slice${completed === 1 ? '' : 's'}` });
		if (gate.status || gate.summary) lines.push({ text: gate.summary || `integration gate ${gate.status}`, role: gate.status === 'passed' ? 'success' : statusRole(gate.status) });
		if (payload.final_sha) lines.push({ text: `final ${shortSha(payload.final_sha)}`, role: 'dim' });
		return { key: `event:${eventKey(event)}`, label: 'Summary', lines };
	}
	if (type === 'run_completed') {
		return { key: `event:${eventKey(event)}`, label: 'Run', meta: '(completed)', lines: [{ text: 'handoff artifacts are ready', role: 'success' }] };
	}
	if (type === 'worktrees_cleaned') {
		return { key: `event:${eventKey(event)}`, label: 'Cleanup', lines: [{ text: 'worker worktrees cleaned', role: 'dim' }] };
	}
	if (type === 'checkpoint_written') {
		const completed = Array.isArray(payload.completed_slices) ? payload.completed_slices.length : 0;
		const remaining = Array.isArray(payload.remaining_slices) ? payload.remaining_slices.length : 0;
		return {
			key: `event:${eventKey(event)}`,
			label: 'State',
			lines: [{ text: `checkpoint written • ${completed} done • ${remaining} remaining`, role: 'dim' }],
		};
	}
	if (type === 'progress') {
		return progressEventBlock(payload, details, event);
	}
	return genericEventBlock(type, payload, event);
}

function progressEventBlock(progress, details, event) {
	const phase = valueOrDash(progress.phase);
	if (phase === 'ready_to_merge') {
		return {
			key: progressKey(progress, event),
			label: 'Todos',
			meta: progress.slice_id ? `(${progress.slice_id})` : '',
			lines: [{ raw: `◐ ${progress.slice_id || 'slice'}${fglessMeta('ready to merge')}` }],
		};
	}
	if (phase === 'completed') {
		return undefined;
	}
	if (progress.command) {
		return commandProgressBlock(progress, details, { event });
	}
	const label = phaseLabel(phase);
	const lines = [];
	if (progress.slice_id) lines.push({ text: `slice ${progress.slice_id}` });
	if (progress.message) lines.push({ text: progress.message, role: progressRole(phase) });
	return lines.length ? { key: progressKey(progress, event), label, lines } : undefined;
}

function commandProgressBlock(progress, details, options = {}) {
	const phase = valueOrDash(progress.phase);
	const command = valueOrDash(progress.command);
	const label = commandBlockLabel(phase, command);
	const metaParts = [];
	if (label === 'Worker' && progress.slice_id) metaParts.push(progress.slice_id);
	if (label === 'Worker' && progress.attempt) metaParts.push(`attempt ${progress.attempt}`);
	if (label !== 'Worker') metaParts.push(command === '-' ? phase : commandMeta(command));
	if (options.current) metaParts.push('now');
	const lines = [];
	if (command !== '-' && !(label === 'Worker' && command === 'pi')) lines.push({ text: command, role: 'dim' });
	const scope = progress.slice_id ? `slice ${progress.slice_id}` : phase.replace(/_/g, ' ');
	const elapsed = progress.phase_started_at ? ` • elapsed ${formatElapsed(progress.phase_started_at)}` : '';
	lines.push({ text: `${scope}${elapsed}` });
	if (progress.message) lines.push({ text: progress.message, role: progressRole(phase) });
	if (options.current && progress.updated_at) lines.push({ text: `updated ${ageLabel(progress.updated_at)}`, role: 'dim' });
	return { key: progressKey(progress, options.event), semanticKey: progressSemanticKey(progress), label, meta: metaParts.length ? `(${metaParts.join(' • ')})` : '', lines };
}

function progressSemanticKey(progress) {
	return [progress.phase || '', progress.slice_id || '', progress.attempt || '', progress.command || '', progress.message || ''].join('\0');
}

function workerBlock(worker, progress, details, options = {}) {
	const slice = progress && progress.slice_id ? progress.slice_id : monitorSliceLabel(details);
	const meta = [];
	if (slice !== '-') meta.push(slice);
	if (progress && progress.attempt) meta.push(`attempt ${progress.attempt}`);
	if (options.current) meta.push('now');
	return {
		key: progressKey(progress || {}, options.event),
		semanticKey: progressSemanticKey(progress || {}),
		label: 'Worker',
		meta: meta.length ? `(${meta.join(' • ')})` : '',
		lines: [
			{ text: `Supervisor: ${supervisorLabel(worker)}` },
			{ text: `Process: ${workerProcessLabel(worker)}` },
			{ text: `Runtime: ${formatElapsed(worker.attempt_started_at)}` },
			{ text: `Last worker event: ${lastWorkerEventLabel(worker)}` },
			{ text: `Last semantic progress: ${worker.last_semantic_progress_at ? ageLabel(worker.last_semantic_progress_at) : 'unknown'}` },
			{ text: `Timeout: ${workerTimeoutLabel(worker)}` },
		],
	};
}

function genericEventBlock(type, payload, event) {
	const summary = eventSummary({ payload });
	return summary ? { key: `event:${eventKey(event)}`, label: eventLabel(type), lines: [{ text: summary, role: type === 'error' || type === 'blocked' ? 'error' : 'dim' }] } : undefined;
}

function incidentLines(details, theme) {
	const incidents = (details.events || [])
		.map(incidentSummary)
		.filter(Boolean)
		.slice(-8);
	return incidents.map((incident) => treeLine(theme, truncatePlain(incident, 180), 'warning'));
}

function incidentSummary(event) {
	const type = event && event.type ? String(event.type) : '';
	const payload = (event && event.payload) || {};
	if (type === 'run_error') return `run_error: ${valueOrDash(payload.error || payload.message)}`;
	if (type === 'run_resumed') return 'run_resumed';
	if (type === 'worktree_cleanup_error' || type === 'daemon_recovery_cleanup_error') return `${type}: ${valueOrDash(payload.error || payload.message)}`;
	if (type === 'integration_repair_completed') return `integration_repair_completed: ${[payload.status, payload.summary].filter(Boolean).join(' ') || '-'}`;
	if (type === 'run_incident') return `${valueOrDash(payload.kind || type)}: ${valueOrDash(payload.message || payload.error)}`;
	return '';
}

function feedBlocksToActivityLines(blocks, theme) {
	return blocks.map((block) => {
		const heading = [block.label, block.meta].filter(Boolean).join(' ');
		const summary = feedBlockSummary(block);
		const text = summary ? `${heading}: ${summary}` : heading;
		return treeLine(theme, truncatePlain(text, 180), feedBlockRole(block));
	});
}

function feedBlockSummary(block) {
	return (block.lines || [])
		.map((line) => {
			if (line.raw !== undefined) return stripAnsi(String(line.raw)).trim();
			return stripAnsi(String(line.text || '')).trim();
		})
		.filter(Boolean)
		.join(' • ');
}

function feedBlockRole(block) {
	const line = (block.lines || []).find((candidate) => candidate && candidate.role);
	return line ? line.role : 'dim';
}

function renderFeedBlock(block, theme) {
	const lines = [sectionHeading(theme, block.label, block.meta || '')];
	for (const line of block.lines || []) {
		if (line.raw !== undefined) {
			lines.push(styleRawTodoLine(String(line.raw), theme));
		} else {
			lines.push(treeLine(theme, line.text, line.role));
		}
	}
	return lines;
}

function outputTailBlockLines(outputTail) {
	const trimmed = String(outputTail || '').trimEnd();
	if (!trimmed) return [{ text: '-', role: 'dim' }];
	return trimmed
		.split(/\r?\n/)
		.slice(-OUTPUT_TAIL_LINES)
		.map((line) => ({ text: truncatePlain(line, 160), role: 'dim' }));
}

function styleRawTodoLine(line, theme) {
	const status = line.startsWith('☒') ? 'merged' : line.startsWith('◐') ? 'running' : line.startsWith('✗') ? 'failed' : 'pending';
	const role = statusRole(status);
	if (!line.includes('  ')) return fg(theme, role, line);
	const [head, ...rest] = line.split('  ');
	return `${fg(theme, role, head)}${fg(theme, 'dim', `  ${rest.join('  ')}`)}`;
}

function fglessMeta(text) {
	return `  ${text}`;
}

function progressKey(progress, event) {
	return event ? `event:${eventKey(event)}` : `current:${progress.phase || ''}:${progress.slice_id || ''}:${progress.command || ''}`;
}

function compareEvents(left, right) {
	const leftId = Number(left && left.id);
	const rightId = Number(right && right.id);
	if (Number.isFinite(leftId) && Number.isFinite(rightId) && leftId !== rightId) return leftId - rightId;
	const leftTime = Date.parse((left && left.created_at) || '');
	const rightTime = Date.parse((right && right.created_at) || '');
	if (Number.isFinite(leftTime) && Number.isFinite(rightTime) && leftTime !== rightTime) return leftTime - rightTime;
	return String(eventKey(left)).localeCompare(String(eventKey(right)));
}

function eventKey(event) {
	if (!event) return 'missing';
	if (event.id !== undefined && event.id !== null) return `id:${event.id}`;
	return `${event.created_at || ''}:${event.type || ''}:${JSON.stringify(event.payload || {}).slice(0, 200)}`;
}

function footerLine(theme, config, updatedAt) {
	const updated = updatedAt ? `updated ${updatedAt.toLocaleTimeString()}` : 'waiting';
	const scope = config.mode === 'run' ? `run ${config.runId}` : `latest ${shortPath(config.repo)}`;
	return fg(theme, 'dim', `↑↓/Pg scroll • q/Esc detach • r refresh • ${updated} • ${scope}`);
}

function phaseLabel(phase) {
	const normalized = String(phase || '').toLowerCase();
	if (normalized.startsWith('worker')) return normalized === 'worker_verify' ? 'Shell' : 'Worker';
	if (normalized.includes('gate')) return 'Shell';
	if (normalized.includes('merge')) return 'Merge';
	if (normalized.includes('repair')) return 'Repair';
	if (normalized === 'ready_to_merge') return 'Todos';
	if (normalized === 'completed' || normalized === 'started' || normalized === 'integration_setup') return 'Run';
	return titleCase(normalized.replace(/_/g, ' ') || 'Activity');
}

function commandBlockLabel(phase, command) {
	const normalized = String(phase || '').toLowerCase();
	const text = String(command || '').toLowerCase();
	if (normalized === 'worker_running' || text === 'pi') return 'Worker';
	if (normalized.includes('merge') || text.startsWith('git merge')) return 'Merge';
	if (normalized.includes('repair')) return 'Repair';
	return 'Shell';
}

function commandMeta(command) {
	let text = String(command || '').trim();
	text = text.replace(/^(?:[A-Za-z_][A-Za-z0-9_]*=(?:"[^"]*"|'[^']*'|\S+)\s*)+/, '');
	if (text.startsWith('PATH=')) text = text.replace(/^PATH=(?:"[^"]*"|'[^']*'|\S+)\s*/, '');
	return truncatePlain(text || command || '-', 34);
}

function progressRole(phase) {
	const normalized = String(phase || '').toLowerCase();
	if (normalized === 'completed' || normalized === 'ready_to_merge') return 'success';
	if (normalized.includes('failed') || normalized.includes('blocked')) return 'error';
	if (normalized.includes('repair')) return 'warning';
	if (normalized.includes('worker') || normalized.includes('gate') || normalized.includes('merge')) return 'accent';
	return 'dim';
}

function eventLabel(type) {
	return titleCase(String(type || 'activity').replace(/_/g, ' '));
}

function titleCase(value) {
	return String(value || '')
		.split(/\s+/)
		.filter(Boolean)
		.map((part) => part.charAt(0).toUpperCase() + part.slice(1))
		.join(' ');
}

function shortRunId(value) {
	const text = String(value || '').trim();
	if (text.length <= 30) return text || '-';
	return `${text.slice(0, 11)}…${text.slice(-10)}`;
}

function shortPath(value) {
	const text = String(value || '').trim();
	if (!text || text === '-') return '-';
	const parts = text.split('/').filter(Boolean);
	if (parts.length <= 2) return text;
	return `…/${parts.slice(-2).join('/')}`;
}

function workerProcessLabel(worker) {
	return worker.pid ? `running pid=${worker.pid}` : 'running';
}

function overlayTopBorder(theme, width, title) {
	const label = ` ${title.trim()} `;
	if (visibleWidth(label) >= width - 2) {
		return border(theme, `╭${'─'.repeat(width)}╮`);
	}
	const left = Math.floor((width - visibleWidth(label)) / 2);
	const right = Math.max(0, width - visibleWidth(label) - left);
	return border(theme, `╭${'─'.repeat(left)}`) + fg(theme, 'accent', bold(theme, label)) + border(theme, `${'─'.repeat(right)}╮`);
}

function overlayBottomBorder(theme, width) {
	return border(theme, `╰${'─'.repeat(width)}╯`);
}

function overlayLine(theme, value, width, scrollChar) {
	const text = String(value || '');
	const hasScrollColumn = scrollChar !== undefined;
	const textWidth = hasScrollColumn ? Math.max(1, width - 1) : width;
	const content = text ? ` ${text}` : '';
	const body = padRight(truncatePlain(content, textWidth), textWidth);
	const bar = hasScrollColumn ? fg(theme, scrollChar === '┃' ? 'accent' : 'dim', scrollChar) : '';
	const padded = bg(theme, 'customMessageBg', `${body}${bar}`);
	return border(theme, '│') + padded + border(theme, '│');
}

function splitFixedFooter(lines, availableRows) {
	const all = [...(lines || [])];
	if (availableRows < 4 || all.length === 0) return { body: all, footer: [] };
	let footerStart = all.length - 1;
	if (footerStart > 0 && all[footerStart - 1] === '') footerStart--;
	const footer = all.slice(footerStart);
	if (footer.length >= availableRows) return { body: all, footer: [] };
	return { body: all.slice(0, footerStart), footer };
}

function scrollbarChar(row, viewportRows, totalRows, offset) {
	if (row >= viewportRows) return ' ';
	if (viewportRows <= 0 || totalRows <= viewportRows) return ' ';
	const thumbSize = Math.max(1, Math.floor((viewportRows * viewportRows) / totalRows));
	const maxThumbTop = Math.max(0, viewportRows - thumbSize);
	const maxOffset = Math.max(1, totalRows - viewportRows);
	const thumbTop = Math.round((clamp(offset, 0, maxOffset) / maxOffset) * maxThumbTop);
	return row >= thumbTop && row < thumbTop + thumbSize ? '┃' : '│';
}

function scrollInputAction(data, pageSize) {
	const text = String(data || '');
	const lowered = text.toLowerCase();
	if (text === '\x1b[A' || lowered === 'up' || lowered === 'arrowup' || text === 'k') return -1;
	if (text === '\x1b[B' || lowered === 'down' || lowered === 'arrowdown' || text === 'j') return 1;
	if (text === '\x1b[5~' || lowered === 'pageup' || lowered === 'page-up') return -Math.max(1, pageSize || 1);
	if (text === '\x1b[6~' || lowered === 'pagedown' || lowered === 'page-down') return Math.max(1, pageSize || 1);
	if (text === '\x1b[H' || text === '\x1bOH' || text === '\x1b[1~' || lowered === 'home') return 'top';
	if (text === '\x1b[F' || text === '\x1bOF' || text === '\x1b[4~' || lowered === 'end') return 'bottom';
	return undefined;
}

function clamp(value, min, max) {
	return Math.max(min, Math.min(max, Number(value) || 0));
}

function border(theme, text) {
	return fg(theme, 'borderAccent', text);
}

function sectionHeading(theme, label, meta = '') {
	const heading = chip(theme, label);
	return meta ? `${heading} ${fg(theme, 'dim', meta)}` : heading;
}

function chip(theme, label) {
	const text = ` ${label} `;
	const styled = fg(theme, 'text', bold(theme, text));
	return bg(theme, 'selectedBg', styled);
}

function treeLine(theme, text, role) {
	const body = role ? fg(theme, role, text) : String(text || '');
	return `${fg(theme, 'dim', '└')} ${body}`;
}

function fg(theme, role, text) {
	try {
		return theme && typeof theme.fg === 'function' ? theme.fg(role, String(text || '')) : String(text || '');
	} catch (_error) {
		return String(text || '');
	}
}

function bg(theme, role, text) {
	try {
		return theme && typeof theme.bg === 'function' ? theme.bg(role, String(text || '')) : String(text || '');
	} catch (_error) {
		return String(text || '');
	}
}

function bold(theme, text) {
	try {
		return theme && typeof theme.bold === 'function' ? theme.bold(String(text || '')) : String(text || '');
	} catch (_error) {
		return String(text || '');
	}
}

function strike(theme, text) {
	try {
		return theme && typeof theme.strikethrough === 'function' ? theme.strikethrough(String(text || '')) : String(text || '');
	} catch (_error) {
		return String(text || '');
	}
}

function statusIcon(status) {
	const normalized = String(status || '').toLowerCase();
	if (['completed', 'merged', 'passed', 'success'].includes(normalized)) return '✓';
	if (normalized === 'running') return '●';
	if (normalized === 'ready_to_merge') return '◆';
	if (normalized === 'repair_needed') return '↻';
	if (normalized === 'pending') return '○';
	if (normalized === 'blocked') return '!';
	if (normalized === 'failed') return '✗';
	if (normalized === 'cancelled' || normalized === 'interrupted') return '×';
	return '•';
}

function sliceCheckbox(status) {
	const normalized = String(status || '').toLowerCase();
	if (isDoneSliceStatus(normalized)) return '☒';
	if (['running', 'ready_to_merge', 'repair_needed'].includes(normalized)) return '◐';
	if (['failed', 'blocked', 'cancelled', 'interrupted'].includes(normalized)) return '✗';
	return '☐';
}

function statusRole(status) {
	const normalized = String(status || '').toLowerCase();
	if (['completed', 'merged', 'passed', 'success'].includes(normalized)) return 'success';
	if (['failed', 'blocked'].includes(normalized)) return 'error';
	if (['cancelled', 'interrupted', 'repair_needed'].includes(normalized)) return 'warning';
	if (['running', 'ready_to_merge', 'selected'].includes(normalized)) return 'accent';
	return 'dim';
}

function isDoneSliceStatus(status) {
	return ['completed', 'merged', 'passed', 'success'].includes(String(status || '').toLowerCase());
}

function shortSha(value) {
	const text = String(value || '').trim();
	return text ? text.slice(0, 8) : '';
}

function ageLabel(value) {
	const elapsed = formatElapsed(value);
	return elapsed === '-' ? '-' : `${elapsed} ago`;
}

function supervisorLabel(worker) {
	return worker.process_observed_at ? `alive, observed child ${formatElapsed(worker.process_observed_at)} ago` : 'starting, no child observation yet';
}

function lastWorkerEventLabel(worker) {
	if (!worker.last_event_at) return 'none';
	const kind = worker.last_event_kind ? ` (${worker.last_event_kind})` : '';
	return `${formatElapsed(worker.last_event_at)} ago${kind}`;
}

function workerTimeoutLabel(worker) {
	const timeoutSeconds = Number(worker.attempt_timeout_seconds || 0);
	if (!timeoutSeconds) return 'disabled';
	const startedAt = Date.parse(worker.attempt_started_at || '');
	if (!Number.isFinite(startedAt)) return `${timeoutSeconds}s`;
	const elapsedSeconds = Math.max(0, Math.floor((Date.now() - startedAt) / 1000));
	if (elapsedSeconds >= timeoutSeconds) {
		return `${timeoutSeconds}s, exceeded by ${formatDurationSeconds(elapsedSeconds - timeoutSeconds)}`;
	}
	return `${timeoutSeconds}s, remaining ${formatDurationSeconds(timeoutSeconds - elapsedSeconds)}`;
}

function workerQuietWarning(worker) {
	const warningSeconds = Number(worker.no_output_warning_seconds || 0);
	if (!warningSeconds) return '';
	const reference = Date.parse(worker.last_event_at || worker.attempt_started_at || '');
	if (!Number.isFinite(reference)) return '';
	const quietSeconds = Math.max(0, Math.floor((Date.now() - reference) / 1000));
	if (quietSeconds < warningSeconds) return '';
	const suffix = Number(worker.attempt_timeout_seconds || 0) === 0 ? '; no timeout configured' : '';
	return `worker is quiet for ${formatDurationSeconds(quietSeconds)}; this may be normal${suffix}`;
}

function waitingLines(repo, theme) {
	return [
		sectionHeading(theme, 'Run', 'waiting'),
		treeLine(theme, `repo ${valueOrDash(repo)}`),
		treeLine(theme, 'waiting for the latest active daemon-owned run', 'dim'),
		'',
		sectionHeading(theme, 'Hint'),
		treeLine(theme, 'start a run normally; this overlay will attach when status --latest returns one', 'dim'),
	];
}

function eventLines(events, theme) {
	if (!events.length) return [treeLine(theme, '-', 'dim')];
	return events.slice(-EVENT_TAIL_LINES).map((event) => {
		const summary = eventSummary(event);
		const type = valueOrDash(event.type);
		const text = summary ? `${eventTime(event.created_at)} ${type} • ${summary}` : `${eventTime(event.created_at)} ${type}`;
		return treeLine(theme, text, type === 'error' || type === 'blocked' ? 'error' : 'dim');
	});
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

function outputTailLines(outputTail, theme) {
	const trimmed = String(outputTail || '').trimEnd();
	if (!trimmed) return [treeLine(theme, '-', 'dim')];
	return trimmed
		.split(/\r?\n/)
		.slice(-OUTPUT_TAIL_LINES)
		.map((line) => treeLine(theme, truncatePlain(line, 160), 'dim'));
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
	return formatDurationSeconds(Math.max(0, Math.floor((Date.now() - startedAt) / 1000)));
}

function formatDurationSeconds(seconds) {
	const safeSeconds = Math.max(0, Number(seconds) || 0);
	const hours = Math.floor(safeSeconds / 3600);
	const minutes = Math.floor((safeSeconds % 3600) / 60);
	const rest = safeSeconds % 60;
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
module.exports._test = {
	KhazadMonitorOverlay,
	scrollInputAction,
	scrollbarChar,
	splitFixedFooter,
};
