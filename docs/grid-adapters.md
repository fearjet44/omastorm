# Grid radar adapters

The contract for georeferenced radar mosaics in the live picture.

Related: [DESIGN.md](../DESIGN.md), [protocol.md](protocol.md),
[radar-fetch.md](radar-fetch.md), and source research
[#38](https://github.com/wesleygrimes/omastorm/issues/38).

This file defines settled behavior. Implementation status, source research,
and delivery order belong in issues and pull requests, not here.

## Problem

Omastorm draws one polar sweep: NOAA NEXRAD Level II, one station at a
time. The GPU samples gates. There is no mosaic, GeoTIFF, or radar-tile
path. Site search is clipped to the NEXRAD envelope.

Many radar networks publish a **georeferenced composite**, not open
polar volumes. Those products require a grid path; a new polar decoder
cannot represent them.

The same path supports free national and continental mosaics without a
country mode or a second app.

## Non-goals

- An international mode, region picker, or settings page for "where".
- Vendoring provider GeoTIFFs, COGs, or HDF5 into this MIT tree or
  Releases.
- Choosing HDF5 when the same provider product is available as COG or
  GeoTIFF.
- Precolored consumer tiles or any paid / key-gated feed as a default
  source.
- Polar ODIM / DX / ORD `PVOL` readers. Those stay PolarFamily.
- Faking NEXRAD Level II history, tilts, or a painting sweep on a
  mosaic that has none.
- Forecasts or a second chrome.

## Source vs site

Today a **site** is a dish. `hello.sites` lists them. `select_site`
starts the live loop on one. The map centre and that source are
independent: loading a frame never moves the camera.

A **source** is compiled into a `SourceRegistry`. It is one adapter:
either a PolarFamily site feed or a GridFamily mosaic. The live loop
has **one** active source.
Switching cancels the previous poller, as `select_site` does now.

| | PolarFamily | GridFamily |
|---|---|---|
| Kind | `site` | `mosaic` |
| What it is | One dish, polar sweep | Georeferenced raster |
| Coverage | Range around a lat/lon | The mosaic's ground box |
| Geometry | Gates, rays, tilt when real | Grid, CRS, affine |
| Frames | May be `partial` while painting | Complete snapshots |
| History | NEXRAD: last two hours, 60 max | Whatever the adapter actually has |

Map and search pick a **covering** source, where covering means the
source's coverage contains the centre, not that its dish is nearest.
No separate international switch. Prefer a covering PolarFamily site
when one exists. Otherwise pick the covering GridFamily mosaic. A centre
with no covering source is the map without radar, as a station with no
frame is today.

A mosaic is not a fake dish. Do not invent a station id, rings, or a
tilt so it fits `hello.sites`. Lock pins the source. Unlock returns to
covering-source selection from the centre. Choosing a polar site in
search still locks that dish and centres on it. Choosing a mosaic
locks that mosaic; the camera stays the user's.

## On screen

Same chrome, timeline, treatments, and keyboard. Radar color still
comes only from `frame.palette`. Pixels, Glyphs, and Stipple stay.

- **No international mode.** Geography and search already choose.
- **Product line.** Mosaic frames show `frame.productName`, not a
  station sweep name. Attribution belongs on screen with the data, as
  OSM and Natural Earth already do.
- **Tilt.** Show an elevation only when the product is a real tilt.
  Mosaics omit it. Do not send a dummy `0.5`.
- **Rings / lock.** Polar sites keep rings. Mosaics have none, and
  stroke their coverage edge where a dish strokes its footprint arc.
  The lock means the source is pinned, and is yellow on one predicate
  for both families.
- **Timeline.** One tick per real frame, no empty pads. Mosaic
  frames are complete; there is no outlined in-progress sweep unless
  the adapter truly publishes one.
- **History.** Adapters declare their depth. Many mosaics keep
  minutes, not two hours. Loop what is there. Do not pad, repeat, or
  label a short catalog as a 2 h Level II scrubber.
- **Age.** LIVE / STALE / UNAVAILABLE still follow the newest
  complete frame's age, not the join.

## Adapter interface

This is the Rust adapter boundary. Adapters are compiled in and are not
discovered at runtime.

```rust
enum Family { Polar, Grid }
enum Kind { Site, Mosaic }

// A dish sees a circle. A mosaic covers a box, or a polygon where the
// grid is projected and a lat/lon box would over-claim. Selection
// tests containment against this; the lock paints from it.
struct GeoPoint {
    lat: f64,
    lon: f64,
}

enum Coverage {
    Circle { lat: f64, lon: f64, radius_km: f64 },
    Box { north: f64, south: f64, east: f64, west: f64 },
    Polygon(Vec<GeoPoint>),
}

struct SiteCoverage {
    site_id: String,
    coverage: Coverage,
}

enum AdapterCoverage {
    Sites(Vec<SiteCoverage>),
    Mosaic {
        coverage: Coverage,
        selection_priority: i32,
    },
}

struct SourceMetadata {
    id: String,
    family: Family,
    kind: Kind,
    name: String,
    attribution: String,
    coverage: AdapterCoverage,
}

enum AdapterTarget {
    Site { site_id: String },
    Mosaic,
}

// The adapter boundary is explicit about the two geometries. PolarFrame is
// today's sweep frame; GridFrame is the shape below. Neither impersonates
// the other to cross the registry.
enum AdapterFrame {
    Polar(PolarFrame),
    Grid(GridFrame),
}

trait RadarAdapter: Send + Sync {
    type Frame;

    fn metadata(&self) -> &SourceMetadata;
    async fn poll(&self, target: &AdapterTarget) -> Result<Vec<Self::Frame>, String>;
    async fn backfill(&self, target: &AdapterTarget) -> Result<Vec<Self::Frame>, String>;
}
```

`SourceRegistry` uses enum dispatch over every adapter this build
compiled. Each implementation binds `Frame` to `PolarFrame` or
`GridFrame`; the registry wraps those values in `AdapterFrame` for
shared state without exposing boxed-future plumbing. Hello lists the
adapters as `sources`, not one row per dish. A PolarFamily adapter is
one feed with many sites (`hello.sites`). A GridFamily adapter is one
mosaic. The live loop calls `poll` / `backfill` on the one active
selection. PolarFamily NEXRAD keeps today's chunk join
([radar-fetch.md](radar-fetch.md)); GridFamily adapters fetch their
own objects. Coverage is stored by the adapter and borrowed by the
registry: a polar feed returns one `SiteCoverage` per dish, while a
grid returns its one mosaic coverage. Selection does not allocate a
polygon on every settled center. `AdapterTarget` makes the selected
polar site explicit instead of hiding mutable selection inside the
adapter.

Hot path: unsigned HTTPS, no API key, no account. Timeouts and body
caps stay. A source that needs a key on every poll does not ship as a
default.

## GridFrame

GridFamily frames use this wire shape. PolarFamily frames keep the
sweep + azimuth lookup in [protocol.md](protocol.md).

A grid frame is measured values on a georeferenced raster, not a
precolored map and not polar gates.

| Field | Role |
|---|---|
| `id`, `scanTime` | Same job as today |
| `status` | `complete` (mosaics do not paint a sweep) |
| `product`, `productName`, `units` | Engine vocabulary; UI lays it out |
| `palette`, `bounds` | Shared color classes; v2 bounds are JSON numbers, including fractional values |
| `texture` | Runtime RGBA8 PNG of classified values, `tex/` rule unchanged |
| `width`, `height` | Raster size |
| `crs`, `geotransform` | Native georeference; shader maps a map cell to a pixel |

No `azimuthLut`. No rays/gates. The decoder classifies each native
measured value against `bounds` before writing the texture: R is palette
class + 1, G bit 0 is missing, G bit 1 is undetect, and B/A are zero/255.
This texture is a display product, not a lossless export of provider
measurements. Grid frames omit polar `scale` / `offset`, so the dBZ
weak-return floor is disabled. Every adapter defines bounds and a palette
in its own units. The engine still owns product, unit, and color; radar
arrays still never enter JSON or QML.

The UI samples a map cell through the grid's georeference, not through
site-relative slant range. That is a second sampling path. It is not
an overlay of someone else's JPEG.

## Protocol v2

Grid frames are not additive to protocol v1. A v1 client requires the
polar `azimuthLut` path and sweep geometry, so omitting those fields for
a mosaic would make it reject the whole state. Grid support uses protocol
v2; a v1 client reports the existing incompatible-version error instead
of trying to draw a grid.

`hello` lists compiled sources. Polar sites remain in `hello.sites`.
Mosaics do not masquerade as sites.

```json
{"type":"hello","v":2,"engine":"0.1.13",
 "sites":[{"id":"KTLX","sourceId":"nexrad",
           "name":"Oklahoma City","state":"OK",
           "lat":35.33306,"lon":-97.27748,"altM":388.0,
           "coverage":{"kind":"circle","radiusKm":460}}],
 "sources":[{"id":"nexrad","family":"polar","kind":"site",
             "name":"NOAA NEXRAD","attribution":"NOAA NEXRAD"},
            {"id":"fixture-mosaic","family":"grid","kind":"mosaic",
             "name":"Fixture mosaic",
             "attribution":"Omastorm fixture",
             "selectionPriority":100,
             "coverage":{"kind":"box","north":1,"south":0,
                         "east":1,"west":0}}]}
```

Wire keys use camelCase exactly as shown. Every source carries `id`,
`family`, `kind`, `name`, and `attribution`; grid sources also
carry `coverage` and `selectionPriority`. Every site carries `sourceId`
and its circle `coverage`; dispatch never infers a source from a
station-id prefix.

In v2, `state.mode` is `archived` or `live`; it takes over the job of
v1's `state.source`. `state.sourceId` names the active adapter from
`hello.sources`. Do not reuse `source` for both concepts. Mosaic `frame`
objects carry `kind: "mosaic"` and georeference fields and omit polar-only
keys. The affine, CRS, width, and height define the texture extent.
Attribution and selection coverage come from the active source in
`hello.sources`; they are not repeated on every frame. Polar frames carry
`kind: "polar"` and keep today's sweep fields. These are the wire forms
of `AdapterFrame`.

`select_site` remains polar-only. A mosaic is selected by its source id
through `select_source`. Unknown ids `error`, each against its own table.
Follow / lock / view_center keep their jobs: the engine never moves the
camera; unlocked follow uses the deterministic selection rule below.

The gazetteer and map envelope include every compiled live source.
Search still returns `place` or `site`; a mosaic is not a site row.
Enter on a place in mosaic-only coverage centres, unlocks, and selects
that mosaic.

## Attribution and license

Every adapter carries an attribution string. Show it on screen with
the data and record the provider and license in README. OSM (ODbL) and
Natural Earth stay.

| Rule | Why |
|---|---|
| Live fetch, runtime cache only | Do not vendor government rasters under MIT |
| No GeoTIFF / COG / HDF5 in git or Releases | Same; fixtures are synthetic and tiny |
| App code stays MIT | Provider share-alike applies to redistributed data, not this tree |
| Provider and license recorded | Attribution and redistribution terms are source-specific |
| No key on the hot path | A token in config is not a default source |

An adapter is not compiled into `SourceRegistry` until its access and
license terms satisfy these rules. Unconfirmed providers remain in
external research, not this specification.

## Rules

Settled. Ask before violating.

### Lock and coverage

The lock is yellow when the source is pinned and the camera centre is
outside that source's coverage. One predicate for both families: a
distance test for a dish, a point-in-box or point-in-polygon test for a
mosaic.

Coverage is adapter-declared; `coverageKm` lives in the NEXRAD adapter,
not `ui/RadarMap.qml`. Mosaics stroke their coverage edge where dishes
stroke the footprint arc, densified in Mercator, and have no rings.

### CRS

Grids sample their native CRS in the shader: lon/lat, then the grid's
CRS, then `inverse(geotransform)` to a pixel. One resample, from
provider pixels. The engine does not warp rasters to Mercator.

A CRS ships only when its forward projection is closed-form GLSL.
Identity (lon/lat), Mercator, transverse Mercator, polar stereographic,
and Lambert conformal conic are the supported set. Anything else fails
the build.

A CRS entry carries its ellipsoid; the grid path does not assume the
6371 km sphere the polar path uses. Where a projdef omits a datum
shift, the adapter names the resulting error.

### Source selection

Covering means the source's coverage contains the centre. On a settled
centre:

1. If any polar site contains the centre, PolarFamily wins. Among polar
   sites, today's nearest-site hand-off remains: the held site stays until
   another beats it by `HANDOFF_RATIO` 0.8 and
   `HANDOFF_MARGIN_KM` 1 km.
2. With no covering polar site, keep the held grid while its coverage
   still contains the centre. Overlapping grids therefore do not flap.
3. If the held grid no longer covers, choose the covering grid with the
   highest `selection_priority`; break an equal priority by adapter id so
   registry order cannot change the result.
4. With no covering source, show the map without radar.

Grid priority is compiled adapter metadata, not a country mode or a user
preference. A national product normally ranks above a continental
fallback. Every adapter declares its value, and overlapping adapters
have a selection test. Priority selects on entry only: it does not
preempt a still-covering held grid.

### Commands

`select_site` is polar-only: a station from `hello.sites`, implying its
source. Internal hand-off uses it.

`select_source` names an id from `hello.sources`. For a mosaic that is
the whole selection; for a polar feed it is that feed with the site
nearest the centre. Unknown ids error against their own table. Protocol
v2 retains `select_site` and adds `select_source`.

## See also

Parent research: [#38](https://github.com/wesleygrimes/omastorm/issues/38).
Provider candidates, access findings, and delivery status stay there
until they become settled source contracts.
