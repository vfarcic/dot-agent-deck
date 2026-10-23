## Hashing Library Updated for a Security Advisory

`ctutils`, a constant-time helper library underneath the hashing code the deck and the desktop app use, is updated from 0.4.3 to 0.4.4, which fixes RUSTSEC-2026-0385: its mask generation and `u128` comparisons were not constant-time. Nothing changes in how you use or configure the deck.
