# Candidate data sources

Researched 2026-09-02. Every entry below was checked with a live request from
this machine, and the *content* was inspected — not just the status code. That
distinction is the whole discipline here: on the same day this was compiled, a
tile provider returned HTTP 200 with a valid PNG that had "API KEY REQUIRED"
stamped across the image, Cefas returned HTTP 200 with `text/html` and an
Angular shell for three different "API" paths, and PSKReporter returned HTTP 200
with a well-formed but completely empty envelope. **Record counts are the only
assertion that catches those.**

Constraints applied: personal, self-hosted, non-commercial. Keyless preferred, a
free self-serve account acceptable, anything requiring approval, payment or
**academic affiliation rejected** — see `REACH.md` for why OpenSky's registered
tier is out of reach.

Kinds refer to `argus_core::EntityKind`.

---

## Built

- **SondeHub radiosondes** — `sondehub.rs`, layer `radiosondes`. See below.
- **Aviation hazards (SIGMET)** — `sigmet.rs`, layer `sigmets`.
- **Storm overflows** — `stormoverflow.rs`, layer `storm-overflows`. See below.
- **EA flood warnings + river gauges** — `eaflood.rs`, layers `flood-warnings`
  and `river-gauges`. See below.

## A — verified, ready to build

### Storm overflows / sewage discharge (UK water companies) — **built**
`stormoverflow.rs`. Nine keyless ArcGIS FeatureServers, one per company,
**15,273 outfalls** and 122 discharging on the live pull that verified it.

The service names are not derivable from the company name — South West Water
publishes theirs as `NEH_outlets_PROD` and Anglian as
`stream_service_outfall_locations_view`. The reliable way to find all nine is a
title search against ArcGIS Online rather than browsing each org's service list:

```sh
curl -sG https://www.arcgis.com/sharing/rest/search \
  --data-urlencode 'q=title:"Storm Overflow Activity"' --data-urlencode f=json
```

Eight companies publish the Water UK common model; **Scottish Water publishes
its own schema entirely** — `ASSET_ID`, `STATUS_ID` (13 overflowing, 14 recent,
15 none, 16 no data), ISO-8601 dates in *string* fields, plus asset names,
licence numbers, overflow types and durations the others do not carry.

What the live pull taught, none of which is visible from a single record:

- **`f=geojson`, never `f=json`.** Scottish Water's layer is natively
  EPSG:27700; `f=json` returns British National Grid eastings and northings
  that deserialise perfectly and place every Scottish outfall in the Gulf of
  Guinea. GeoJSON output makes the server reproject to WGS84.
- **Page it.** The cap is 2,000 features, and 1,000 on Anglian's server. In
  GeoJSON the `exceededTransferLimit` warning sits inside a `properties` object
  that is *absent* on the last page, so the reliable stop is a short page. Order
  by the object-id field or `resultOffset` silently skips and repeats rows —
  and that field is `OBJECTID` on seven companies, `ObjectId` on the other two.
- **South West Water uses lowerCamelCase** (`status`, `statusStart`,
  `lastUpdated`) for the same common model everyone else spells in PascalCase.
  Serde aliases absorb it; without them, 1,344 outfalls decode to nothing and
  Devon looks like it has no sewers.
- **`Status = -1` means the monitor is offline**, not that the outfall is clear.
  460 outfalls nationally were in that state. Folding it into "not discharging"
  would report a river as clean because nobody is watching it.
- **The nine companies do not mean the same thing by `LastUpdated`**, and this
  is the trap that cost the most. Seven stamp it in bulk when the feed is
  republished — 2,251 of UU's 2,252 records carry one identical value, an hour
  old — which makes it look like a free `observed_at`: the store's
  `(kind, key, observed_at, source)` conflict key would then turn every poll
  between republishes into no writes at all. **Northumbrian Water stamps each
  record when that record last changed**: median eight days, oldest fifty-seven.
  Dated by that field, 1,301 of its 1,575 outfalls fall outside the 24-hour
  `Station` horizon and the whole North East silently leaves the map — with
  every monitor working perfectly. `StatusStart` fails the same way and harder;
  its values go back to 2024.

  The driver therefore dates a station by **when it fetched the feed**, the one
  instant all nine agree on, and carries the company's stamp as an attribute.
  Where that stamp is older than the horizon — or the monitor reports offline —
  the observation is marked `Quality::Stale`: still drawn, because a monitor
  that stopped reporting is worth seeing, but never dressed as a live reading.
  1,808 of 15,273 outfalls nationally, most of them Northumbrian's.

  The cost is 15,300 rows a poll with no dedupe, four polls an hour. Against the
  ADS-B layer's ~10,000 rows every fifteen seconds that is under three per cent
  of what the store already absorbs — a fair price for not letting one field's
  spelling decide whether a county exists.
- **Scottish Water's empty `END_DATETIME` does not mean "still running".** 603
  of 2,073 assets carry one while only 36 were overflowing. The status is the
  only field that knows.
- **Anglian publishes AWS00528 twice**, identical but for the object id, so the
  natural key is the outfall id and never `OBJECTID`.

**Licence caveat**: the Stream portal asserts open terms but not one of the nine
FeatureServers carries a non-empty `copyrightText`. The driver therefore credits
each company by name and states the terms are the company's own rather than
asserting an open licence the endpoint does not — confirm before publishing
anything derived.

### Environment Agency real-time flood monitoring — **built**
`eaflood.rs`. Two sources from one API: `flood-warnings` (event) and
`river-gauges` (station). **5,525 stations, 4,481 of them reporting a position
and a current reading**, in two requests per poll.

Built as two sources, not the three kinds the research note guessed. A gauge's
readings are not `measure` entities — that kind is for a scalar not tied to a
discrete object, and `EntityKind::Station` names "a river gauge" explicitly — so
the level, flow and rainfall instruments ride as attributes on the station that
houses them. Warnings and gauges are split because they want different cadences
(5 min against 15) and are different layer toggles.

What the live pull taught:

- **The scalar-or-array trap, which is the big one.** This is JSON-LD flattened
  to JSON and the flattening does not force cardinality: a field is a bare
  scalar with one value and an *array* with two. `lat`/`long` are floats on
  4,894 stations and an array on **one** (E85123, two positions 100 m apart).
  `status`, `RLOIid`, `catchmentName`, `dateOpened` and `label` each do it on
  one or two records; one reading of 5,347 has an array `value`. Declared as
  `f64`, that single station fails — and since items arrive as one array, serde
  fails the **whole document**: every river gauge in England lost to one station
  that cannot make its mind up. Every varying field needs a `OneOrMany`. Same
  lesson as the SIGMET null vertex: tolerance belongs at the smallest element.
- **The gauges are not England only.** The Agency also publishes the National
  Tide Gauge Network, which rings the whole UK — Aberdeen, Leith, Wick,
  Ullapool, Tobermory, Portrush — 21 stations outside England, reaching Lerwick
  at 60.15N. A live test asserting an England bounding box is what caught it.
  Warnings genuinely are England only, so the two sources declare different
  coverage.
- **`/id/floodAreas` silently truncates to 500** of its 4,208 rows on a request
  that looks identical to the one `/id/stations` answers in full at 5,525. Name
  `_limit` explicitly on every list endpoint rather than learning which have a
  default.
- **Flood warning timestamps carry no timezone** (`2015-02-02T19:32:00`) where
  readings do (`...Z`). The API documents them as UTC; read as local they would
  be an hour out all summer.
- **630 stations publish no position at all** and 47 readings were over 24 h
  old, the oldest 29 days. Gauges with no position are skipped; stale ones are
  left to the station horizon, which is the correct answer here — unlike the
  storm overflow feeds, a reading's timestamp is unambiguously when the water
  was measured.
- **There were no flood warnings in force in England** when this was built, and
  none at `min-severity=4` either, so the warning decoder is built against the
  Agency's own published example. The live test asserts the shape of whatever
  is in force rather than demanding warnings exist.
- Flood area outlines are separate fetches (`/id/floodAreas/{code}/polygon`,
  GeoJSON `FeatureCollection`). They are static, so they go through the same
  `GeometryCache` the NWS driver uses for zone outlines, with a per-poll fetch
  budget.

Still unbuilt companion: the Hydrology API at
`/hydrology/id/stations?observedProperty=groundwaterLevel` adds groundwater and
offers native `.geojson`.

### Open-Meteo (air quality, pollen, marine, flood)
`air-quality-api.open-meteo.com/v1/air-quality`, `marine-api…/v1/marine`,
`flood-api…/v1/flood`. Keyless, CC BY 4.0, explicitly non-commercial. Covers
pollen, UV, wave height and GloFAS river discharge — four domains this plan
misses, from one provider. Pollen is CAMS Europe only. Kind: `measure`.

### wspr.live — HF propagation
`https://db1.wspr.live/?query=…` — ClickHouse over HTTP, keyless. 30,263 spots
in a ten-minute window; each row has both transmitter and receiver coordinates,
so it draws as a great-circle path. **~4.4M rows/day — must be filtered or
aggregated server-side**, which the full ClickHouse dialect makes easy. Kind:
`event`.

### NOAA Aviation Weather Center
`https://aviationweather.gov/api/data/metar?bbox=49,-11,61,2&format=json` (60 UK
stations, including RAF aerodromes), `/taf`, and `/isigmet` (146 active
international SIGMETs, each with a polygon). Keyless, US public domain. Kinds:
`station`+`measure`, and `event` for the hazard polygons.

### Elexon Insights + National Grid Carbon Intensity
`https://data.elexon.co.uk/bmrs/api/v1/datasets/FUELINST` gives 5-minute GB fuel
mix; `https://api.carbonintensity.org.uk/regional` gives 14 DNO regions with
live generation mix. Both keyless, no registration at all. The UK counterpart to
the planned ENTSO-E/EIA-930, at finer resolution. **Unresolved**: per-BM-unit
output (B1610) returned an empty `data` array and its swagger is not at any
standard path — plant-level generation needs more archaeology.

### Others verified keyless and ready
- **data.police.uk** — street-level crime, OGL v3. Monthly with ~2-month lag and
  locations snapped to anonymised "on or near" points; both facts must be
  surfaced in the UI or it reads as precise when it is not. Kind `event`.
- **NOAA NDBC** — `data/latest_obs/latest_obs.txt`, 884 buoys, fixed-width.
  Wave height, period, SST, pressure. Kind `station`+`measure`.
- **Argo floats** — Ifremer ERDDAP `tabledap/ArgoFloats.json`. ~4,000 floats,
  one profile per ~10 days. **Percent-encode `>` `<` `,` or Tomcat 400s.**
- **SatNOGS** — 4,453 amateur ground stations plus observations joinable to the
  existing satellite layer by `norad_cat_id`. CC BY-SA 4.0.
- **Global Meteor Network** — daily trajectory files, ~1,400 meteors/day, each
  with begin/end lat/lon/height, i.e. a real 3D `event` LineString. CC BY 4.0.
- **TfL Unified API** — keyless: line status, road disruptions (107 live), and
  bus arrivals carrying vehicle registrations, which is a de-facto vehicle track
  if polled by `vehicleId`. Greater London.
- **EMODnet Human Activities WFS** — offshore platforms and wind farms with
  operator, status and capacity. Kind `feature`, European.
- **FDSN station metadata** — Raspberry Shake (1,566 UK station-epochs) and
  EarthScope. **`service.iris.edu` 307-redirects to `service.earthscope.org`;
  follow redirects or you silently get nothing.** ORFEUS returns 204/404 for the
  UK — do not use it.
- **AERONET** — 1,674 aerosol sites. A 2026 query for one site returned only a
  45-byte banner and no rows, so probe per-site before assuming currency.
- **NASA JPL SSD/CNEOS** — fireballs carry lat/lon/altitude and are the only
  spatial one; close approaches and Sentry risk are non-spatial side-panel data.
- **PlanIt** — UK planning applications with point geometry and a state machine.
  Date-bounded queries are the reliable form; bbox+recent timed out at 45s.
- **UK retail fuel prices** — per-retailer JSON under the CMA scheme. Asda 790
  sites updated today; **Applegreen's file was 18 months stale**, so check
  `last_updated` per feed and mark stale ones.
- **GBIF** — 1.37M GB occurrence records for 2026. Roughly half are CC BY-NC,
  which suits this project but must be filtered if that ever changes.
- **Others**: PSKReporter (filter by band/mode, *not* callsign — callsign
  queries return an empty envelope), FSA food hygiene, Helioviewer (solar
  imagery, `raster`), OurAirports (86,021 rows, public domain reference table),
  Safecast (radiation, but London samples were over a year old), AuroraWatch UK,
  NOAA CO-OPS tides (US only), OSM notes.

## B — promising, unverified

- **UK Bus Open Data Service** — 401 without a key; registration is free and
  self-serve. Live positions for every bus in England. **The biggest remaining
  gap in the plan**, and one signup from being verifiable.
- **openAIP** — airspace structure as `feature` polygons; free account, 403 seen.
- **National Highways DATEX II** — 401 "Invalid Subscription Key"; Azure APIM
  free tier, likely self-serve. The legacy unauthenticated endpoints
  (`trafficengland.com`, `m.highwaysengland.co.uk`) are **dead**.
- **ONS Open Geography** — 3,904 keyless ArcGIS services (wards, constituencies,
  LSOAs). The natural join surface for crime and planning data.
- **Met Office DataHub** — free tier exists, but Open-Meteo covers it keyless.

## C — checked and rejected

Knowing a source is a dead end saves repeating the investigation.

| Source | Verdict |
|---|---|
| AviationAPI | **DNS does not resolve** — the OSINT index entry has rotted |
| UK National Chargepoint Registry | **DNS returns no answer** — retired or moved |
| Open Charge Map | 403, needs a key; static POIs with no live availability |
| Global Fishing Watch | 401; tokens are request-and-approve, not self-serve |
| APRS-IS direct | `# Login by user not allowed` — needs a real callsign. The planned SDR route sidesteps this |
| Thames Water developer API | 504 three times, and needs client credentials — the ArcGIS route above gives Thames keyless and working |
| Cefas WaveNet | Angular SPA; three "API" paths all returned the same 3,855-byte HTML shell with HTTP 200 |
| EURDEP / JRC | 404; the real network is restricted to national authorities |
| UK Street Manager | **DNS does not resolve**; permits are S3 bulk exports, not an API |
| NSTA oil & gas | ArcGIS root 404s; EMODnet covers platforms anyway |
| EAWS avalanche, Copernicus EMS, Aloft bird radar | 404 — paths moved, not relocated |
| SILSO sunspots | Works, but a single global scalar with no geometry |
| wheretheiss.at | Works, but redundant — the CelesTrak/SGP4 pipeline already does 25544 better, offline |
| ip-api, Mylnikov WiFi, and the OSINT index's geospatial section | Lookup services, not feeds: no enumerable population, no time dimension, and IP-derived coordinates are fiction at a city centroid |

The OSINT index yielded essentially nothing usable: almost every geospatial
entry is a keyed geocoding, IP-lookup or AI-inference service. A map needs feeds
with a position and a clock, and that is a different kind of thing.

## Suggested order

Easiest first, which is also roughly most-reusable first:

1. ~~SondeHub~~ — **done**; reused the aircraft track machinery verbatim.
2. ~~Aviation weather~~ — **SIGMETs done**; METAR still unbuilt.
3. ~~Storm overflows~~ — **done**; nine feeds, two schemas, ~600 lines.
4. NDBC buoys — one fixed-width file.
5. ~~EA flood monitoring~~ — **done**; two sources, not the three kinds guessed.
6. Register for BODS, then build it.

## Process notes

- Send `--compressed`. SondeHub's telemetry endpoint returns gzip and looks like
  corrupt binary without it.
- Assert on record counts, never on status codes. Three separate services here
  returned 200 with unusable bodies.
- A paged API needs that assertion as a *test*, not just a spot check. A lost
  page is a valid, decodable, silently short answer — it looks like a county
  with no sewers, not like an error. `storm_overflow_live.rs` polls all nine
  companies behind `ARGUS_NETWORK_TESTS=1` and fails on a feed that has quietly
  become half a feed.
- Check a field's *cardinality* across the whole layer, not its type in one
  record. The EA API's scalar-or-array flattening shows up on one station in
  five thousand and fails the entire document.
- Check a timestamp field's *distribution*, not one record. Every trap in the
  storm overflow feeds — the bulk stamp, Northumbrian's per-record stamp, the
  603 empty end-dates against 36 live spills — was invisible in a sample and
  obvious in a `groupByFieldsForStatistics` count over the whole layer.
