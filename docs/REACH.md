# Reaching Argus from off the LAN

Argus binds to `127.0.0.1` by default, which is right for development and wrong
for a phone. This is the ladder from there to "works from mobile data", and the
trade-off at each rung.

## 1. USB, for development

```sh
adb reverse tcp:8787 tcp:8787
adb shell am start -n org.argus.droid/.MainActivity -e server http://127.0.0.1:8787
```

Nothing is bound to the network. The daemon sees the request arrive on
loopback, so `server.auth = "loopback_exempt"` applies and no pairing is needed.
Good for iterating; useless once the cable comes out.

## 2. The LAN

```toml
[server]
bind = "0.0.0.0:8787"
public_url = "http://192.168.1.20:8787"   # this machine's LAN address
auth = "required"
```

`public_url` is not optional here. It is what the pairing QR carries, and it
defaults to the bind — so leaving it unset with `bind = "0.0.0.0"` produces a QR
pointing at `0.0.0.0`, and leaving the loopback bind produces one pointing at
the phone itself. `argusd` warns about both at startup rather than letting
pairing fail with a bare connection error.

`auth = "required"` is the important line. Without it, loopback is exempt —
which is still safe, because a phone on the LAN is not loopback — but
"required" removes the question entirely.

Traffic is plain HTTP. The honest statement of what protects it: the network
boundary and the per-device bearer token, not transport encryption. A device
already on the LAN could read it. That is the same trade-off
`network_security_config.xml` in the Android app spells out, and rung 3 is how
to stop making it.

## 3. Tailscale, for mobile data — and HTTPS for free

```sh
sudo systemctl enable --now tailscaled.service
sudo tailscale up                 # opens a browser to authenticate
tailscale ip -4                   # the 100.x.y.z address for this machine
```

Two things bit on the way through, both of which look like Tailscale being
broken and are not.

**This kernel has no TUN driver.** `CONFIG_TUN is not set` in
`6.18.39-gentoo-gentoo-bore`, so `/dev/net/tun` does not exist, `tailscaled`
cannot create a `tailscale0` interface, and it exits 1 on every start until
systemd gives up with `start-limit-hit`. `tailscale up` then reports "failed to
connect to local tailscaled", which points at the wrong thing entirely. The
symptom to recognise in `journalctl -u tailscaled`:

```
is CONFIG_TUN enabled in your kernel? `modprobe tun` failed
CreateTUN("tailscale0") failed; /dev/net/tun does not exist
```

The fix used here needs no kernel rebuild — `/etc/default/tailscaled`:

```sh
FLAGS="--tun=userspace-networking"
```

tailscaled then runs its own network stack, which is what Tailscale ships for
containers. The trade-off stated plainly: this node is reachable *from* the
tailnet for whatever tailscaled itself serves, which is all Argus needs, but it
cannot route arbitrary traffic *to* tailnet peers as a normal interface. That
would need `CONFIG_TUN=m` and a kernel rebuild — a deliberate job on a machine
with TPM-sealed LUKS, not a quick one.

(`CONFIG_NF_TABLES is not set` too, which is where the harmless
`cleanup: list tables: protocol not supported` line comes from. Userspace mode
needs no netfilter.)

**Serve is a tailnet feature, off by default.** `tailscale serve` blocks
silently waiting for it to be turned on — it prints a one-time enable URL and
then sits there, so piping its output through `head` hides the prompt and looks
like a hang. Enable it once in the admin console, per tailnet, not per machine.

Then either bind to the tailnet address directly:

```toml
[server]
bind = "100.x.y.z:8787"
public_url = "http://100.x.y.z:8787"
auth = "required"
```

...or, better, let Tailscale terminate TLS with a real certificate for the
tailnet name, which is the only rung where the cleartext trade-off goes away:

```sh
sudo tailscale serve --bg --https=443 http://127.0.0.1:8787
tailscale serve status            # prints the https://<host>.<tailnet>.ts.net URL
```

With `serve`, the daemon keeps its loopback bind and Tailscale is the only thing
that can reach it. Set `public_url` to the `https://…ts.net` name so the pairing
QR carries it. The API honours `X-Forwarded-Proto`, so the style document it
generates uses `https` URLs rather than downgrading its own tiles to cleartext —
confirmed against the live endpoint, not assumed.

### `auth = "required"` is mandatory here, not advisable

A reverse proxy on loopback makes every caller behind it *look* like loopback.
With `serve` running and the default `loopback_exempt` policy,
`GET /v1/layers` answered **200 with no token** from another machine on the
tailnet — the daemon's whole API, and the operator's whole feed history, open to
anything on the tailnet.

The exemption justifies itself on the grounds that a process able to reach
loopback could already read the config file. That is true of a local `curl` and
false the moment something forwards other people's connections through it.

Two things changed as a result. `argusd` now refuses the loopback exemption to
any request carrying `X-Forwarded-For`, `X-Forwarded-Proto`, `X-Forwarded-Host`
or `Forwarded` — such a request did not originate where its socket claims, and
forging one of those headers can only ever *remove* an exemption. And this
deployment sets `auth = "required"` so the question does not arise.

If you put anything else in front of the daemon — nginx, Caddy, a tunnel —
assume the same trap applies and set `auth = "required"`.

### Verifying from the machine that serves it

In userspace mode this host has no `tailscale0` and no MagicDNS, so it cannot
resolve or dial its own tailnet name; `curl https://athena.tail1f65b0.ts.net`
fails with "Could not resolve host", which looks like Serve being broken and is
not. Use tailscaled's own outbound proxy (configured in
`/etc/default/tailscaled`):

```sh
curl -x http://localhost:1055 https://athena.tail1f65b0.ts.net/v1/health
```

### Android

With TLS real, the app's cleartext allowance in `network_security_config.xml`
is narrowed to `127.0.0.1`, `localhost` and `10.0.2.2` — the `adb reverse` and
emulator paths, which never leave the device. Every other host, including a
plain `http://` LAN address, is now refused by the platform.

## What is *not* here

No push provider. Alerts reach the phone over the WebSocket the foreground
service holds open, and an alert raised while that socket was down is still
pending for that device — `alerts.delivered_to` is per-device, so reconnecting
is the whole replay mechanism. Routing a notification about the operator's own
airspace through a third party's servers to arrive back on their own phone is
not something this project is willing to do, and it would not be more reliable
than a socket to a machine they control.
