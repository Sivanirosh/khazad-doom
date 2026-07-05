import test from 'node:test';
import assert from 'node:assert/strict';
import khazadMonitor from '../extensions/khazad-monitor/index.js';

const { AmbientRunTracker, KhazadMonitorOverlay, ambientWidgetLines, projectionLines } = khazadMonitor._test;

function plainTheme() {
	return {
		fg(_role, text) {
			return String(text || '');
		},
		bg(_role, text) {
			return String(text || '');
		},
		bold(text) {
			return String(text || '');
		},
		strikethrough(text) {
			return String(text || '');
		},
	};
}

function makeOverlay(rows = 20) {
	let renders = 0;
	const overlay = new KhazadMonitorOverlay({
		pi: { exec: async () => ({ code: 0, stdout: '{}', stderr: '' }) },
		config: {
			mode: 'run',
			runId: 'kd-20260626-abcdef',
			repo: '/tmp/repo',
			intervalMs: 1000,
			eventsLimit: 50,
			bin: 'khazad-doom',
		},
		theme: plainTheme(),
		tui: { terminal: { rows } },
		done: () => {},
		requestRender: () => {
			renders += 1;
		},
	});
	overlay.state = {
		loading: false,
		details: noisyDetails(),
		waitingRepo: undefined,
		error: '',
		lastUpdated: new Date('2026-06-26T20:35:00Z'),
		lastCommand: 'khazad-doom status --run kd-20260626-abcdef',
	};
	return { overlay, renders: () => renders };
}

function noisyDetails() {
	const now = '2026-06-26T20:35:00Z';
	return {
		run: {
			id: 'kd-20260626-abcdef',
			status: 'running',
			repo_path: '/tmp/example/repo',
			selected_slice_id: 'slice-001,slice-002,slice-003,slice-004,slice-005',
			started_at: '2026-06-26T20:34:00Z',
		},
		slice_runs: [
			{ slice_id: 'slice-001', status: 'merged', attempts: 1, commit_sha: 'aaaaaaaaaaaaaaaa' },
			{ slice_id: 'slice-002', status: 'running', attempts: 1 },
			{ slice_id: 'slice-003', status: 'pending' },
			{ slice_id: 'slice-004', status: 'pending' },
			{ slice_id: 'slice-005', status: 'pending' },
		],
		progress: {
			phase: 'worker_running',
			slice_id: 'slice-002',
			attempt: 1,
			message: 'slice worker is running',
			phase_started_at: '2026-06-26T20:34:30Z',
			updated_at: now,
			worker: {
				attempt_started_at: '2026-06-26T20:34:30Z',
				pid: 1234,
				process_observed_at: now,
				last_event_at: now,
				last_event_kind: 'stdout',
				last_semantic_progress_at: now,
				attempt_timeout_seconds: 0,
				no_output_warning_seconds: 0,
			},
			output_tail: Array.from({ length: 8 }, (_, index) => `tail line ${index + 1}`).join('\n'),
		},
		economics: {
			agent_call_count: 2,
			command_execution_count: 3,
			duplicate_command_count: 1,
			cache_hits: 4,
			cache_misses: 5,
			repair_policy: 'auto',
			repair_attempts: 0,
			repair_max_attempts: 1,
			gate_fail_fast: true,
			sla_violations: [],
		},
		events: Array.from({ length: 18 }, (_, index) => ({
			id: index + 1,
			created_at: `2026-06-26T20:34:${String(index).padStart(2, '0')}Z`,
			type: 'progress',
			payload: {
				phase: index % 2 === 0 ? 'worker_running' : 'worker_verify',
				slice_id: 'slice-002',
				message: `event message ${index + 1}`,
			},
		})),
	};
}

function duplicateProneDetails() {
	const details = noisyDetails();
	details.run.id = 'kd-20260626-211646-8af9eacc';
	details.run.selected_slice_id = 'KF-CHECK-VALIDATOR-01,KF-DASHBOARD-01,KF-DEMO-CURSUS-01,KF-HINT-LADDER-01,KF-PATTERN-TAGS-01';
	details.slice_runs = [
		{ slice_id: 'KF-CHECK-VALIDATOR-01', status: 'running' },
		{ slice_id: 'KF-DASHBOARD-01', status: 'pending' },
		{ slice_id: 'KF-DEMO-CURSUS-01', status: 'pending' },
		{ slice_id: 'KF-HINT-LADDER-01', status: 'pending' },
		{ slice_id: 'KF-PATTERN-TAGS-01', status: 'pending' },
	];
	details.progress.slice_id = 'KF-CHECK-VALIDATOR-01';
	details.progress.command = 'pi';
	details.progress.message = 'slice worker is running';
	details.events = [
		{
			id: 1,
			created_at: '2026-06-26T21:16:46Z',
			type: 'run_started',
			payload: {
				selected_slices: details.run.selected_slice_id.split(','),
			},
		},
		{ id: 2, created_at: '2026-06-26T21:16:46Z', type: 'progress', payload: { phase: 'started', message: 'run accepted by daemon' } },
		{ id: 3, created_at: '2026-06-26T21:16:46Z', type: 'progress', payload: { phase: 'integration_setup', message: 'creating integration worktree' } },
		{ id: 4, created_at: '2026-06-26T21:16:47Z', type: 'slice_started', payload: { slice_id: 'KF-CHECK-VALIDATOR-01' } },
		{ id: 5, created_at: '2026-06-26T21:16:47Z', type: 'progress', payload: { phase: 'worker_started', slice_id: 'KF-CHECK-VALIDATOR-01', message: 'slice worker started' } },
		{ id: 6, created_at: '2026-06-26T21:16:47Z', type: 'progress', payload: { phase: 'worker_running', slice_id: 'KF-CHECK-VALIDATOR-01', attempt: 1, command: 'pi', message: 'slice worker is running' } },
	];
	return details;
}

function sectionCount(lines, label) {
	const pattern = new RegExp(`│\\s+${label}\\s`);
	return lines.filter((line) => pattern.test(line) && !line.includes('└')).length;
}

test('khazad monitor overlay collapses duplicate historical sections into activity', () => {
	const { overlay } = makeOverlay(40);
	overlay.state.details = duplicateProneDetails();

	const lines = overlay.render(120);
	const text = lines.join('\n');

	assert.equal(sectionCount(lines, 'Todos'), 1);
	assert.equal(sectionCount(lines, 'Run'), 1);
	assert.equal(sectionCount(lines, 'Worker'), 1);
	assert.equal(sectionCount(lines, 'Activity'), 1);
	assert.equal(sectionCount(lines, 'Economics'), 1);
	assert.match(text, /Activity.*recent/);
	assert.match(text, /Agent calls: 2 \| Commands: 3/);
	assert.match(text, /Run \(started\): 5 selected slices/);
	assert.doesNotMatch(text, /Worker \(KF-CHECK-VALIDATOR-01 • attempt 1\): slice KF-CHECK-VALIDATOR-01/);
});

test('latest overlay keeps the last terminal run visible when no active run remains', async () => {
	const completed = duplicateProneDetails();
	completed.run.status = 'completed';
	completed.progress = {
		phase: 'completed',
		message: 'run completed; handoff artifacts are ready',
		updated_at: '2026-06-26T21:57:02Z',
	};
	let call = 0;
	let argsSeen = [];
	const overlay = new KhazadMonitorOverlay({
		pi: {
			exec: async (_bin, args) => {
				call += 1;
				argsSeen = args;
				return { code: 0, stdout: call === 1 ? JSON.stringify(completed) : 'null', stderr: '' };
			},
		},
		config: {
			mode: 'latest',
			repo: '/tmp/repo',
			intervalMs: 1000,
			eventsLimit: 50,
			bin: 'khazad-doom',
		},
		theme: plainTheme(),
		tui: { terminal: { rows: 30 } },
		done: () => {},
		requestRender: () => {},
	});

	await overlay.poll();
	assert.equal(overlay.state.details.run.id, completed.run.id);
	await overlay.poll();

	assert.equal(overlay.state.details.run.id, completed.run.id);
	assert.equal(overlay.state.details.run.status, 'completed');
	assert.equal(overlay.state.waitingRepo, undefined);
	assert.ok(argsSeen.includes('--include-terminal'));
});

test('khazad monitor overlay escalates completed runs with incidents', () => {
	const { overlay } = makeOverlay(40);
	const details = duplicateProneDetails();
	details.run.status = 'completed';
	details.events.push(
		{ id: 7, created_at: '2026-06-26T21:50:34Z', type: 'run_error', payload: { error: 'read slice for closing' } },
		{ id: 8, created_at: '2026-06-26T21:53:11Z', type: 'run_resumed', payload: {} },
		{ id: 9, created_at: '2026-06-26T21:53:41Z', type: 'integration_repair_completed', payload: { status: 'fixed', summary: 'stabilized flaky smoke' } },
		{ id: 10, created_at: '2026-06-26T21:56:47Z', type: 'run_incident', payload: { severity: 'warning', kind: 'slice_close_skipped', message: 'slice metadata missing' } },
	);
	overlay.state.details = details;

	const text = overlay.render(120).join('\n');

	assert.match(text, /Incidents/);
	assert.match(text, /run_error: read slice for closing/);
	assert.match(text, /run_resumed/);
	assert.match(text, /integration_repair_completed: fixed stabilized flaky smoke/);
	assert.match(text, /slice_close_skipped: slice metadata missing/);
});

test('khazad monitor overlay renders status incidents outside event tail', () => {
	const { overlay } = makeOverlay(40);
	const details = noisyDetails();
	details.events = [];
	details.incidents = [
		{ severity: 'warning', kind: 'slice_close_skipped', message: 'slice metadata missing' },
	];
	overlay.state.details = details;

	const text = overlay.render(120).join('\n');

	assert.match(text, /Incidents/);
	assert.match(text, /slice_close_skipped: slice metadata missing/);
});

test('khazad monitor overlay renders active parallel layer fields', () => {
	const { overlay } = makeOverlay(40);
	const details = noisyDetails();
	details.progress.parallel_layer = true;
	details.progress.parallel_slices = ['slice-001', 'slice-002'];
	details.progress.slice_id = 'slice-001,slice-002';
	details.progress.attempt = 1;
	overlay.state.details = details;

	const text = overlay.render(120).join('\n');

	assert.match(text, /Worker\s+\(parallel layer: slice-001, slice-002 • attempt 1 • now\)/);
	assert.match(text, /Parallel layer: slice-001, slice-002/);
});

test('khazad monitor overlay caps tall feeds and keeps a visible scrollbar/footer', () => {
	const rows = 20;
	const maxOverlayRows = Math.min(Math.floor((rows * 86) / 100), rows - 2);
	const { overlay } = makeOverlay(rows);

	const lines = overlay.render(72);

	assert.equal(lines.length, maxOverlayRows);
	assert.match(lines.at(-1), /^╰/);
	assert.ok(lines.some((line) => line.includes('┃')), 'expected a scrollbar thumb');
	assert.ok(lines.some((line) => line.includes('q/Esc detach')), 'expected fixed footer hints');
});

test('khazad monitor overlay scroll keys move the viewport and request render', () => {
	const { overlay, renders } = makeOverlay(18);
	const before = overlay.render(72).join('\n');

	overlay.handleInput('\x1b[B');
	const afterDown = overlay.render(72).join('\n');

	assert.equal(renders(), 1);
	assert.notEqual(afterDown, before);
	assert.equal(overlay.scrollOffset, 1);

	overlay.handleInput('\x1b[F');
	assert.equal(overlay.scrollOffset, overlay.maxScrollOffset);
});

test('khazad monitor overlay keeps short states compact', () => {
	const { overlay } = makeOverlay(20);
	overlay.state = {
		loading: true,
		details: undefined,
		waitingRepo: undefined,
		error: '',
		lastUpdated: undefined,
		lastCommand: 'khazad-doom status --latest',
	};

	const lines = overlay.render(72);

	assert.ok(lines.length < Math.min(Math.floor((20 * 86) / 100), 20 - 2));
	assert.match(lines.at(-1), /^╰/);
	assert.ok(!lines.some((line) => line.includes('┃')), 'short content should not show a scrollbar');
});

test('khazad monitor paints daemon projection blocks verbatim', () => {
	const feed = projectionFixture();
	const lines = projectionLines(feed, plainTheme()).join('\n');

	assert.match(lines, /Run/);
	assert.match(lines, /Worker needs answer/);
	assert.match(lines, /khazad-doom answer kd-run q-1 <answer>/);
});

test('khazad ambient widget uses projection and attention lines', () => {
	assert.deepEqual(ambientWidgetLines(projectionFixture()).slice(0, 2), [
		'Run kd-run is running',
		'slice-001: Worker needs answer — answer with: khazad-doom answer kd-run q-1 <answer>',
	]);
});

test('khazad ambient tracker suppresses pre-attach notifications and dedupes terminal transitions', () => {
	const notifications = [];
	const widgets = [];
	const tracker = new AmbientRunTracker({
		pi: { exec: async () => ({ code: 0, stdout: 'null', stderr: '' }) },
		ctx: {
			ui: {
				setWidget: (id, lines) => widgets.push({ id, lines }),
				notify: (text, level) => notifications.push({ text, level }),
			},
		},
		repo: '/tmp/repo',
		intervalMs: 1000,
		lingerMs: 0,
	});
	const details = detailsWithFeed('running', [{ id: 'q-1', state: 'pending', question: 'Old question' }]);
	tracker.attach(details);
	tracker.render(details);
	tracker.notifyTransitions(details);

	const withNewQuestion = detailsWithFeed('running', [
		{ id: 'q-1', state: 'pending', question: 'Old question' },
		{ id: 'q-2', state: 'pending', question: 'New question' },
	]);
	tracker.notifyTransitions(withNewQuestion);
	tracker.notifyTransitions(withNewQuestion);
	const terminal = detailsWithFeed('completed', []);
	tracker.notifyTransitions(terminal);
	tracker.notifyTransitions(terminal);

	assert.ok(widgets.some((item) => item.lines.includes('Run kd-run is running')));
	assert.equal(notifications.length, 2);
	assert.match(notifications[0].text, /q-2/);
	assert.equal(notifications[1].text, 'Run kd-run is completed');
});

function projectionFixture() {
	return {
		feed_version: 1,
		summary_line: 'Run kd-run is running',
		attention: [
			{
				text: 'slice-001: Worker needs answer — answer with: khazad-doom answer kd-run q-1 <answer>',
				role: 'attention',
			},
		],
		blocks: [
			{
				label: 'Attention',
				meta: '',
				lines: [
					{
						text: 'slice-001: Worker needs answer — answer with: khazad-doom answer kd-run q-1 <answer>',
						role: 'attention',
					},
				],
			},
			{ label: 'Run', meta: '(running)', lines: [{ text: 'Run kd-run is running', role: 'info' }] },
		],
	};
}

function detailsWithFeed(status, questions) {
	const feed = projectionFixture();
	feed.summary_line = `Run kd-run is ${status}`;
	if (questions.some((question) => question.id === 'q-2')) {
		feed.attention = [
			...feed.attention,
			{
				text: 'slice-001: New question — answer with: khazad-doom answer kd-run q-2 <answer>',
				role: 'attention',
			},
		];
	}
	return {
		run: { id: 'kd-run', status },
		feed,
		questions,
	};
}
