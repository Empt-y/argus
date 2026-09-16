# Data sources

A log of every feed that has been checked for Argus: what it actually returns,
what it cost to build, and what went wrong. Research started 2026-09-02; each
entry was checked with a real request from this machine and the response body
was inspected, not just the status code. That matters more than it sounds. On
one day of research a tile provider returned HTTP 200 with a PNG that had "API
KEY REQUIRED" drawn across it, Cefas returned 200 with an Angular shell for
three different "API" paths, and PSKReporter returned 200 with a well-formed
but empty envelope. Record counts are the only check that catches those.

Constraints: personal, self-hosted, non-commercial. Keyless is preferred, a
free self-serve account is fine, and anything that needs approval, payment or
an academic affiliation is out (see `REACH.md` for the OpenSky case).

Entity kinds refer to `argus_core::EntityKind`.

## Built

| Layer | Driver | What it is |
|---|---|---|
| `flights` | `readsb.rs`, `opensky.rs` | ADS-B via adsb.lol → adsb.fi → OpenSky |
| `satellites` | `celestrak.rs`, `spacetrack.rs` | TLEs propagated with SGP4 |
| `earthquakes` | `usgs.rs`, `emsc.rs` | USGS → EMSC |
| `weather-alerts` | `nws.rs` | US NWS alerts with zone polygons |
| `sigmets` | `sigmet.rs` | Aviation hazard polygons, NOAA AWC |
| `radiosondes` | `sondehub.rs` | Weather balloons, SondeHub |
| `storm-overflows` | `stormoverflow.rs` | UK sewage discharge, nine water companies |
| `flood-warnings` | `eaflood.rs` | Environment Agency flood warnings |
| `river-gauges` | `eaflood.rs` | EA river, tide and rainfall gauges |
| `buoys` | `ndbc.rs` | NOAA NDBC buoys and coastal stations |

Notes on the ones that had something to teach follow.

### Storm overflows (UK water companies)

Nine keyless ArcGIS FeatureServers, one per company. 15,273 outfalls, 122
discharging on the pull that verified it.

The service names aren't derivable from the company name (South West Water's
is `NEH_outlets_PROD`, Anglian's is `stream_service_outfall_locations_view`).
The reliable way to find all nine is a title search on ArcGIS Online:

```sh
curl -sG https://www.arcgis.com/sharing/rest/search \
  --data-urlencode 'q=title:"Storm Overflow Activity"' --data-urlencode f=json
```

Eight companies use the Water UK common model. Scottish Water has its own
schema: `ASSET_ID`, `STATUS_ID` (13 overflowing, 14 recent, 15 none, 16 no
data), ISO dates in string fields, plus asset names, licence numbers and
durations the others don't publish.

Things that only showed up on the full pull:

- Use `f=geojson`, not `f=json`. Scottish Water's layer is natively
  EPSG:27700, and `f=json` returns British National Grid coordinates that
  deserialise fine and put every Scottish outfall in the Gulf of Guinea.
- Page it. The cap is 2,000 features (1,000 on Anglian). In GeoJSON the
  `exceededTransferLimit` flag is inside a `properties` object that is absent
  on the last page, so the reliable stop condition is a short page. Order by
  the object id or `resultOffset` skips and repeats rows — and the id field is
  `OBJECTID` on seven servers and `ObjectId` on two.
- South West Water uses lowerCamelCase (`status`, `statusStart`) for the same
  model everyone else spells in PascalCase. Without serde aliases, 1,344
  outfalls decode to nothing and Devon appears to have no sewers.
- `Status = -1` means the monitor is offline, not that the outfall is clear.
  460 outfalls were in that state.
- The companies don't agree on what `LastUpdated` means. Seven stamp every
  record when the feed is republished (2,251 of United Utilities' 2,252 records
  carry one identical value). Northumbrian stamps each record when *that
  record* changed — median eight days old, oldest 57. Using it as
  `observed_at` would put 1,301 of Northumbrian's 1,575 outfalls outside the
  24-hour station horizon and take the North East off the map with every
  monitor working. `StatusStart` is worse; its values go back to 2024.

  So the driver dates a station by when it fetched the feed and carries the
  company's stamp as an attribute. Where that stamp is older than the horizon,
  or the monitor is offline, the observation is `Quality::Stale`: still drawn,
  but not presented as a live reading. That's 1,808 of 15,273, mostly
  Northumbrian's. The cost is 15,300 rows per poll with no dedupe, four polls
  an hour — under 3% of what the ADS-B layer already writes.
- Scottish Water's empty `END_DATETIME` doesn't mean "still running": 603 of
  2,073 assets have one, 36 were overflowing. Only the status field knows.
- Anglian publishes AWS00528 twice, differing only in object id. The natural
  key is the outfall id.

Licence: the Stream portal says open terms, but none of the nine servers has a
non-empty `copyrightText`. The driver credits each company by name and states
the terms are the company's own. Check before publishing anything derived.

### Environment Agency flood monitoring

Two sources from one API: `flood-warnings` (event) and `river-gauges`
(station). 5,525 stations, 4,481 with both a position and a current reading,
in two requests per poll.

Two sources rather than the three kinds originally guessed. A gauge's readings
are not `measure` entities — that kind is for a scalar not tied to a discrete
object — so level, flow and rainfall are attributes on the station. Warnings
and gauges are separate sources because they want different cadences (5 min vs
15) and are different things for a client to switch on.

- The big one: the API is JSON-LD flattened to JSON, and the flattening
  doesn't fix cardinality. A field is a bare scalar when there's one value and
  an array when there are two. `lat`/`long` are floats on 4,894 stations and an
  array on one (E85123, two positions 100 m apart). `status`, `RLOIid`,
  `catchmentName`, `dateOpened` and `label` each do it on one or two records;
  one reading in 5,347 has an array `value`. Declared as `f64`, that one
  station fails to deserialise and takes the whole document with it. Every
  field that can vary is a `OneOrMany`.
- The gauges are not England only. The Agency also publishes the National Tide
  Gauge Network, which covers the whole UK — 21 stations outside England,
  reaching Lerwick at 60.15N. A live test asserting an England bbox caught
  this. Warnings really are England only, so the two sources declare different
  coverage.
- `/id/floodAreas` silently truncates to 500 of its 4,208 rows on a request
  that looks identical to the one `/id/stations` answers in full. Pass `_limit`
  explicitly on every list endpoint.
- Flood warning timestamps have no timezone (`2015-02-02T19:32:00`); readings
  do. The docs say UTC. Read as local they'd be an hour out all summer.
- 630 stations have no position and are skipped. 47 readings were over a day
  old (oldest 29 days); those are left to the station horizon, which is right
  here because a reading's timestamp really is when the water was measured.
- No flood warnings were in force when this was built, so the warning decoder
  is written against the Agency's published example. The live test checks the
  shape of whatever is in force rather than requiring warnings to exist.
- Flood area outlines are separate static fetches and go through the same
  `GeometryCache` as NWS zones, with a per-poll budget.
- The flood API is slow. `/id/floods` returned 290 bytes in 12 s and then 45 s;
  `readings?latest` took 35 s and 64 s. That's server latency, not payload.
  The daemon's shared HTTP client has a 30 s timeout, so these two sources get
  their own 120 s client. The live test had passed with its own 120 s client
  and then every poll in the daemon failed — see process notes.

Not yet built: the Hydrology API at
`/hydrology/id/stations?observedProperty=groundwaterLevel` adds groundwater and
serves native GeoJSON.

### NOAA NDBC buoys

`data/latest_obs/latest_obs.txt` is the whole network in one 22 KB request,
plus `data/stations/station_table.txt` for names and types (refreshed daily).
Keyless, US public domain. 876 stations on 2026-09-15.

- Not fixed-width, despite how the header looks. Latitude has three decimals
  on 868 rows and two on 8, a temperature of exactly 30 °C prints as `30`, and
  ids run from four characters (`CWCI`) to seven (`4403587`). Every row has
  exactly 22 whitespace-separated fields; that count is the contract, and a row
  with any other count is counted as malformed rather than guessed at.
- Not only buoys: C-MAN coastal stations, NOS water level stations, NERRS
  estuary sites, 63 Gulf oil platforms under `K` call signs, Canadian and
  Korean partner stations, 45 drifting buoys, and a mooring at 22°S. Extent is
  71°N to 22°S across the antimeridian, so `Coverage::Global`.
- `MM` (missing) is the majority value in 11 of 14 measurement columns. 840 of
  876 stations report no tide, 832 no visibility. A station reporting one thing
  is kept; one reporting nothing is skipped.
- The station table lowercases some ids (`katp`, `0y2w3`); the observation
  file uppercases all of them. Case-folded, all 876 join. 148 table rows have
  no name and fall back to the id.
- 341 of the 596 station notes are HTML fragments (`<a href>`, `<br>`, `<p>`).
  This didn't show in the sample and did show in the stored row. They're
  flattened to text, and the live test now rejects markup in any text
  attribute.
- Timestamps are UTC and hourly, mostly at :00 or :48–:50. The file is the
  latest reading per station; 861 of 876 were within three hours, the oldest
  was 3.6 h. The live test asserts that distribution, because a file that
  stops being rebuilt still decodes perfectly.

Units are kept as published and named in the attribute keys (`wind_speed_ms`,
`pressure_hpa`, `visibility_nmi`, `tide_ft`). Converting a one-decimal reading
in feet to metres would print precision the sensor doesn't have.

## Verified, ready to build

### Open-Meteo (air quality, pollen, marine, flood)
`air-quality-api.open-meteo.com/v1/air-quality`, `marine-api…/v1/marine`,
`flood-api…/v1/flood`. Keyless, CC BY 4.0, non-commercial. Pollen, UV, wave
height and GloFAS river discharge from one provider. Pollen is CAMS Europe
only. Kind `measure`.

### wspr.live (HF propagation)
`https://db1.wspr.live/?query=…` — ClickHouse over HTTP, keyless. 30,263 spots
in a ten-minute window, each with transmitter and receiver coordinates, so it
draws as a great-circle path. About 4.4M rows a day, so it has to be filtered
or aggregated server-side, which the ClickHouse dialect makes easy. Kind
`event`.

### NOAA Aviation Weather Center (METAR/TAF) — done
Built from the bulk cache, not the query API. `api/data/metar?bbox=` thins
by bounding-box area — 62 for the UK box, 100 for all of Europe, 158 for the
world — and there is no documented way to turn that off (`help=true` names
`zoom` and `density` parameters the spec does not). The cache files are the
whole network in one request each:

- `data/cache/metars.cache.csv.gz` — 5,128 aerodromes, 250 KB, rebuilt every
  minute. 44 columns read by position because four are called `sky_cover`;
  the driver checks the header verbatim and fails the poll if it moves.
- `data/cache/tafs.cache.xml.gz` — 2,971 forecasts, 327 KB; the `.csv.gz`
  the naming pattern suggests is a 404. Attached to the aerodrome's station
  as raw text plus validity, refreshed every 30 minutes.
- `data/cache/stations.cache.json.gz` — 9,875 sites, names and countries,
  refreshed daily.

Served as `application/octet-stream` with no `Content-Encoding`, so the HTTP
layer does not inflate them; the driver does. Keyless, public domain. Kind
`station`, layer `metars`, global.

### Elexon Insights and National Grid Carbon Intensity
`https://data.elexon.co.uk/bmrs/api/v1/datasets/FUELINST` for 5-minute GB fuel
mix; `https://api.carbonintensity.org.uk/regional` for 14 DNO regions with
live generation mix. Both keyless. Unresolved: per-BM-unit output (B1610)
returned an empty `data` array and its swagger isn't at any standard path.

### Others, verified keyless
- **data.police.uk** — street-level crime, OGL v3. Monthly, ~2 month lag,
  locations snapped to anonymised points; the UI needs to say both or it reads
  as precise. Kind `event`.
- **Argo floats** — Ifremer ERDDAP `tabledap/ArgoFloats.json`, ~4,000 floats,
  one profile per ~10 days. Percent-encode `>` `<` `,` or Tomcat returns 400.
- **SatNOGS** — 4,453 amateur ground stations plus observations, joinable to
  the satellite layer by `norad_cat_id`. CC BY-SA 4.0.
- **Global Meteor Network** — daily trajectory files, ~1,400 meteors/day, each
  with begin/end lat/lon/height, i.e. a real 3D LineString. CC BY 4.0.
- **TfL Unified API** — keyless line status, road disruptions (107 live), and
  bus arrivals with vehicle registrations, which becomes a track if polled by
  `vehicleId`. Greater London only.
- **EMODnet Human Activities WFS** — offshore platforms and wind farms with
  operator, status, capacity. Kind `feature`, European.
- **FDSN station metadata** — Raspberry Shake (1,566 UK station-epochs) and
  EarthScope. `service.iris.edu` 307-redirects to `service.earthscope.org`;
  follow redirects or you silently get nothing. ORFEUS returns 204/404 for the
  UK.
- **AERONET** — 1,674 aerosol sites, but a 2026 query for one site returned a
  45-byte banner and no rows. Probe per site before assuming currency.
- **NASA JPL SSD/CNEOS** — fireballs have lat/lon/altitude; close approaches
  and Sentry risk are non-spatial.
- **PlanIt** — UK planning applications with point geometry. Date-bounded
  queries work; bbox+recent timed out at 45 s.
- **UK retail fuel prices** — per-retailer JSON under the CMA scheme. Asda's
  790 sites were updated the same day; Applegreen's file was 18 months stale.
  Check `last_updated` per feed.
- **GBIF** — 1.37M GB occurrence records for 2026, about half CC BY-NC.
- Also: PSKReporter (filter by band/mode; callsign queries return an empty
  envelope), FSA food hygiene, Helioviewer (solar imagery), OurAirports
  (86,021 rows, public domain), Safecast (radiation, but London samples were
  over a year old), AuroraWatch UK, NOAA CO-OPS tides (US only), OSM notes.

## Promising, unverified

- **UK Bus Open Data Service** — 401 without a key; registration is free and
  self-serve. Live positions for every bus in England. The biggest remaining
  gap, and one signup from being verifiable.
- **openAIP** — airspace polygons; free account, 403 seen.
- **National Highways DATEX II** — 401 "Invalid Subscription Key"; Azure APIM
  free tier, probably self-serve. The old unauthenticated endpoints
  (`trafficengland.com`, `m.highwaysengland.co.uk`) are dead.
- **ONS Open Geography** — 3,904 keyless ArcGIS services (wards,
  constituencies, LSOAs). The natural join surface for crime and planning.
- **Met Office DataHub** — free tier exists, but Open-Meteo covers it keyless.

## Checked and rejected

| Source | Why |
|---|---|
| AviationAPI | DNS doesn't resolve |
| UK National Chargepoint Registry | DNS returns nothing; retired or moved |
| Open Charge Map | 403, needs a key; static POIs with no live availability |
| Global Fishing Watch | 401; tokens are request-and-approve |
| APRS-IS direct | `# Login by user not allowed` — needs a real callsign. The SDR route sidesteps this |
| Thames Water developer API | 504 three times, and needs client credentials; the ArcGIS route works keyless |
| Cefas WaveNet | Angular SPA; three "API" paths all returned the same 3,855-byte HTML with HTTP 200 |
| EURDEP / JRC | 404; the real network is restricted to national authorities |
| UK Street Manager | DNS doesn't resolve; permits are S3 bulk exports, not an API |
| NSTA oil & gas | ArcGIS root 404s; EMODnet covers platforms anyway |
| EAWS avalanche, Copernicus EMS, Aloft bird radar | 404 — paths moved |
| SILSO sunspots | Works, but a single global scalar with no geometry |
| wheretheiss.at | Works, but the CelesTrak/SGP4 pipeline already does 25544 offline |
| ip-api, Mylnikov WiFi, the OSINT index's geospatial section | Lookup services, not feeds: no enumerable population, no time dimension, and IP-derived coordinates are a city centroid |

The OSINT index was essentially useless for this: almost every geospatial
entry is a keyed geocoder, IP lookup or AI inference service. A map needs feeds
with a position and a clock.

## Suggested order

Easiest first, roughly most reusable first:

1. ~~SondeHub~~ — done; reused the aircraft track machinery.
2. ~~Aviation weather~~ — done; SIGMETs, then METAR/TAF from the bulk cache.
3. ~~Storm overflows~~ — done; nine feeds, two schemas.
4. ~~NDBC buoys~~ — done.
5. ~~EA flood monitoring~~ — done.
6. Register for BODS, then build it.

## Process notes

Things that have gone wrong more than once, in the order they were learned.

- Send `--compressed`. SondeHub's telemetry endpoint returns gzip and looks
  like corrupt binary without it.
- Assert on record counts, never on status codes. Three services returned 200
  with unusable bodies.
- Make that assertion a test, not a spot check. A lost page is valid,
  decodable and short — it looks like a county with no sewers, not an error.
  `storm_overflow_live.rs` polls all nine companies behind
  `ARGUS_NETWORK_TESTS=1` and fails on a feed that has quietly halved.
- A live test with its own `HttpClient` isn't testing the daemon's. The EA
  driver passed its live test with a 120 s client and then failed every poll
  in `argusd`, whose shared client allows 30 s. Deploy and read
  `sources.last_error` before believing a driver works.
- Check a field's cardinality across the whole layer, not its type in one
  record. The EA scalar-or-array flattening shows up on one station in five
  thousand and fails the whole document.
- Check a timestamp field's distribution, not one record. Every trap in the
  storm overflow feeds — the bulk stamp, Northumbrian's per-record stamp, the
  603 empty end dates against 36 live spills — was invisible in a sample and
  obvious in a `groupByFieldsForStatistics` count over the layer.
- "Fixed-width" in a research note means "the header lined up in the sample".
  NDBC's file has ragged decimals and ids from four to seven characters.
  Splitting on whitespace and holding the field count to 22 survives that;
  cutting at byte offsets would have read `4403587`'s latitude as `7  46.5`.
- Look at what actually landed in the store. The NDBC live test passed with
  HTML in the station notes because nothing asserted on the text; the stored
  row is where it showed.
- A bounding box that returns a plausible count is not evidence of
  completeness. AWC's METAR query returned 62 UK stations, which matched the
  research note; the same box split in two returned 90, and the bulk cache
  had 110. Split the box once and compare before believing a bbox endpoint
  returns everything inside it.
- A `_ft` column is not necessarily in feet, and a `_c` column is not
  necessarily in degrees. AWC's `vert_vis_ft` is in hundreds of feet and its
  `maxT24hr_c` is in tenths of a degree; both are only visible next to a
  column that is scaled correctly. Skip a column you cannot reconcile rather
  than publish it under a unit it does not have.
- The machine's own clock is an input. Every station layer read as `delayed`
  by about an hour on the day METARs were built, and the cause was Athena
  running 59 minutes ahead with NTP off, not the feeds. Check `date -u`
  against an upstream `Date:` header before reading lag as a source fault.
