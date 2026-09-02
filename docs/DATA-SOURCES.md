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

## A — verified, ready to build

### Storm overflows / sewage discharge (UK water companies)
One keyless ArcGIS FeatureServer per company. United Utilities:
`https://services5.arcgis.com/5eoLvR0f8HKb7HWP/arcgis/rest/services/United_Utilities_Storm_Overflow_Activity/FeatureServer/0`
— and equivalents for Thames (`services2.arcgis.com/g6o32ZDQ33GpCIu3`), Severn
Trent (`services1.arcgis.com/NO7lTIlnxRMMG9Gw`), Anglian
(`services3.arcgis.com/VCOY1atHWVcDlvlJ`), Yorkshire
(`services-eu1.arcgis.com/1WqkK5cDKUbF0CkH`), Northumbrian
(`services-eu1.arcgis.com/MSNNjkZ51iVh8yBj`), South West
(`services-eu1.arcgis.com/OMdMOtfhATJPcHe3`), Wessex
(`services.arcgis.com/3SZ6e0uCvPROr4mS`), Scottish Water
(`services3.arcgis.com/Bb8lfThdhugyc4G3`).

`?where=1=1&outFields=*&f=geojson` verified. UU 2,252 outfalls, Thames 573;
`where=Status=1` gave 5 UU outfalls discharging at the time of checking, with
`LastUpdated` fifteen minutes old. Kind: `station` per outfall plus `event` per
discharge. ~15-20k outfalls, poll 15 min. **Licence caveat**: the Stream portal
asserts open terms but the FeatureServer's own `copyrightText` is empty —
confirm before publishing anything derived.

### Environment Agency real-time flood monitoring
`https://environment.data.gov.uk/flood-monitoring/id/floods`, `/id/stations`,
`/data/readings?latest`. The response envelope self-declares OGL v3. Flood
warnings carry real polygons; ~5,000 river, tide and rainfall gauges. Kinds:
`event`, `station`, `measure`. England. Companion: the Hydrology API at
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
2. Aviation weather — SIGMET polygons and METAR land straight into existing kinds.
3. Storm overflows — nine keyless GeoJSON fetches, high novelty per line.
4. NDBC buoys — one fixed-width file.
5. EA flood monitoring — one driver yields three kinds.
6. Register for BODS, then build it.

## Process notes

- Send `--compressed`. SondeHub's telemetry endpoint returns gzip and looks like
  corrupt binary without it.
- Assert on record counts, never on status codes. Three separate services here
  returned 200 with unusable bodies.
