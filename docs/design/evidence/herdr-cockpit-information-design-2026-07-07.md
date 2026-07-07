# Herdr cockpit information-design feedback — 2026-07-07

Source: operator screenshot/review after HERDR-04/HERDR-05 made the Herdr panes live.

## Diagnosis

The cockpit is functionally wired but still reads like an event log. Operators can see that work is happening, but must decipher envelope events and raw activity lines to understand the state of the run.

This is an editorial/information-design failure, not an architecture failure:

- FEED-01 concentrated run/feed wording in the shared projection seam.
- HERDR-04 concentrated worker stream rendering in the Pi contract / worker activity painter seam.
- HERDR-05 concentrated gate/repair pane behavior in the cockpit painter seam.

## Observed warts

### Monitor / feed pane

- `Last semantic progress: unknown` remains unhelpful even while wrapper-mode workers emit tool execution events.
- Raw JSON leaks into activity lines, e.g. cockpit-ready payloads.
- Duplicate activity entries obscure milestones.
- Activity mixes altitude levels: infra ticks, state changes, shell progress, and milestones all appear together.
- Economics read as confusing zeros while a worker is active; the display should count in-flight work or clearly distinguish unknown/in-flight from complete zero.

### Worker pane

- Envelope events such as message start/end and turn start/end carry little operator value.
- Delta compaction reports chunk counts while hiding the actual assistant/tool payload.
- Tool events lack identity: operators need to see what command/file/tool is active, bounded and redacted for display.
- Pi emits typed reasoning/progress events in JSON mode; painter policy should render only what the typed wire carries and label it as reasoning/progress, never reconstruct hidden chain-of-thought.

### Gate / repair pane

- HERDR-05 made active command painting possible, but idle fallback still risks becoming the generic monitor feed.
- The gate pane should answer gate-specific questions: current verification profile, latest gate result, repair policy/state, active command tail, and next gate/repair action.

## Design rule

Each pane should answer one question as a state summary first, not an event log:

- Monitor/feed pane: "Am I needed, and where are we?"
- Worker pane: "What is this worker doing right now?"
- Gate/repair pane: "Is verification/repair running, and what is the gate state?"

Raw events remain available for drill-down through daemon artifacts and inspection commands.

## Follow-up slices

- `FEED-02` — shared projection information design: altitude tiers, humanized feed lines, dedupe, semantic-progress wiring from wrapper-mode tool events, in-flight economics, and golden projection fixtures.
- `HERDR-04B` — worker pane semantic painter: render typed Pi event payloads, suppress envelope noise, show coalesced assistant text/reasoning and tool identity/outcome, backed by recorded ndjson golden fixtures. Implemented at the Pi-contract formatter seam so wire-format recognition remains confined to `src/pi_contract.rs` and renderer output stays display-only.
- `HERDR-05B` — gate pane semantic summary: active gate command state and gate-scoped idle summary over FEED-02 fields, backed by golden fixtures. Implemented as a display-only shared projection/gate-summary seam that avoids generic monitor/activity fallback when idle.

## Chain-of-thought fence

The exposure boundary is Pi's typed JSON event stream. The painter may render content present in typed `pi_contract` events, including provider-emitted reasoning/progress summaries if present. It must not infer, reconstruct, or expose hidden chain-of-thought from timing, token counts, raw unknown fields, or provider-private data. If Pi stops emitting reasoning/progress events, the pane shows less.
