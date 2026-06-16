// arena — blind A/B eval of a prompt/skill change.
//
// Condenses the ad-hoc "bake-off": run an OLD and a NEW version of an artifact
// (a prompt or skill) against the SAME input, then have diverse-lens judges
// blind-score the two outputs and report an honest verdict (ties / old-wins
// included). Re-invokable any time a prompt changes.
//
// INVOKE:  Workflow({ name: "arena", args: { ... } })
//
// args (all optional except a way to name the two variants):
//   Convenience for a skill template change:
//     { skill: "fw-plan", oldRef: "HEAD~1" }
//       -> A = `git show <oldRef>:_skill_templates/<skill>.md`  (default oldRef HEAD)
//          B = the working-tree _skill_templates/<skill>.md
//   Explicit variants (any artifact):
//     { a: { label, source }, b: { label, source }, task: "what the artifact does" }
//       source is one of: { git: "REF:PATH" } | { file: "PATH" } | { text: "<inline>" }
//   Input (the shared thing both variants process):
//     { input: { kind: "generated", domain: "<describe a realistic input>" } }   (default)
//     { input: { kind: "file", path: "<path>" } }                               (a real artifact)
//   Judging:
//     { rubric: ["dim1", "dim2", ...] }   // defaults to a general rubric
//     { judges: 3 }                        // number of diverse-lens judges
//
// EXAMPLES:
//   Workflow({ name:"arena", args:{ skill:"fw-plan", oldRef:"HEAD~1",
//     input:{ kind:"file", path:"docs/some-plan.md" },
//     rubric:["grader strength","decomposition","schema fidelity"] }})
//   Workflow({ name:"arena", args:{
//     a:{ label:"terse", source:{ text:"Summarize the input in one sentence." }},
//     b:{ label:"structured", source:{ text:"Summarize the input as three bullets." }},
//     task:"a summarizer prompt", input:{ kind:"generated", domain:"a product changelog" }}})

export const meta = {
  name: 'arena',
  description: 'Blind A/B eval of a prompt/skill change: run an old and a new variant on the same input, then diverse-lens judges score the two outputs and report an honest verdict.',
  whenToUse: 'After changing a prompt or skill, to prove the new version beats the old (or honestly surface a tie / regression) before committing. Invoke with Workflow({name:"arena", args:{...}}).',
  phases: [
    { title: 'Scenario', detail: 'invent or load the shared input both variants will process' },
    { title: 'Run', detail: 'the old and new variant each produce output from that input' },
    { title: 'Judge', detail: 'diverse-lens judges blind-score both outputs on the rubric' },
  ],
}

const A = args || {}
const SKILL_DIR = 'packages/flywheel-orchestrator/src/flywheel_orchestrator/_skill_templates/'

function resolveVariants() {
  if (A.skill) {
    const path = SKILL_DIR + A.skill + '.md'
    const oldRef = A.oldRef || 'HEAD'
    return {
      a: { label: 'OLD (' + oldRef + ')', source: { git: oldRef + ':' + path } },
      b: { label: 'NEW (working tree)', source: { file: path } },
      task: A.task || ('the /' + A.skill + ' skill (' + A.skill + '.md)'),
    }
  }
  return { a: A.a, b: A.b, task: A.task || 'the artifact under test' }
}
const V = resolveVariants()

const input = A.input || { kind: 'generated', domain: A.domain || 'a realistic, representative use of this artifact' }
const rubric = A.rubric && A.rubric.length
  ? A.rubric
  : ['Best achieves the artifact intent (does the job it exists to do)', 'Correctness and rigor (no errors or hand-waving)', 'Robustness against shortcuts, edge cases, and gaming', 'Clarity and usefulness to whoever must act on it']
const numJudges = A.judges || 3

const LENSES = [
  { key: 'intent-fidelity', focus: 'Which output best achieves the artifact stated intent and does the job it exists to do?' },
  { key: 'rigor-correctness', focus: 'Which output is more correct and rigorous, with fewer errors, gaps, or hand-waving?' },
  { key: 'robustness-gaming', focus: 'Which output is harder to game and more robust to shortcuts, edge cases, and adversarial inputs?' },
  { key: 'clarity-usefulness', focus: 'Which output is clearer and more useful to the person who has to act on it?' },
  { key: 'adoption-ergonomics', focus: 'Which output could an operator actually use to completion with less friction and less risk?' },
]
const lenses = LENSES.slice(0, Math.max(1, Math.min(numJudges, LENSES.length)))

function loadInstr(source) {
  if (!source) return 'No artifact source was provided.'
  if (source.git) return 'Load the artifact: run `git show ' + source.git + '` and use its FULL output as your instructions.'
  if (source.file) return 'Load the artifact: read the file ' + source.file + ' in full and use it as your instructions.'
  if (source.text) return 'Your instructions are exactly:\n"""\n' + source.text + '\n"""'
  return 'No recognized artifact source.'
}

const WINNER = { type: 'string', enum: ['Output 1', 'Output 2', 'Tie'] }
const JUDGE_SCHEMA = {
  type: 'object',
  additionalProperties: false,
  properties: {
    lens: { type: 'string' },
    dimensions: {
      type: 'array',
      items: {
        type: 'object',
        additionalProperties: false,
        properties: { dimension: { type: 'string' }, winner: WINNER, why: { type: 'string' } },
        required: ['dimension', 'winner', 'why'],
      },
    },
    overall_winner: WINNER,
    margin: { type: 'string', enum: ['decisive', 'clear', 'slight', 'tie'] },
    notes: { type: 'string' },
  },
  required: ['lens', 'dimensions', 'overall_winner', 'margin', 'notes'],
}

if (!V.a || !V.a.source || !V.b || !V.b.source) {
  log('arena: missing variants. Pass args.skill (+ optional oldRef), or args.a{source} and args.b{source}.')
  return { error: 'missing variants', got: { a: V.a, b: V.b } }
}

// --- Scenario: the shared input both variants process ---
phase('Scenario')
let inputDirective
if (input.kind === 'file') {
  inputDirective = 'THE SHARED INPUT: read the file ' + input.path + ' in full and use its entire contents as the input to process.'
  log('arena: input = file ' + input.path)
} else {
  const scenario = await agent(
    [
      'Invent ONE realistic, representative input for a controlled A/B test of an artifact.',
      'THE ARTIFACT: ' + V.task + '. DOMAIN / what the input should look like: ' + (input.domain || 'a typical use'),
      '',
      'Produce a single, concrete, self-contained input that the artifact would plausibly be run against, rich enough to discriminate between a strong and a weak version of the artifact (include the kind of edge or subtlety where quality differences show). Return ONLY the input itself, as raw text/markdown, no preamble.',
    ].join('\n'),
    { label: 'scenario', phase: 'Scenario' }
  )
  inputDirective = 'THE SHARED INPUT (process exactly this):\n\n' + scenario
  log('arena: generated a scenario from domain "' + (input.domain || '') + '"')
}

const runPrompt = (variant) => [
  'You are faithfully RUNNING an artifact (a prompt or skill) against an input, to produce exactly what that artifact would produce. This is a controlled A/B comparison; another agent is running a different version on the same input.',
  '',
  loadInstr(variant.source),
  'If the instructions contain placeholder tokens, treat them as their default paths: __FW_TASKS_DIR__ -> .flywheel/tasks, __FW_SPECS_DIR__ -> .flywheel/specs, __FW_AUDITS_DIR__ -> .flywheel/audits, __FW_PROPOSALS_DIR__ -> .flywheel/proposals, __FW_LOGS_DIR__ -> .flywheel/logs; __FW_DELIVERY__ -> use the task-directory convention.',
  '',
  'WHAT THE ARTIFACT IS: ' + V.task,
  '',
  inputDirective,
  '',
  'FAIRNESS RULES (critical): follow the artifact LITERALLY -- produce exactly the structure, discipline, and format it prescribes, no more and no less. Do NOT import rigor, structure, or quality the artifact does not itself prescribe. Where a step would interact with a human (a question) or write/create files, do NOT actually do it -- produce the artifact it would have PRESENTED for confirmation. You may read repo files the artifact tells you to read for grounding, but modify nothing.',
  '',
  'Return the COMPLETE output this artifact would produce, as raw markdown.',
].join('\n')

phase('Run')
const runs = await parallel([
  () => agent(runPrompt(V.a), { label: 'run:A', phase: 'Run' }),
  () => agent(runPrompt(V.b), { label: 'run:B', phase: 'Run' }),
])
const runA = runs[0]
const runB = runs[1]
if (!runA || !runB) {
  return { error: 'a run failed', variants: { a: V.a.label, b: V.b.label }, runA, runB }
}

const judgePrompt = (lens) => [
  'You are BLINDLY judging TWO outputs produced by two versions of the same artifact (' + V.task + ') from the SAME input. You are NOT told which version produced which -- judge purely on the artifacts, through the "' + lens.key + '" lens.',
  '',
  'LENS FOCUS: ' + lens.focus,
  '',
  'RUBRIC DIMENSIONS (score each, name a winner with a concrete, evidence-based reason): ' + JSON.stringify(rubric),
  '',
  input.kind === 'file' ? 'SHARED INPUT: the file ' + input.path + ' (read it for ground truth if useful).' : inputDirective,
  '',
  '================ OUTPUT 1 ================',
  runA,
  '',
  '================ OUTPUT 2 ================',
  runB,
  '',
  'For each rubric dimension name the winner (Output 1 / Output 2 / Tie) with a concrete reason citing the outputs. Then give an overall winner and a margin. "Tie" only when genuinely indistinguishable. Be discriminating and HONEST -- a real tie, or an Output-1 win, must be reported as such, never smoothed toward whichever looks newer.',
].join('\n')

phase('Judge')
const judges = (
  await parallel(lenses.map((l) => () => agent(judgePrompt(l), { label: 'judge:' + l.key, phase: 'Judge', schema: JUDGE_SCHEMA })))
).filter(Boolean)

const tally = { 'Output 1': 0, 'Output 2': 0, Tie: 0 }
for (const j of judges) tally[j.overall_winner] = (tally[j.overall_winner] || 0) + 1
log('arena tally -> Output 1 (' + V.a.label + '): ' + tally['Output 1'] + ' | Output 2 (' + V.b.label + '): ' + tally['Output 2'] + ' | Tie: ' + tally.Tie)

return {
  mapping: { 'Output 1': V.a.label, 'Output 2': V.b.label },
  task: V.task,
  inputKind: input.kind,
  runA,
  runB,
  judges,
  tally,
}
