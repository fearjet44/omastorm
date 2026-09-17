.pragma library
// Map centre, remembered view, and radar lock (DESIGN.md, location and
// remembered state). Pure functions so a check can drive them without a
// window. Config.toml holds deliberate preferences; state.json holds the
// last camera and the UI radar lock.

var DEFAULT_SPAN = 210;
var MIN_SPAN = 25;

function validLat(value) {
    return typeof value === "number" && isFinite(value) && Math.abs(value) <= 90;
}
function validLon(value) {
    return typeof value === "number" && isFinite(value) && Math.abs(value) <= 180;
}
function validPair(lat, lon) { return validLat(lat) && validLon(lon); }

function clampSpan(value) {
    var n = typeof value === "number" && isFinite(value) ? value : DEFAULT_SPAN;
    return Math.max(MIN_SPAN, n);
}

function parseLatitude(text) {
    var t = String(text).trim();
    if (!t) return { empty: true };
    var n = Number(t);
    if (!isFinite(n)) return { error: "latitude is not a number" };
    if (Math.abs(n) > 90) return { error: "latitude must be in [-90, 90]" };
    return { value: n };
}
function parseLongitude(text) {
    var t = String(text).trim();
    if (!t) return { empty: true };
    var n = Number(t);
    if (!isFinite(n)) return { error: "longitude is not a number" };
    if (Math.abs(n) > 180) return { error: "longitude must be in [-180, 180]" };
    return { value: n };
}
function parseCoordFields(latText, lonText) {
    var lat = parseLatitude(latText), lon = parseLongitude(lonText);
    if (lat.empty && lon.empty) return { empty: true };
    if (lat.empty || lon.empty) return { error: "latitude and longitude must both be set" };
    if (lat.error) return { error: lat.error };
    if (lon.error) return { error: lon.error };
    return { lat: lat.value, lon: lon.value };
}

function distanceKm(lat1, lon1, lat2, lon2) {
    var r = Math.PI / 180, dp = (lat2 - lat1) * r, dl = (lon2 - lon1) * r;
    var h = Math.sin(dp / 2) ** 2 + Math.cos(lat1 * r) * Math.cos(lat2 * r) * Math.sin(dl / 2) ** 2;
    return 2 * 6371 * Math.asin(Math.sqrt(Math.max(0, Math.min(1, h))));
}

function containsCoverage(coverage, lat, lon, dishLat, dishLon) {
    if (!coverage || !validPair(lat, lon)) return false;
    if (coverage.kind === "circle") {
        var clat = coverage.lat !== undefined && coverage.lat !== null ? coverage.lat : dishLat;
        var clon = coverage.lon !== undefined && coverage.lon !== null ? coverage.lon : dishLon;
        if (!validPair(clat, clon)) return false;
        return distanceKm(lat, lon, clat, clon) <= (coverage.radiusKm || 0);
    }
    if (coverage.kind === "box") {
        if (lat < coverage.south || lat > coverage.north) return false;
        if (coverage.west <= coverage.east)
            return lon >= coverage.west && lon <= coverage.east;
        return lon >= coverage.west || lon <= coverage.east;
    }
    if (coverage.kind === "polygon" && coverage.vertices && coverage.vertices.length >= 3) {
        var inside = false, verts = coverage.vertices, j = verts.length - 1;
        for (var i = 0; i < verts.length; i++) {
            var a = verts[i], b = verts[j];
            var intersect = ((a.lat > lat) !== (b.lat > lat))
                && (lon < (b.lon - a.lon) * (lat - a.lat) / (b.lat - a.lat) + a.lon);
            if (intersect) inside = !inside;
            j = i;
        }
        return inside;
    }
    return false;
}

function nearestSite(sites, lat, lon) {
    var best = null, bestKm = Infinity;
    if (!validPair(lat, lon) || !sites) return null;
    for (var s of sites) {
        var km = distanceKm(lat, lon, s.lat, s.lon);
        if (km < bestKm) { bestKm = km; best = s; }
    }
    return best;
}

function envView(text) {
    if (!text) return null;
    var parts = String(text).split(",");
    if (parts.length !== 3) return null;
    var lat = Number(parts[0]), lon = Number(parts[1]), span = Number(parts[2]);
    return validPair(lat, lon) && isFinite(span) && span > 0 ? { lat: lat, lon: lon, span: span } : null;
}

// Explicit centre from config.toml: both keys, both in range, or null.
function configCenter(values) {
    if (!values || values.center_lat === undefined && values.center_lon === undefined) return null;
    if (values.center_lat === undefined || values.center_lon === undefined) return null;
    return validPair(values.center_lat, values.center_lon)
        ? { lat: values.center_lat, lon: values.center_lon } : null;
}

function polarLock(siteId) {
    var id = String(siteId || "").trim().toUpperCase();
    return id ? { sourceId: "nexrad", target: { kind: "site", siteId: id } } : null;
}
function mosaicLock(sourceId) {
    var id = String(sourceId || "").trim();
    return id ? { sourceId: id, target: { kind: "mosaic" } } : null;
}
// Remembered lock: object form, with one-release migration of a NEXRAD site string.
function parseLock(value) {
    if (typeof value === "string") return polarLock(value);
    if (!value || typeof value !== "object") return null;
    if (typeof value.sourceId !== "string" || !value.target || typeof value.target !== "object") return null;
    if (value.target.kind === "site") return polarLock(value.target.siteId);
    if (value.target.kind === "mosaic") return mosaicLock(value.sourceId);
    return null;
}
function lockEquals(a, b) {
    a = parseLock(a); b = parseLock(b);
    if (!a && !b) return true;
    if (!a || !b) return false;
    if (a.sourceId !== b.sourceId || a.target.kind !== b.target.kind) return false;
    if (a.target.kind === "site") return a.target.siteId === b.target.siteId;
    return true;
}
function lockSiteId(lock) {
    var l = parseLock(lock);
    return l && l.target.kind === "site" ? l.target.siteId : "";
}
function lockKey(lock) {
    var l = parseLock(lock);
    if (!l) return "";
    return l.target.kind === "site" ? l.target.siteId : l.sourceId + ":mosaic";
}

function configLock(values) {
    if (!values || typeof values.locked_radar !== "string") return "";
    return values.locked_radar.trim().toUpperCase();
}

function configErrors(values) {
    var errors = [];
    if (!values) return errors;
    var hasLat = values.center_lat !== undefined, hasLon = values.center_lon !== undefined;
    if (hasLat !== hasLon)
        errors.push("center_lat and center_lon must both be set");
    else if (hasLat && !validPair(values.center_lat, values.center_lon))
        errors.push("center_lat/center_lon must be latitudes in [-90, 90] and longitudes in [-180, 180]");
    if (values.locked_radar !== undefined) {
        if (typeof values.locked_radar !== "string")
            errors.push("locked_radar must be a quoted station id");
        else if (!values.locked_radar.trim())
            errors.push("locked_radar must be a quoted station id");
    }
    if (values.home_site !== undefined)
        errors.push("home_site is unused; location is a place (center_lat/center_lon or search)");
    if (values.follow !== undefined)
        errors.push("follow is unused; the map follows the nearest radar unless locked");
    return errors;
}

// Remembered view from state.json. Invalid fields are dropped, not fatal.
function parseState(raw) {
    var empty = { lat: undefined, lon: undefined, span: undefined, lock: null, name: "" };
    if (raw === undefined || raw === null || raw === "") return empty;
    try {
        var json = typeof raw === "string" ? JSON.parse(raw) : raw;
        if (!json || typeof json !== "object") return empty;
        var lat = json.lat, lon = json.lon, span = json.span;
        return {
            lat: validLat(lat) ? lat : undefined,
            lon: validLon(lon) ? lon : undefined,
            span: typeof span === "number" && isFinite(span) && span > 0 ? span : undefined,
            lock: parseLock(json.lock),
            name: typeof json.name === "string" ? json.name : ""
        };
    } catch (e) { return empty; }
}

function stateObject(viewLat, viewLon, span, lock, name) {
    var o = {};
    if (validPair(viewLat, viewLon)) { o.lat = viewLat; o.lon = viewLon; }
    if (typeof span === "number" && isFinite(span) && span > 0) o.span = span;
    var parsed = parseLock(lock);
    if (parsed) o.lock = parsed;
    if (name) o.name = name;
    return o;
}

// Map centre at launch: explicit config, remembered view, Omarchy weather,
// else nothing (the location picker). Captures may pass env as the first
// argument to outrank the rest for that process.
function resolvePlace(explicit, remembered, weather, env) {
    if (env && validPair(env.lat, env.lon))
        return { lat: env.lat, lon: env.lon, name: "", source: "view", span: env.span };
    if (explicit && validPair(explicit.lat, explicit.lon))
        return { lat: explicit.lat, lon: explicit.lon, name: "", source: "config", span: remembered && remembered.span };
    if (remembered && validPair(remembered.lat, remembered.lon))
        return { lat: remembered.lat, lon: remembered.lon, name: remembered.name || "", source: "state", span: remembered.span };
    if (weather && validPair(weather.lat, weather.lon))
        return { lat: weather.lat, lon: weather.lon, name: weather.name || "", source: "weather", span: remembered && remembered.span };
    return null;
}

// RESET target: configured centre, else weather, else none (keep the camera,
// restore the default span).
function resolveReset(explicit, weather) {
    if (explicit && validPair(explicit.lat, explicit.lon))
        return { lat: explicit.lat, lon: explicit.lon, name: "", source: "config" };
    if (weather && validPair(weather.lat, weather.lon))
        return { lat: weather.lat, lon: weather.lon, name: weather.name || "", source: "weather" };
    return null;
}

// wttr.in `?format=j2` nearest_area (DESIGN.md, approximate IP location).
// j2 stays under a small body size; Omarchy's weather `j1` is much larger.
function parseWttrCoord(value) {
    // Number(null) and Number("") are 0 — reject those before coercing.
    if (typeof value === "number") return value;
    if (typeof value === "string" && value.trim()) return Number(value.trim());
    return NaN;
}

function parseWttrHome(raw) {
    try {
        var json = typeof raw === "string" ? JSON.parse(raw) : raw;
        var areas = json && json.nearest_area;
        if (!areas || !areas.length) return null;
        var area = areas[0];
        var lat = parseWttrCoord(area.latitude), lon = parseWttrCoord(area.longitude);
        if (!validPair(lat, lon)) return null;
        var name = "";
        var labels = [].concat(area.areaName || [], area.region || []);
        for (var i = 0; i < labels.length; i++) {
            var value = labels[i] && labels[i].value;
            if (typeof value === "string" && value.trim()) {
                name = value.trim().replace(/[\x00-\x1f\x7f]/g, "").slice(0, 100);
                break;
            }
        }
        return { name: name, lat: lat, lon: lon };
    } catch (e) { return null; }
}

// `/` search (DESIGN.md): three or four letters is a site id or prefix.
function looksLikeSiteId(query) {
    return /^[A-Za-z]{3,4}$/.test(String(query).trim());
}

// Decimal degrees, latitude then longitude, comma or space (Google Maps).
// null means the text is not a coordinate pair; otherwise parseCoordFields.
function parseCoordQuery(text) {
    var t = String(text).trim();
    if (!t) return { empty: true };
    var m = t.match(/^([+-]?\d+(?:\.\d+)?)\s*[, ]\s*([+-]?\d+(?:\.\d+)?)$/);
    if (!m) return null;
    return parseCoordFields(m[1], m[2]);
}

function coordRow(lat, lon) {
    return {
        kind: "place",
        name: lat.toFixed(4) + ", " + lon.toFixed(4),
        where: "coordinates",
        label: lat.toFixed(4) + ", " + lon.toFixed(4),
        lat: lat,
        lon: lon
    };
}

// Mix site rows, place rows, and an optional coordinate row. Empty query
// with browseSites lists the nearest dishes; otherwise type to search.
// Places first unless the query looks like a site id.
function mergeSearch(siteRows, placeRows, coord, query, browseSites, limit) {
    var cap = typeof limit === "number" && limit > 0 ? limit : 4;
    var sites = siteRows || [], places = placeRows || [];
    var q = String(query || "").trim();
    var extra = coord && coord.lat !== undefined ? [coordRow(coord.lat, coord.lon)] : [];
    if (coord && coord.error) return extra.slice(0, cap);
    if (!q) return browseSites ? sites.slice(0, cap) : extra.slice(0, cap);
    var combined = looksLikeSiteId(q) ? sites.concat(extra, places) : extra.concat(places, sites);
    return combined.slice(0, cap);
}
