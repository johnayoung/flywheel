---
description: Turn a vague feature idea into a structured specification through guided discovery
---

Transform an ambiguous feature request into a clear, actionable specification by systematically identifying and resolving unknowns.

## INPUT

$ARGUMENTS

The input can be anything from a single sentence ("add caching") to a paragraph describing a vague idea.

## CORE PRINCIPLE

**You are a requirements analyst, not an implementer.** Your job is to ask questions until there is zero ambiguity about what needs to be built. Do NOT assume, guess, or fill in gaps yourself.

## STEP 1: INITIAL CONTEXT GATHERING

Before asking questions, understand what exists:

```bash
# Understand project structure
ls -la
cat README.md 2>/dev/null | head -100

# Discover apps/modules in this repo
# Look for top-level directories containing: go.mod, package.json, Cargo.toml,
# pyproject.toml, setup.py, build.gradle, pom.xml, mix.exs, Gemfile, etc.
for dir in */; do
  for marker in go.mod package.json Cargo.toml pyproject.toml setup.py build.gradle pom.xml mix.exs Gemfile; do
    if [ -f "$dir$marker" ]; then
      echo "App: $dir ($marker)"
    fi
  done
done

# Also check repo root for single-app repos
for marker in go.mod package.json Cargo.toml pyproject.toml; do
  [ -f "$marker" ] && echo "Root app: $marker"
done

# Find related code using keywords from the request
grep -ri "<keywords-from-request>" -l | head -20
```

Identify:
- Which app(s) this likely affects based on discovered project structure
- Existing patterns that might be relevant
- Similar features already implemented

## STEP 2: IDENTIFY UNKNOWNS

Analyze the request and list EVERY ambiguous aspect:

| Category    | Unknown                               | Why It Matters            |
| ----------- | ------------------------------------- | ------------------------- |
| Scope       | What's included/excluded?             | Prevents scope creep      |
| Behavior    | How should X work?                    | Determines implementation |
| Edge Cases  | What happens when Y?                  | Prevents bugs             |
| Integration | How does this connect to Z?           | Architecture decisions    |
| Constraints | Performance/security/UX requirements? | Shapes solution           |
| Priority    | What's MVP vs nice-to-have?           | Focuses effort            |

## STEP 3: QUESTION DISCOVERY LOOP

For EACH unknown, ask a focused question using AskUserQuestion:

### Question Format

Each question MUST:
1. Be specific and answerable
2. Include 3-5 concrete options
3. Have a clear recommendation with reasoning
4. Explain the impact of each choice

### Question Categories (work through these)

**1. Scope Questions**
- What's the minimum viable version?
- What's explicitly OUT of scope?
- Which use cases must work vs nice-to-have?

**2. Behavior Questions**
- How should the happy path work?
- What should happen on errors?
- What are the user-visible outcomes?

**3. Technical Questions**
- Which existing patterns to follow?
- What are the integration points?
- Performance/security requirements?

**4. Edge Case Questions**
- What if the input is invalid?
- What if a dependency fails?
- What about concurrent access?

### Asking Questions

Use the AskUserQuestion tool for EACH unknown. Structure each question:

```
Question: <Clear, specific question>
Header: <2-3 word category>

Options:
1. <Option A> (Recommended) - <Why this is recommended>
2. <Option B> - <Trade-offs>
3. <Option C> - <Trade-offs>
4. <Option D> - <Trade-offs> (if needed)
```

**CRITICAL RULES:**
- Ask ONE category of questions at a time (1-3 related questions max)
- Wait for answers before moving to next category
- Do NOT proceed until the user has answered
- If user selects "Other", probe for specifics
- Track all decisions made

## STEP 4: COMPILE DECISIONS

After each round of questions, update your running list of decisions:

```
## Decisions Made

### Scope
- [x] MVP includes X, Y, Z
- [x] Out of scope: A, B, C

### Behavior
- [x] On success: <behavior>
- [x] On error: <behavior>

### Technical
- [x] Follows pattern from <existing code>
- [x] Integrates with <system>

### Still Unknown
- [ ] <remaining question>
```

## STEP 5: COMPLETION CHECK

Before generating the spec, verify:

- [ ] All scope boundaries defined
- [ ] Happy path behavior clear
- [ ] Error handling defined
- [ ] Edge cases addressed
- [ ] Integration points identified
- [ ] No remaining ambiguity

If ANY checkbox is unchecked, return to STEP 3.

## STEP 6: GENERATE FEATURE SPEC

**Spec files MUST be placed at `.workflow/specs/NNNNN-FEATURE-<name>.md`.**

### Determine Next Sequence Number

Before creating the spec file, determine the next available sequence number:

```bash
# Find the highest existing sequence number across active and archived specs
ls .workflow/specs/[0-9]*-FEATURE-*.md .workflow/specs/archive/[0-9]*-FEATURE-*.md 2>/dev/null | \
  sed 's/.*\/\([0-9]*\)-.*/\1/' | sort -n | tail -1
```

- If no numbered specs exist, start at `00001`
- Otherwise, increment the highest number by 1
- Always zero-pad to 5 digits (e.g., `00001`, `00002`, `00042`)

Create `.workflow/specs/NNNNN-FEATURE-<name>.md`:

```markdown
# Feature: <Name>

## Summary
<2-3 sentence description of what this feature does>

## Background
<Why this feature is needed, context from discussion>

## Scope

### In Scope
- <Specific capability 1>
- <Specific capability 2>

### Out of Scope
- <Explicitly excluded item 1>
- <Explicitly excluded item 2>

## Requirements

### Functional Requirements
1. **<FR-1>**: <Requirement>
   - Acceptance: <How to verify>
2. **<FR-2>**: <Requirement>
   - Acceptance: <How to verify>

### Non-Functional Requirements
- **Performance**: <Specific requirement or "No specific requirement">
- **Security**: <Specific requirement or "Standard practices">
- **UX**: <Specific requirement or "N/A">

## Behavior Specification

### Happy Path
1. <Step 1>
2. <Step 2>
3. <Expected outcome>

### Error Handling
| Error Condition | Expected Behavior |
| --------------- | ----------------- |
| <Condition 1>   | <Behavior>        |
| <Condition 2>   | <Behavior>        |

### Edge Cases
| Case     | Expected Behavior |
| -------- | ----------------- |
| <Case 1> | <Behavior>        |
| <Case 2> | <Behavior>        |

## Technical Context

### Affected Apps
- <app>: <what changes>

### Integration Points
- <System/API>: <How it integrates>

### Relevant Existing Code
- `<file path>`: <Why relevant>

## Decisions Log

| Decision | Choice             | Rationale |
| -------- | ------------------ | --------- |
| <Topic>  | <What was decided> | <Why>     |

## Open Questions
<Any questions that couldn't be resolved, requiring stakeholder input>

## Next Steps
Run `/task NNNNN-FEATURE-<name>` to generate implementation tasks from this spec.
```

## STEP 7: PRESENT AND CONFIRM

Present the spec to the user for final review:

```
## Feature Spec Complete

I've created `.workflow/specs/NNNNN-FEATURE-<name>.md` with:
- <X> functional requirements
- <Y> edge cases documented
- <Z> key decisions recorded

**Summary of what we defined:**
<Brief bullet list of key points>

**Ready to proceed?**
- Run `/task NNNNN-FEATURE-<name>` to generate implementation tasks
- Or review the spec and let me know if anything needs adjustment
```

## ANTI-PATTERNS

- **DO NOT** place spec files anywhere except `.workflow/specs/`
- **DO NOT** assume answers to unclear requirements
- **DO NOT** ask yes/no questions - always provide concrete options
- **DO NOT** ask more than 3 related questions at once
- **DO NOT** generate the spec until ALL unknowns are resolved
- **DO NOT** include implementation details - that's for `/task`
- **DO NOT** skip the question loop - even "obvious" features have hidden complexity

## EXAMPLE QUESTION FLOW

**Input:** "add caching to the API"

**Round 1 - Scope:**
- "Which endpoints need caching?"
- "What data should be cached?"

**Round 2 - Behavior:**
- "Cache invalidation strategy?"
- "TTL requirements?"

**Round 3 - Technical:**
- "In-memory vs distributed cache?"
- "Cache key structure?"

**Round 4 - Edge Cases:**
- "Behavior on cache miss during outage?"
- "Per-user vs shared cache?"

Each round uses AskUserQuestion and waits for responses before proceeding.
