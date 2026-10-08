# Initial Offline Gameplay Rules

Schema version: 1.

These are project-owned rules and original encounter content. Fireball's name, icon, range, cast
time, and mana cost are read from the installed build's Spell.dbc; creature display models are
resolved from the installed client tables. No server database or client spawn data is copied into
this file.

## Starting Placement

New characters use race-specific project-authored anchors in their TBC starting areas. Human
placement uses the Northshire position already established in this repository's build-8606 world
probes. The other race positions are authored area anchors and should be visually checked against
build 8606 before expanding their content. Saved character positions always take precedence.

## Starter Encounter

- World entry creates three level-1 hostiles around the new character's saved starting position.
  This local pack is original practice content; it is not claimed to reproduce stock creature
  spawns. The adapter selects matching installed display models where available.
- Each hostile has 60 health and respawns 30,000 simulation milliseconds after death.
- Every character knows the common Attack action (client spell ID 6603), placed in action-bar slot
  1. Mage characters also know Fireball (client spell ID 133), placed in slot 2. Other classes do
  not receive a fabricated class spell.
- Auto-attack requires a living hostile within 5 yards. It deals a fixed 5 physical damage every
  2,000 simulation milliseconds, beginning one swing interval after Attack starts. It stops when
  toggled off, the target dies, or the target leaves range.
- Fireball is limited to a living hostile within 35 yards, uses the client's 1,500 ms cast time and
  30-mana cost, and deals a fixed 30 damage. The new Mage has 100 mana and no regen yet; two
  successful casts defeat one practice hostile. Mana is charged when the cast starts and is not
  refunded on interruption.
- Cooldown lockout is 1,500 simulation milliseconds, beginning when the cast starts. Combat uses
  injected elapsed time; no wall clock or random roll affects the outcome.

The cast damage, hostile health, encounter placement, and respawn time are project decisions. This
is the first combat slice, not a claim that the Phase 5 quest, AI, loot, XP, and save acceptance
criteria are complete.
