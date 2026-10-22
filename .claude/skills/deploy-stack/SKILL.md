---
name: deploy-stack
description: Build and deploy trunk-oxide and trunk-discord to the running podman-compose stack on this host, check they are healthy, confirm a change is live, and roll back. Use for "deploy", "ship it", "restart the scanner", "is it running the new build" or "roll back".
---

# Deploying the stack

Everything is a `just` recipe (see `justfile`); run `just --list`. The stack
is rootless podman-compose, project `trunk-oxide`, working dir `~/trunk-oxide`.
Only `trunk-oxide` (recorder, `Dockerfile.trunk-recorder`), `trunk-discord`
(`Dockerfile.discord`), `trunk-monitor` (the #monitoring dashboard,
`Dockerfile.monitor`) and `trunk-weather` (the Tempest weather station into
VictoriaMetrics, `Dockerfile.weather`; needs `secrets/tempest_token`) carry
our code. The transcriber (`trunk-parakeet`, the
`parakeet` profile), jaeger and victoriametrics are not touched by a deploy.

## The deploy

```
just deploy                      # snapshot, build, up, verify: all four
just build trunk-monitor         # or just some of them,
just up trunk-monitor            # so a change to one does not
just verify trunk-monitor        # take the scanner off the air
```

Deploying replaces a live scanner, so do it only when the user has said to.
It builds the working tree and does not commit or push: do that first if the
change should be on master (`git pull --rebase` if the push is rejected).

1. **`snapshot`** tags the running images `:previous` (so `rollback` needs no
   rebuild). It must run before `build`, which retags `:latest`.
2. **`build`** builds both images from the working tree, uncommitted changes
   included. A cold cache installs the Rust toolchain and builds every
   dependency first: about 12 minutes. Warm, it is mostly the changed crates.
3. **`up`** recreates both services with `--force-recreate --no-deps
   --no-build`. `--force-recreate` is the point: podman-compose compares the
   compose *config*, not the image, so without it a rebuilt image is ignored
   and the old container keeps running. trunk-oxide has to re-open its
   dongles; with the gain-calibration cache warm ("keeping the last sweep's
   gain" in its log) it is recording again within seconds, but both services
   are off the air from the recreate until they report healthy.
4. **`verify`** polls both containers' health for 180 s and, if one is not
   healthy, prints its status and last 40 log lines.

When running it from an agent session, put `just build` in the background
and wait on the log with an `until grep` loop instead of polling; it can take
longer than a foreground command may run.

## Before it starts

trunk-oxide exits at startup unless `user.slice` has `CPUWeight >= 1000`
(`[oxide] min_cpu_weight`; `deploy/systemd/user.slice.d/`). `just up` checks
this first and prints the fix. A restart of the user manager drops the
runtime setting: `systemctl --user set-property --runtime user.slice
CPUWeight=1000`. `daemon-reload` does not apply it to a running slice.

## Did the new build go live?

- `just status` shows what is running and when it was created.
- The running image ID should equal `podman images` for
  `localhost/trunk-oxide_trunk-oxide:latest` (and `_trunk-discord`):
  `podman inspect trunk-oxide_trunk-oxide_1 --format '{{.Image}}'`.
- `just logs trunk-oxide` / `just logs trunk-discord 200`.
- Metrics are in VictoriaMetrics on `localhost:8428` and keep dotted names:
  `curl 'localhost:8428/api/v1/query' --data-urlencode
  'query={__name__="trunk.oxide.calls_ended"}'`. A counter does not exist
  until its first increment, so "no series" can mean "nothing has happened
  yet" rather than "broken": wait for a matching call before concluding.
- Traces are in Jaeger (`localhost:16686`); each transmission's recorder span
  roots the work done for it.

## Rolling back

```
just rollback    # :previous back to :latest, up, verify
```

`:previous` is whatever the last `snapshot` saw, so a second deploy replaces
it: roll back before deploying again. To go back further, check out the old
commit and `just deploy` (warm cache, minutes).

## Gotchas learned

- Image swaps need `--force-recreate` (above). `podman-compose up -d` alone
  says nothing and changes nothing.
- A proto change touches both services: always deploy both together (the
  recipes do).
- podman-compose 1.3.0 mangles healthchecks into `/bin/sh -c ...`; the images
  ship `crates/trunk-sh` as `/bin/sh` for that. A container left over from an
  older image may need `podman update --health-cmd
  '["/usr/local/bin/grpc_health_probe","-addr","unix:///run/service/grpc.sock"]' <container>`.
- Do not `podman-compose down`: it takes the transcriber and observability
  containers with it. Recreate only the two services.
- Do not rebuild or restart `trunk-parakeet` for a change to our crates unless
  its own crates changed (`Dockerfile.parakeet`, `--profile parakeet`).
- In a `justfile`, a literal `{{` is written `{{{{` and the closing `}}` needs
  no escape. Doubling both gives Go templates a stray `}}` and the health
  check never matches.
- Live config (`config.toml`, the CSVs, `config-discord.toml`) is bind-mounted
  read-only, so a config-only change needs `just up`, not a build.
