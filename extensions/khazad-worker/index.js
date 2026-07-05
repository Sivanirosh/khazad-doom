'use strict';

const net = require('node:net');

function khazadWorkerExtension(pi) {
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

module.exports = khazadWorkerExtension;
