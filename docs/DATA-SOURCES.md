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
| `metars` | `metar.rs` | Aerodrome METARs with TAFs, NOAA AWC bulk cache |
| `buses` | `bods.rs` | Every bus in England, Bus Open Data Service |
| `argo-floats` | `argo.rs` | Argo profiling floats, latest surfacing |
| `meteors` | `gmn.rs` | Global Meteor Network trajectories |
| `ground-stations` | `satnogs.rs` | SatNOGS stations and what each is listening to |
| `road-disruptions` | `tfl.rs` | TfL road disruptions, Greater London |
| `carbon-intensity` | `carbon.rs` | Grid carbon intensity on DNO boundaries |
| `offshore-platforms` | `emodnet.rs` | EMODnet oil and gas platforms |
| `wind-farms` | `emodnet.rs` | EMODnet offshore wind farm outlines |
| `air-quality` | `openmeteo.rs` | CAMS air quality and pollen, a lattice over each AOI |
| `sea-state` | `openmeteo.rs` | Open-Meteo wave model, the same lattice |
| `seismographs` | `raspberryshake.rs` | Raspberry Shake citizen seismographs, FDSN |
| `street-crime` | `police.rs` | data.police.uk street-level crime, a month at a time |
| `submarine-cables` | `cables.rs` | TeleGeography cable routes, schematic, with owners and landings |
| `cable-landings` | `cables.rs` | TeleGeography landing points and what lands there |
| `power-grid` | `osmpower.rs` | OSM power lines, substations and plants via Overpass, per AOI |
| `fires` | `firms.rs` | NASA FIRMS VIIRS active fires, worldwide, last day (keyed) |
| `hf-propagation` | `wspr.rs` | WSPR beacon paths touching each AOI, great circles, ten-minutely |
| `river-discharge` | `glofas.rs` | GloFAS discharge via Open-Meteo, sampled at EA river gauges |
| `fireballs` | `cneos.rs` | NASA/JPL CNEOS fireballs seen from orbit, since 1988 |
| `airports` | `ourairports.rs` | OurAirports gazetteer: every airfield with runways |

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

### Open-Meteo (air quality, pollen, marine) — done
`air-quality-api.open-meteo.com/v1/air-quality` and `marine-api…/v1/marine`,
keyless, CC BY 4.0, non-commercial. Both take a comma list of points in one
request (156 answered in 0.27 s) and answer for the model cell containing
each, reporting the cell's own centre — the air model is 0.1°, the wave
model 1/12°. So a "gridded" layer here is a lattice of points over each
`[[aoi]]`, sized to at most 80 points per area (`openmeteo::lattice`: home
at 0.25°, British Isles at 1.5°), each a `measure` with
`Quality::Modeled`, dated by the model hour so repeat polls write nothing.
Air quality hourly, marine three-hourly; the free tier is 10,000 calls a
day counted per point and scaled past ten variables, and this comes to
about 6,700. Two findings from the whole-lattice check: pollen fields are
`null` outside Europe (written only when present), and the wave model
answers a coastal *land* point with the nearest sea cell up to 0.2° away,
so two lattice points can come back as one cell — deduped by cell in
`decode`. Deep inland it answers `null` for everything; dropped. The home
area has two sea cells (the Thames estuary and the Wash); the British Isles
area 65. `flood-api…/v1/flood` (GloFAS river discharge) is still unbuilt:
it needs river points to sample, which a lattice does not give.

### wspr.live (HF propagation) — done
`https://db1.wspr.live/?query=…` — ClickHouse over HTTP, keyless, CC BY-NC.
30,452 spots in a ten-minute window worldwide. Aggregated server-side per
`(band, tx_loc, rx_loc)` with the transmitter or receiver inside the AOI
box: 6,770 rows, 1.3 MB, 0.37 s for the British Isles. ClickHouse rejects an
aggregate alias that shadows a column used in WHERE (`any(tx_lat) AS tx_lat`
→ `ILLEGAL_AGGREGATION`); alias to a new name. Kind **`measure`**, not
event: a path open ten minutes ago is a reading of the ionosphere now, and
the six-hour measure horizon retires it honestly. Drawn as the great circle
(a vertex every 250 km; the path to Sydney goes over the Middle East, not
the Atlantic). Key `wspr:{band}:{tx_loc}:{rx_loc}`, ten-minute cadence,
~40k rows an hour with both AOIs. 10 rows in 6,770 had both ends in the
same square (a station hearing itself); dropped.

### EMODnet platforms and wind farms — done
`ows.emodnet-humanactivities.eu/wfs` GetFeature as GeoJSON with
`srsName=EPSG:4326`, which comes out longitude-first. `platforms` is 1,617
points; `windfarmspoly` 600 outlines (a wind farm at 29°N 13°W is the
Canaries, not a bug). `platformid` is missing or shared on a handful, so the
key is the WFS feature id. These are the first `feature`-kind layers, and
building them meant building the feature path: `Store::write_features`
versions rows in `features` (a new version only when geometry, label or
attrs change — compared in the database, because jsonb turns `41.0` into
`41` and Rust would say "changed" every poll), and the tile, catalogue and
detail queries read current features alongside entities. Polled daily.

### TfL road disruptions — done
`api.tfl.gov.uk/Road/all/Disruption`, keyless, 131 records in 314 KB. Each
has `point` as a JSON array *inside a string* (`"[0.054,51.471]"`) and a
GeoJSON `geography` Point; 32 also carry a `geometry` polygon of the affected
area. Dated by `lastModifiedTime` — TfL touches every active record daily —
because works run for weeks and the event horizon is seven days. Ended ones
dropped; planned ones (start in the future) kept and flagged. Kind `event`,
layer `road-disruptions`, coverage fixed to Greater London.

### SatNOGS — done
`network.satnogs.org/api/stations/` is 4,470 stations in one 3.4 MB
request, no paging. `api/observations/?start=&end=` pages 25 at a time by a
`Link: rel="next"` header (`HttpClient::get_page` reads it); ±20 minutes
around now is a few pages and gives each station the pass it is recording
or the next one, with the NORAD number as the join to `satellites`.
4,135 of 4,470 are `Offline`, 207 sit on Null Island, and `success_rate` is
a number on 2,137 stations and the boolean `false` on 2,333 — declared
numeric, one `false` failed the whole list. Poll time is the observation;
`Online` is live, the rest stale with `last_seen`. Kind `station`, layer
`ground-stations`.

### Global Meteor Network — done
`traj_summary_data/daily/`: one semicolon-separated file per solar-longitude
day (04:00 to 04:00 UTC), 86 columns, 4,031 trajectories on a September
night — three times the note's estimate. Aliases `latest_daily` (the day
being built, eight rows at 06:00) and `yesterday` (the last complete day,
which is the file dated *two* days back). Lines end `\n\r`, newline then
carriage return; untrimmed, the header is never found and a file decodes to
nothing. Columns read by header name, sigmas skipped. First poll backfills
the event horizon from the directory index, which is 386 KB and once took
the server over thirty seconds — patient client. Kind `event` with a
LineString from ignition to extinction; heights in attrs because the store's
geometry is 2D. Layer `meteors`.

### Argo floats — done
Ifremer ERDDAP `tabledap/ArgoFloats.json`. 4,315 floats reported in thirty
days; the layer is each float's latest surfacing. Profile-level columns with
`distinct()` answer in 20 s; anything touching a pressure level does not —
`pres<=12` over twelve days took 317 s and `orderByMin` hit the proxy's 300 s
limit — so the readings are not fetched and a card links to the float's own
page. Poll time is the observation time and a surfacing older than the
station horizon is `Quality::Stale`, by the storm-overflow reasoning: 90% of
the array is between surfacings at any moment and hiding it would be
wrong. `position_qc` 4 and 9 dropped. Percent-encode `>` `<` `,` or Tomcat
returns 400. Kind `station`, layer `argo-floats`, patient client.

### Bus Open Data Service — done
`https://data.bus-data.dft.gov.uk/api/v1/datafeed/?api_key=…&boundingBox=minLng,minLat,maxLng,maxLat`
returns SIRI-VM XML for every bus in the box; free self-serve key. 28,088
vehicles across England on a weekday morning, 375 operators, TfL's fleet
included. The bounding box is honest — halves of England summed to within
two of the whole — but the wire is not compressed: England is 31 MB per
request, so the driver is `Coverage::Bounded` and the AOIs decide the cost.
GTFS-RT from the same service is an eighth of the size and was rejected: no
operator, line or destination, and vehicle ids only unique per operator.
A quarter of every response is stale — 2,009 of 28,088 over six hours old —
and is dropped at ten minutes. Kind `vehicle` (new for this, migration
0008), layer `buses`. Config section `[sources.buses]` — the section name
must equal the source id for the credential resolver to find the key.

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

### Carbon intensity by DNO region — done
`api.carbonintensity.org.uk/regional`: 14 regions plus four aggregates,
half-hourly forecast intensity and a nine-fuel mix, no geometry. The
boundaries are NESO's "GIS Boundaries for GB DNO Licence Areas" GeoJSON
(3 MB, EPSG:27700 — `argus_core::geo::bng_to_wgs84` converts, ~5 m), cached
in `reference_geometry`. The two id numberings are joined by a hand table in
`carbon.rs`. Kind `measure`, layer `carbon-intensity`, dated by the period
so repeat polls write nothing — which also means the health panel reads
`delayed` for most of each half hour, since the scheduler's threshold is
two minutes for every kind. Known and tolerated. This layer is what
exposed the tile projection bug (see process notes).

Still unbuilt from Elexon: `bmrs/api/v1/datasets/FUELINST` for the 5-minute
GB fuel mix (a single national scalar, no geometry) and per-BM-unit output
(B1610), which returned an empty `data` array.

### data.police.uk street-level crime — done
`api/crimes-street/all-crime?poly=lat,lng:…&date=YYYY-MM`, OGL v3, keyless,
15 requests/s. `api/crimes-street-dates` lists the months; the newest was
two months back (2026-07 in September). The API refuses with a bare **503**
when a polygon holds over 10,000 records, and a 0.05° × 0.05° box in
Islington holds 8,231 in a month and takes 10.6 s to answer, so the driver
crawls half-degree tiles aligned to the grid (the home area's tiles are the
British Isles area's tiles, and a tile is claimed once per month per cycle)
and quarters any tile the API refuses, down to 0.02°. Uses the patient
client. Kind `event`; **the month-only date is stamped at poll time** with
`month` carried, because stamping at the first of the month would put every
crime on the DVR at midnight on a day it did not happen, and with the seven
day event horizon it would never be live. Cadence three days so the layer
stays inside that horizon. `Quality::Delayed` for the two-month lag. The
card says the month and that the point is snapped to an anonymised
location; 1,267 of 8,231 records (all the anti-social behaviour) have no
`persistent_id` and no outcome, so the numeric `id` is the key. Scotland is
not in the dataset; those tiles answer `[]` at once. Live test crawls
central London and asserts on >10,000 records, which only a working split
can produce.

### FDSN Raspberry Shake — done
`data.raspberryshake.org/fdsnws/station/1/query?network=AM&level=channel&format=text&endafter=<now>`:
one 2 MB pipe-separated response, 15,399 channel rows for 6,186 stations
with an open epoch (28,108 station epochs without the `endafter` filter;
6,186 open). No availability service on this host (404), so "active" means
an open epoch, not data flowing now. The channel set is the only thing that
tells one product from another — every SiteName is "Raspberry Shake Citizen
Science Station" — and `raspberryshake::model` reads it: EHZ alone is a 1D,
EH[ZNE] a 3D, EHZ + EN? a 4D, HDF a Boom, EHZ + HDF a Shake & Boom, SHZ the
original 50 Hz unit. Live count: 2,067 1D, 1,636 3D, 1,784 4D, 540 Shake &
Boom, 140 Boom; 364 in the British Isles. 17 stations at Null Island are
dropped. Kind `station`, poll time as observation time, six-hourly.

### TeleGeography submarine cables — done
`submarinecablemap.com/api/v3/cable/cable-geo.json` (728 MultiLineStrings,
740 KB), `landing-point/landing-point-geo.json` (1,925 points), `cable/all.json`
(707 ids) and `cable/{id}.json` for owners, suppliers, length ("45,000 km" as
text), RFS year, planned flag and landing points. CC BY-SA 4.0. The routes are
schematic paths between landings, not seabed tracks, and the card says so.
Two `feature` layers, weekly; the per-cable records are 707 requests at the
one-a-second pace, so a poll is twelve minutes, and a failed record leaves a
cable written from its route alone.

### OpenStreetMap power grid via Overpass — done
`overpass-api.de/api/interpreter?data=…` with `out geom` for
`power=line|substation|plant` in 2° tiles aligned to the grid. A 3° tile
around London was 3.5 MB in 60 s; 2° tiles on the patient client. **Three
quarters of substations are untagged 11 kV street kiosks** (14,536 of 20,588
in one tile): kept only when `substation=` says what it is or `voltage` ≥ 33
kV, `minor_distribution` excluded in the query. `voltage` is a `;` list on
double-circuit lines. Plant relations carry outer way members with geometry
under `out geom`; a closed one is the outline. Key `osm:way/123`; OSM ids do
not expire, so a split way leaves its old id behind. Live: the Berkshire tile
gave 2,120 lines, 1,343 substations, 401 plants. ODbL.

**Overpass etiquette, learned the hard way**: the public instance gives an
address two slots and holds one for a while after each heavy query, so
tiles sent back to back get 429s (45 of 55 on the first daemon run), and
fifty-five heavy queries twice in one afternoon — two daemon restarts, each
re-crawling everything — got this address **refused at the TCP level** by
both overpass-api.de servers within the hour. The driver now asks
`/api/status` for a free slot before each tile, waits ten seconds between
tiles, retries a 429 after a minute up to six times, and the runtime resumes
any source with a cadence of six hours or more from its `last_success` after
a restart instead of polling at once. Do not add a mirror to route around a
block; wait for it to lift.

### NASA FIRMS active fires — done
`firms.modaps.eosdis.nasa.gov/api/area/csv/{MAP_KEY}/{product}/world/1` for
`VIIRS_SNPP_NRT`, `VIIRS_NOAA20_NRT`, `VIIRS_NOAA21_NRT`: ~70k rows and
5.5 MB each in two seconds, ~200k detections a day worldwide. Keyed by a
FIRMS MAP_KEY (not an Earthdata token) under `[sources.firms]`; a bad key
answers `Invalid MAP_KEY.` with status 200, which the decoder turns into an
`Auth` error. Quota 5,000 transactions per ten minutes and a world day costs
about 36, so a three-product poll is ~110; half-hourly. `acq_time` is HHMM
**unpadded** (`7` = 00:07). No detection id: the key is satellite + position +
acquisition minute, and the driver remembers a day's keys so a re-read
writes only what is new. Kind `event`, dated by acquisition.

### GloFAS river discharge via Open-Meteo — done
`flood-api.open-meteo.com/v1/flood?latitude=…&longitude=…&daily=river_discharge&past_days=7&forecast_days=1`.
The design question from item 13 — where to sample a river model — is
answered by the EA's own gauge list: `flood-monitoring/id/stations?parameter=level&type=SingleLevel`
is 2,307 stations, every one with `riverName`, in 1,078 tenth-degree cells;
the driver reads it and asks for those cells, labelled by the river and town
of the gauge. **Open-Meteo's 600-calls-a-minute limit counts each point of a
multi-point request**: eleven batches of a hundred sent back to back lost
half of them to 429s (580 of 1,078 cells arrived), so batches go ten seconds
apart. ~10% of cells answer null for every day (no modelled river there);
dropped. Daily, `measure`, `Modeled`, dated by the model day, ~1,100 calls.

### CNEOS fireballs and OurAirports — done
`ssd-api.jpl.nasa.gov/fireball.api?req-loc=true&vel-comp=true`: 887 rows since
1988 as `fields` + `data` arrays of strings, 80 KB (`req-loc=true` already
drops the records without a position); `lat-dir`/`lon-dir` are separate
hemisphere letters. The store refuses anything before 1990, so the one 1988
record is skipped in the driver rather than logged as a driver bug. Chelyabinsk is
`2013-02-15 03:20:26`, 441 kt. Kind `event` dated by detection, so the live
view is usually empty and the DVR fills it — that is honest for forty a
year. OurAirports: `raw.githubusercontent.com/davidmegginson/ourairports-data/main/airports.csv`
(86,083 rows, 12.7 MB) and `runways.csv` (48,000). The project's own
`ourairports.com/data/` answered nothing to a plain GET; the GitHub Pages
mirror at `davidmegginson.github.io` and raw GitHub both work. Public domain.
Kind `feature`, weekly, key `airport:{ident}`; 10,508 carry an ICAO code,
the join to `metars`. Closed airfields (13,524) are kept and marked.

### FSA food hygiene ratings — done
`api.ratings.food.gov.uk/Authorities` (needs `x-api-version: 2` or the route
itself 404s; `?api-version=2` does not work) lists 363 local authorities with
a `FileName` each: `ratings.food.gov.uk/OpenDataFiles/FHRS{code}en-GB.xml`,
which 307-redirects to `/api/open-data-files/…` — follow it or you get a
37-byte body. One XML document per authority, single-line, 575 MB for
613,379 establishments; Birmingham's is 10 MB. Refreshed nightly per
authority from its own extract (`Header/ExtractDate` ranged from April to
yesterday across the set). OGL v3. Counted over the whole register before
building: **157,539 (26%) have no `Geocode`** and are skipped (Birmingham
3,774, North Yorkshire 3,224 of them); `RatingValue` is spelled two ways
for the same state and Welsh authorities carry `cy-gb` keys in their
English files, so the rating is normalised from `RatingKey` (24 distinct
keys → 0–5 or one of six words); 70,980 rating dates are empty; 79
records carry `RightToReply` as double-escaped HTML, not stored. Scores
are points lost (0 best): hygiene /25, structural /25, confidence in
management /30. Kind `feature`, weekly, key `fhrs:{FHRSID}`; nothing that
changes nightly (the extract date) goes in the attrs or every row would
re-version every week. A business that leaves the register stays as its
last version, like a closed airfield.

### AuroraWatch UK — done
`aurorawatch-api.lancs.ac.uk/0.2/status/all-site-status.xml` names the
alerting site and its level (`green|yellow|amber|red`);
`0.2/project/{awn,samnet,bgs_sch}.xml` define 26 magnetometer sites with
coordinates; `0.2/status/project/{project}/{site}-activity.xml` is a site's
last 24 hourly values in nT with thresholds (50/100/200) — and exists for
**five of the 26** (404 for the rest, hence `HttpClient::get_bytes_if_present`).
Of the five, only Sumburgh Head (`SUM`, the alerting site) was current;
Crooktree was a month stale, `LAN1` two years, `SID` eight. Timestamps are
`2026-09-17T11:59:59+0000` — an offset without a colon, which RFC 3339
parsing refuses. `LAN2` exists in both SAMNET and the BGS schools project, so
the key carries the project. Kind `station`, every 15 min, observed-at = the
document's `updated` (its assembly time; the newest hour's value is a running
one, revised through the hour). The "national scalar"
is therefore spatial after all: it is the reading at the instrument that
decides it, and the card says so. CC BY-NC-SA 3.0. HTTP only.

### IODA internet outages — done
`api.ioda.inetintel.cc.gatech.edu/v2/outages/events?from=&until=&limit=&page=`
(`from` is mandatory; `limit` 2,000 was hit in a 24 h window, so page).
Locations `country/TO`, `region/1906`, `asn/41678`, `geoasn/3269-1906`;
datasources `bgp` (1,330 of 2,000), `ping-slash24`, `merit-nt`, `gtr`;
`status` always 0 and `fraction`/`uncertainty` always null; three
(location, start) pairs repeat with another datasource, so the key carries
it. An event still running is listed with its duration so far, capped at
14 days. Outlines: `v2/topo/region` and `v2/topo/country` are TopoJSON in a
JSON envelope, **served `Content-Encoding: gzip`** (14 MB → 37 MB and 7 → 20),
4,581 Natural Earth admin-1 regions keyed `properties.id` and 247 countries
keyed `properties.usercode` (8 with null geometry). Decoded by
`argus_ingest::topojson` (no transform in these files; the decoder handles
one anyway) and kept in the geometry cache. AS-wide events are drawn on the
AS's registration country via RIPEstat `rir-stats-country` (Ash's call —
RIPEstat `geoloc` refuses an ASN). First poll ≈ 5 min: two topologies and
~600 RIPEstat lookups at 4/s. Kind `event`, 10 min, key
`ioda:{location}:{datasource}:{start}`, dated by start (the 7-day event
horizon means a two-week-old ongoing outage lives in the DVR, not the live
view). Licence: free for non-commercial use with attribution.

### GRIP BGP hijacks and leaks — done
`api.grip.inetintel.cc.gatech.edu/json/events` 301s to `/v1/json/events`;
`?length=N` (DataTables-style; `recordsTotal` 10,000). 500 events spanned
five hours and were **4.6 MB**, because `asinfo` (AS-Rank name, org, country)
repeats per event — worth it, it is what names the networks. Types seen:
`defcon` 270, `submoas` 136, `moas` 94 (`edges` exists). A `pfx_event` is
`prefix` for MOAS and `sub_pfx`/`super_pfx` for the rest; MOAS names no
victim. `summary.inference_result.primary_inference.suspicion_level`: 80 for
237 of 500, 20 or below for the rest and those are labelled `legitimate`, so
the layer keeps suspicion ≥ 20. Placed by RIPEstat `geoloc` of
`summary.prefixes[0]` (445 distinct in 500 events). Kind `event`, 10 min,
key `grip:{id}`, dated `view_ts`. Non-commercial with attribution.

### RIPE RIS Live — done, as a rate per collector
`wss://ris-live.ripe.net/v1/ws/?client=<name>`, subscribe with
`{"type":"ris_subscribe","data":{"type":"UPDATE"}}`. Measured unfiltered:
**4,580 messages/s**, 207,097 prefix announcements, 6,679 withdrawals and
88,592 distinct prefixes in 20 s from 23 collectors (RRC25 and RRC23 the
loudest). Not storable per prefix and not geolocatable at that rate; what
is kept is the rate per collector — updates, prefixes announced and
withdrawn, peers heard, per minute. Collector positions are a table at city
precision (`stat.ripe.net/data/rrc-info` names the city and exchange but has
no coordinates; RRC02/08/09 are deactivated). The socket is held open by a
background task with 5 s→5 min reconnect backoff; five minutes of silence
fails the poll. Kind `measure`, every minute, key `ris:rrc01`. Frames are
parsed by `serde_json` into a struct of the five fields the tally needs;
CPU cost noted in the commit message that landed it.

### RIPE Atlas probes — done
`atlas.ripe.net/api/v2/probes/?page_size=500&status__in=1,2` (`status=1,2`
is a 400: "not one of the available choices"); the cursor is `next` in the
body. 17,238 connected or disconnected probes in 35 pages (60,650 in all —
the rest abandoned or never connected); 1,067 anchors; 14 with null
`geometry`; 287 with no `asn_v4`; 6,589 with an empty description; a dozen
tags each. Kind `station` on the fixture pattern (dated by poll,
`status_since`/`last_connected` as attrs, disconnected = `stale`), every 30
min because 17k rows a poll add up; key `atlas:{id}`. Addresses are not
stored, the AS and prefix are.

### PeeringDB exchanges and facilities — done
`peeringdb.com/api/fac` (5.7 MB, 5,874 facilities, **610 without
coordinates**), `/api/ix` (1.4 MB, 1,324 exchanges, **no coordinates at
all**), `/api/ixfac` (927 KB, 4,541 presences). An exchange is placed through
its facilities: 915 of 1,324 have a located one; 409 have none and are left
unplaced. Several buildings → MultiPoint (the first MultiPoint feature in
the store; `write_features` and the tiler take it as any geometry). All
`status: ok`. CC0. Anonymous access is throttled; the three pulls are 10 s
apart, weekly. Kind `feature`, keys `pdb:fac:{id}`, `pdb:ix:{id}`.

### root-servers.org — done
`root.json` is gone (404 with a trailing-slash redirect); the data is in
`root-servers.org/map-data.js` (210 KB) as `const roots = new Map([...])`
(13 letters: operator, addresses, ASN) and `const sites = [...]` (1,573
rows: root, town, country, lat, lon, instances, ipv4, ipv6) — JSON once the
brackets are matched. Rows repeat per instance in places (J-root Amsterdam
is five rows at one coordinate) and town names are typed by hand ("Chicago"
/ "CHICAGO", "Dar es Salaam" / "Dar Es Salaam"); co-located rows merge with
instances summed and case-insensitive towns → 1,467 sites, 2,028 instances,
keys unique. Kind `feature`, weekly, key `root:{letter}:{cc}:{town}[:n]`.

### GDELT 2.0 events — done
`data.gdeltproject.org/gdeltv2/lastupdate.txt` (http 301s to https) names the
newest `YYYYMMDDHHMMSS.export.CSV.zip`: a ZIP of one tab-separated CSV, no
header, **61 columns**, ~1,340 events per 15-minute file, 81–99 KB. **A listed
file may not exist yet** — at 16:00 the 15:15, 15:30, 15:45 and 16:00 files all
404'd though `lastupdate.txt` and the 128 MB `masterfilelist.txt` named them —
so the driver walks forward from the last file it has and stops at the first
404. ActionGeo_Type over a file: city 529, country 401, state 203, US state
132, landmark 39, none 36; `NumSources` is 1 in a fresh file (mentions
accumulate later). `Day` was 2025-09-17 on 16 of 1,340 rows dated 2026-09-17
(GDELT's bug); `DATEADDED` is the file stamp and is the date used. The GEO API
404s and the DOC API 429s past one request per 5 s; neither is used. Kept
whole, per Ash — every root class, ~130k/day, names verbatim. The archive is
read by `argus_ingest::zip` (one entry, stored or deflated, no crate); codes
by `sources::cameo` (20 roots + 290 codes). Kind `event`, 15 min, key
`gdelt:{GlobalEventID}`.

### NASA GIBS imagery overlays — done (not ingested)
`gibs.earthdata.nasa.gov/wmts/epsg3857/best/{layer}/default/{YYYY-MM-DD}/{TileMatrixSet}/{z}/{y}/{x}.{jpg|png}`,
keyless, public domain, CORS on. Served as a catalogue at `/v1/overlays` and as
hidden raster layers in the style document, dated to the DVR instant and never
later than yesterday (today's product is still being assembled). Built:
`MODIS_Terra_CorrectedReflectance_TrueColor` (Level9), `VIIRS_SNPP_CorrectedReflectance_TrueColor`
(Level9), `VIIRS_SNPP_DayNightBand_At_Sensor_Radiance` (Level8),
`GHRSST_L4_MUR_Sea_Surface_Temperature` (Level7), `MODIS_Terra_Aerosol_Optical_Depth_3km`
(Level6). `VIIRS_Black_Marble` and `VIIRS_SNPP_Thermal_Anomalies_375m_All` 404
on this endpoint. Note the WMTS path order is `{z}/{y}/{x}`. Products
publish on their own lag: the swath products are whole the day after, but
MUR SST is two days behind (`<Default>` in `1.0.0/WMTSCapabilities.xml`
names each product's newest day), and asking for yesterday 404s on every
tile. Each product carries its `lag_days` in the catalogue.

### Others, verified keyless
- **TfL Unified API** — keyless line status and bus arrivals with vehicle
  registrations; road disruptions are built (below). BODS carries TfL's bus
  positions already, so the arrivals route is moot.
- **FDSN station metadata** — Raspberry Shake is built (above). EarthScope:
  `service.iris.edu` 307-redirects to `service.earthscope.org`; follow
  redirects or you silently get nothing. ORFEUS returns 204/404 for the UK.
- **AERONET** — 1,674 aerosol sites, but a 2026 query for one site returned a
  45-byte banner and no rows. Probe per site before assuming currency.
- **NASA JPL SSD/CNEOS** — fireballs built (above); close approaches and
  Sentry risk are non-spatial.
- **PlanIt** — UK planning applications with point geometry. Date-bounded
  queries work; bbox+recent timed out at 45 s.
- **UK retail fuel prices** — per-retailer JSON under the CMA scheme. Asda's
  790 sites were updated the same day; Applegreen's file was 18 months stale.
  Check `last_updated` per feed.
- **GBIF** — 1.37M GB occurrence records for 2026, about half CC BY-NC.
- Also: PSKReporter (filter by band/mode; callsign queries return an empty
  envelope), FSA food hygiene, Helioviewer (solar imagery), OurAirports
  (86,021 rows, public domain), Safecast (radiation, but London samples were
  over a year old), NOAA CO-OPS tides (US only), OSM notes. OurAirports, FSA
  food hygiene and AuroraWatch UK are built (above).

## Promising, unverified

- **openAIP** — airspace polygons; free account, 403 seen.
- **National Highways DATEX II** — 401 "Invalid Subscription Key"; Azure APIM
  free tier, probably self-serve. The old unauthenticated endpoints
  (`trafficengland.com`, `m.highwaysengland.co.uk`) are dead.
- **ONS Open Geography** — 3,904 keyless ArcGIS services (wards,
  constituencies, LSOAs). The natural join surface for crime and planning.
- **Met Office DataHub** — free tier exists, but Open-Meteo covers it keyless.

## Checked and rejected

- **ACLED** — key and registration. **UCDP GED** — `ucdpapi.pcr.uu.se` answers
  401 "API token required. Add header: x-ucdp-access-token". **ReliefWeb** —
  v1 is 410 (decommissioned); v2 is 403 without an *approved* `appname`
  (self-registration on their site). All three would be the conflict and
  humanitarian companions to GDELT if a key is ever obtained; ReliefWeb's
  country-level records could draw on the cached IODA country outlines.

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
6. ~~BODS~~ — done; the numbered list is finished.
7. ~~Argo floats~~ — done.
8. ~~Global Meteor Network~~ — done.
9. ~~SatNOGS~~ — done.
10. ~~TfL road disruptions~~ — done.
11. ~~Carbon intensity on DNO boundaries~~ — done.
12. ~~EMODnet platforms and wind farms~~ — done, and the feature path with them.
13. ~~Open-Meteo, FDSN Raspberry Shake, data.police.uk~~ — done; the
    month-only dates are stamped at poll time with the month carried.
14. ~~Phase 8 infrastructure: submarine cables, power grid~~ — done, and
    ~~Phase 7 fires (FIRMS)~~ — done, keyed.
15. ~~CNEOS fireballs, OurAirports~~ — done.
16. ~~Open-Meteo flood~~ — done, sampled at the EA gauges.
17. ~~wspr.live~~ — done, aggregated per path in ClickHouse.
18. ~~Phase 7 imagery and night lights~~ — done as GIBS overlays, not
    ingested. ~~FSA food hygiene~~ — done.
19. ~~AuroraWatch UK~~ — done, placed at the alerting magnetometer.
    The keyless list is empty.
20. Phase 8, the internet — the geolocation step is RIPEstat (`geoloc` for
    a prefix, `rir-stats-country` for an AS): ~~IODA outages~~ — done,
    ~~GRIP hijacks~~ — done, ~~RIS Live churn per collector~~ — done,
    ~~RIPE Atlas probes~~ — done, ~~PeeringDB exchanges and facilities~~ —
    done, ~~root-server sites~~ — done. **Phase 8 is complete.**
21. Phase 9, conflict and news: ~~GDELT events~~ — done. Then GDACS
    disasters, NASA EONET natural events, Smithsonian weekly volcanic
    activity. ACLED, UCDP and ReliefWeb are keyed (below).

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
- A keyed source's config section must be named after its *source id*, not
  a friendly name: the credential resolver looks up `[sources.<id>]`. The
  bus driver spent one deploy in `key_required` with a perfectly good key
  under `[sources.buses]` while its id was `bods-buses`.
- After fixing a fast clock, cargo will not rebuild anything: the artifacts
  are stamped an hour in the future and look newer than every edit. A
  `find target -newermt "$(date)" -exec touch -d '3 hours ago' {} +` is
  cheaper than `cargo clean`.
- Draw a country-sized polygon before trusting the tiler. Fourteen point
  layers hid a projection bug — latitude mapped linearly across each tile
  instead of through Mercator — because inside a high-zoom tile the error
  is invisible. The first big polygon drew a second Britain off Iceland.
  Any new geometry path should be checked at z2 as well as z12, by fetching
  a tile and mapping the vertices back to degrees.
- A field that is a number in every sample can still be a boolean in half
  the layer. SatNOGS's `success_rate` is `false` on 2,333 of 4,470 stations.
  Count the JSON types per field over the whole response before declaring
  any of them; `serde_json::Value` for the doubtful ones costs nothing.
- Look at line endings in a hex dump, not a terminal. GMN's files end every
  line `\n\r`; `head` shows a clean file and `str::lines` hands back lines
  that begin with `\r`, which fail every `starts_with`.
- A file listed in an index may not exist yet. GDELT names a 15-minute
  file up to an hour before it can be fetched. Walk forward from the last
  one you have and stop at the first 404, rather than treating the index
  as truth or the 404 as a failure.
- The machine's own clock is an input. Every station layer read as `delayed`
  by about an hour on the day METARs were built, and the cause was Athena
  running 59 minutes ahead with NTP off, not the feeds. Check `date -u`
  against an upstream `Date:` header before reading lag as a source fault.
