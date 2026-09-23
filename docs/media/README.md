# README media

User-facing stills live in `readme/` and are committed so GitHub can render
the README. Working takes (the demo video, ad-hoc stills) stay gitignored in
this directory.

```sh
bash scripts/capture-readme.sh
```

Isolated daemons, no login plugin, no desktop config. Needs a working desktop
OpenGL session. Windows are 640×480.

The existing U.S. radar screenshots below are **presentation stills**, not a live take: they paint the archived
Moore/KTLX volume (`data/raw/KTLX20130520_201643_V06.gz`) with live chrome
(LIVE, a short age, no ARCHIVED badge) so the README shows a real storm
instead of whatever the feed is doing today. Tokyo Night is the dark theme;
Flexoki Light is the light one.
The aviation window and popover are live captures from September 22, 2026.

| File | What it is |
| --- | --- |
| `readme/omastorm-europe.png` | Actual OPERA radar over Warsaw, from the Europe demo |
| `readme/hero.png` | Window + popover, Tokyo Night |
| `readme/window.png` | Window, Tokyo Night |
| `readme/window-light.png` | Window, Flexoki Light |
| `readme/themes.png` | Dark and light side by side |
| `readme/popover.png` | Bar popover, Tokyo Night |
| `readme/aviation-window.png` | Live NEXRAD window with airport categories and a raw METAR |
| `readme/aviation-popover.png` | Bar popover with airport categories |
| `readme/onboard.png` | First-run location prompt |
| `readme/search-city.png` | Search, city query |
| `readme/search-site.png` | Search, site id |
| `readme/search-coords.png` | Search, pasted coordinates |
| `readme/search-error.png` | Search, latitude out of range |
| `readme/locate-fail.png` | Approximate-location overlay |
| `readme/treatments.png` | Pixels, Glyphs, Stipple side by side |

`bash scripts/capture-demo.sh` still writes `omastorm-demo.mp4` and
`omastorm-preview.gif` here for omastorm.com; those are not in the README
until the take is recut against the current keys.

The Europe demo is `omastorm-europe.mp4` (24 seconds, 800×600), published
with [Omastorm 0.1.14](https://github.com/wesleygrimes/omastorm/releases/tag/v0.1.14).
Videos stay out of git; attach them to a plugin release before adding public links.
Its poster is committed as `readme/omastorm-europe.png` and mirrored in
`site/media/omastorm-europe.png`. Unlike the U.S. presentation stills, this
capture retains the actual OPERA scan and displayed age.

Radar: NOAA NEXRAD and EUMETNET OPERA (CC BY 4.0). Map: © OpenStreetMap contributors
([ODbL](https://opendatacommons.org/licenses/odbl/1-0/)); Natural Earth, public
domain.
