//! `/bin/sh` for the distroless images.
//!
//! podman-compose rewrites every healthcheck, `CMD` lists included, into a
//! `/bin/sh -c` string, so a `["CMD", probe, args...]` check reaches the
//! container as
//!
//! ```text
//! /bin/sh -c "/bin/sh -c /usr/local/bin/grpc_health_probe' '-addr' 'unix:///run/service/grpc.sock"
//! ```
//!
//! An image with no shell then reports unhealthy forever. This binary takes
//! exactly `sh -c COMMAND`, splits COMMAND into words the way a POSIX shell
//! would (whitespace, `'...'`, `"..."`, `\`) and execs them. The nested
//! `/bin/sh -c` needs nothing special: it execs this binary again, which
//! runs the probe. Anything that needs a real shell (pipes, redirects,
//! expansions, globs) is refused with exit status 2 rather than run wrong.

use std::os::unix::process::CommandExt;
use std::process::{Command, ExitCode};

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let [flag, command] = args.as_slice() else {
        return refuse("usage: sh -c COMMAND");
    };
    if flag != "-c" {
        return refuse("usage: sh -c COMMAND");
    }
    let words = match split(command) {
        Ok(words) => words,
        Err(why) => return refuse(why),
    };
    let Some((program, rest)) = words.split_first() else {
        return ExitCode::SUCCESS;
    };
    let err = Command::new(program).args(rest).exec();
    eprintln!("sh: {program}: {err}");
    ExitCode::from(127)
}

fn refuse(why: &str) -> ExitCode {
    eprintln!("sh: {why} (this is not a full shell)");
    ExitCode::from(2)
}

/// Characters that mean something to a real shell when unquoted.
const SPECIAL: &[char] = &['|', '&', ';', '<', '>', '(', ')', '$', '`', '*', '?', '['];

/// Characters that mean something only at the start of a word.
const SPECIAL_FIRST: &[char] = &['#', '~'];

/// Splits `command` into words, honoring quotes and backslashes and refusing
/// anything a shell would do more with than split.
fn split(command: &str) -> Result<Vec<String>, &'static str> {
    let mut words = Vec::new();
    let mut word: Option<String> = None;
    let mut chars = command.chars();
    while let Some(c) = chars.next() {
        match c {
            ' ' | '\t' | '\n' => words.extend(word.take()),
            '\'' => {
                let word = word.get_or_insert_default();
                loop {
                    match chars.next() {
                        Some('\'') => break,
                        Some(c) => word.push(c),
                        None => return Err("unterminated '"),
                    }
                }
            }
            '"' => {
                let word = word.get_or_insert_default();
                loop {
                    match chars.next() {
                        Some('"') => break,
                        Some('\\') => match chars.next() {
                            Some(c @ ('"' | '\\' | '$' | '`')) => word.push(c),
                            Some('\n') => {}
                            Some(c) => {
                                word.push('\\');
                                word.push(c);
                            }
                            None => return Err("unterminated \""),
                        },
                        Some('$' | '`') => return Err("expansions are not supported"),
                        Some(c) => word.push(c),
                        None => return Err("unterminated \""),
                    }
                }
            }
            '\\' => match chars.next() {
                Some('\n') => {}
                Some(c) => word.get_or_insert_default().push(c),
                None => return Err("trailing \\"),
            },
            c if SPECIAL.contains(&c) || (word.is_none() && SPECIAL_FIRST.contains(&c)) => {
                return Err("pipes, redirects, expansions and globs are not supported");
            }
            c => word.get_or_insert_default().push(c),
        }
    }
    words.extend(word);
    Ok(words)
}

#[cfg(test)]
mod tests {
    use super::split;

    #[test]
    fn podman_composes_healthcheck_unwraps_to_the_probe() {
        let outer = split(
            "/bin/sh -c /usr/local/bin/grpc_health_probe' '-addr' 'unix:///run/service/grpc.sock",
        )
        .unwrap();
        assert_eq!(
            outer,
            [
                "/bin/sh",
                "-c",
                "/usr/local/bin/grpc_health_probe -addr unix:///run/service/grpc.sock"
            ]
        );
        assert_eq!(
            split(&outer[2]).unwrap(),
            [
                "/usr/local/bin/grpc_health_probe",
                "-addr",
                "unix:///run/service/grpc.sock"
            ]
        );
    }

    #[test]
    fn quotes_and_backslashes_join_and_escape() {
        assert_eq!(
            split(r#"a 'b c' "d \"e\"" f\ g"#).unwrap(),
            ["a", "b c", "d \"e\"", "f g"]
        );
        assert_eq!(split("'' x").unwrap(), ["", "x"]);
        assert_eq!(split("  ").unwrap(), Vec::<String>::new());
        assert_eq!(split(r"a '|' \;").unwrap(), ["a", "|", ";"]);
        assert_eq!(split("-x=a#b~").unwrap(), ["-x=a#b~"]);
    }

    #[test]
    fn what_needs_a_real_shell_is_refused() {
        for command in [
            "a | b",
            "a > f",
            "a; b",
            "a && b",
            "echo $HOME",
            "ls *",
            "a #b",
            "cd ~",
            "a 'b",
            "a \"b",
            "a \\",
            "\"$x\"",
        ] {
            assert!(split(command).is_err(), "{command}");
        }
    }
}
