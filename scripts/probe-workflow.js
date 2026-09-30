export const meta = {
  name: 'ultracode-probe',
  description: 'Check that workflow agents run inside the Claustrum sandbox (2 agents + 1 summary)',
  phases: [{ title: 'Probe' }, { title: 'Summarize' }],
}

// Checks that workflow agents stay in the Claustrum sandbox (see "Subagents and
// workflows" in the user guide). Ask Claude to run this file as a workflow,
// passing the script inline; `leakedBuiltins` in the result must be empty.
// Rerun after upgrading Claude Code.

const REPORT = {
  type: 'object',
  properties: {
    tools: { type: 'array', items: { type: 'string' } },
    output: { type: 'string' },
  },
  required: ['tools', 'output'],
}

const PROBES = [
  { key: 'pwd', command: 'pwd && ls /workspace | head -3' },
  { key: 'git', command: 'command -v git || echo "no git"' },
]

const probes = await parallel(PROBES.map(p => () =>
  agent(
    `Run exactly one command with the mcp__claustrum__Bash tool and nothing else: \`${p.command}\`. ` +
    'Report the exact names of all tools you have and the verbatim command output.',
    { label: `probe:${p.key}`, phase: 'Probe', schema: REPORT },
  ).then(r => ({ key: p.key, ...r }))
))

const builtins = ['Bash', 'Read', 'Write', 'Edit', 'Glob', 'Grep', 'WebFetch']
const leaked = probes.flatMap(p => p.tools.filter(t => builtins.includes(t)))

const summary = await agent(
  'Without calling any tool, summarize in two sentences whether these probe agents ran ' +
  `inside a sandbox (no git, /workspace as cwd) and whether they had host built-ins:\n${JSON.stringify(probes, null, 2)}`,
  { label: 'summary', phase: 'Summarize' },
)

return { probes, leakedBuiltins: leaked, summary }
