---
name: design
description: Create or revise a portable visual direction and selected previews or assets for a website, app, dashboard, report, slide, or content surface.
metadata:
  version: "1.3.1"
---

# Design

Turn the accepted intent into one inspectable visual direction. Use the host's
lifecycle methods (spec, build, review, ship), if installed, for lifecycle work;
this skill supplies design judgment.
Stay in the current workspace. Default new working material to project-local
`design/`; select the actual existing context, preserve owner files and reviewed
identities, and do not migrate folders or load a collection. Keep one canonical
`DESIGN.md` at the project root. If the project already has `design.md` or another
case variant, use that file as it is: never add a second name that differs only in
case (case-insensitive file systems treat them as one). Choosing or opening a
project or canvas is read-only; it never rewrites the direction, generates
boilerplate or moves canonical files.

Read the selected brief, canonical `DESIGN.md`, and relevant prior decisions.
Reuse the accepted outcome, audience/job, primary action or decision, surfaces,
real content/data, constraints, rights, proof, and review choice. Ask only for a
missing decision that materially changes the direction. A brief may already be
in the task; a separate `BRIEF.md` is useful for durable work and required only
by the optional snapshot helper.

Choose one visual idea that serves that job. `DESIGN.md` owns the semantic
direction: hierarchy, typography, color, spacing, composition, states, responsive
behavior, imagery, motion, and concrete do/don't guidance. Existing project
tokens, components and code own implementation truth; link and reconcile them
with the direction instead of copying a competing token source. Previews,
assets and editable sources are selected companions.

Load only what the task needs:

- [Source selection](references/source-selection.md) for supplied references,
  discovery of a missing visual role, or comparison of unresolved directions.
- [Authoring](references/design-solution.md) when making or revising the direction
  or its rendered surface.
- [Before/after comparison](references/before-after.md) when revising an existing
  visual surface and a comparison helps assess the change, or when requested.
  Preserve a useful baseline before editing when practical.
- [Portable helpers and snapshots](references/portable-work.md) to serve a
  selected HTML preview or deliver an exact snapshot to another owner.
- [openpaperdesign editor](../openpaperdesign/SKILL.md) for a selected editable
  companion: a paper-rs project under `design/`, edited through its tools and by
  the person in the canvas.

Use [Review design](../review-design/SKILL.md) for the selected design proof.
Preserve an accepted `independent` or `owner` review choice and named reviewer.
Independent PASS needs no second owner gate; owner mode needs that owner's
exact bound decision. Same-workspace work needs no invented handoff or receiver
acceptance. Repairs stay with the writer, refresh affected proof, and reopen
review of changed bytes. Keep failed attempts and recovered provenance; a copy
of an accepted artifact is not fresh acceptance.

Design owns visual direction and expressive assets. The implementation owner
owns implementation; content production owns thesis, script, source media,
editing, rendering, packaging and publication. Select those methods when the
accepted task needs them; suggest a bounded route for an unresolved ownership
gap without inventing decisions or an automatic chain.

Return the direction and selected artifact paths, actual proof, limitations,
and the authorized next step. For requested accumulated-state inspection use
the [read-only audit](../review-design/references/audit.md).

---

*Provenance: adapted for openpaperdesign from AIOS `skills/design` 1.2.1
(onlinesourdough/AIOS-plugin `a14f2857d8e229e7d3ce2c9beb15b876094e6d85`, v0.28.1), MIT
License, Copyright (c) 2026 onlinesourdough; see [LICENSE](LICENSE) and
[NOTICE](../../NOTICE.md). Changes: host-neutral wording, the OpenPencil adapter replaced by
the openpaperdesign editor; 1.3.1 adds the root `DESIGN.md` case rule and read-only opening.*
