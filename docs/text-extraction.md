# Text extraction

`moment capture … --text` and `Ctrl+Shift+C` in the editor read the text in
a screenshot and copy it as plain text. English is the main target; other
European languages, Chinese, Japanese, Korean, Vietnamese, Thai and Arabic
(and anything else Tesseract has a model for) work too, including
screenshots that mix them line by line.

## Requirements

[Tesseract](https://github.com/tesseract-ocr/tesseract) 4 or 5 and one model
per language. moment opens `libtesseract.so.5` when text is first extracted,
so it builds and runs without it; until it is installed, extraction reports
what to install.

| Language | Arch | Debian/Ubuntu |
| --- | --- | --- |
| Engine | `tesseract` | `tesseract-ocr` |
| English | `tesseract-data-eng` | `tesseract-ocr-eng` |
| Japanese | `tesseract-data-jpn` | `tesseract-ocr-jpn` |
| Chinese (simplified / traditional) | `tesseract-data-chi_sim` / `-chi_tra` | `tesseract-ocr-chi-sim` / `-chi-tra` |
| Korean | `tesseract-data-kor` | `tesseract-ocr-kor` |
| Vietnamese | `tesseract-data-vie` | `tesseract-ocr-vie` |
| Thai | `tesseract-data-tha` | `tesseract-ocr-tha` |
| Arabic | `tesseract-data-ara` | `tesseract-ocr-ara` |
| German, French, Spanish, … | `tesseract-data-deu`, `-fra`, `-spa`, … | `tesseract-ocr-deu`, `-fra`, `-spa`, … |

Models are found in `$TESSDATA_PREFIX`, `/usr/share/tessdata` (Arch) or
`/usr/share/tesseract-ocr/{5,4.00}/tessdata` (Debian/Ubuntu).

## How it reads

1. The region is converted to gray, made dark-text-on-light per area (dark
   themes, light windows on dark desktops and mixed layouts all work), and
   enlarged 2× unless the capture is already HiDPI.
2. The primary model (English unless [configured](configuration.md))
   reads the whole region and finds the text lines.
3. Lines it reads with less than 90% confidence are re-read by every other
   installed model at once, one thread per model. The most confident
   reading wins if it beats the primary by 5 points, and a non-Latin model
   only wins with letters of its own script (CJK models otherwise "read"
   English with full-width punctuation). A reading of 93% or more settles
   a line, so slower models skip it.
4. Spacing is rebuilt for scripts Tesseract splits into one word per
   character: none inside Chinese, Japanese and Thai, Korean word breaks from
   the gaps between syllables. CJK "small form" punctuation (`﹖`) becomes
   the full-width form screens show (`？`).

The text is one line per text line, in reading order.

## Speed and memory

Release build, 16-core desktop, all ten models above installed:

| Input | Time |
| --- | --- |
| Chat screenshot, 473×293, 5 lines | ~150 ms |
| Synthetic 9-language list, 700×308 | ~140 ms |
| First extraction after start-up (models loading in parallel) | +100–200 ms |
| 2560×1440 full of dense chat text, ~150 lines | ~3.4 s |

The primary model loads in the background when the daemon starts and stays
loaded (about 30 MB). The others load on first use, in parallel, and are
unloaded again after two idle minutes; all ten together take about 400 MB
while loaded.

## Limits

- Only the screenshot is read, not annotations drawn on it.
- Debian/Ubuntu ship Tesseract's "fast" models. With both `chi_sim` and
  `chi_tra` installed they sometimes read simplified Chinese as the
  traditional characters (识别 → 識別); install only the variant you read,
  or list it in `ocr.languages`. Arch's models do not show this.
- Vertical text, handwriting and text smaller than about 9 px are not read
  reliably.
