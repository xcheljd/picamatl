# CMYK / YCCK JPEG support

Four-component `DCTDecode` streams — `/DeviceCMYK`, `ICCBased` with `N 4`,
`Separation`/`DeviceN` over four inks — now have a dedicated path through the
image pipeline instead of falling into a decoder that guesses.

## What was wrong before

`decode_jpeg_scaled` declined CMYK and YCCK with the comment *"CMYK/YCCK need
a color conversion we don't want to hand-roll"*, and every caller then fell
back to `image::load_from_memory_with_format`. That fallback does not decline:
the `image` crate converts CMYK to RGB and hands back a three-channel
`DynamicImage`. `encode_jpeg` re-encoded those three channels, and the
replacement was written back under a **`/ColorSpace` that still said
`/DeviceCMYK`**.

This was not theoretical. `corpus-expanded/cmyk-jpeg.pdf` (Mozilla pdf.js test
suite) reproduced it on `main`:

| | payload | components | APP14 transform |
| --- | ---: | ---: | ---: |
| input | 13,857 B | 4 | 0 (CMYK) |
| `main` output | 4,183 B | **3** | none |
| this branch | 9,851 B | 4 | 2 (YCCK) |

Rendered page 1 at 72 dpi through Ghostscript and compared against the
original render:

| | mean abs. diff | max abs. diff | pixels differing > 32 |
| --- | ---: | ---: | ---: |
| `main` output | 5.911 | **251** | 27,975 (5.77%) |
| this branch | 0.188 | 30 | 0 (0.00%) |

The damage is confined to exactly the image bounding box, `(27, 28)-(227, 178)`
— the 200×150 CMYK image. The README's claim that "amatl's conservative CMYK
fallback path declines it" was wrong; it re-encoded it, and the smaller number
main reported for this file was bought with a corrupt page. That number moves
from 76.7% to 78.2% on this branch, which is the honest one.

## The design, and why the classic bug class is unreachable

Everything works in **raw stored-sample space** and never interprets it.

- **Decode.** `decode_cmyk_jpeg_scaled` asks libjpeg for `JCS_CMYK` output
  explicitly. For a YCCK stream (APP14 transform 2) libjpeg applies the
  inverse Adobe transform; for a plain CMYK stream (transform 0) it passes
  samples through. Either way we hold the samples as stored.
- **Resample.** All four channels go through Lanczos3 — the same filter and
  the same `image` implementation the RGB path gets from `resize_exact` —
  each as an independent 8-bit plane, so the fourth channel can never be
  mistaken for alpha by an image type that has one.
- **Encode.** `in_color_space = JCS_CMYK` plus `jpeg_set_colorspace(JCS_YCCK)`.
  libjpeg applies the forward transform and emits the APP14 marker itself, so
  the marker and the pixel transform cannot disagree — which is precisely the
  mechanism that produces channel-swapped CMYK JPEGs.

**amatl never parses APP14 and never inverts a channel.** The "is it
inverted?" question — Adobe writes CMYK JPEGs with `255-x` samples, most other
producers do not — is a property of the sample values, and those pass through
unchanged. Whatever convention the input used, the output uses the same one, so
the unchanged `/ColorSpace` keeps meaning exactly what it meant before.

### What we found about `/Decode`

Streams carrying a `/Decode` array are **declined outright**. The common one on
DeviceCMYK is `[1 0 1 0 1 0 1 0]`, a producer inverting the whole image.

Passing raw samples through would still be correct under such a remap *in
exact arithmetic*, because inversion and a linear resampling kernel commute.
They stop commuting once Lanczos3's ringing overshoot is clamped to `0..=255`,
because the clamp happens on the pre-`/Decode` values: an overshoot clipped at
255 before inversion is not the same as one clipped at 0 after it. The
asymmetry is small, but it lands on exactly the images where a polarity mistake
is invisible to us and glaring to a reader. `plan_mask_resample` declines on
`/Decode` for the same reason.

The check is scoped to the CMYK path only, so the RGB pipeline's behaviour is
byte-for-byte unchanged.

## Verification

`CMYK_DECODE_BACK_MAX_MAD = 24.0`, applied **per channel**, not pooled. The
shared `DECODE_BACK_MAX_MAD` of 96 was sized for three-channel catastrophes and
cannot catch four-channel damage — measured on the fixtures here:

| corruption | pooled MAD | caught by 96? |
| --- | ---: | --- |
| C/K swap | 29.3 | no |
| C/M swap | 36.7 | no |
| full C,M,Y,K rotation | 72.5 | no |
| whole-image inversion | 106.2 | yes |

Per channel, all four blow past 24.0. `cmyk_verification_rejects_inverted_and_rotated_channels`
pins that sensitivity so the positive tests cannot pass by being blind.

A legitimate resample-plus-q78 round trip in raw sample space lands in the
single digits, so 24.0 leaves generous headroom. The ceiling is a backstop; the
guarantee is structural.

### Truncation

libjpeg is deliberately lenient about truncated scans: it fills the missing
data with flat grey, raises a **warning** rather than an error, and returns a
decodable image. Decode-back verification would then compare that grey against
itself and agree. `ends_with_eoi` is the structural guard that stops it — a
stream that does not contain a whole image is exactly the "any uncertainty
declines" case.

## Independent decoder cross-check

Run 2026-08-24 on this machine. Three decoders independent of mozjpeg were
used: **Pillow 12.3.0** (libjpeg-turbo 3.1.4.1), **ImageMagick 7** (`magick
... cmyk:-`), and **`djpeg`** from the system libjpeg-turbo. Ghostscript
provided a fourth, whole-PDF check (the render table above).

Fixture round trip, `cmyk_plain.jpg` (transform 0) vs `cmyk_ycck.jpg`
(transform 2), same picture in both Adobe flavours:

```
djpeg  RGB   MAD plain vs ycck : 1.216
PIL    CMYK  MAD plain vs ycck : 1.397
IM     CMYK  MAD plain vs ycck : 1.397
  per channel  C 2.087  M 1.321  Y 1.929  K 0.251   (PIL and IM agree exactly)
PIL vs IM on the same file      : 0.000
```

Real file, `corpus-expanded/cmyk-jpeg.pdf`, original payload vs this branch's
optimized payload:

```
PIL    CMYK per-channel MAD : C 5.60  M 2.05  Y 2.87  K 2.21
IM     CMYK per-channel MAD : C 5.60  M 2.05  Y 2.87  K 2.21
djpeg  RGB MAD              : 1.32
```

All well inside ordinary q78 requantization noise, and the two independent CMYK
decoders agree to the byte on sample polarity (MAD 0.000 between them), which
is what rules out an inversion that both amatl and one decoder share.

## Fixtures

Regenerate with:

```sh
python3 fixtures/jpeg/generate_cmyk.py
cargo run --release --example gen_cmyk_ycck -- \
    fixtures/jpeg/cmyk_plain.jpg fixtures/jpeg/cmyk_ycck.jpg
```

| file | geometry | APP14 | source |
| --- | --- | --- | --- |
| `cmyk_plain.jpg` | 96×64 | transform 0 (CMYK) | Pillow |
| `cmyk_ycck.jpg` | 96×64 | transform 2 (YCCK) | `examples/gen_cmyk_ycck.rs` |
| `cmyk_large.jpg` | 640×480 | transform 0 (CMYK) | Pillow |

`cmyk_large.jpg` drawn into a 144×108 pt box is 320 effective DPI against the
130 DPI target, so it exercises a real downsample; drawn into 432×324 pt it is
~107 DPI and exercises the dimension-preserving requant instead.

No YCCK encoder is available in this tree's Python or ImageMagick tooling
(this ImageMagick build does not accept `-colorspace YCCK`), so `cmyk_ycck.jpg`
is generated here. It is not simply "whatever amatl emits": the generator calls
libjpeg directly rather than reusing the library's private helpers, and the
result is validated against Pillow, ImageMagick and `djpeg` above.

## Consent

None of this is new consent surface. CMYK images take the same paths RGB
images already took — the over-resolution downsample and the
dimension-preserving requant — under the same default-on contract, the same
`jpeg_quality`, the same never-larger guard, and the same 5% minimum-savings
rule. No new flag; adding one would have meant leaving the corruption above
on by default.
