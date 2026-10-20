//! The CPU share the radio is promised when the host is busy.
//!
//! The channelizer and decoders must keep up with the dongles in real time,
//! and nice cannot buy that against work in another cgroup: the kernel
//! splits CPU between sibling cgroups by `cpu.weight` first and only then
//! between the threads inside each by nice. So the deployment gives the
//! radio a heavy cgroup (`deploy/systemd/user.slice.d/`: the rootless
//! containers' `user.slice` at 1000 against a terminal session's default
//! 100), and the scanner refuses to start without one: some cgroup it runs
//! in, below the root, must have a `cpu.weight` of at least
//! [`trunk_config::OxideConfig::min_cpu_weight`].
//!
//! Inside a container the cgroup namespace hides the ancestors, so the
//! host's cgroup tree is mounted read-only at [`HOST_CGROUP`] and the
//! container is found in it by the ID podman writes to [`CONTAINERENV`].

use std::path::{Path, PathBuf};

use anyhow::{Context as _, bail};

/// Where a container sees the host's cgroup tree.
pub const HOST_CGROUP: &str = "/host/sys/fs/cgroup";
/// The cgroup tree outside a container.
const CGROUP: &str = "/sys/fs/cgroup";
/// What podman tells a container about itself.
const CONTAINERENV: &str = "/run/.containerenv";
/// How deep under the root a container's scope is looked for.
const MAX_DEPTH: usize = 8;

/// One cgroup on the way to the root and its weight.
#[derive(Debug, PartialEq, Eq)]
pub struct Level {
    pub path: PathBuf,
    pub weight: u32,
}

/// Fails unless a cgroup this process runs in has a `cpu.weight` of at
/// least `min` (0 checks nothing). The chain from this process's cgroup up,
/// for the log.
pub fn require(min: u32) -> anyhow::Result<Vec<Level>> {
    if min == 0 {
        return Ok(Vec::new());
    }
    let (root, own) = if let Ok(env) = std::fs::read_to_string(CONTAINERENV) {
        let id = container_id(&env).context("no id in /run/.containerenv")?;
        let root = PathBuf::from(HOST_CGROUP);
        if !root.join("cgroup.controllers").exists() {
            bail!(
                "the host's cgroup tree is not mounted at {HOST_CGROUP}: add \
                     /sys/fs/cgroup:{HOST_CGROUP}:ro to the container's volumes"
            );
        }
        let own = find_scope(&root, &format!("libpod-{id}.scope"))
            .with_context(|| format!("container {id} not found under {HOST_CGROUP}"))?;
        (root, own)
    } else {
        let own =
            std::fs::read_to_string("/proc/self/cgroup").context("reading /proc/self/cgroup")?;
        let rel = own_cgroup(&own).context("no cgroup v2 entry in /proc/self/cgroup")?;
        (PathBuf::from(CGROUP), Path::new(CGROUP).join(rel))
    };
    let chain = ancestry(&root, &own)?;
    check(&chain, min)?;
    Ok(chain)
}

/// The `id="…"` of a `.containerenv`.
fn container_id(env: &str) -> Option<&str> {
    env.lines()
        .find_map(|l| l.strip_prefix("id=\""))
        .and_then(|v| v.strip_suffix('"'))
        .filter(|id| !id.is_empty())
}

/// The cgroup v2 path in `/proc/self/cgroup` (`0::/a/b`), relative.
fn own_cgroup(proc_cgroup: &str) -> Option<&str> {
    proc_cgroup
        .lines()
        .find_map(|l| l.strip_prefix("0::"))
        .map(|p| p.trim_start_matches('/'))
}

/// The directory named `name` under `root`, breadth first.
fn find_scope(root: &Path, name: &str) -> Option<PathBuf> {
    let mut level = vec![root.to_path_buf()];
    for _ in 0..MAX_DEPTH {
        let mut next = Vec::new();
        for dir in level {
            let Ok(entries) = std::fs::read_dir(&dir) else {
                continue;
            };
            for e in entries.flatten() {
                if e.file_type().is_ok_and(|t| t.is_dir()) {
                    if e.file_name() == name {
                        return Some(e.path());
                    }
                    next.push(e.path());
                }
            }
        }
        level = next;
    }
    None
}

/// `own` and each cgroup above it, up to but not including `root` (which
/// has no `cpu.weight`), with their weights.
fn ancestry(root: &Path, own: &Path) -> anyhow::Result<Vec<Level>> {
    let mut chain = Vec::new();
    let mut dir = own;
    while dir != root {
        let file = dir.join("cpu.weight");
        let text = std::fs::read_to_string(&file)
            .with_context(|| format!("reading {}", file.display()))?;
        let weight = text
            .trim()
            .parse()
            .with_context(|| format!("{}: {text:?}", file.display()))?;
        chain.push(Level {
            path: dir.strip_prefix(root).unwrap_or(dir).to_path_buf(),
            weight,
        });
        dir = dir
            .parent()
            .with_context(|| format!("{} is not under {}", own.display(), root.display()))?;
    }
    Ok(chain)
}

fn check(chain: &[Level], min: u32) -> anyhow::Result<()> {
    if chain.iter().any(|l| l.weight >= min) {
        return Ok(());
    }
    let weights: Vec<String> = chain
        .iter()
        .map(|l| format!("{}={}", l.path.display(), l.weight))
        .collect();
    bail!(
        "no cgroup this scanner runs in has cpu.weight >= {min} ({}), so a busy host can starve \
         the channelizer: install deploy/systemd/user.slice.d/50-trunk-oxide-cpu.conf into \
         ~/.config/systemd/user/user.slice.d/ and apply it (the commands are in the file), or \
         set [oxide] min_cpu_weight = 0 to run without",
        weights.join(", ")
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tree(name: &str, weights: &[(&str, &str)]) -> PathBuf {
        let root = std::env::temp_dir().join(format!("cpu-weight-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        for (dir, w) in weights {
            let d = root.join(dir);
            std::fs::create_dir_all(&d).unwrap();
            std::fs::write(d.join("cpu.weight"), format!("{w}\n")).unwrap();
        }
        root
    }

    #[test]
    fn a_heavy_ancestor_is_enough() {
        let root = tree(
            "heavy",
            &[
                ("u", "100"),
                ("u/svc", "100"),
                ("u/svc/user.slice", "1000"),
                ("u/svc/user.slice/libpod-ab.scope", "100"),
            ],
        );
        let own = find_scope(&root, "libpod-ab.scope").unwrap();
        let chain = ancestry(&root, &own).unwrap();
        assert_eq!(
            chain.iter().map(|l| l.weight).collect::<Vec<_>>(),
            [100, 1000, 100, 100]
        );
        assert!(check(&chain, 1000).is_ok());
        assert!(check(&chain, 1001).is_err());
    }

    #[test]
    fn default_weights_are_refused() {
        let root = tree("light", &[("u", "100"), ("u/libpod-cd.scope", "100")]);
        let own = find_scope(&root, "libpod-cd.scope").unwrap();
        let err = check(&ancestry(&root, &own).unwrap(), 1000).unwrap_err();
        assert!(err.to_string().contains("u/libpod-cd.scope=100"), "{err}");
    }

    #[test]
    fn the_container_and_cgroup_are_read() {
        let env = "engine=\"podman-5.4.2\"\nname=\"x\"\nid=\"da47\"\nrootless=1\n";
        assert_eq!(container_id(env), Some("da47"));
        assert_eq!(container_id("name=\"x\"\n"), None);
        assert_eq!(
            own_cgroup("0::/user.slice/a.scope\n"),
            Some("user.slice/a.scope")
        );
    }

    #[test]
    fn zero_checks_nothing() {
        assert!(require(0).unwrap().is_empty());
    }
}
