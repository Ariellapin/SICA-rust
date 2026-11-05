//! How a command line reaches the host shell, and what to tell the model
//! when it fails.
//!
//! One place for `run-cli`, `run-pwsh`, their background jobs and the
//! backend's user hooks, because each of them got the Windows quoting wrong
//! the same way: `Command::new("cmd").args(["/C", line])` lets Rust quote
//! `line` for the MSVC argument rules, which turns every inner `"` into
//! `\"` — and `cmd.exe` does not read `\"`. Six idealist tickets were that
//! one bug: `type "C:\a b\x.md"` arrived as `type \"C:\a b\x.md\"` ("the
//! filename, directory name, or volume label syntax is incorrect"), and
//! `findstr /C:"two words"` looked for `"two` in a file named `words"`.

use std::path::{Path, PathBuf};

use tokio::process::Command;

/// Which shell ran a failed command, for [`failure_hint`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Shell {
    /// `cmd.exe` on Windows, `/bin/sh` elsewhere.
    Cmd,
    PowerShell,
}

/// The host shell running `command` as one command line: `cmd.exe` on
/// Windows, `/bin/sh -c` elsewhere.
///
/// On Windows the line is handed to cmd **raw**, as `/S /C "<command>"`:
/// with `/S`, cmd strips exactly the outer pair of quotes and runs the rest
/// as typed, so the quotes inside it mean what they would at a prompt.
pub fn shell_command(command: &str) -> Command {
    #[cfg(windows)]
    {
        let mut c = Command::new("cmd");
        c.raw_arg("/S /C").raw_arg(format!("\"{command}\""));
        c
    }
    #[cfg(not(windows))]
    {
        let mut c = Command::new("/bin/sh");
        c.args(["-c", command]);
        c
    }
}

/// PowerShell running `command`, and the executable's name for messages.
/// On Windows the system `powershell.exe`; everywhere else, and on a
/// Windows without it, `pwsh` (PowerShell Core).
///
/// Unlike cmd, `powershell.exe` splits its command line by the MSVC rules,
/// so the quoting `Command::arg` applies is the quoting it reads back.
pub fn pwsh_command(command: &str) -> (Command, &'static str) {
    let exe = if cfg!(windows) && which_in_path("powershell.exe").is_some() {
        "powershell"
    } else {
        "pwsh"
    };
    let mut c = Command::new(exe);
    c.args(["-NoLogo", "-NoProfile", "-NonInteractive", "-Command", command]);
    (c, exe)
}

/// `name` in a directory on `PATH`, as given (no `PATHEXT`).
pub fn which_in_path(name: &str) -> Option<PathBuf> {
    let path = std::env::var_os("PATH")?;
    std::env::split_paths(&path).map(|d| d.join(name)).find(|c| c.is_file())
}

/// Verbs PowerShell cmdlets start with: the approved list's common ones,
/// plus the pipeline cmdlets whose verbs are not on it (`ForEach-Object`,
/// `Where-Object`, `Sort-Object`, `Tee-Object`).
const CMDLET_VERBS: &[&str] = &[
    "add", "clear", "close", "compare", "compress", "convert", "convertfrom", "convertto",
    "copy", "debug", "disable", "enable", "enter", "exit", "expand", "export", "find",
    "foreach", "format", "get", "group", "import", "invoke", "join", "measure", "move",
    "new", "out", "pop", "push", "read", "receive", "remove", "rename", "resolve",
    "restart", "resume", "select", "send", "set", "sort", "split", "start", "stop",
    "suspend", "tee", "test", "update", "wait", "where", "write",
];

/// The cmdlet a command line opens with (`Get-ChildItem` in
/// `(Get-ChildItem *.txt).Count`), when it opens with one.
fn leading_cmdlet(command: &str) -> Option<&str> {
    let s = command.trim_start().trim_start_matches(['(', '@', '&', ' ']);
    let end = s
        .find(|c: char| c.is_whitespace() || "()|;,".contains(c))
        .unwrap_or(s.len());
    let token = &s[..end];
    let (verb, noun) = token.split_once('-')?;
    let noun_ok = noun.chars().next().is_some_and(|c| c.is_ascii_alphabetic())
        && noun.chars().all(|c| c.is_ascii_alphanumeric());
    (noun_ok && CMDLET_VERBS.contains(&verb.to_ascii_lowercase().as_str())).then_some(token)
}

/// A command line that is PowerShell rather than cmd: it opens with a
/// cmdlet (`Get-ChildItem …`) or assigns a variable (`$n = …`). cmd.exe has
/// neither, so `run-cli` hands such a line to PowerShell instead of failing
/// with "is not recognized" and costing the model a turn (`sessions/103`).
/// Returns what gave it away, for the note on the result.
pub fn powershell_marker(command: &str) -> Option<String> {
    let s = command.trim_start();
    if s.starts_with('$')
        && s[1..].starts_with(|c: char| c.is_ascii_alphabetic() || c == '_' || c == '{')
    {
        return Some("it assigns a PowerShell variable".into());
    }
    leading_cmdlet(s).map(|c| format!("`{c}` is a PowerShell cmdlet"))
}

/// Why `run-cli` should run `command` in PowerShell rather than cmd.exe,
/// when it should: on Windows, a [`powershell_marker`] line — unless its
/// cmdlet is a program cmd would find. A PowerShell line sent to cmd can
/// only fail ("is not recognized"), so it runs where it can, and the
/// result carries this reason as a note.
pub fn powershell_route(command: &str, cwd: &Path) -> Option<String> {
    if !cfg!(windows) {
        return None;
    }
    let why = powershell_marker(command)?;
    if leading_cmdlet(command).is_some_and(|c| cmd_resolves(c, cwd)) {
        return None;
    }
    Some(why)
}

/// Whether cmd.exe would find `name` as a program: in `cwd` or on `PATH`,
/// as given or with a `PATHEXT` extension. A `Verb-Noun` batch file of the
/// user's own wins over the PowerShell reading.
fn cmd_resolves(name: &str, cwd: &Path) -> bool {
    let exts = std::env::var("PATHEXT").unwrap_or_else(|_| ".COM;.EXE;.BAT;.CMD".into());
    let path = std::env::var_os("PATH").unwrap_or_default();
    std::iter::once(cwd.to_path_buf())
        .chain(std::env::split_paths(&path))
        .any(|dir| {
            dir.join(name).is_file()
                || exts
                    .split(';')
                    .filter(|e| !e.is_empty())
                    .any(|e| dir.join(format!("{name}{e}")).is_file())
        })
}

/// Whether `s` has a `;` outside quotes — a POSIX command separator that
/// cmd.exe passes to the program as an argument instead.
fn has_bare_semicolon(s: &str) -> bool {
    let mut quote: Option<char> = None;
    for c in s.chars() {
        match (quote, c) {
            (None, '"' | '\'') => quote = Some(c),
            (Some(q), c) if c == q => quote = None,
            (None, ';') => return true,
            _ => {}
        }
    }
    false
}

/// A doubled quote opening a token (`-split ''@@''`): the model escaped
/// the quote for the outer quoting, which the one-line call does not need —
/// the command reaches the shell exactly as written. An empty string
/// (`-ne ''`) is not one, and neither is PowerShell's own escape inside a
/// string (`'it''s'`), which does not open a token.
fn has_doubled_quote(s: &str) -> bool {
    let b = s.as_bytes();
    (0..b.len().saturating_sub(2)).any(|i| {
        let pair = (b[i] == b'\'' && b[i + 1] == b'\'') || (b[i] == b'"' && b[i + 1] == b'"');
        let opens = i == 0 || b[i - 1].is_ascii_whitespace() || b"(=,{[".contains(&b[i - 1]);
        pair && opens && !b[i + 2].is_ascii_whitespace() && !b"),;}]".contains(&b[i + 2])
    })
}

/// One line telling the model how to recover from a failure whose cause is
/// recognisable from the command and its stderr; `None` otherwise. Every
/// pattern here was a real idealist ticket the model then spent a turn on.
pub fn failure_hint(shell: Shell, command: &str, stderr: &str) -> Option<&'static str> {
    let lower = stderr.to_ascii_lowercase();
    if lower.contains("collection was modified; enumeration operation may not execute") {
        return Some(
            "the loop changes the collection it is enumerating — enumerate a copy, \
             e.g. `foreach ($k in @($table.Keys))`.",
        );
    }
    match shell {
        Shell::PowerShell => {
            if stderr.contains("InvalidVariableReferenceWithDrive") {
                return Some(
                    "in a double-quoted string `$name:` reads as a drive-qualified variable \
                     (like `$env:PATH`) — write `${name}:` or `$($name):`.",
                );
            }
            // `[int]"abc"` fails with these same ids, so the command has to
            // hold a `+` for this to be about `+`. A loop's `$i++` is not one.
            let string_to_number = stderr.contains("InvalidCastFromStringToInteger")
                || stderr.contains("InvalidCastFromStringToDoubleOrSingle");
            if string_to_number && command.replace("++", "").contains('+') {
                return Some(
                    "`+` takes its type from the value on its left, so a number there tries \
                     to read the string on its right as a number — write the number inside \
                     the string instead, e.g. `\"$($i+1): text\"`.",
                );
            }
            let parse_error = stderr.contains("ParserError");
            if parse_error && (command.contains("\\\"") || has_doubled_quote(command)) {
                return Some(
                    "the command reaches PowerShell exactly as written between the outer \
                     quotes — do not double (`''`) or backslash-escape (`\\\"`) the quotes \
                     inside it.",
                );
            }
            if stderr.contains("TerminatorExpectedAtEndOfString")
                && command.to_ascii_lowercase().contains(".ps1")
            {
                return Some(
                    "Windows PowerShell 5.1 reads a script without a byte-order mark as ANSI, \
                     so a non-ASCII character (an em dash, a curly quote) can end a string \
                     early — rewrite the script with write-file, which saves `.ps1` files \
                     with the mark, or keep it ASCII.",
                );
            }
            None
        }
        Shell::Cmd => {
            if !cfg!(windows) {
                return None;
            }
            if lower.contains("is not recognized as an internal or external command") {
                return Some(
                    "cmd.exe could not find that program — if it is a PowerShell cmdlet or \
                     alias (`ls`, `cat`, `rm`), call run-pwsh instead.",
                );
            }
            if has_bare_semicolon(command) {
                return Some(
                    "cmd.exe does not separate commands with `;` — join them with `&` (or \
                     `&&` to stop at the first failure), or run the line with run-pwsh.",
                );
            }
            None
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cmdlet_lines_read_as_powershell() {
        assert!(powershell_marker("Get-ChildItem poses_*.txt | Select-Object Name").is_some());
        assert!(powershell_marker("(Get-Content wk.txt).Count").is_some());
        assert!(powershell_marker("  where-object { $_ }").is_some());
        assert!(powershell_marker("$n = 3; $n * 2").is_some());
        assert!(powershell_marker("${x} = 1").is_some());
        assert_eq!(leading_cmdlet("@(Get-Item a).Length"), Some("Get-Item"));
    }

    #[test]
    fn cmd_lines_stay_cmd() {
        for line in [
            "git status",
            "dir /b *.txt",
            "cargo-fmt --all",
            "type \"C:\\a b\\x.md\"",
            "echo $HOME",
            "start-server.bat",
            "Get-",
            "Get-1x",
            "%PROGRAMFILES%\\x.exe",
        ] {
            assert_eq!(powershell_marker(line), None, "{line}");
        }
    }

    #[test]
    fn cmd_resolves_finds_a_batch_file_by_pathext() {
        let dir = std::env::temp_dir().join(format!("sica-shell-resolve-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("Start-Server.cmd"), "@echo off").unwrap();
        assert!(cmd_resolves("Start-Server", &dir));
        assert!(!cmd_resolves("Get-NoSuchThingAnywhere", &dir));
        // So a user's own `Verb-Noun` batch file stays a cmd line.
        assert_eq!(powershell_route("Start-Server --port 1", &dir), None);
        if cfg!(windows) {
            assert!(powershell_route("Get-NoSuchThingAnywhere x", &dir).is_some());
        } else {
            assert_eq!(powershell_route("Get-ChildItem", &dir), None, "cmd.exe only");
        }
    }

    #[test]
    fn hints_name_the_fix_for_each_ticketed_failure() {
        let ps = Shell::PowerShell;
        // 48f29b2a614d
        assert!(failure_hint(ps, "\"L$i: x\"", "+ FullyQualifiedErrorId : InvalidVariableReferenceWithDrive")
            .unwrap()
            .contains("${name}:"));
        // f39e36c14bb6
        assert!(failure_hint(ps, "& .\\x.ps1", "Collection was modified; enumeration operation may not execute.")
            .unwrap()
            .contains("@($table.Keys)"));
        // 6d8744a29a92
        assert!(failure_hint(ps, "$p=$ln -split ''@@'',2", "ParserError: (:) [] UnrecognizedToken")
            .unwrap()
            .contains("exactly as written"));
        // 7706070d9498
        assert!(failure_hint(
            ps,
            "powershell -File build_prompts.ps1",
            "+ FullyQualifiedErrorId : TerminatorExpectedAtEndOfString"
        )
        .unwrap()
        .contains("byte-order mark"));
        // 2abbd0a970d2
        assert!(failure_hint(
            ps,
            "for($i=34;$i -lt 45;$i++){ Write-Output (($i+1) + \": \" + $c[$i]) }",
            "+ FullyQualifiedErrorId : InvalidCastFromStringToInteger"
        )
        .unwrap()
        .contains("$($i+1)"));
        assert!(failure_hint(ps, "$x = 1.5; $x + \"a\"", "InvalidCastFromStringToDoubleOrSingle").is_some());
        // The same id from a cast has no `+` to blame, and `$i++` is not one.
        assert_eq!(failure_hint(ps, "[int]\"abc\"", "InvalidCastFromStringToInteger"), None);
        assert_eq!(
            failure_hint(ps, "for($i=0;$i -lt 2;$i++){ [int]\"abc\" }", "InvalidCastFromStringToInteger"),
            None
        );
        // An empty string, and PowerShell's own escape, are not doubled quotes.
        assert_eq!(failure_hint(ps, "$_.Trim() -ne ''", "ParserError"), None);
        assert_eq!(failure_hint(ps, "Write-Output 'it''s'", "ParserError"), None);
    }

    #[cfg(windows)]
    #[test]
    fn cmd_hints() {
        // d0d7dd031b68 (an alias, which routing does not catch)
        assert!(failure_hint(Shell::Cmd, "ls", "'ls' is not recognized as an internal or external command,")
            .unwrap()
            .contains("run-pwsh"));
        // 08cee6f916ee
        assert!(failure_hint(Shell::Cmd, "grep -c a f.txt; grep -c b f.txt", "grep: f.txt;: The system cannot find the file")
            .unwrap()
            .contains("`&`"));
        assert_eq!(failure_hint(Shell::Cmd, "echo \"a;b\"", "x"), None);
    }

    /// The bug behind six tickets: quotes inside a `run-cli` line must reach
    /// cmd.exe as typed.
    #[cfg(windows)]
    #[tokio::test]
    async fn cmd_gets_the_line_as_typed() {
        let dir = std::env::temp_dir().join(format!("sica shell quoting {}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join("two words.txt");
        std::fs::write(&file, "alpha\r\npeach fuzz here\r\nbeta\r\n").unwrap();

        let run = |line: String| async move {
            let out = shell_command(&line).output().await.unwrap();
            (out.status.code(), String::from_utf8_lossy(&out.stdout).into_owned())
        };
        // 7514f52b417c: a quoted path with spaces.
        let (code, out) = run(format!("type \"{}\"", file.display())).await;
        assert_eq!(code, Some(0), "{out}");
        assert!(out.contains("peach fuzz here"));
        // 708173c0efeb: a quoted search string with a space in it.
        let (code, out) = run(format!("findstr /C:\"peach fuzz\" \"{}\"", file.display())).await;
        assert_eq!(code, Some(0), "{out}");
        assert_eq!(out.trim(), "peach fuzz here");
        // 95a820313c39: a nested `powershell -Command "…"` with escaped quotes.
        let (code, out) = run(format!(
            "powershell -NoProfile -Command \"(Get-Content -Path \\\"{}\\\" | Measure-Object -Line).Lines\"",
            file.display()
        ))
        .await;
        assert_eq!(code, Some(0), "{out}");
        assert_eq!(out.trim(), "3");
    }
}
