# Notices

openpaperdesign is a fork of [paper-rs](https://github.com/michaelshimeles/paper-rs)
(MIT OR Apache-2.0); see the repository's `LICENSE-MIT`, `LICENSE-APACHE` and
`THIRD_PARTY.md`. The browser editor bundles the Inter font (SIL OFL 1.1,
`assets/fonts/OFL.txt` in the repository). The home view embeds Geist Pixel Square, unmodified,
for its `onlinesourdough` wordmark (SIL OFL 1.1, Copyright 2024 The Geist Project Authors;
`crates/openpaperdesign-serve/assets/geist-OFL.txt` in the repository). Not affiliated with
or endorsed by Paper (paper.design).

## Ported design methods

`skills/design` and `skills/review-design` are adapted from AIOS
(https://github.com/onlinesourdough/AIOS-plugin), commit
`a14f2857d8e229e7d3ce2c9beb15b876094e6d85` (release v0.28.1), received as a source-only
handoff (archive SHA-256 `c07f55536371d2d17b27a435f6d350ddcf4d490a46af71cdf14e374eb2686d8c`,
28 files, every file hash checked against its manifest on 2026-10-12).

- License: MIT, Copyright (c) 2026 onlinesourdough; each ported skill folder keeps its
  `LICENSE`. Their upstream inspiration sources stay referenced by link and license in
  `skills/design/references/taste-sources.md`; nothing from them is vendored.
- Changes: AIOS names and links to AIOS-only skills replaced with host-neutral wording; the
  OpenPencil adapter (`openpencil-workbench`) and the OpenPencil route of
  `scripts/create-handoff.mjs` left out (it depends on an external editor this plugin does
  not ship), and the openpaperdesign editor named as the editable companion instead.
- Their tests are ported to `plugin/tests/design/` in the repository (without the OpenPencil
  cases).
- Fixed on port (2026-10-12): in `review-design/scripts/audit.py`, a reviewed companion entry
  named like canonical evidence (for example `DESIGN.md`) replaced the canonical reviewed
  hash, so a stale review could audit as PASS. Canonical hashes are now checked separately
  and such companion entries fail. Also fixed on port (B2b review round 2): repeated
  evidence was collapsed to one value (duplicate `proof.json` keys, repeated REVIEW.md
  records), so contradictory evidence could audit as PASS. The audit now rejects duplicate
  JSON keys at any depth and repeated records or fields, and `create-handoff.mjs` refuses a
  REVIEW.md with repeated records. The AIOS original is unchanged; its owners may want the
  same fixes.
