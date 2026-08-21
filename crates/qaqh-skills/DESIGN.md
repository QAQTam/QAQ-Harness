# DeepX Skill Runtime V2

This document is authoritative for skill discovery, context assembly, runtime state, IPC, and persistence.

## Ownership and context order

`SkillContextManager` owns catalog snapshots, Requested/Active/Unavailable state, budget, hot reload, queued UI transitions, revisions, and session conversion. `MessageStore` owns conversation history only and never infers skill state from historical system messages.

Each LLM request is assembled in this order:

1. persisted base system prompt;
2. transient stable catalog system slot;
3. conversation history;
4. the current user-message clone with workspace and Requested annotations;
5. current-round tool results;
6. a final system `skill_context_envelope` containing the complete authoritative active set.

Catalog and envelope messages are never written to history. Removing all skills emits an explicit empty active set. Stateful providers receive the complete final envelope every lap; it supersedes older remote instructions. OpenAI-compatible providers are the initial supported target for trailing system messages.

## Discovery and tools

Discovery precedence is `.deepx/skills`, `.agents/skills`, and `skills` under the workspace, followed by the equivalent user roots. Scanning and file sizes are bounded. Invalid entries do not stop discovery and appear as Unavailable diagnostics.

The fixed `skills` schema supports `activate`, `retain`, `release`, `resource`, `list`, and `validate`; it is not expanded per discovered skill. Successful lifecycle actions return ordered typed `SkillEffect` values. Parallel results are committed in original tool-call order. Generic `read` rejects or excludes discovered `SKILL.md` files and managed resources with `USE_SKILLS_TOOL`.

## State machine and turn lifecycle

Frontend Load and `$skill-name` create Requested state and add only a temporary explanation to the current LLM user-message clone. The model should call `skills.activate`. The first ignored lap adds a final-system reminder; the second ignored lap performs the same typed activation in the host with source `user_forced` and runs another model lap.

Every user turn freezes a `SkillTurnSnapshot`. Permission, ask-user, plan review, and additional model laps reuse that snapshot epoch. UI operations received during a turn are queued until the next user-turn boundary. Only successful `TurnComplete` consumes a lease; cancel and abort do not.

Active skills stay injected until explicitly released (no turn lease / auto-unload — removed to keep the system-prompt prefix stable and preserve cache hits across long-running tasks). `release` removes immediately.

## Budget and hot update

Total skill instructions are limited to 10% of effective input capacity with a 64K-token ceiling; one skill is limited to 32K. Bodies are never truncated. Reclamation order is oldest retain revision, then name. Activation is rejected if reclamation cannot satisfy the budget.

Catalog directory metadata is fingerprinted every user turn while unchanged rendered catalog bytes are reused. Active bodies are reloaded and validated every turn. A changed body replaces the old body immediately and resets its lease. Changes of at most 200 lines include a diff; larger changes include hashes and add/remove counts only. Missing or invalid active files are removed instead of retaining stale instructions.

## IPC and persistence

`SkillsStatus` V2 carries catalog revision, context epoch, operation revision, budget usage, diagnostics, and complete runtime entries. Frontend commands use stable `operation_id` values; duplicate IDs resolve idempotently, stale expected revisions return `SKILL_OPERATION_STALE`, and every resolution is followed by a revisioned snapshot. The frontend reducer rejects older snapshots and is the event-derived source of skill state.

`SkillSessionStateV2` persists names, activation order, source, stable state, lease, and revisions in JSON and Turso metadata. It never stores instruction bodies, transient requests, in-flight operations, or one-shot notices. Resume reloads each name from disk, restores order, resets valid leases to three turns, and marks missing entries Unavailable. Legacy sessions default to empty V2 state; historical `[DEEPX_SKILL_V1]` messages are not used for recovery.
