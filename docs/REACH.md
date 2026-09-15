# Reaching Argus from off the LAN

Argus binds to `127.0.0.1` by default, which is fine for development and no
use for a phone. There are three ways up from there, and each one changes what
`auth` needs to be.

## 1. USB, for development

```sh
adb reverse tcp:8787 tcp:8787
adb shell am start -n org.argus.droid/.MainActivity -e server http://127.0.0.1:8787
```

Nothing is bound to the network. The daemon sees the request arrive on
loopback, so `server.auth = "loopback_exempt"` applies and no pairing is
needed. Good for iterating, useless once the cable comes out.

## 2. The LAN

```toml
[server]
bind = "0.0.0.0:8787"
public_url = "http://192.168.1.20:8787"   # this machine's LAN address
auth = "required"
```

`public_url` matters here. It's what the pairing QR contains, and it defaults
to the bind address, so leaving it unset with `bind = "0.0.0.0"` gives you a QR
pointing at `0.0.0.0`, and leaving the loopback bind gives you one pointing at
the phone itself. `argusd` warns about both at startup.

`auth = "required"` is the important line. Strictly the loopback exemption is
still safe here — a phone on the LAN isn't loopback — but setting it removes
the question.

Traffic is plain HTTP. What protects it is the network boundary and the
per-device bearer token, not encryption; another device on the LAN could read
it. The Android app's `network_security_config.xml` used to allow this and no
longer does (see below), so in practice this rung is for the web client only.

## 3. Tailscale, for mobile data, with HTTPS

```sh
sudo systemctl enable --now tailscaled.service
sudo tailscale up                 # opens a browser to authenticate
tailscale ip -4                   # the 100.x.y.z address for this machine
```

Two things went wrong on the way through, and both looked like Tailscale
being broken when it wasn't.

**No TUN driver in the kernel.** This machine's kernel has `CONFIG_TUN` unset,
so `/dev/net/tun` doesn't exist, `tailscaled` can't create `tailscale0`, and it
exits 1 on every start until systemd gives up with `start-limit-hit`.
`tailscale up` then says "failed to connect to local tailscaled", which points
at the wrong thing. The line to look for in `journalctl -u tailscaled`:

```
is CONFIG_TUN enabled in your kernel? `modprobe tun` failed
CreateTUN("tailscale0") failed; /dev/net/tun does not exist
```

The fix that doesn't need a kernel rebuild is `/etc/default/tailscaled`:

```sh
FLAGS="--tun=userspace-networking"
```

tailscaled then runs its own network stack, as it does in containers. The
trade-off: this node is reachable *from* the tailnet for whatever tailscaled
itself serves, which is all Argus needs, but it can't route arbitrary traffic
*to* tailnet peers as a normal interface. That would need `CONFIG_TUN=m` and a
rebuild. (`CONFIG_NF_TABLES` is unset too, which is where the harmless
`cleanup: list tables: protocol not supported` line comes from.)

**Serve is off by default per tailnet.** `tailscale serve` blocks waiting for
it to be enabled — it prints a one-time enable URL and then sits there. Piping
its output through `head` hides the prompt and it looks like a hang. Enable it
once in the admin console.

Then either bind to the tailnet address directly:

```toml
[server]
bind = "100.x.y.z:8787"
public_url = "http://100.x.y.z:8787"
auth = "required"
```

or, better, let Tailscale terminate TLS with a real certificate for the
tailnet name:

```sh
sudo tailscale serve --bg --https=443 http://127.0.0.1:8787
tailscale serve status            # prints the https://<host>.<tailnet>.ts.net URL
```

With `serve`, the daemon keeps its loopback bind and Tailscale is the only
thing that can reach it. Set `public_url` to the `https://…ts.net` name so the
pairing QR carries it. The API honours `X-Forwarded-Proto`, so the style
document it generates uses `https` tile URLs rather than downgrading itself.

### Why `auth = "required"` is mandatory behind a proxy

A reverse proxy on loopback makes every caller look like loopback. With
`serve` running and the default `loopback_exempt` policy, `GET /v1/layers`
answered 200 with no token from another machine on the tailnet. The whole
API and the whole feed history were open to anything on the tailnet.

The exemption's reasoning — a process that can reach loopback could already
read the config file — is true for a local `curl` and false the moment
something forwards other people's connections through it. Two things changed
as a result: `argusd` now refuses the loopback exemption to any request
carrying `X-Forwarded-For`, `X-Forwarded-Proto`, `X-Forwarded-Host` or
`Forwarded` (forging one of those can only *remove* an exemption), and this
deployment sets `auth = "required"`.

If you put anything else in front of the daemon — nginx, Caddy, a tunnel —
assume the same applies.

### Checking it from the machine that serves it

In userspace mode this host has no `tailscale0` and no MagicDNS, so it can't
resolve its own tailnet name; `curl https://<host>.<tailnet>.ts.net` fails
with "Could not resolve host". Go through tailscaled's own outbound proxy
instead (configured in `/etc/default/tailscaled`):

```sh
curl -x http://localhost:1055 https://<host>.<tailnet>.ts.net/v1/health
```

### Android

With TLS in place, the app's cleartext allowance in
`network_security_config.xml` is narrowed to `127.0.0.1`, `localhost` and
`10.0.2.2` — the `adb reverse` and emulator paths, which never leave the
device. A plain `http://` LAN address is refused by the platform.

## What isn't here

There's no push provider. Alerts reach the phone over the WebSocket the
foreground service holds open, and an alert raised while that socket was down
is still pending for that device — `alerts.delivered_to` is per-device, so
reconnecting is the replay mechanism. Routing a notification about your own
airspace through a third party's servers to get it back to your own phone
isn't something I want to do, and it wouldn't be more reliable than a socket
to a machine you control.
