# Running the radio stack on this host: rootless podman-compose, project
# `trunk-oxide`. trunk-oxide and trunk-discord carry our code; the transcriber
# (the `parakeet` profile), jaeger and victoriametrics are not rebuilt by a
# deploy and stay up.

set shell := ["bash", "-euo", "pipefail", "-c"]

# The services whose images come from this tree.
services := "trunk-oxide trunk-discord trunk-monitor trunk-weather"

# How long `verify` waits for both to report healthy.
healthy_within := "180"

default:
    @just --list

# Cold, the Rust toolchain and the dependencies build first and it takes 10
# minutes or more.

# Build the images from the working tree: all of them, or the services named.
build *which=services:
    TARGET_CPU=$(rustc --print target-cpus | sed -n 's/.*currently \([a-z0-9_-]*\)).*/\1/p' | head -n1); \
    echo "building for ${TARGET_CPU:=native}"; \
    TARGET_CPU=$TARGET_CPU podman-compose build {{ which }}

# trunk-oxide exits at startup unless the user slice outweighs terminal
# scopes (deploy/systemd/user.slice.d). A restart of the user manager drops
# the runtime setting; `daemon-reload` does not apply it to a running slice.

# Fail early if the user slice is too light for trunk-oxide to start.
check-cpu-weight:
    #!/usr/bin/env bash
    set -euo pipefail
    weight=$(systemctl --user show user.slice -p CPUWeight --value)
    if [ "$weight" -lt 1000 ]; then
        echo "user.slice CPUWeight is $weight; trunk-oxide will refuse to start." >&2
        echo "  systemctl --user set-property --runtime user.slice CPUWeight=1000" >&2
        exit 1
    fi

# --force-recreate is what swaps the image: podman-compose compares the
# compose config, not the image, so without it an unchanged config keeps the
# old container. The scanner is off the air from here until it is healthy.

# Naming the services lets a change to one leave the scanner on the air.

# Recreate the services from the images just built: all of them, or those named.
up *which=services: check-cpu-weight
    podman-compose up -d --no-deps --no-build --force-recreate {{ which }}

# Wait for the services to report healthy; on a timeout show why.
verify *which=services:
    #!/usr/bin/env bash
    set -euo pipefail
    deadline=$((SECONDS + {{ healthy_within }}))
    for name in {{ which }}; do
        container=trunk-oxide_${name}_1
        until [ "$(podman inspect "$container" --format '{{{{.State.Health.Status}}')" = healthy ]; do
            if [ "$SECONDS" -ge "$deadline" ]; then
                echo "$container is not healthy after {{ healthy_within }}s:" >&2
                podman ps -a --filter "name=$container" --format '{{{{.Names}} {{{{.Status}}' >&2
                podman logs --tail 40 "$container" >&2
                exit 1
            fi
            sleep 3
        done
        echo "$container healthy"
    done

# Tag the images now running as :previous, so a bad deploy can be undone
# without a rebuild. Run before `build`, which retags :latest.

# Keep the running images as :previous.
snapshot:
    #!/usr/bin/env bash
    set -euo pipefail
    for name in {{ services }}; do
        image=localhost/trunk-oxide_${name}
        if podman image exists "$image:latest"; then
            podman tag "$image:latest" "$image:previous"
        fi
    done

# Put the :previous images back as :latest.
restore:
    #!/usr/bin/env bash
    set -euo pipefail
    for name in {{ services }}; do
        image=localhost/trunk-oxide_${name}
        # a service added since the last deploy has nothing to go back to
        if podman image exists "$image:previous"; then
            podman tag "$image:previous" "$image:latest"
        fi
    done

# Snapshot, build, replace and check. The whole deploy.
deploy: snapshot build up verify

# Go back to the images the last deploy replaced.
rollback: restore up verify

# What is running, and from which image.
status:
    podman ps --filter name=trunk-oxide_ --format 'table {{{{.Names}}\t{{{{.Status}}\t{{{{.CreatedAt}}'

# Recent logs of one service (trunk-oxide or trunk-discord).
logs service="trunk-oxide" lines="100":
    podman logs --tail {{ lines }} trunk-oxide_{{ service }}_1
