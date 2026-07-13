import assert from 'node:assert/strict';
import { access, readFile } from 'node:fs/promises';
import test from 'node:test';

const skillDir = new URL('../skills/khazad-doom/', import.meta.url);

async function readSkillFile(name) {
	return readFile(new URL(name, skillDir), 'utf8');
}

function markdownTargets(markdown) {
	return [...markdown.matchAll(/\[[^\]]+\]\(([^)#]+\.md)(?:#[^)]+)?\)/g)]
		.map((match) => match[1]);
}

function fencedCodeBlocks(markdown) {
	return [...markdown.matchAll(/```[^\n]*\n([\s\S]*?)```/g)]
		.map((match) => match[1]);
}

function fencedCode(markdown) {
	return fencedCodeBlocks(markdown).join('\n');
}

function numberedStepSections(markdown) {
	const headings = [...markdown.matchAll(/^## \d+\. .+$/gm)];
	return headings.map((heading, index) => {
		const end = headings[index + 1]?.index ?? markdown.length;
		return markdown.slice(heading.index, end);
	});
}

test('khazad-doom skill is a model-invoked branch router with live context pointers', async () => {
	const skill = await readSkillFile('SKILL.md');
	const frontmatter = skill.match(/^---\n([\s\S]*?)\n---/);
	assert.ok(frontmatter, 'skill frontmatter is missing');
	assert.match(frontmatter[1], /^description: /m);
	assert.doesNotMatch(frontmatter[1], /disable-model-invocation/);
	assert.doesNotMatch(frontmatter[1], /argument-hint/);

	const targets = markdownTargets(skill).sort();
	assert.deepEqual(targets, ['OPERATIONS.md', 'RUNS.md', 'SLICES.md']);
	for (const target of targets) {
		await access(new URL(target, skillDir));
	}

	const bodyWords = skill.replace(frontmatter[0], '').trim().split(/\s+/).length;
	assert.ok(bodyWords <= 500, `primary skill regrew branch reference (${bodyWords} words)`);
});

test('the router and every disclosed step have checkable completion criteria', async () => {
	const skill = await readSkillFile('SKILL.md');
	assert.match(skill, /\*\*Routing is complete when:\*\*/);

	for (const name of ['SLICES.md', 'RUNS.md', 'OPERATIONS.md']) {
		const sections = numberedStepSections(await readSkillFile(name));
		assert.ok(sections.length > 0, `${name} has no numbered steps`);
		for (const section of sections) {
			assert.match(section, /\*\*[^*]+ is complete when:\*\*/i, `${name} has a step without a completion criterion`);
		}
	}
});

test('slice discovery isolates requested setup mutations', async () => {
	const blocks = fencedCodeBlocks(await readSkillFile('SLICES.md'));
	const discovery = blocks.find((block) => block.includes('khazad-doom slices list'));
	const setup = blocks.find((block) => block.includes('khazad-doom init'));
	assert.ok(discovery, 'read-only discovery block is missing');
	assert.doesNotMatch(discovery, /khazad-doom init|--write/);
	assert.match(setup ?? '', /khazad-doom slices schema --write/);
});

test('ordinary KD runs keep cockpit policy implicit', async () => {
	const skill = await readSkillFile('SKILL.md');
	const runs = await readSkillFile('RUNS.md');
	const examples = fencedCode(`${skill}\n${runs}`);

	assert.match(runs, /Ordinary real runs omit `--cockpit`\./);
	assert.match(runs, /`--cockpit direct`: operator-requested headless execution or a bounded test/);
	assert.match(runs, /Whenever an exception is selected, state its reason/);
	assert.match(runs, /Override: <override> — <reason>\./);
	assert.doesNotMatch(examples, /khazad-doom run[^\n]*--cockpit\s+(?:direct|herdr)/);
	assert.doesNotMatch(skill, /khazad-doom run[^\n]*--cockpit\s+direct/);
});

test('run-start branch preserves the non-blocking daemon handoff', async () => {
	const runs = await readSkillFile('RUNS.md');
	assert.match(runs, /For real Pi work, launch without `--wait`\./);
	assert.match(runs, /Started KD run `<run-id>` in the background\./);
	assert.match(runs, /Monitor: `<run_monitor_command>`/);
	assert.match(runs, /End the turn after this handoff\./);
	assert.doesNotMatch(fencedCode(runs), /khazad-doom run[^\n]*--wait/);
});

test('resume handoff constructs the monitor command from its run id', async () => {
	const runs = await readSkillFile('RUNS.md');
	assert.match(runs, /unlike run start, resume returns only the run ID/);
	assert.match(runs, /`khazad-doom monitor --run <run-id>`/);
	assert.match(runs, /The initial `run` command accepts `--origin-notification-target/);
});
