# Installing the module

## Install and upgrade

On every platform, install and upgrade insula with the CortexKit CLI:

```sh
ck setup        # first install: places ck-insula and registers it with the daemon
ck upgrade      # replaces a running build safely
```

Both place the release binary (`ck-insula`, or `ck-insula.exe` on Windows),
register it in the daemon's config, and restart the module. Use them rather
than copying a binary by hand: a running executable cannot be replaced in
place on Windows, and the daemon's config folder differs per platform.

## Verify

```sh
ck module status insula --json      # health.status and health.metrics.buildCommit
ck quota                            # the usage.get array, rendered
```

`buildCommit` is the commit the running process was built from. After an
upgrade it must name the release you installed; if it names the previous one,
the old process is still serving.

## Development install (macOS and Linux)

The subc daemon (`ck-subc`) spawns and supervises the modules listed in its
config, `$XDG_CONFIG_HOME/cortexkit/subc.jsonc` (default
`~/.config/cortexkit/subc.jsonc`). Add an entry whose `program` is the built
binary; [`examples/subc.jsonc`](../examples/subc.jsonc) is a complete file:

```jsonc
{
  "version": 1,
  "modules": {
    "insula": {
      "program": "/abs/path/to/target/release/ck-insula"
    }
  }
}
```

`args`, `env` and `enabled` are optional. The daemon appends
`--subc <connection-file>` and sets `SUBC_MODULE_ID` itself; do not put either
in the entry. On its next start the daemon spawns the module, which registers
its management surface; consumers reach it through `catalog.list`,
`route.open` and then `usage.get`.

On macOS, `scripts/deploy.sh` replaces the build at
`~/.local/share/cortexkit/bin/ck-insula`:

```sh
scripts/deploy.sh            # build, sign, place, restart, verify
scripts/deploy.sh --stage    # build and sign into ~/.local/share/cortexkit/staging only
```

It refuses a dirty tree, a branch other than `master`, and a `HEAD` that is not
`origin/master` (`CK_DEPLOY_ALLOW_DIRTY=1` overrides all three). It builds the
release binary, signs it with the hardened runtime, checks that it executes,
moves it into place by rename rather than overwriting the running executable,
runs `ck module restart insula`, and fails unless the new process reports
`HEAD` as its `buildCommit`.

Elsewhere, do the same by hand: rename the new binary over the path in
`subc.jsonc` and run `ck module restart insula`, so the daemon stops and
restarts the process itself.

After a deploy, run the checkers in
[`verifying.md`](verifying.md#checkers-after-deploying).

## Configuration

The module reads `$XDG_CONFIG_HOME/cortexkit/ck-quota.jsonc` (default
`~/.config/cortexkit/ck-quota.jsonc`; on Windows `%APPDATA%\cortexkit\ck-quota.jsonc`)
at start and checks it again every 5 seconds, so an edit takes effect without
a restart: once a check applies it, setting `auto_use_resets` to 0 stops every
later redemption decision, and setting it back arms them again. Each applied change is logged once, for example
`[insula] config reloaded: codex.auto_use_resets 3600 -> 0 (disarmed)`, and
`health.check` reports the value in force and when it was loaded under
`quotaConfig`. A missing file, or one deleted while the module runs, means the
defaults. A file that cannot be read or parsed keeps the last good config
(at start, the defaults), and the warning names the file and the reason.
Today the file has one setting:

```jsonc
{
  "codex": {
    // Seconds before a banked Codex reset credit expires at which insula may
    // redeem it. 0 (the default) never redeems and never relaxes usedPercent.
    // Clamped to 30 days.
    "auto_use_resets": 86400
  }
}
```

Redemption spends real Codex credit; see [`SECURITY.md`](../SECURITY.md) and
[`consumer-contract.md`](consumer-contract.md#usedpercent-can-be-lower-than-what-the-provider-reported).
Arm one host per account: the redemption journal is per host, so two hosts
armed for the same Codex account do not coordinate.

## Remove

Delete the `insula` entry from `subc.jsonc` (or set `"enabled": false`) and
restart the daemon. insula leaves one file behind:
`$XDG_STATE_HOME/cortexkit/insula/redemptions.json`, the Codex redemption
journal. Keep it if the module may be reinstalled: a pending redemption in it
is what stops a reinstalled module from spending the same credit twice.
