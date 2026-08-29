#!/usr/bin/env python3
"""Build an Argus terrain grid from Environment Agency LiDAR.

Why this exists
---------------
Cesium's world terrain is a hosted service. Argus exists because hosted
services go away, rate-limit, or start charging, and because a system that
watches the sky over your own house should not need someone else's permission
to know how high the ground is. England publishes 1 m LiDAR under the Open
Government Licence, which is thirty times finer than the global terrain it
replaces, so self-hosting is not a compromise here — it is an upgrade.

What it produces
----------------
A pair of files the daemon reads directly:

  <name>.bin   raw float32, row-major, north row first, EPSG:4326
  <name>.json  the georeferencing and the datum the heights are in

Deliberately not a GeoTIFF. The server's job is to answer "how high is the
ground at this point" thousands of times a second; a flat grid with a known
origin is a multiply and an index, and needs no geospatial library in the
daemon at all. GDAL does the hard part here, once, offline.

Heights are ORTHOMETRIC (Ordnance Datum Newlyn), because that is what the
Environment Agency publishes. Cesium wants ellipsoidal heights. The conversion
is a geoid lookup, and it is done in the client, which already carries a
verified EGM96 implementation for exactly this reason — see `web/src/geo/
datum.ts`. The `datum` field in the sidecar is what tells it to.

Usage
-----
    prepare_terrain.py --bbox W S E N --out data/terrain/london --metres 2
"""

from __future__ import annotations

import argparse
import json
import math
import shutil
import subprocess
import sys
import tempfile
from pathlib import Path

# The 1 m composite DTM. DTM, not DSM: this is the ground, and a surface model
# would sit aircraft on the roofs of terminal buildings rather than on the
# apron beside them.
WCS = (
    "https://environment.data.gov.uk/geoservices/datasets/"
    "13787b9a-26a4-4775-8523-806d13af58fc/wcs"
)
COVERAGE = (
    "13787b9a-26a4-4775-8523-806d13af58fc__Lidar_Composite_Elevation_DTM_1m"
)
# The service is happy to scale server-side, which is the difference between
# moving a few hundred megabytes and a few hundred gigabytes.
CHUNK_M = 5000
NODATA = -9999.0


def run(cmd: list[str], **kw) -> subprocess.CompletedProcess:
    proc = subprocess.run(cmd, capture_output=True, text=True, **kw)
    if proc.returncode != 0:
        sys.exit(f"failed: {' '.join(cmd[:4])}…\n{proc.stderr[:2000]}")
    return proc


def to_bng(lon: float, lat: float) -> tuple[float, float]:
    """WGS84 degrees to British National Grid metres, via GDAL."""
    out = run(
        ["gdaltransform", "-s_srs", "EPSG:4326", "-t_srs", "EPSG:27700"],
        input=f"{lon} {lat}\n",
    ).stdout.split()
    return float(out[0]), float(out[1])


def fetch(east0: int, north0: int, east1: int, north1: int, scale: float, dest: Path) -> bool:
    """One WCS chunk. Returns False for a chunk with no LiDAR coverage."""
    url = (
        f"{WCS}?SERVICE=WCS&VERSION=2.0.1&REQUEST=GetCoverage"
        f"&COVERAGEID={COVERAGE}"
        f"&SUBSET=E({east0},{east1})&SUBSET=N({north0},{north1})"
        f"&FORMAT=image/tiff&SCALEFACTOR={scale}"
    )
    proc = subprocess.run(
        ["curl", "-sS", "--max-time", "600", "-o", str(dest), "-w", "%{http_code}", url],
        capture_output=True, text=True,
    )
    if proc.returncode != 0 or proc.stdout.strip() != "200":
        # Outside England the service answers with an error document, not a
        # raster. That is a legitimate outcome for an edge chunk, not a failure
        # of the run — the coverage simply stops at the coastline.
        print(f"    no coverage ({proc.stdout.strip()})")
        dest.unlink(missing_ok=True)
        return False
    if dest.stat().st_size < 1024:
        dest.unlink(missing_ok=True)
        return False
    return True


def main() -> None:
    ap = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    ap.add_argument("--bbox", nargs=4, type=float, required=True, metavar=("W", "S", "E", "N"))
    ap.add_argument("--out", required=True, help="output path without extension")
    ap.add_argument("--metres", type=float, default=2.0, help="target ground resolution")
    ap.add_argument("--keep", action="store_true", help="keep the intermediate chunks")
    args = ap.parse_args()

    for tool in ("gdaltransform", "gdalwarp", "gdalbuildvrt", "gdal_translate", "gdalinfo", "curl"):
        if not shutil.which(tool):
            sys.exit(f"{tool} not found on PATH")

    west, south, east, north = args.bbox
    out = Path(args.out)
    out.parent.mkdir(parents=True, exist_ok=True)

    # The corners alone are not enough — a projected box is not a rectangle in
    # degrees, so sample the edges and take the envelope.
    xs, ys = [], []
    for i in range(5):
        for j in range(5):
            x, y = to_bng(west + (east - west) * i / 4, south + (north - south) * j / 4)
            xs.append(x)
            ys.append(y)
    e0 = int(math.floor(min(xs) / CHUNK_M) * CHUNK_M)
    e1 = int(math.ceil(max(xs) / CHUNK_M) * CHUNK_M)
    n0 = int(math.floor(min(ys) / CHUNK_M) * CHUNK_M)
    n1 = int(math.ceil(max(ys) / CHUNK_M) * CHUNK_M)

    # Ask the service for roughly the resolution we intend to keep. Pulling 1 m
    # to throw it away is just someone else's bandwidth.
    scale = min(1.0, 1.0 / args.metres)
    chunks_x = (e1 - e0) // CHUNK_M
    chunks_y = (n1 - n0) // CHUNK_M
    print(
        f"bbox {west},{south},{east},{north} -> BNG {e0},{n0}..{e1},{n1}\n"
        f"{chunks_x}x{chunks_y} chunks of {CHUNK_M} m at scale {scale} "
        f"(~{args.metres} m ground)"
    )

    tmp = Path(tempfile.mkdtemp(prefix="argus-terrain-"))
    tiles: list[Path] = []
    try:
        for i in range(chunks_x):
            for j in range(chunks_y):
                ce0, cn0 = e0 + i * CHUNK_M, n0 + j * CHUNK_M
                dest = tmp / f"c_{ce0}_{cn0}.tif"
                print(f"  chunk {len(tiles) + 1}/{chunks_x * chunks_y}: E{ce0} N{cn0}", flush=True)
                if fetch(ce0, cn0, ce0 + CHUNK_M, cn0 + CHUNK_M, scale, dest):
                    tiles.append(dest)
        if not tiles:
            sys.exit("no chunks had LiDAR coverage — is the bbox inside England?")

        vrt = tmp / "all.vrt"
        run(["gdalbuildvrt", "-q", str(vrt), *map(str, tiles)])

        # One warp to WGS84. Degrees per pixel from metres, latitude-corrected
        # so pixels stay roughly square on the ground rather than in degrees.
        mid = math.radians((south + north) / 2)
        dlat = args.metres / 111_320.0
        dlon = args.metres / (111_320.0 * max(math.cos(mid), 1e-6))
        warped = tmp / "warped.tif"
        run([
            "gdalwarp", "-q", "-overwrite",
            "-t_srs", "EPSG:4326",
            "-te", str(west), str(south), str(east), str(north),
            "-tr", str(dlon), str(dlat),
            "-r", "bilinear",
            "-ot", "Float32",
            "-dstnodata", str(NODATA),
            str(vrt), str(warped),
        ])

        info = json.loads(run(["gdalinfo", "-json", str(warped)]).stdout)
        width, height = info["size"]
        gt = info["geoTransform"]

        # ENVI is a flat band-sequential dump in native byte order, which for
        # one Float32 band is exactly the array the daemon wants to mmap.
        raw = tmp / "grid.img"
        run(["gdal_translate", "-q", "-of", "ENVI", "-ot", "Float32", str(warped), str(raw)])
        shutil.copyfile(raw, out.with_suffix(".bin"))

        meta = {
            "format": "argus-dem-1",
            "width": width,
            "height": height,
            # Pixel corners, north-west origin, matching the raster itself.
            "west": gt[0],
            "north": gt[3],
            "lon_step": gt[1],
            "lat_step": gt[5],
            "east": gt[0] + gt[1] * width,
            "south": gt[3] + gt[5] * height,
            "nodata": NODATA,
            # The one thing a consumer cannot guess and must not assume.
            "datum": "orthometric",
            "source": "Environment Agency LIDAR Composite DTM 1m (OGL v3)",
            "attribution": "© Environment Agency copyright and/or database right 2026",
            "ground_metres": args.metres,
        }
        out.with_suffix(".json").write_text(json.dumps(meta, indent=2) + "\n")
        size_mb = out.with_suffix(".bin").stat().st_size / 1e6
        print(
            f"\nwrote {out.with_suffix('.bin')} ({width}x{height}, {size_mb:.1f} MB)\n"
            f"      {out.with_suffix('.json')}"
        )
    finally:
        if args.keep:
            print(f"chunks kept in {tmp}")
        else:
            shutil.rmtree(tmp, ignore_errors=True)


if __name__ == "__main__":
    main()
