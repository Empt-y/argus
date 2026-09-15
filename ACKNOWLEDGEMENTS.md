# Acknowledgements

## God's Eye View

Argus started because of [God's Eye View](https://github.com/bilawalsidhu/gods-eye-view)
by Bilawal Sidhu (MIT). That project showed that one person could build a
photorealistic live globe from public feeds, and it made the case for the
whole idea.

Argus is an independent implementation: no code is copied, and the
architecture is different on purpose — a persistent Rust daemon with a
historical archive and native clients, rather than a browser app with
in-memory state. What carries over is the idea, and a set of lessons its
author wrote down clearly, in particular:

- that a live-data interface must be honest about when data is modelled,
  delayed, estimated or simply absent
- that altitude without a stated datum will eventually put an aircraft
  underground
- that entity headings must be projected in screen space to stay world-stable
- that paid feeds need budget governors, not just rate limits

Its `docs/CURRENT-STATE.md` is a candid engineering logbook and worth reading
on its own.

## Data sources

Each source declares its provider, licence and credit line in its driver's
`Attribution`, and both clients show that verbatim in the attribution panel.
Several upstream licences (ODbL, CC BY-NC-SA, NASA terms) require it, so it's
enforced in the source registry rather than left to the UI.
