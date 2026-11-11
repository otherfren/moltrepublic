// SPDX-License-Identifier: GPL-3.0-or-later

//! What this node's user can decide about a wake command WITHOUT running
//! it: the program behind its first word is found, reachable, readable,
//! executable, and a script's interpreter exists. The command runs through
//! `sh -c`, so anything only the shell can resolve passes unchecked.

use std::ffi::{OsStr, OsString};
use std::io::Read as _;
use std::path::{Path, PathBuf};

use rustix::fs::{accessat, Access, AtFlags, CWD};

/// What `sh` searches when `PATH` is unset.
const DEFAULT_PATH: &str = "/usr/local/bin:/usr/bin:/bin";

/// First words the shell runs itself.
const SHELL_WORDS: [&str; 44] = [
    "!", ".", ":", "[", "alias", "break", "case", "cd", "command", "continue", "do", "echo",
    "eval", "exec", "exit", "export", "false", "for", "function", "if", "kill", "local", "printf",
    "pwd", "read", "readonly", "return", "set", "shift", "source", "test", "then", "time",
    "times", "trap", "true", "type", "ulimit", "umask", "unset", "until", "wait", "while", "{",
];

/// The environment the check resolves against (moltd's own).
pub(crate) struct CheckEnv {
    pub(crate) path: Option<OsString>,
    pub(crate) home: Option<OsString>,
}

impl CheckEnv {
    pub(crate) fn of_process() -> CheckEnv {
        CheckEnv {
            path: std::env::var_os("PATH"),
            home: std::env::var_os("HOME"),
        }
    }
}

/// The first check `command` fails, as one compact line.
pub(crate) fn check_command(command: &str, env: &CheckEnv) -> Result<(), String> {
    let Some(word) = program_word(command) else {
        return Ok(());
    };
    if word.is_empty() {
        return Err("no command".to_string());
    }
    if SHELL_WORDS.contains(&word.as_str()) {
        return Ok(());
    }
    let file = if let Some(rest) = word.strip_prefix("~/") {
        let Some(home) = env.home.as_ref() else {
            return Ok(());
        };
        let p = Path::new(home).join(rest);
        check_path(&p)?;
        p
    } else if word.contains('/') {
        let p = PathBuf::from(&word);
        check_path(&p)?;
        p
    } else {
        on_path(&word, env).map_err(|e| match e {
            Some(found) => format!("not executable: {}", found.display()),
            None => "not found on PATH".to_string(),
        })?
    };
    check_shebang(&file, env)
}

/// `command` is ONE program with its arguments: `sh -c "exec <command>"`
/// runs the same thing with the program as the direct child, so a kill
/// reaches the script and not a shell in front of it.
pub(crate) fn runs_one_program(command: &str) -> bool {
    let line = command.trim_start();
    if line.contains([';', '|', '&', '<', '>', '(', ')', '\n']) {
        return false;
    }
    match split_word(line) {
        Some((word, _)) => {
            !word.is_empty() && !word.contains('=') && !SHELL_WORDS.contains(&word.as_str())
        }
        None => false,
    }
}

/// The word naming the program: `None` when only the shell can tell.
fn program_word(command: &str) -> Option<String> {
    let mut rest = command.trim_start();
    loop {
        let (word, tail) = split_word(rest)?;
        // `NAME=value prog`: the assignment is not the program
        let assigns = word.split_once('=').is_some_and(|(name, _)| {
            !name.is_empty()
                && !name.starts_with(|c: char| c.is_ascii_digit())
                && name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
        });
        if !assigns {
            return Some(word);
        }
        if word.starts_with("PATH=") || tail.trim_start().is_empty() {
            return None;
        }
        rest = tail.trim_start();
    }
}

/// One shell word and what follows it; `None` when it needs the shell.
fn split_word(s: &str) -> Option<(String, &str)> {
    const UNDECIDABLE: &[char] = &['$', '`', '\\', '*', '?', '[', '{', '"', '\''];
    if let Some(q) = s.chars().next().filter(|c| *c == '\'' || *c == '"') {
        let body = &s[1..];
        let end = body.find(q)?;
        let (word, tail) = (&body[..end], &body[end + 1..]);
        let ends = tail.chars().next().map_or(true, |c| c.is_whitespace() || ";|&<>()".contains(c));
        if !ends || (q == '"' && word.contains(['$', '`', '\\'])) {
            return None;
        }
        return Some((word.to_string(), tail));
    }
    let end = s
        .find(|c: char| c.is_whitespace() || ";|&<>()".contains(c))
        .unwrap_or(s.len());
    if end == 0 && !s.is_empty() {
        return None;
    }
    let word = &s[..end];
    if word.contains(UNDECIDABLE) && word != "[" && word != "{" {
        return None;
    }
    if word.starts_with('~') && !word.starts_with("~/") {
        return None;
    }
    Some((word.to_string(), &s[end..]))
}

fn can(path: &Path, access: Access) -> bool {
    accessat(CWD, path, access, AtFlags::EACCESS).is_ok()
}

/// Every directory on the way traversable, then the file itself.
fn check_path(path: &Path) -> Result<(), String> {
    let mut prefix = PathBuf::new();
    let mut parts = path.components().peekable();
    while let Some(part) = parts.next() {
        let parent = prefix.clone();
        prefix.push(part);
        if parts.peek().is_none() {
            break;
        }
        match std::fs::metadata(&prefix) {
            Ok(m) if !m.is_dir() => return Err(format!("not a directory: {}", prefix.display())),
            Ok(_) if !can(&prefix, Access::EXEC_OK) => {
                return Err(format!("not traversable: {}", prefix.display()))
            }
            Ok(_) => {}
            Err(e) => return Err(stat_fault(&e, &prefix, &parent)),
        }
    }
    check_file(path)
}

fn stat_fault(e: &std::io::Error, at: &Path, parent: &Path) -> String {
    match e.kind() {
        std::io::ErrorKind::NotFound => format!("not found: {}", at.display()),
        std::io::ErrorKind::PermissionDenied => format!("not traversable: {}", parent.display()),
        _ => format!("not reachable: {} ({e})", at.display()),
    }
}

fn check_file(path: &Path) -> Result<(), String> {
    let parent = path.parent().unwrap_or(Path::new("."));
    let meta = std::fs::metadata(path).map_err(|e| stat_fault(&e, path, parent))?;
    if !meta.is_file() {
        return Err(format!("not a file: {}", path.display()));
    }
    if !can(path, Access::READ_OK) {
        return Err(format!("not readable: {}", path.display()));
    }
    if !can(path, Access::EXEC_OK) {
        return Err(format!("not executable: {}", path.display()));
    }
    Ok(())
}

/// The first executable `name` on PATH; else a non-executable hit, if any.
fn on_path(name: &str, env: &CheckEnv) -> Result<PathBuf, Option<PathBuf>> {
    let default = OsString::from(DEFAULT_PATH);
    let path = env.path.as_ref().unwrap_or(&default);
    let mut blocked = None;
    for dir in std::env::split_paths(path) {
        let candidate = if dir.as_os_str().is_empty() {
            PathBuf::from(name)
        } else {
            dir.join(name)
        };
        if !candidate.is_file() {
            continue;
        }
        if can(&candidate, Access::EXEC_OK) {
            return Ok(candidate);
        }
        blocked.get_or_insert(candidate);
    }
    Err(blocked)
}

/// A script's `#!` interpreter exists and runs; `env NAME` is looked up.
fn check_shebang(file: &Path, env: &CheckEnv) -> Result<(), String> {
    let mut head = [0u8; 256];
    let n = std::fs::File::open(file)
        .and_then(|mut f| f.read(&mut head))
        .unwrap_or(0);
    let Some(line) = head[..n].strip_prefix(b"#!") else {
        return Ok(());
    };
    let line = line.split(|b| *b == b'\n').next().unwrap_or_default();
    let line = String::from_utf8_lossy(line);
    let mut words = line.split_whitespace();
    let Some(interpreter) = words.next() else {
        return Ok(());
    };
    let path = Path::new(interpreter);
    if std::fs::metadata(path).is_err() {
        return Err(format!("interpreter missing: {interpreter}"));
    }
    if check_file(path).is_err() {
        return Err(format!("interpreter not executable: {interpreter}"));
    }
    if path.file_name() != Some(OsStr::new("env")) {
        return Ok(());
    }
    let named = words.find(|w| !w.starts_with('-') && !w.contains('='));
    match named {
        Some(name) if !name.contains('/') && on_path(name, env).is_err() => {
            Err(format!("interpreter not on PATH: {name}"))
        }
        _ => Ok(()),
    }
}

#[cfg(test)]
mod tests {
    use std::os::unix::fs::PermissionsExt as _;

    use super::*;

    fn env(path: &Path) -> CheckEnv {
        CheckEnv {
            path: Some(path.as_os_str().to_os_string()),
            home: Some(path.as_os_str().to_os_string()),
        }
    }

    fn file(path: &Path, body: &str, mode: u32) {
        std::fs::write(path, body).expect("write");
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode)).expect("chmod");
    }

    fn mode(path: &Path, mode: u32) {
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode)).expect("chmod");
    }

    #[test]
    fn a_missing_program_names_where_it_is_missing() {
        let dir = tempfile::tempdir().expect("tmp");
        let e = env(dir.path());
        assert_eq!(check_command("no-such-wake arg", &e), Err("not found on PATH".to_string()));
        let gone = dir.path().join("sub/wake.sh");
        assert_eq!(
            check_command(&gone.display().to_string(), &e),
            Err(format!("not found: {}", dir.path().join("sub").display()))
        );
        let gone = dir.path().join("wake.sh");
        assert_eq!(
            check_command(&format!("{} --flag", gone.display()), &e),
            Err(format!("not found: {}", gone.display()))
        );
    }

    #[test]
    fn a_closed_directory_on_the_way_is_not_traversable() {
        let dir = tempfile::tempdir().expect("tmp");
        let closed = dir.path().join("closed");
        std::fs::create_dir(&closed).expect("dir");
        let script = closed.join("wake.sh");
        file(&script, "#!/bin/sh\n", 0o755);
        mode(&closed, 0o600);
        let got = check_command(&script.display().to_string(), &env(dir.path()));
        mode(&closed, 0o700);
        if can(&script, Access::EXEC_OK) && got.is_ok() {
            return; // root passes every directory
        }
        assert_eq!(got, Err(format!("not traversable: {}", closed.display())));
    }

    #[test]
    fn a_file_without_the_bits_is_not_readable_or_not_executable() {
        let dir = tempfile::tempdir().expect("tmp");
        let e = env(dir.path());
        let script = dir.path().join("wake.sh");
        file(&script, "#!/bin/sh\n", 0o644);
        let line = script.display().to_string();
        assert_eq!(check_command(&line, &e), Err(format!("not executable: {line}")));
        assert_eq!(
            check_command("wake.sh", &e),
            Err(format!("not executable: {line}")),
            "a PATH hit without the bit"
        );
        mode(&script, 0o300);
        if !can(&script, Access::READ_OK) {
            assert_eq!(check_command(&line, &e), Err(format!("not readable: {line}")));
        }
        assert_eq!(
            check_command(&dir.path().display().to_string(), &e),
            Err(format!("not a file: {}", dir.path().display()))
        );
    }

    #[test]
    fn a_script_needs_its_interpreter() {
        let dir = tempfile::tempdir().expect("tmp");
        let e = env(dir.path());
        let script = dir.path().join("wake.sh");
        let line = script.display().to_string();
        file(&script, "#!/no/such/interpreter -x\necho\n", 0o755);
        assert_eq!(check_command(&line, &e), Err("interpreter missing: /no/such/interpreter".to_string()));
        let plain = dir.path().join("plain");
        file(&plain, "", 0o644);
        file(&script, &format!("#!{}\n", plain.display()), 0o755);
        assert_eq!(
            check_command(&line, &e),
            Err(format!("interpreter not executable: {}", plain.display()))
        );
        file(&script, "#!/usr/bin/env no-such-shell\n", 0o755);
        assert_eq!(check_command(&line, &e), Err("interpreter not on PATH: no-such-shell".to_string()));
    }

    #[test]
    fn a_runnable_command_passes() {
        let dir = tempfile::tempdir().expect("tmp");
        let e = env(dir.path());
        let script = dir.path().join("wake.sh");
        file(&script, "#!/bin/sh\necho ok\n", 0o755);
        let line = script.display().to_string();
        for cmd in [
            line.clone(),
            format!("  {line} --once"),
            format!("'{line}' a b"),
            format!("FOO=1 BAR=2 {line}"),
            "wake.sh".to_string(),
            "~/wake.sh".to_string(),
        ] {
            assert_eq!(check_command(&cmd, &e), Ok(()), "{cmd}");
        }
        let with_env = dir.path().join("env.sh");
        file(&with_env, "#!/usr/bin/env -S wake.sh\n", 0o755);
        assert_eq!(check_command(&with_env.display().to_string(), &e), Ok(()));
        let system = CheckEnv { path: None, home: None };
        assert_eq!(check_command("sh -c true", &system), Ok(()), "the default PATH");
    }

    #[test]
    fn only_a_plain_program_line_is_exec_ed() {
        for cmd in ["/srv/wake.sh", " ~/wake.sh --once \"$MOLT_WAKE_REASON\"", "'/srv/my wake.sh' a", "claude -p hi"] {
            assert!(runs_one_program(cmd), "{cmd}");
        }
        for cmd in [
            "exit 3",
            "true",
            "FOO=1 /srv/wake.sh",
            "cd /srv && ./wake.sh",
            "/srv/wake.sh > /tmp/log",
            "$HOME/wake.sh",
            "/srv/wake.sh; echo done",
            "",
        ] {
            assert!(!runs_one_program(cmd), "{cmd}");
        }
    }

    #[test]
    fn what_only_the_shell_can_resolve_is_not_refused() {
        let dir = tempfile::tempdir().expect("tmp");
        let e = env(dir.path());
        for cmd in [
            "exit 3",
            "cd /srv && ./wake.sh",
            "if true; then no-such-wake; fi",
            "{ no-such-wake; }",
            "while :; do sleep 1; done",
            "echo \"$MOLT_WAKE_REASON\" >> /tmp/x",
            "$HOME/bin/wake.sh",
            "\"$HOME/bin/wake.sh\"",
            "~other/wake.sh",
            "PATH=/opt/bin no-such-wake",
            "FOO=1",
            "(cd /srv; ./wake.sh)",
            "`which wake`",
        ] {
            assert_eq!(check_command(cmd, &e), Ok(()), "{cmd}");
        }
        assert_eq!(check_command("   ", &e), Err("no command".to_string()));
    }
}
