# Wild Buzzard production-vector workflow

- Status: required workflow for the next logo candidate
- Scope: logo geometry only; this document does not approve any current
  candidate
- Product constraints: `../../AGENTS.md` and
  `../GUEST_SETTINGS_DESKTOP_INTEGRATION_PLAN.md`

## Current internal candidate

`wb-common-buzzard-three-quarter-b2` is the deterministic source candidate
currently emitted by `guest/branding/generate.py`. It has passed structural
generation and exact-size raster review, but it has not passed the human,
reverse-image, trademark-register, or professional-review gates below. Its
presence in runtime assets is for development integration and does not change
its `clearance-pending` status.

## Decision

The production mark must be drawn and revised in an interactive vector editor.
No coding model may produce the final mark by guessing SVG path coordinates in
text. No image model, raster tracer, or image-to-SVG model may be treated as the
author of production geometry.

The correct division of labour is:

1. an image model produces disposable anatomy and composition studies;
2. a dedicated vector-design agent operates a pinned vector editor and changes
   visible nodes and handles, not raw `d` strings;
3. deterministic tooling exports, normalizes, validates, and renders those
   edits;
4. independent visual and structural reviewers accept or reject each revision;
   and
5. only an accepted, manually reconstructed revision is imported into
   `guest/branding/buzzard-mark.json` and used by the asset generator.

This is deliberately different from asking a language model to emit plausible
Bezier coordinates. SVG path syntax can be valid while the anatomy, silhouette,
optical balance, and tiny-size rasterization are poor.

## Model and tool assessment

### Models available to this project

| Capability | Appropriate use | Production geometry? |
|---|---|---|
| The callable image-generation tool | Raster pose, anatomy, palette, and composition studies. Its exact backing model identifier is not exposed by the tool interface, so provenance must describe it as the callable image-generation tool rather than inventing a model name. | No. Output is raster concept material. |
| GPT Image 2 | If direct API use is separately authorized, this is OpenAI's currently documented state-of-the-art image generation and editing model. It remains an image-in/image-out model, not an audited SVG editor. | No. Use only for disposable raster studies. |
| `gpt-5.6-sol` at `ultra` or `max` reasoning | Dedicated vector-editor operator, visual comparison, revision planning, validation-tool development, and evidence synthesis. | It may operate the editor, but must not guess final path text. |
| StarVector 8B | Research-only SVG draft generation or comparison. The project describes it as an image/text-to-SVG model trained for icons and logotypes, but it is not installed here and its raw output does not satisfy Wild Buzzard's manual-reconstruction, two-to-four-path, originality, or review requirements. | No raw output. Do not add it to the production toolchain. |
| Potrace or VTracer | Deterministic raster-to-vector conversion. These are useful for experiments on project-authored source drawings, but tracing a third-party reference or a generated raster violates this workflow. | No for this mark. |

The strongest available arrangement is therefore not “pick one model and ask
for SVG.” It is a `gpt-5.6-sol` vector-design agent operating Inkscape with
full-resolution render feedback, while the image-generation tool is limited to
concept boards. A specialist SVG generator such as StarVector may be evaluated
outside the production path, but it does not replace manual reconstruction and
review.

OpenAI documents GPT Image 2 as an image-input/image-output model and GPT-5.6
Sol as the frontier GPT-5.6 variant. Those descriptions support the division
above; neither source says that either model produces release-ready, original,
audited SVG path geometry.

### Tool audit on the current development host (2026-08-10)

Present:

- `xmllint`;
- Python 3.12 and Pillow;
- Rust/Cargo;
- Podman and Docker;
- the locked guest Rust workspace's `resvg`, `usvg`, `svgtypes`, `kurbo`, and
  `tiny-skia` dependencies; and
- the repository's deterministic branding generator and security tests.

Not present as host commands:

- Inkscape;
- `resvg` or `rsvg-convert` CLI;
- Potrace, VTracer, AutoTrace, or SVG Cleaner; and
- CairoSVG or `svgpathtools` Python modules.

An attempt to start the already-cached Ubuntu 26.04 image for a disposable
package audit failed before execution with `Disk quota exceeded`. Do not work
around that by installing graphics packages on the host. Clean the container
storage quota first, then use either:

- the official pinned Inkscape AppImage, kept outside the repository; or
- a digest-pinned disposable development container with only the editable
  work directory mounted.

Inkscape publishes an official Linux AppImage, so a system installation is not
required. Record the selected version, download URL, SHA-256, and container
image digest (if used) in the candidate provenance.

## Authorship and reference boundary

### Allowed reference use

- Consult multiple authoritative common-buzzard sources, including RSPB and
  BTO, to learn species-level anatomy.
- Write an anatomy brief before drawing: compact broad head, low/sloped crown,
  short thick neck, modest hooked beak, substantial chest, broad rounded folded
  shoulders, natural small eye, near-front three-quarter pose, and off-axis
  gaze.
- Consult multiple photographs for variation. Do not derive the complete
  outline, pose, eye/beak construction, or plumage pattern from one photograph.
- Keep reference photographs and generated concepts outside the repository.
- Record URLs, access dates, prompts, and hashes for concepts that materially
  informed the work.

### Prohibited reference use

- Do not import a third-party photograph into the vector document.
- Do not place a photograph under the drawing and trace its contour.
- Do not trace, auto-trace, vectorize, or optimize a generated raster into the
  production paths.
- Do not copy a third-party logo, SVG, icon, stock vector, or path data.
- Do not use an image-to-SVG model's output as the editable master.
- Do not sample a third-party mark's geometry or colour lockup.

Generated images may answer high-level questions such as “is a calm off-axis
three-quarter pose more readable than a profile?” They must not answer “where
should this production node be?” The vector designer must reconstruct the
portrait on a blank artboard from the written anatomy brief and visual
judgement.

## Required roles

Use separate agents so the same author is not the only reviewer.

### Vector designer

- Model: `gpt-5.6-sol`, `ultra` or `max` reasoning.
- Owns one candidate working directory and one Inkscape document.
- Operates the Pen/Bezier, B-spline, Node, and Boolean tools visually.
- Changes nodes and handles in the editor; never hand-edits path coordinate
  strings.
- Produces one small, named revision at a time and explains the intended visual
  change.
- Does not regenerate shipped assets or alter clearance status.

### Species and art-direction reviewer

- Model: a separate `gpt-5.6-sol` high-or-greater agent.
- Read-only access to candidate renders.
- Compares the pose and anatomy with the written species brief and several
  authoritative references.
- Rejects generic eagle, falcon, owl, penguin, parrot, chicken, shield, and
  aggressive mascot readings.
- Must state what is actually visible, not what the candidate metadata claims.

### Geometry and rendering reviewer

- Model: a separate `gpt-5.6-sol` high-or-greater agent.
- Owns structural, security, deterministic-render, and pixel-difference checks.
- Does not make aesthetic edits.
- Confirms that editor export, canonical geometry, and generated variants are
  byte- and render-consistent.

### Orchestrator and human gate

- The root agent coordinates revisions and evidence but does not override a
  failed visual or structural gate.
- A responsible human explicitly accepts the silhouette and final colour mark.
- Reverse-image and trademark searches remain separate gates and never become
  an automated “safe” label.

## Working files and provenance

Use a dedicated directory outside the repository for each concept family:

```text
<external-work-dir>/wb-buzzard-<candidate-id>/
├── brief.md
├── toolchain.json
├── references.md
├── concepts/                 # disposable raster studies
├── working-master.svg        # Inkscape editing document
├── exports/                  # plain SVG revisions
├── renders/                  # exact-size PNGs and contact sheets
├── reviews/                  # anatomy and geometry decisions
└── revision-log.jsonl
```

`toolchain.json` records exact application versions, hashes, and container
digests. `revision-log.jsonl` records the source hash, export hash, render
hashes, change intent, reviewers, and decision for every revision.

Only these materials enter the repository after acceptance:

- normalized two-to-four-path geometry;
- the final written provenance and revision identity;
- approved generated variants;
- reproducibility and validation tests; and
- honest similarity/trademark evidence.

The editable Inkscape document is a construction aid, not a hidden second
canonical source. After acceptance, an importer extracts the approved path
geometry into `guest/branding/buzzard-mark.json`; the importer shows a reviewable
diff and never silently replaces production geometry.

## Exact production loop

### 1. Freeze the brief

Before generating or drawing anything, record:

- the required near-front three-quarter pose and gaze direction;
- common-buzzard anatomical cues;
- the prohibited visual families;
- 256 by 256 artboard and at least 24 units of safe space;
- two to four filled production paths;
- the required dark, light, unboxed, and symbolic palettes; and
- the 16, 24, 32, 64, 256, and wallpaper acceptance sizes.

Do not move these targets in response to a weak candidate.

### 2. Make a raster concept board

Generate genuinely different pose families, not recolours of one image. At a
minimum compare:

1. head turned slightly left, low broad mantle;
2. head turned slightly right, different shoulder rhythm; and
3. near-centred head with asymmetric gaze and non-mirrored shoulder weight.

Generate enough resolution to inspect anatomy, but treat every output as
disposable. Select only a pose direction and written anatomical observations.
Do not select contours, pixel boundaries, or exact markings to trace.

### 3. Construct the silhouette in Inkscape

1. Open a new blank 256 by 256 document.
2. Add guides for the 24-unit safe area, horizontal centre, vertical centre,
   eye line, shoulder line, and chest baseline.
3. Keep all reference imagery in a separate viewer, never embedded or overlaid
   in the SVG.
4. Draw the complete outer portrait as one filled path with the Pen/Bezier or
   B-spline tool.
5. Adjust nodes, segment curvature, and tangent handles with the Node tool.
6. Add only the minimum separate filled paths needed for beak/negative space,
   eye/cere, and one broad plumage mass.
7. Test the silhouette in one colour before adding internal colour regions.
8. Keep natural asymmetry, but judge optical balance at true size rather than
   forcing mathematical mirroring.

Inkscape's documentation explicitly supports moving nodes and handles,
dragging path segments directly, and removing unnecessary nodes. This is the
authoring interface required here: coordinate text is an export, not the design
surface.

### 4. Export and normalize

Export a plain SVG revision. A future `wildbuzzard-branding-check` helper must:

1. parse XML without network or entity resolution;
2. parse path data with `svgtypes`, not a regular expression alone;
3. flatten transforms into path coordinates;
4. convert primitives used during construction into paths;
5. normalize to absolute `M`, `L`, `C`, and `Z` commands;
6. quantize coordinates deterministically to a documented precision;
7. verify every coordinate is finite and in a bounded range;
8. require every filled subpath to close;
9. report path, subpath, segment, and node counts;
10. calculate geometry and painted bounds while excluding icon background
    rectangles; and
11. emit a candidate JSON diff without changing the canonical file.

The reviewer applies that diff only after the visual gates pass. This makes the
editor export, rather than a language model's typed numbers, the origin of each
coordinate.

### 5. Render a fixed matrix

Render each revision from the normalized candidate with the guest workspace's
pinned `resvg` stack. Produce:

- true-size 16, 24, 32, 64, and 256 pixel PNGs;
- nearest-neighbour enlarged inspection copies of the 16, 24, 32, and 64 pixel
  renders, while retaining the original true-size files;
- a 2048 pixel monochrome silhouette;
- a 2048 pixel dark icon;
- a 2048 pixel light icon;
- 16:9, 16:10, 3:2, 4:3, and 21:9 wallpaper previews with the mark at exactly
  20% of the shorter side; and
- an alpha-only and high-contrast silhouette view.

Also render the accepted revision with a second implementation: the pinned
Inkscape export or the actual GTK/librsvg icon path in the guest. Differences
between independent renderers must be explained; they cannot be dismissed as
“close enough” without inspection.

### 6. Inspect at true size

For every revision, inspect the true-size images before zoomed copies. At each
size answer in writing:

- Does this read as one coherent bird portrait?
- Does it read specifically as a common buzzard rather than a generic raptor?
- Is the pose near-front three-quarter rather than a full profile?
- Is the gaze visibly off-axis without looking frightened or aggressive?
- Are the broad head, short neck, modest hook, chest, and folded shoulders
  present?
- Does the outer silhouette remain coherent with all detail paths hidden?
- Do internal gaps stay open and intentional after antialiasing?
- Is the icon optically centred and adequately separated from every edge?
- Does the symbolic mark remain recognizable at 16 and 24 pixels?

Metadata such as `species: Buteo buteo` is never evidence for these answers.
The rasterized shape is the evidence.

### 7. Revise one variable at a time

Each revision changes one named class of issue, such as crown slope, beak
length, eye placement, neck transition, left shoulder weight, or chest width.
Do not simultaneously rewrite the entire bird after review; that destroys the
ability to identify which change improved or damaged the mark.

Every revision returns to export, normalization, complete render matrix, and
independent review. A revision is accepted only on visible evidence.

## Path simplification without destroying the mark

Inkscape's Simplify operation reduces nodes while approximately preserving
shape, and Potrace exposes an explicit curve-optimization tolerance. Neither is
an approval oracle.

Use this procedure:

1. preserve the pre-simplification revision and hash;
2. simplify a copy once, never repeatedly by habit;
3. normalize both revisions;
4. compare segment and node counts;
5. calculate a maximum sampled contour deviation in 256-unit coordinates;
6. produce a 2048-pixel alpha difference image;
7. compare both at 16, 24, 32, 64, and 256 pixels; and
8. keep the simpler revision only when reviewers see no anatomical or optical
   regression.

Pixel difference is a regression signal, not a design-quality score. A small
numeric difference can still damage an eye, beak tip, or tiny negative space.
There is no arbitrary reward for the fewest possible nodes. A high segment
count must be justified, but intentional curvature takes precedence over an
automated node target.

## Structural acceptance gates

The canonical geometry fails if any item below is false:

- artboard and viewBox are exactly `0 0 256 256`;
- there are exactly two, three, or four production path elements;
- canonical geometry contains only filled, closed paths;
- the icon wrapper may add one local background rectangle, but no other
  primitive changes geometry;
- there are no strokes, filters, gradients, blur, masks, clip paths, text,
  fonts, raster images, foreign objects, animation, scripts, event attributes,
  CSS, hyperlinks, `use`, data URLs, or external references;
- all path commands parse completely and all coordinates are finite;
- all transforms are flattened before canonical import;
- all painted geometry stays within the documented safe area;
- no path has accidental zero-area contours, unclosed contours, or unexplained
  self-intersections;
- dark, light, symbolic, icon, and wallpaper variants derive from identical
  approved geometry;
- generation is byte-identical on two consecutive runs;
- `resvg` renders every required size without warning or failure;
- an independent renderer produces the same intended composition; and
- all SVG processing runs in W3C-style secure static conditions: no script,
  interaction, animation, or external resource loading.

The existing regular-expression path filter is a useful injection defence, but
it is not a complete geometry parser. Production acceptance requires an actual
SVG path parser and renderer.

## Visual acceptance gates

The candidate fails if any reviewer reasonably sees:

- a generic eagle or falcon profile;
- an owl facial disc, round penguin head, chicken/parrot beak, or vulture head;
- a heraldic shield or security-company badge;
- direct symmetrical eye contact;
- a detached eye, beak, or wing emblem rather than a whole upper-body portrait;
- a giant or razor-sharp beak inconsistent with a common buzzard;
- a thin neck, narrow chest, or absent folded shoulders;
- a silhouette that works only because of colour or internal detail;
- collapsed gaps, muddy antialiasing, or unrecognizable 16/24-pixel output;
- different geometry between dark and light modes;
- raster stretching in wallpaper output; or
- a material similarity to an existing logo family.

Approval requires a dated human decision tied to exact source and render
hashes. “The generator passed” or “the SVG is valid” is not visual approval.

## Originality and clearance gates

After visual acceptance but before asset wiring:

1. freeze and hash the normalized geometry;
2. render 2048-pixel dark, light, and monochrome search images;
3. perform Google Lens plus an independent reverse-image search;
4. perform ordinary buzzard/hawk/eagle/raptor and software/security/Linux logo
   searches;
5. search the relevant WIPO, UK IPO, EUIPO/TMview, and intended-territory
   registers;
6. record close results and compare silhouette, face, beak, eye, negative
   space, pose, colour lockup, and goods/services;
7. redesign and repeat the entire search when similarity is material; and
8. obtain professional trademark review before calling the public brand
   legally cleared.

No automated similarity score can mark the candidate “cleared.”

## Asset-wiring gate

Do not run the production generator against a new candidate until all of these
exist:

- approved vector-review record;
- accepted source and render hashes;
- passed structural report;
- passed multi-size visual report;
- completed non-legal similarity screening record; and
- an honest clearance status.

Only then import the approved normalized path data into
`guest/branding/buzzard-mark.json`, regenerate every variant, verify the
manifest/migration mappings, run deterministic and security tests, and inspect
the generated files again. Any material geometry edit invalidates the previous
render and similarity evidence.

## Authoritative technical sources

- [W3C SVG 2 overview](https://www.w3.org/TR/SVG2/) defines SVG as XML-based,
  resolution-independent two-dimensional graphics.
- [W3C SVG 2 paths](https://www.w3.org/TR/SVG2/paths.html) defines path elements,
  path data, and Bezier commands.
- [W3C SVG 2 coordinate systems and `viewBox`](https://www.w3.org/TR/SVG2/coords.html#ViewBoxAttribute)
  defines viewport-to-user-coordinate mapping.
- [W3C SVG 2 conformance processing modes](https://www.w3.org/TR/SVG2/conform.html#secure-static-mode)
  defines secure static mode as disabling script, external references,
  animation, and interactivity.
- [Inkscape: editing paths with the Node tool](https://inkscape-manuals.readthedocs.io/en/1.1/editing-paths.html)
  documents visual node, handle, and segment editing.
- [Inkscape: Node tool options](https://inkscape-manuals.readthedocs.io/en/1.1/node-operations.html)
  recommends keeping paths editable with as few nodes as practical.
- [Inkscape: Pencil and B-spline tools](https://inkscape-manuals.readthedocs.io/en/1.3/pencil-tool.html)
  documents visually authored paths and adjustable smoothing.
- [Official Inkscape Linux AppImage page](https://inkscape.org/release/all/gnulinux/appimage/)
  provides a no-system-install editor distribution.
- [Potrace technical documentation](https://potrace.sourceforge.net/potrace.pdf)
  explains curve optimization, tolerance, and coordinate quantization.
- [VTracer official repository](https://github.com/visioncortex/vtracer)
  identifies it as a raster-to-SVG tracer, not a semantic originality or logo
  approval tool.
- [`resvg` official repository](https://github.com/linebender/resvg) documents
  its static SVG renderer, extensive regression suite, portability, and
  reproducible rendering goal.
- [StarVector official repository](https://github.com/joanrod/star-vector)
  documents its image/text-to-SVG model and its icon/logotype focus.
- [OpenAI GPT Image 2 model page](https://developers.openai.com/api/docs/models/gpt-image-2)
  describes the current model as image input/output.
- [OpenAI GPT-5.6 model guidance](https://developers.openai.com/api/docs/guides/latest-model)
  identifies `gpt-5.6-sol` as the frontier GPT-5.6 variant and documents its
  high-through-max reasoning options.
