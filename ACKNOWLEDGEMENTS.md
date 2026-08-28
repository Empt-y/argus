# Acknowledgements

## God's Eye View

Argus exists because of [God's Eye View](https://github.com/bilawalsidhu/gods-eye-view)
by Bilawal Sidhu (MIT licensed). That project demonstrated that a photorealistic
live globe fused from public feeds could be built by one person, and it made the
case for the whole category.

Argus is an independent, clean-room implementation: no source code is copied,
and the architecture is deliberately different — a persistent Rust daemon with a
historical archive and native clients, rather than a single-process browser app
with in-memory state. What carries over is the idea, and a set of hard-won
lessons its author documented well, in particular:

- that a live-data interface must be honest about when data is modelled,
  delayed, estimated or simply absent
- that altitude without a stated datum will eventually put an aircraft
  underground
- that entity headings must be projected in screen space to stay world-stable
- that paid feeds need budget governors, not just rate limits

Its `docs/CURRENT-STATE.md` is an unusually candid engineering logbook and is
worth reading on its own merits.

## Data sources

Each source declares its own provider, licence and required credit line in its
driver's `Attribution`, and both clients surface that verbatim in the
attribution panel. Several upstream licences (ODbL, CC BY-NC-SA, NASA terms)
require this; it is enforced in the source registry rather than left to the UI.
