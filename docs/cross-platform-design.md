# Cross-platform credential discovery — design

Status: DESIGN, not built. Nothing in this document is on master yet.

This module runs on macOS today. Everything it knows how to read — credential
files, the browser cookie store, its own state — is located by a path built from
`$HOME`, and one whole provider cohort is compiled out entirely off macOS. This
describes what full Windows and Linux parity requires, what parity is
*achievable* (it is not total, and the gap is not ours to close), and how each
part fails when it cannot work.

## The measured starting point

| | |
|---|---|
| path-resolution sites in `quota-core` + `quota-module` | 19 |
| of those with any Windows branch | 0 |
| providers compiled out off macOS | 9 (the cookie cohort) |
| `quota-core` has ever been compiled for Windows | no |

This table is the state *before* the work started, kept as the baseline the
design was written against. The Status section below records what has since
changed.

Nothing here has been compiled for Windows, so the first deliverable is a build
at all — every other item is unverifiable until one exists. It has to be a
*native* build: cross-compiling from macOS fails in a dependency's C build for
lack of MSVC headers, while a sibling repository pinning the same dependency
version builds it cleanly on a native Windows runner.

## The rule that decides every path question

There are two kinds of path in this module and they take **opposite** rules.

**Files we own** — the redemption journal, the quota config, the vault handle
file — follow the host's convention, and specifically the convention the subc
daemon already uses, since we are one of its modules. That is
`$XDG_CONFIG_HOME`, else `%APPDATA%` on Windows, else `~/.config`. Matching the
daemon matters more than matching any external standard: an operator configuring
this fleet should find every module's files in one place.

**Files another tool owns** — `auth.json`, `oauth_creds.json`, the JetBrains
settings XML, the Chrome cookie store — follow **that tool's** behaviour, which
is frequently *not* the host convention. This inverts the instinct, and getting
it backwards produces a Windows build that compiles, runs, reports healthy, and
finds nothing.

The concrete case, verified in the tools' own sources:

| tool | Windows location | follows host convention? |
|---|---|---|
| Codex CLI | `%USERPROFILE%\.codex\auth.json` | no |
| Gemini CLI | `%USERPROFILE%\.gemini\oauth_creds.json` | no |
| OpenCode | `%USERPROFILE%\.local\share\opencode\auth.json` | no |
| Kilo | `%USERPROFILE%\.local\share\kilo\auth.json` | no |
| Codebuff | `%USERPROFILE%\.config\manicode\credentials.json` | no |
| JetBrains | `%APPDATA%\JetBrains\<Product>\options\…` | **yes** |

Five of six keep their POSIX-shaped path on Windows because they are Node CLIs
built on `os.homedir()`. JetBrains is a native application and does not. A
helper that "handles Windows" by mapping every `~/.config` to `%APPDATA%` would
break five of these six while looking like the correct fix.

Consequence for the code: there is no single `config_dir()` that serves both
kinds. The helper exposes them separately, and each third-party path states in a
comment which tool's behaviour it is matching.

## Cookie decryption is not uniformly achievable

Chromium's cookie encryption differs per platform, and one platform has moved
out of reach.

**Linux.** Two schemes, and the correct one is chosen by the **value's own
prefix**, never by the operating system:

- `v10` — key is `PBKDF2-HMAC-SHA1("peanuts", "saltysalt", 1, 16)`, the constant
  fallback used when no keyring is available.
- `v11` — same KDF, but the password comes from the Secret Service (libsecret /
  gnome-keyring) or KWallet.

Both then use AES-128-CBC with an IV of 16 spaces. Note the iteration count is
**1** on Linux where macOS uses **1003** — the same function, a different
constant, and a wrong choice fails as garbage plaintext rather than an error.

A profile can hold both prefixes at once, which is why dispatch is per value.

Which backend a profile actually used is decided by Chrome's `--password-store=`
flag, and it is worth knowing the exact values because they are how a test host
is made to produce each scheme deliberately rather than by hoping:

| value | backend | scheme produced |
| --- | --- | --- |
| `basic` | constant fallback password | `v10` |
| `gnome-libsecret` | Secret Service (`org.freedesktop.secrets`) | `v11`, *if unlocked* |
| `kwallet`, `kwallet5`, `kwallet6` | KWallet over D-Bus | `v11`, *if unlocked* |
| absent, or anything else | desktop-environment detection | either |

The flag requests a backend; it does not guarantee one. Measured on a host
whose Secret Service was running but whose `login` collection was **locked**:
Chrome asked for `gnome-libsecret` and wrote `v10` anyway, with no error and no
indication in the profile that the request had not been honoured.

Two consequences. Testing a `v11` reader needs the keyring genuinely unlocked,
which in practice means a real desktop login rather than a headless run — a
headless box will keep producing `v10` and appear to confirm a code path it
never exercised. And `v10` is more common in the field than a reading of the
flag suggests, since any host whose keyring is locked at browser start degrades
to it silently.

**What the shipped Linux verification does and does not establish.** The `v10`
path was proven against a real Chrome profile: cookies decrypted, and
`qwen-cloud` served a live weekly window matching the macOS reading exactly. But
that VM's Chrome had **no desktop identity in its own environment** —
`XDG_CURRENT_DESKTOP`, `XDG_SESSION_TYPE` and `DESKTOP_SESSION` are all unset in
`/proc/<pid>/environ` — so detection could only land on the basic store. The
profile was 96 cookies, every one `v10`.

That is a correct proof of the `v10` reader and **no evidence at all about how
often a desktop Linux user is on `v11`**. A GNOME or KDE session with an
unlocked keyring is the case that produces `v11`. So the honest statement of
Linux coverage is: the scheme we read is proven; the share of users it covers is
unmeasured.

**A later attempt to produce `v11` on that VM failed, and the reason narrows the
ask.** The VM does have a real desktop login — `loginctl` reports an active
Wayland session on seat0, not the headless shape assumed above — so the earlier
explanation was wrong about the machine. What blocks it is the keyring itself:
the `Login` collection stays locked, and it cannot be unlocked without the
password it was created with. Piping the account password to `gnome-keyring-
daemon --unlock` leaves it locked, because this VM's keyring was created by a
different install and its password is not the current account password. There is
also no default collection, so a Secret Service client gets
`PromptDismissedException` where a desktop user would get a dialog.

So producing `v11` needs a keyring whose password is known at creation time —
either a fresh desktop user logged in through the display manager, or a keyring
deliberately recreated with a known password. Both are available on a desktop
box; neither is reachable through `prlctl exec` on this one. The blocker is a
credential, not the absence of a desktop.

Reaching `v11` requires the Secret Service API over D-Bus (`org.freedesktop.
secrets`) to fetch the `Chrome Safe Storage` password, then the same PBKDF2 at
one iteration. The obstacle is not the crypto — it is identical to `v10` bar the
password source — but that a locked or absent keyring must degrade to reporting
no cookies rather than to a wrong answer, and that failure mode cannot be
exercised on a box where the keyring is never unlocked in the first place.

Note there is no `detect` or `gnome` value: unrecognised strings fall through to
detection rather than being rejected, so a typo silently becomes "detect" and the
resulting profile proves nothing about the scheme you meant to test. On KDE
Plasma 6 detection selects KWallet 6.

`v12` (an xdg-portal Secret Portal scheme using AES-256-GCM) exists for sandboxed
installs. Out of scope initially; it must be *recognised* and refused by name
rather than falling into the "not a cookie we understand" bucket.

**Windows.** Two schemes, and one of them is closed to us:

- `v10` — key is DPAPI-unwrapped from `Local State`, then AES-256-GCM. Reachable.
- `v20` — App-Bound Encryption, introduced in Chrome 127. The key is held by
  Chrome's elevation service, which **validates the calling executable**. A
  non-Chrome process cannot obtain it.

So Windows parity is **partial by construction**. ABE is the default for
standard Chrome installs, so on a current Windows desktop the interesting
cookies are likely `v20` and unreadable by us. Some profiles remain `v10` (per-
user installs, custom data directories, policy-disabled ABE), and mixed profiles
are normal.

**A second obstacle sits in front of the encryption, and it was measured rather
than anticipated.** While Chrome is running on Windows it holds the cookie
database under an exclusive lock that refuses a copy *and* refuses an open with
`FileShare.ReadWrite` — verified against a live profile:

```
Copy-Item …\Network\Cookies      → "being used by another process"
[IO.File]::Open(…,'Open','Read','ReadWrite') → "being used by another process"
```

That is stricter than macOS, where Chrome also holds the file open but a copy
succeeds — which is what the existing extraction relies on. So a Windows reader
cannot reuse the snapshot approach: it would need a Volume Shadow Copy, or to
read only while Chrome is not running, and the second is not a real option for a
background poller on a desktop.

This matters for sequencing. The cost of Windows cookie support is the shadow-copy
machinery *plus* whatever fraction of profiles are still `v10`, and the first
half is paid even when the second turns out to be zero. Establish the `v10`/`v20`
split on a real profile before building either.

**Measured, and it settles the lane.** The cookie database is locked, but
`Local State` is not, and it answers the question directly — it carries the key
blobs, each tagged with the scheme that owns it. On a current default install
(Chrome 152, Windows 11, per-user install, non-administrator account):

```
os_crypt.app_bound_encrypted_key  → prefix "APPB",  644 bytes
os_crypt.encrypted_key            → prefix "DPAPI", 293 bytes
```

The `APPB` blob is present, so App-Bound Encryption is active and new cookies
are written as `v20` — sealed to Chrome's own executable. The legacy `DPAPI` key
remains beside it for cookies written before ABE, which is why profiles are
mixed rather than one or the other.

So Windows cookie support would cost shadow-copy machinery to reach a database
whose *current* entries are unreadable by construction, in exchange for whatever
stale `v10` remnants a profile still holds. **The lane is closed on evidence**,
not on documentation. Reopening it needs a change in what Chrome does, not a
change in effort here.

Worth keeping as method: the locked file was not the only witness. The question
was "which scheme does this profile use", and an unlocked file answered it —
waiting for the browser to quit would have produced the same answer more slowly.

This is not a gap to engineer around. Reading `v20` would mean impersonating
Chrome to its own elevation service, which is precisely what the mechanism
exists to prevent.

**What that demands of the failure path.** A `v20`-only profile is a distinct
condition and must not report as "no cookie found". The remedy differs
completely: no-cookie means log in, `v20` means this cannot work here at all and
no user action changes that. It gets its own `CookieError` variant, its own
message naming App-Bound Encryption, and it is **non-transient** — retrying is
futile in a way that a locked keychain is not.

**Decided: a new terminal class**, not `local_source_unavailable`. That class is
classified *transient* because it means a desktop program comes and goes;
putting a permanent host-level impossibility behind it serves a prior healthy
window stale forever and the condition never reaches a verdict.

The two obvious existing classes are both traps, and for opposite reasons:

- `credential_absent` is dishonest — a credential exists, and consumers are
  authorised to discard stored provider state when they see it.
- `credential_unusable` is counted by `class_means_credential_stopped_working`,
  so it would tell the operator to sign in again when no user action can help,
  and would inflate `cookieLoginsStale` — a number whose entire value is that it
  stays near zero until something needs attention.

The new class must therefore be wired through the whole chain rather than just
named: `classify()` non-transient, `class_means_credential_stopped_working()`
false (judged explicitly, since
`every_error_class_is_classified_for_credential_staleness` fails on an unjudged
class), an `error_class()` string, and a `docs/consumer-contract.md` row whose
self-recovery column reads *no — cannot work on this host*.

### Precedence: which error a mixed profile produces

Mixed `v10`/`v20` profiles are normal, so the decision is per requested domain
after filtering, never per profile, and it must intercept **before** the
existing skip-and-fold path. Today an undecryptable value is skipped silently
and an empty jar becomes `NoCookie`, whose remedy is "log in" — precisely the
wrong-remedy reporting this design exists to avoid.

1. If any matching cookie decrypts → **succeed**. A usable session must not be
   failed by unreadable siblings (the cost-asymmetry fence: wrongly rejecting a
   valid response breaks a provider outright).
2. Else if matching rows exist but are all in unsupported schemes → the new
   **terminal** error, naming the scheme.
3. Else no matching rows at all → `NoCookie`, unchanged.

## Cookie store locations

Per-browser, and both the current and historical layouts must be probed:

```
Linux    ~/.config/google-chrome/<Profile>/{Network/,}Cookies
         ~/.config/chromium/<Profile>/{Network/,}Cookies
         ~/.config/BraveSoftware/Brave-Browser/<Profile>/{Network/,}Cookies

         Both spellings, and NOT because one is legacy. Measured on Chrome
         151 for linux arm64: a freshly created profile puts the database at
         <Profile>/Cookies with no Network directory at all, including after
         real network traffic. Documentation and secondary sources describe
         the Network/ layout, so trusting either alone would have produced a
         resolver that finds nothing on a current Chrome -- which is
         indistinguishable from a host where nobody logged in.

         Search both, prefer the most recently written, exactly as the macOS
         path already does.
         (also the flatpak roots under ~/.var/app/…)

Windows  %LOCALAPPDATA%\Google\Chrome\User Data\<Profile>\Network\Cookies
         %LOCALAPPDATA%\Microsoft\Edge\User Data\<Profile>\Network\Cookies
         %LOCALAPPDATA%\BraveSoftware\Brave-Browser\User Data\<Profile>\Network\Cookies
```

Note Windows uses `%LOCALAPPDATA%` here while the *daemon's* config uses
`%APPDATA%` — the two are different directories, and this is a browser fact, not
a host convention we choose.

Older profiles keep `<Profile>/Cookies` without the `Network` segment. The
existing macOS locator already probes both and prefers the most recently
modified; that logic is platform-independent and stays.

## Shape of the change

`browser_cookies.rs` currently mixes three concerns behind one `cfg(macos)`
wall: locating the store, obtaining a key, and decrypting a value. Only the
middle one is truly platform-specific.

```
browser_cookies.rs         cohort-facing API, snapshot sharing, CookieJar,
                           error classification            (all platforms)
browser_cookies/locate.rs  candidate store paths per platform + newest-wins
browser_cookies/key.rs     macOS keychain | Linux secret service | Windows DPAPI
browser_cookies/decrypt.rs prefix dispatch: v10/v11 CBC, v10-win GCM,
                           v20 refuse-by-name, v12 refuse-by-name
```

The locator returns a **profile descriptor**, not a bare path: browser family,
profile root, the `Local State` path, and the platform scheme context. Key
acquisition and decryption both need it, and passing only a path is what allows
one profile's key to be paired with another profile's store.

That also corrects a claim an earlier draft of this document made — that only
key acquisition is platform-specific. Decryption is too: the same `v10` prefix
means a CBC-derived key on macOS and Linux but a DPAPI-derived AES-GCM key on
Windows, so the prefix alone cannot select the algorithm.

### The snapshot cache must become per-store

The snapshot sharing added in `f4cd478` is a single global slot holding one
copied path and one key. That invariant holds today only because macOS locate
returns exactly one store from one browser.

The locate tables above probe several browsers per platform, each with its own
store *and* its own key. A single slot then either thrashes — nine providers
against multiple browsers inside one 45s TTL — or serves one browser's store to
a lookup whose session lives in another. Worse, newest-mtime-wins across
browsers can select a store containing none of the cohort's sessions while the
live one sits in a browser nobody touched today.

So the cache becomes keyed per store: `store path -> { copy, key, taken_at }`,
with the TTL reasoning (below the refresher's 60s base interval) preserved
per entry rather than globally. A Linux profile can also hold both `v10` and
`v11` values with different password sources, so the cached entry holds a key
*per scheme* rather than one key.

One existing behaviour to preserve deliberately: current Chromium prepends a
32-byte `SHA256(host_key)` to the *plaintext* of cookies in database version 24
and later, which we already strip on macOS. It is not macOS-specific and applies
to every platform's decrypt path.

## Status

What has shipped since this was written, so the sections below are read as
design rather than as outstanding work:

| item | state |
|---|---|
| Windows CI leg | **done** — both legs green on every commit |
| `$HOME` resolution with `USERPROFILE` / `HOMEDRIVE`+`HOMEPATH` | **done**, tests run on every platform |
| Cookie scheme classified per value (`v10`/`v11`/`v12`/`v20`) | **done** |
| Linux `v10` cookie reading | **done**, proven against a real Chrome profile |
| Linux `v11` (keyring) | refused by name, pending a host with an unlocked keyring |
| Windows cookies | **closed on evidence** — a live profile carries an active `APPB` app-bound key, so current cookies are sealed to Chrome, and the file lock would have to be defeated first to reach the stale remainder |
| Windows journal rename durability | **gap, recorded** — see `sync_parent_directory` in `codex_resets.rs` |

The two gating items below are both done. They are kept because the reasoning
still explains why the work was ordered this way, and because the second one
describes a failure mode that recurs whenever a new path is added.

## Ordering: what blocks what

Two items gate everything else and are scheduled first rather than discovered
later. Neither is hard; both are silent if skipped.

1. **A Windows build.** Cross-compiling to `x86_64-pc-windows-msvc` from macOS
   fails in a dependency's C build (`ring`, missing `assert.h` — no MSVC
   headers on a Mac). That is a property of *cross*-compiling, not of the
   dependency: a sibling repository in this fleet pins the same `ring` version
   and its native Windows CI leg is green. So the answer is to build natively in
   CI rather than to change the dependency, and "no local Windows check" is a
   constraint on the developer loop rather than a blocker on the work.
2. **`$HOME` is normally unset on Windows.** All 19 existing sites read it. If
   the helper does not resolve `%USERPROFILE%` (with `HOMEDRIVE`/`HOMEPATH` as
   fallback), every third-party resolver returns `None` and the module reports
   "cannot resolve" everywhere — a Windows build that runs, looks healthy, and
   finds nothing.

## Verification

The failure this design is most exposed to is code that compiles for a platform
and is wrong on it, so the checks are chosen against that rather than for
coverage.

1. **Native execution on Windows and Linux, not cross-compilation.** A
   compile-only leg cannot establish parity: DPAPI, Secret Service, `Local
   State` handling and filesystem semantics are only exercised by running there.
   Fleet precedent exists for a `blacksmith-4vcpu-windows-2025` matrix leg
   (broca, claustrum, subconscious and synapse all run one).
2. **Decryption tested against fixtures, per scheme**, with a *known-plaintext*
   vector for each of the Linux `v10`, Linux `v11`, and Windows `v10` paths. A
   wrong KDF constant produces garbage rather than an error, so a test asserting
   only "did not fail" would pass with the macOS iteration count on Linux.
   Fixtures include the `SHA256(host_key)` plaintext prefix, which is a
   database-version property rather than a macOS one.

   Both round counts are now pinned to vectors computed outside this code, and
   the prefix was confirmed present on Linux against a live profile. That prefix
   is worth more than a fixture: because it is a plaintext this code can derive
   independently, a decryption can be checked for correctness rather than merely
   for not erroring — the only self-checking signal in a scheme with no
   integrity check.
3. **The refusal paths tested through the whole chain**, not on the variant
   alone. A consumer sees `CookieError` → `FetchError` → `error_class` →
   `classify`, and the hazard lives at the end of that chain: a terminal
   condition classified transient is served stale forever. Assert non-transience
   and the staleness-metric answer, not just the variant name.
4. **Precedence tested with a mixed-scheme store**: readable-plus-unreadable
   must succeed, unreadable-only must produce the terminal error, and
   no-matching-rows must stay `NoCookie`.
5. **Profile/key pairing tested with two browsers present**, so a cached key
   cannot be served against another browser's store.
6. **Path resolution tested per platform without running on it**, by making the
   resolver take the platform and environment as parameters rather than reading
   globals. This is the only part genuinely verifiable from a developer machine,
   and it is where the five-of-six homedir rule lives.
7. **Owned-file paths pinned against the daemon's actual convention.** "Follow
   what subc does" is a cross-repo promise with nothing currently checking it.
8. **A provider-level proof** that an unsupported-only host produces a degraded
   wire entry carrying the new class — not an empty success, and not an
   indefinitely stale window.

## Execution context decides whether any of this works

Credential access is a property of *who is running*, not only of the platform,
and the failure must stay honest when the answer is no:

- A subc daemon running as a **Windows service** (session 0 / SYSTEM) cannot
  unwrap the interactive user's DPAPI key and cannot see their
  `%LOCALAPPDATA%`. Every cookie provider is then unreadable regardless of ABE.
- A **headless Linux** host has no Secret Service, so `v11` values cannot be
  read at all. `v10` ("peanuts") values still can.

Both must degrade with a message naming the execution context, rather than as a
generic key failure that reads as transient and fixable.

Key-acquisition failures accordingly need finer classification than today's
single "keychain unavailable is transient" mapping. A **locked** keyring is a
condition of the moment and stays transient; a keyring that is **absent**, a
backend that is **unsupported**, or a malformed `Local State` are stable local
facts and must reach a non-transient verdict.

## Facts settled by measurement rather than assumption

- **Chrome's cookie DB is not WAL-journaled** on the current release: measured
  `journal_mode = delete`, with a zero-length `Cookies-journal` sidecar. So the
  existing plain-file copy is not stale-at-birth. This is worth re-checking
  during the port rather than assumed permanently, since a WAL sidecar would
  make the copy silently miss recent cookies — a wrong value rather than an
  error.
- **The cookie database version is 24**, which is the version that prepends the
  host hash to the plaintext, confirming that stripping is required and not
  optional.
- `crates/` contains exactly **one** `cfg(windows)` site today, so the CI
  comment claiming there is no Windows-specific code to exercise is accurate
  *now* and becomes false with the first item of this work.

## Also macOS-gated: antigravity

The parity accounting above (19 path sites, 9 cookie providers) is incomplete.
`antigravity.rs` carries eight `cfg(target_os = "macos")` sites of its own: it
probes for a running local editor process, which is the case that defined
`local_source_unavailable`. Full parity requires its process and port discovery
on Windows and Linux too, and that is a separate mechanism from anything else
here — process enumeration rather than credential reading.

## What this does not attempt

- Reading Windows `v20` cookies. Closed by design, above.
- The `v12` portal scheme. Recognised and refused; built when a host needs it.
- Non-Chromium browsers. Firefox uses a different store and different crypto,
  and no provider in this cohort needs it.
- OS keychain credential reads (Windows Credential Manager, macOS Keychain as a
  general store). Only Chrome's Safe Storage entry is read, and only as the
  cookie key.

  Tools that store credentials in a keychain rather than a file — Codex's
  `keyring` mode, Gemini's encrypted-storage mode — stay out of scope, but they
  must **not** report as `credential_absent`. If the tool's own configuration
  says a credential exists and we cannot read the storage mode it selected, the
  credential is present and unreadable, and calling it absent both misstates the
  remedy and authorises a consumer to discard stored state for that provider.

## Not settled here

- Whether Microsoft Edge has shipped App-Bound Encryption equivalent to Chrome's
  `v20`. If it has, the Edge rows inherit the same terminal refusal and the
  "some profiles remain readable" position weakens further on Windows.
- Which Secret Service / KWallet crate to depend on, and how a headless host
  (no session bus) is distinguished from a keyring that is merely locked. The
  API is async while the current cookie path is `spawn_blocking` behind a global
  mutex, so this needs a deliberate answer rather than an incidental one.
- Browser selection when the newest-modified store is unsupported but an older
  profile holds a usable cookie. Newest-wins is correct for one browser and
  becomes a question with several.
- The exact name and consumer-facing wording of the new terminal class.
