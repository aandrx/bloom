# Design preview privacy and provenance review

Baseline: `4e4a352`. Review scope is the 12 tracked PNGs in `design/previews/`
and their relationship to `design/mockup/index.html`. This review did not start
Bloom, load registration state, or use a real conversation.

## Visible content and privacy

All 12 images were opened and inspected. Each shows the same offline interface
study, with desktop, mobile, search, or terminal-style mockup states. The images
carry the labels "Illustrative messages only, not a live service" and "LOCAL
CONCEPT · OFFLINE". The terminal-style previews are not captures of the current
Rust TUI, and the GPUI filenames are not evidence of an implemented GPUI client.

The previews show sample conversations such as Weekend crew, Jules Lee, Sunday
table, Design systems, and Field notes. Messages concern a coast trip, coffee,
and pastries. No visible email address, phone number, account ID, credential,
personal path, browser profile, or real attachment was identified. Avatars use
initials rather than portraits. The backgrounds are abstract shapes, not visible
personal photos.

These observations support the mockup explanation, but do not prove that every
name or sentence was invented. Pixel inspection cannot establish authorship,
consent, or an asset license. This is a scoped privacy review, not a claim that
arbitrary data cannot be hidden in pixels.

## PNG structure

A read-only parser checked the signature, every chunk CRC, image dimensions,
ancillary chunks, and data after `IEND` for every PNG.

- All 12 files are structurally valid under those checks.
- All contain only `IHDR`, `IDAT`, and `IEND` chunks.
- None has text, EXIF, XMP, time, author, or other ancillary metadata chunks.
- None has trailing bytes after `IEND`.
- Eight images are 1728 by 1320 pixels. Four are 468 by 1012 pixels.
- There are 10 distinct byte sequences. `gpui-home-preview.png` equals
  `gpui-preview.png`, and `gpui-mobile-preview.png` equals `mobile-preview.png`.
  These duplicates are not a privacy defect and were left unchanged.

## Inspected inventory

All filenames below are relative to `design/previews/`. SHA-256 values bind the
review to the inspected bytes. `sha256sum design/previews/*.png` reproduces them.

| File | Visible state | SHA-256 |
| --- | --- | --- |
| `gpui-custom-background-preview.png` | Desktop home with a different abstract background | `d07b9c3f39a4adacbb8aed73a010b4e903e27df233d66d2bc1a5a7fc0674b577` |
| `gpui-empty-search-preview.png` | Desktop search with no results | `92651d930f0052dd14a290fb45dce5cbad4a4419fac1fe9e662d22d58c0f77d3` |
| `gpui-home-preview.png` | Desktop home | `41974b9dfd655a97fb81dec104976fa85405cc2110e63bf6f33cbb9ed684a1e9` |
| `gpui-mobile-preview.png` | Mobile home | `0b81bb5fb1f5f77e846b0536c7a00bba7a76e7fb4de0d63e99a6f161933e203d` |
| `gpui-mobile-thread-preview.png` | Mobile Weekend crew thread | `19c6cb6a91588113f42442716188871c858b4704807e7704cbf68382390deafe` |
| `gpui-preview.png` | Duplicate desktop home | `41974b9dfd655a97fb81dec104976fa85405cc2110e63bf6f33cbb9ed684a1e9` |
| `gpui-search-preview.png` | Desktop search for tide | `5f8a5d86ff41858c0ef2fccfe7246172b7bcf982102e9a506dff8bf5763e9ced` |
| `gpui-thread-preview.png` | Desktop Weekend crew thread | `773c4f06c001f7356d66b7f20f00d15e244c24451141cffec073200f9b0fadfe` |
| `mobile-preview.png` | Duplicate mobile home | `0b81bb5fb1f5f77e846b0536c7a00bba7a76e7fb4de0d63e99a6f161933e203d` |
| `tui-light-preview.png` | Light terminal-style mockup | `b9a5c22717f5cc190f4c63ab8a5d3e8f82cd5ba2f4da61685c284336ed45adea` |
| `tui-mobile-preview.png` | Mobile terminal-style mockup | `ce1b43f8f82d0b7bf309935387fa177a507c2ef33350f5ddd21bab873e362212` |
| `tui-preview.png` | Dark terminal-style mockup | `f7409648c6cee6ae5e5dd43a4fd66f30bfad95aa6c90dc7c63faef1122496d8b` |

## Provenance limits

In the reachable rewritten history, the PNGs and mockup HTML appear together in
root commit `7502f29`, with no later changes to `design/`. Repository history
records that introduction, not the earlier creation process or third-party
rights. No pre-rewrite history or GitHub-side object was inspected in this review.

An independent read-only source review found the following, which the parent
checked against `design/mockup/index.html`:

- The chat titles, initials, and visible coast-trip messages match inline literals
  in the HTML and script at lines 23-29. There is no account or database lookup.
- The default background is an inline SVG at line 23. Small control icons are
  inline SVG paths or text. The source has no external image, stylesheet, script,
  or font reference and no network request code.
- CSS names Geist and Inter before system font fallbacks at line 12. It includes
  no font files, `@font-face`, or font download. The exact font used for the
  existing PNGs is not recorded.
- The custom-background control at lines 23 and 29 accepts a local image and uses
  `URL.createObjectURL`. It revokes the URL on replacement, reset, or page unload.
  No upload or storage code was found. This is source inspection, not a browser
  network trace or a guarantee about a user-selected file.

The visible content and committed page support these being mockup captures. The
existing `tui-shot` and `tui-review` scripts capture the Rust TUI with synthetic
fixtures, not these HTML mockup states. No capture recipe records the browser,
font environment, state-selection steps, or the custom-background input for these
12 PNGs. Exact-byte reproduction from this HTML has not been attempted or proven.
A missing recipe does not prove that the images came from another source.

The custom-background preview shows a different abstract illustration, not an
identifiable photograph. The input asset for it is not in the tracked design
files. The owner confirmed on 2026-10-28 that the preview content is fictional
and that the owner holds the artwork rights. This is owner confirmation, not an
independent legal opinion. The pixel review found no visible privacy issue in it.

The root MIT license is present. There is no design-specific provenance note
identifying the creators of the illustration and icons, whether any were copied,
or a third-party asset license. The dependency notice bundle is not an inventory
of design artwork. Absence of external references does not prove authorship or
remove an attribution obligation if an asset was copied.

## Publication outcome

The pixel and PNG-metadata review found no privacy issue requiring redaction.
No image, design source, or development tool was changed. The owner confirmed the
fictional fixtures and artwork ownership on 2026-10-28; the provenance check is
resolved for publication purposes, subject to the remaining legal review:

1. Resolved: the owner confirmed the sample text and names are illustrative and
   that the preview artwork is owned by the owner.
2. The custom-background input asset is not tracked, but the owner confirmed its
   artwork rights. No replacement is required. Keep using documented project
   artwork for future previews; this does not remove local background
   customization or change the development application.

A future capture recipe would improve reproducibility but is not proof of legal
ownership and is not, by itself, a publication blocker. These images must remain
labeled illustrative rather than screenshots of the current production client.
