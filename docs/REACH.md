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

Tailscale is already installed on this machine (`net-vpn/tailscale`), but
`tailscaled.service` is disabled and the machine has never joined a tailnet.
Both steps need a human: `tailscale up` opens a browser for authentication, and
joining a network is not something a daemon should do on someone's behalf.

```sh
sudo systemctl enable --now tailscaled.service
sudo tailscale up                 # opens a browser to authenticate
tailscale ip -4                   # the 100.x.y.z address for this machine
```

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
QR carries it. The API already honours `X-Forwarded-Proto`, so the style
document it generates will use `https` URLs rather than downgrading its own
tiles to cleartext.

At that point the Android app's cleartext allowance in
`network_security_config.xml` can be narrowed to nothing.

## What is *not* here

No push provider. Alerts reach the phone over the WebSocket the foreground
service holds open, and an alert raised while that socket was down is still
pending for that device — `alerts.delivered_to` is per-device, so reconnecting
is the whole replay mechanism. Routing a notification about the operator's own
airspace through a third party's servers to arrive back on their own phone is
not something this project is willing to do, and it would not be more reliable
than a socket to a machine they control.
