# Third-party code and assets

paper-rs is MIT OR Apache-2.0. It ports a few algorithms from MIT-licensed projects (listed
below with their copyright notices and license text) and bundles the Inter font (SIL OFL 1.1,
`assets/fonts/OFL.txt`). Figma and Paper are proprietary: only their *behavior* was studied
(`analysis/FIGMA_NOTES.md`, `analysis/PAPER_ARCHITECTURE.md`), and no code, shaders, icons or
assets from them are used.

## Ported code

| Project | Copyright | What was ported | Where |
|---|---|---|---|
| [rough.js](https://github.com/rough-stuff/rough) 4.6.4 | Copyright (c) 2019 Preet Shihn | Seeded RNG, `_line` / `_doubleLine`, curves, ellipses, `svgPath`, solid / hachure / cross-hatch / zigzag / dots fills, generator option handling. Verified against rough.js output (`tests/fixtures/rough/`). | `src/render/rough.rs` |
| [hachure-fill](https://github.com/pshihn/hachure-fill) 0.5.2 | Copyright (c) 2023 Preet Shihn | Scan-line hachure algorithm | `src/render/rough.rs` |
| [points-on-curve](https://github.com/pshihn/bezier-points) 0.2.0, [points-on-path](https://github.com/pshihn/points-on-path) 0.2.1 | Copyright (c) 2020 Preet Shihn | Bezier flattening by flatness, Ramer–Douglas–Peucker, `curveToBezier`, path point sets | `src/render/rough.rs` |
| [Excalidraw](https://github.com/excalidraw/excalidraw) | Copyright (c) 2020 Excalidraw | Element → rough options (`adjustRoughness`, dash patterns, fill weights), binding gap and fixed-point rules, elbow-arrow grid/A* router with its segment-count heuristic, elbow corner rounding, arrowhead geometry, laser trail decay constants, sticky-note sizing, `.excalidraw` / `.excalidrawlib` formats | `src/render/rough.rs`, `src/connector.rs`, `src/editor/whiteboard.rs`, `src/io/excalidraw.rs` |

Each ported module starts with a comment naming its source.

## Dependencies

Rust crates are pulled from crates.io under their own licenses (see `Cargo.lock`); notable ones
are wgpu, winit, egui, lyon, ttf-parser, image, i_overlay (MIT), serde and rfd.

## MIT License (applies to each project listed above, with its copyright line)

```
MIT License

Copyright (c) 2019 Preet Shihn            (rough.js)
Copyright (c) 2020 Preet Shihn            (points-on-curve, points-on-path, path-data-parser)
Copyright (c) 2023 Preet Shihn            (hachure-fill)
Copyright (c) 2020 Excalidraw             (Excalidraw)
Copyright (c) 2023 Excalidraw             (@excalidraw/laser-pointer)

Permission is hereby granted, free of charge, to any person obtaining a copy
of this software and associated documentation files (the "Software"), to deal
in the Software without restriction, including without limitation the rights
to use, copy, modify, merge, publish, distribute, sublicense, and/or sell
copies of the Software, and to permit persons to whom the Software is
furnished to do so, subject to the following conditions:

The above copyright notice and this permission notice shall be included in all
copies or substantial portions of the Software.

THE SOFTWARE IS PROVIDED "AS IS", WITHOUT WARRANTY OF ANY KIND, EXPRESS OR
IMPLIED, INCLUDING BUT NOT LIMITED TO THE WARRANTIES OF MERCHANTABILITY,
FITNESS FOR A PARTICULAR PURPOSE AND NONINFRINGEMENT. IN NO EVENT SHALL THE
AUTHORS OR COPYRIGHT HOLDERS BE LIABLE FOR ANY CLAIM, DAMAGES OR OTHER
LIABILITY, WHETHER IN AN ACTION OF CONTRACT, TORT OR OTHERWISE, ARISING FROM,
OUT OF OR IN CONNECTION WITH THE SOFTWARE OR THE USE OR OTHER DEALINGS IN THE
SOFTWARE.
```

## Fonts

Inter 4.1 by Rasmus Andersson — SIL Open Font License 1.1 (`assets/fonts/OFL.txt`). Regular,
Medium, SemiBold, Bold, Italic and Bold Italic are bundled unmodified.

Geist Pixel Square by The Geist Project Authors (https://github.com/vercel/geist-font) — SIL
Open Font License 1.1 (`crates/openpaperdesign-serve/assets/geist-OFL.txt`). Bundled
unmodified (`crates/openpaperdesign-serve/assets/geist-pixel-square.woff2`) and served only
inside the plugin's home view, for its `onlinesourdough` wordmark.
