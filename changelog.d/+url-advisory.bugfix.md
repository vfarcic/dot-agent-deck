## URL Library Updated for a Security Advisory

`url`, the library the deck and the desktop app use to parse URLs, is updated from 2.5.8 to 2.5.9, which fixes RUSTSEC-2026-0592: asking for the origin of a deeply nested `blob:` URL could overflow the stack and crash the program. Nothing changes in how you use or configure the deck.
