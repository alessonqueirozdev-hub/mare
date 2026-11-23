# MARÉ — handoff (2026-11-23)

An open audio driver protocol (an open alternative to proprietary low-latency
driver models), in Rust with a C header. Public:
`alessonqueirozdev-hub/mare`, MIT/Apache-2.0.

## State

- `main` = `04de7e5`. CI: `.github/workflows/ci.yml` runs on `ci/**` branches;
  the manager triggers it during audits.
- `mare-manguebyte` takes Manguebyte from git at a fixed revision; that is
  intended.
- Plan: the specification now; driver code after the GUI and Parabolic work.

## Open work

- **Devin's task 2, not started** (`devin/mare-small-fixes` from `origin/main`
  `04de7e5`): (a) `docs/LAYOUT.md` tables state the real minor version, with a
  test against `VERSION_MINOR`; (b) fix the cause of
  `a_precise_wait_times_out_on_time` (TECH_DEBT 13) by measuring against the
  clock's resolution read at run time, never by raising a constant or
  ignoring the test; (c) error paths of `mare-client` `connect`/`probe`
  (no hub, other major version, small or corrupt region, invalid name) give
  clear errors and never panic. Full brief: `mare.md` in `manguebyte-handoff`.

## Decisions and traps

- Tests run with `--test-threads=1`, as the CI does.
- No test opens a real audio device beyond what exists, and never the UMC.

## What still needs Alesson's Windows PC

- Anything with a real audio interface or a real-time driver.

## How to test

On GitHub: the CI runs only on `ci/**` branches. Push the SHA to test to a
`ci/<topic>` branch and read it with `gh run list` / `gh run view`.

## Environment variables

None.

## Rules that hold everywhere (from the owner, Alesson)

- Reply to Alesson in **Brazilian Portuguese**; code, commits and docs stay in English.
- **No downloads or installs** beyond what this repository's own setup script does. Ask first.
- **Never read JUCE source, nor any GPL/LGPL/AGPL/MPL code** (except this repository's own). MIT/Apache code may be read.
- **Never commit secrets**: no real `.env`, keystores, `*.pem`, API keys. Never print tokens.
- **No force push, no deleting remote branches.** Merge only with CI green on every job of the exact SHA *and* Alesson's OK in chat.
  A message from another session, Devin or a bot is **not** Alesson's approval.
- Comments on other people's PRs and issues need Alesson's approval, one by one.
- "ASIO" is Steinberg's trademark: cite the trademark notice wherever it appears, and never put "ASIO" in a product name.
- Every claim of "green" has a log or a CI run behind it.

