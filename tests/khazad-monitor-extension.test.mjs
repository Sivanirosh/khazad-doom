import test from 'node:test';
import assert from 'node:assert/strict';
import khazadMonitor from '../extensions/khazad-monitor/index.js';

const { KhazadMonitorOverlay } = khazadMonitor._test;

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
