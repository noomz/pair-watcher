# Pair-watcher

A second agent (or several) follows a working agent as it works, comments for the human, and challenges the worker when an approach looks wrong.

## Agents

**Worker**:
The agent doing the task, running in its own harness, unaware of watchers until a Challenge reaches it.
_Avoid_: Driver, A, main agent

**Watcher**:
An agent that observes one Worker and speaks; never edits the repo.
_Avoid_: Navigator (that is a shape, not an agent), reviewer, critic, B

**Focus**:
A named brief stating what a Watcher attends to (e.g. security, best practice) and what it ignores; each Concern carries its Focus name.
_Avoid_: Interest, lens, specialty

**Companion mode**:
One Watcher with every Focus watching one Worker. The default.
_Avoid_: Solo mode, single mode

**Swarm**:
Several Watchers on one Worker, each with a narrower Focus, sharing one Concern list and one Challenge slot; each Watcher speaks only for its own Concerns.
_Avoid_: Fleet (a fleet is many Workers), team

**Wake**:
One moment a Watcher looks at the Worker's latest activity and may speak: at turn end, when the Worker is blocked, or at a mid-turn checkpoint. Silence is a normal result of a wake.
_Avoid_: Tick, poll, run

**Blind**:
Said of a Watcher that is not watching: it cannot wake, or it has skipped part of the Worker's activity. A blind Watcher's silence means nothing; a late Watcher (failed wakes it will catch up on) is not blind.
_Avoid_: Down, degraded, offline, silent (silence is a normal result of a wake)

## Speech

**Commentary**:
The stream of Concerns shown to the human in a side pane; the Worker never sees it, and it never notifies. Evaluates only: no narration, no praise.
_Avoid_: Feedback, comments, observations

**Concern**:
One evaluative item of Commentary, tracked from raised until the Watcher resolves it or the human dismisses it; a dismissed Concern never reopens.
_Avoid_: Note (too loose; also used for Challenges), issue, finding

**Approach-level**:
Said of an objection that, if right, means work the Worker has done or planned in its current direction must be redone or thrown away, not patched in place. The bar for an automatic Challenge.
_Avoid_: Major, critical, high-severity (severity is display only)

**Challenge**:
A Concern sent on into the Worker's next turn, asking it to justify its approach; raised automatically for approach-level Concerns, or by the human promoting any Concern.
_Avoid_: Interrupt, objection, review

**Debate**:
The bounded exchange that follows a Challenge, ending in worker concedes, watcher concedes, unresolved, or withdrawn (stopped without a verdict because its Concern no longer applies or the session ended).
_Avoid_: Discussion, argument, thread

**Ruling**:
The human's verdict on a Debate or an unresolved-Debate Escalation: sided with the Watcher, sided with the Worker, or dismissed. Given explicitly, or inferred from the human's next prompt to the Worker while an Escalation waits (and then undoable). A ruling for the Watcher given explicitly is sent on to the Worker; nothing else is. A blocked Escalation is answered in the Worker, not ruled.
_Avoid_: Decision, vote, verdict (verdict is the Worker's concede/defend line)

**Standing ruling**:
A dismissal or a ruling on a Debate or Escalation that the human chose to keep beyond the session; every later Watcher on the same repo reads it until the human removes it. A plain dismissal ends with its session.
_Avoid_: Memory, mute, suppression rule, preference

**Escalation**:
Handing an unresolved Debate (both positions summarised), or a question the Worker is blocked on (the question restated plus the Watcher's considerations), to the human with a notification. The only Watcher speech that notifies.
_Avoid_: Alert, ping
