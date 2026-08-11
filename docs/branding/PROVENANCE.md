# Wild Buzzard mark: research and provenance

- Candidate: `wb-common-buzzard-three-quarter-b2`
- Geometry revision: `2026-08-10.b2`
- Status: original project-authored candidate; visual-similarity and trademark
  clearance pending
- Copyright license: `AGPL-3.0-or-later`, under the repository `LICENSE`
- Canonical geometry: `guest/branding/buzzard-mark.json`
- Deterministic generator: `guest/branding/generate.py`

## Authorship boundary

The candidate is new project-authored vector geometry constructed on a blank
256 by 256 artboard. No reference photograph, third-party logo, stock vector,
generated raster, or pre-existing SVG path is embedded, auto-traced, sampled,
or copied into the repository. The deterministic JSON contains four compound
filled paths: the portrait silhouette, pale cheek/throat/bib, dark
beak/eye/wing/mottle detail, and restrained Cinnamon cere/feather accents.

The selected direction is a calm near-front three-quarter bust with one
visible natural-sized eye looking slightly to the viewer's left rather than at
the viewer. The rounded head, compact hooked beak, short neck, stocky chest,
uneven folded shoulders, and irregular mottled bib intentionally avoid eagle
heraldry, a falcon helmet, an owl facial disc, mirrored mascot eyes, and the
former central cyclops/flying-wing symbol.

Two disposable external review inputs informed species-level anatomy only:

- a user-supplied Wikimedia common-buzzard reference JPEG, SHA-256
  `97ff810dbef7f1afd8f996069027babd73d4a2e5fe7b85e816103761c9ba4fe2`;
  its exact upstream file page was not recoverable from the review copy, so it
  is deliberately not redistributed or treated as a reusable licensed asset;
- a generated anatomy/composition study, SHA-256
  `cd21ebdfefaec3ea6a91ef9b425a65f62b437733c541d1849630d12990e1`,
  consulted for the broad head, compact beak, substantial shoulders, and calm
  off-axis pose only.

Both files remain outside the repository. Neither was overlaid, traced,
vectorized, or used to choose production control-point coordinates. They are
not runtime assets or distribution dependencies.

## Authoritative species references

Accessed 2026-08-10:

- [RSPB: Buzzard (`Buteo buteo`)](https://www.rspb.org.uk/birds-and-wildlife/buzzard)
  identifies the species and describes a large bird of prey with broad,
  rounded wings, a short neck and tail, highly variable brown/pale plumage, a
  yellow beak with a black tip, and an often hunched posture. Design
  implication: keep a broad, weighty shoulder line, short neck, modest beak,
  and avoid relying on one exact plumage pattern.
- [BTO BirdFacts: Buzzard](https://www.bto.org/learn/about-birds/birdfacts/buzzard)
  confirms the scientific name and presents curated identification media and
  a perched-bird reference. Design implication: use the perched upper-body
  proportions and whole-bird character, not flight-wing geometry.
- [BTO: Identifying Common Buzzard and Honey-buzzard](https://www.bto.org/learn/skills/bird-identification/videos/summer-buzzards-common-buzzard-and-honey-buzzard)
  is an identification resource for the most relevant confusion species.
  Design implication: avoid the Honey-buzzard's more prominent small-head
  impression.
- [Macaulay Library/eBird: Common Buzzard media catalogue](https://media.ebird.org/catalog?mediaType=photo&sort=rating_rank_desc&taxonCode=combuz1&view=list)
  provides independently catalogued `Buteo buteo` observation photographs.
  It was used only to check the range of natural perched head angles and
  plumage variation; no individual photograph was used as a tracing model.

## Concept comparison and selected revision

The retained concept SVGs record earlier silhouette families, not alternate
canonical sources. Their orange profile treatment and the original abstract
flying/cyclops mark were rejected. Revision `b2` moves the beak inward, keeps
the head near the body axis, exposes only one off-axis eye, broadens the mantle,
and replaces the full orange silhouette with a neutral naturalistic mass and a
small Cinnamon accent.

The canonical candidate has four filled compound paths, 24+ units of safe
space, one small off-axis eye, no direct eye contact, and no scale-dependent
stroke. The deterministic review sheet places both icon palettes at actual 16,
24, 32, 64, and 256 physical pixels and the unboxed marks at 256 pixels.
Selection remains provisional until the clearance record and responsible human
visual review are complete.

## Palette and derivation

All dark, light, unboxed, symbolic, guest-icon, host-icon, Settings-icon, and
wallpaper outputs are emitted from the canonical JSON. Palette changes never
alter geometry. The symbolic variant deliberately retains only the outer
portrait/beak path so it stays readable at 16–24 pixels.

The colour values implement the product contract; they are not sampled from a
reference image or third-party mark. The generated SVGs use only filled paths
and rectangles, with no gradients, filters, blur, texture, masks, or strokes.
