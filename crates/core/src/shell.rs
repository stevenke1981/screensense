//! Opt-in shell integration: after every interactive command the shell runs
//! `screensense record --shell <name> --exit <code> --ms <ms> --cwd <dir> -- <command line>`
//! in the background, silently. `screensense shell-init <shell>` prints the snippet;
//! the user evals / sources it from the shell's rc file (see [`rc_line`]).
//!
//! Common contract of the four scripts:
//!
//! * the exit status the user (or their prompt, or a later hook) sees is never changed;
//! * nothing is printed and no job-control line ("[1] 1234") appears;
//! * an empty Enter records nothing; a command line that starts with a space (the
//!   "do not record this" convention of `HISTCONTROL=ignorespace`) and `screensense ...`
//!   itself are skipped; command lines are cut at 4,000 characters;
//! * the command line and directory are passed as single arguments by the shell's own
//!   argument passing (`"$cmd"`), never through a string that is parsed again — except
//!   PowerShell 5.1, whose only way to start a process without waiting needs one
//!   command-line string, which the script quotes with the CommandLineToArgvW rules;
//! * loading the snippet twice is harmless, and existing `PROMPT_COMMAND`, DEBUG trap,
//!   `precmd_functions`, fish event handlers and the PowerShell `prompt` function keep running.
//!
//! Known limits, per shell, are documented in the script comments.

use std::path::Path;

use crate::integrations::forward_slashes;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Shell {
    Bash,
    Zsh,
    Fish,
    PowerShell,
}

impl Shell {
    pub const ALL: [Shell; 4] = [Shell::Bash, Shell::Zsh, Shell::Fish, Shell::PowerShell];

    /// `bash`, `zsh`, `fish`, `powershell` / `pwsh` / `ps` (any case).
    pub fn parse(s: &str) -> Option<Shell> {
        match s.trim().to_ascii_lowercase().as_str() {
            "bash" => Some(Shell::Bash),
            "zsh" => Some(Shell::Zsh),
            "fish" => Some(Shell::Fish),
            "powershell" | "pwsh" | "ps" => Some(Shell::PowerShell),
            _ => None,
        }
    }

    pub fn name(self) -> &'static str {
        match self {
            Shell::Bash => "bash",
            Shell::Zsh => "zsh",
            Shell::Fish => "fish",
            Shell::PowerShell => "powershell",
        }
    }
}

/// The one-line instruction for the user's rc file.
pub fn rc_line(shell: Shell) -> &'static str {
    match shell {
        Shell::Bash => r#"eval "$(screensense shell-init bash)""#,
        Shell::Zsh => r#"eval "$(screensense shell-init zsh)""#,
        Shell::Fish => "screensense shell-init fish | source",
        Shell::PowerShell => "Invoke-Expression (& screensense shell-init powershell | Out-String)",
    }
}

/// The snippet `screensense shell-init <shell>` prints, to be eval'ed / sourced from the
/// shell's rc file. `exe` is embedded as an absolute path, quoted for that shell.
pub fn init_script(shell: Shell, exe: &Path) -> String {
    let p = forward_slashes(exe);
    let (template, quoted) = match shell {
        Shell::Bash => (BASH, quote_posix(&p)),
        Shell::Zsh => (ZSH, quote_posix(&p)),
        Shell::Fish => (FISH, quote_fish(&p)),
        Shell::PowerShell => (POWERSHELL, quote_powershell(&p)),
    };
    template.replace("@@EXE@@", &quoted)
}

/// Single-quoted for bash / zsh: `'` becomes `'\''`.
fn quote_posix(s: &str) -> String {
    format!("'{}'", s.replace('\'', r"'\''"))
}

/// Single-quoted for fish: inside quotes only `\\` and `\'` are escapes.
fn quote_fish(s: &str) -> String {
    format!("'{}'", s.replace('\\', r"\\").replace('\'', r"\'"))
}

/// Single-quoted for PowerShell: a quote is doubled. PowerShell also treats the
/// typographic single quotes as quote characters, so those are doubled too.
fn quote_powershell(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    out.push('\'');
    for c in s.chars() {
        out.push(c);
        if matches!(c, '\'' | '\u{2018}' | '\u{2019}' | '\u{201A}' | '\u{201B}') {
            out.push(c);
        }
    }
    out.push('\'');
    out
}

// ---------------------------------------------------------------------------
// bash
// ---------------------------------------------------------------------------
//
// How it works: a DEBUG trap notes the start of the first simple command after a prompt
// (time, directory). A function at the FRONT of PROMPT_COMMAND reads `$?` before anything
// else can change it, reports, and returns that same status; a function at the END of
// PROMPT_COMMAND re-arms the trap, so commands that PROMPT_COMMAND itself runs are not
// mistaken for the user's. The command line comes from `history 1` (the whole line;
// `$BASH_COMMAND` would only be the first simple command of a pipeline), with
// HISTTIMEFORMAT emptied for that call. "A new command was typed" is decided by the
// history number, so lines that bash did not add to history are not reported.
const BASH: &str = r####"# ScreenSense shell integration for bash.
# Add to ~/.bashrc:   eval "$(screensense shell-init bash)"
# Reports every interactive command (line, exit status, duration, directory) to
# `screensense record`, in the background and silently. Lines that start with a space and
# `screensense ...` commands are skipped. Limits: needs history on (a command bash does not
# put in history is not reported, so with HISTCONTROL=ignoredups an immediate repeat of the
# same line is not reported either); another tool that later puts its own command in FRONT
# of PROMPT_COMMAND can disturb the exit status we read.
if [[ $- == *i* && -z ${__ss_on-} ]]; then
  __ss_on=1
  __ss_exe=@@EXE@@
  __ss_armed=
  __ss_ran=
  __ss_hist=
  __ss_cwd=
  __ss_t0=
  __ss_s0=0
  __ss_chain=

  __ss_ret() { return "$1"; }

  # DEBUG trap: runs before every simple command; only the first one after a prompt counts.
  __ss_debug() {
    local __ss_rc=$?
    if [[ -n $__ss_armed && -z ${COMP_POINT-} && ${BASH_SUBSHELL:-0} -eq 0 ]]; then
      __ss_armed=
      __ss_ran=1
      __ss_cwd=$PWD
      __ss_t0=${EPOCHREALTIME-}
      __ss_s0=$SECONDS
    fi
    if [[ -n $__ss_chain ]]; then
      __ss_ret "$__ss_rc"
      eval "$__ss_chain"
      __ss_rc=$?
    else
      __ss_rc=0
    fi
    : "$1"
    return "$__ss_rc"
  }

  # $1 = exit status of the command that just ran.
  __ss_report() {
    local line n text first t1 ms=0 re='^[[:space:]]*([0-9]+)[ *] (.*)$'
    line=$(HISTTIMEFORMAT= builtin history 1) || return 0
    [[ $line =~ $re ]] || return 0
    n=${BASH_REMATCH[1]}
    text=${BASH_REMATCH[2]}
    if [[ $n == "$__ss_hist" ]]; then return 0; fi
    __ss_hist=$n
    case $text in [[:space:]]*) return 0 ;; esac
    first=${text%%[[:space:]]*}
    first=${first//[\"\']/}
    first=${first##*[/\\]}
    case $first in screensense | screensense.exe) return 0 ;; esac
    if [[ -n $__ss_t0 && -n ${EPOCHREALTIME-} ]]; then
      t1=${EPOCHREALTIME//[.,]/}
      ms=$(( (10#$t1 - 10#${__ss_t0//[.,]/}) / 1000 ))
    else
      ms=$(( (SECONDS - __ss_s0) * 1000 ))
    fi
    if (( ms < 0 )); then ms=0; fi
    ( "$__ss_exe" record --shell bash --exit "$1" --ms "$ms" --cwd "$__ss_cwd" -- "${text:0:4000}" </dev/null >/dev/null 2>&1 & )
    return 0
  }

  # First in PROMPT_COMMAND: read $? before anything else can change it, give it back.
  __ss_precmd() {
    local __ss_s=$?
    if [[ -n $__ss_ran ]]; then
      __ss_ran=
      __ss_report "$__ss_s"
    fi
    return "$__ss_s"
  }

  # Last in PROMPT_COMMAND: from here on, the next DEBUG trap is a user command.
  __ss_arm() { __ss_armed=1; }

  __ss_chain=$(trap -p DEBUG)
  if [[ -n $__ss_chain ]]; then
    eval "__ss_tr=($__ss_chain)"
    __ss_chain=${__ss_tr[2]-}
    unset __ss_tr
  fi
  trap '__ss_debug "$_"' DEBUG

  if (( BASH_VERSINFO[0] > 5 || (BASH_VERSINFO[0] == 5 && BASH_VERSINFO[1] >= 1) )) && [[ $(declare -p PROMPT_COMMAND 2>/dev/null) == "declare -a"* ]]; then
    PROMPT_COMMAND=(__ss_precmd "${PROMPT_COMMAND[@]}" __ss_arm)
  else
    PROMPT_COMMAND="__ss_precmd"$'\n'"${PROMPT_COMMAND:+$PROMPT_COMMAND$'\n'}__ss_arm"
  fi
fi
"####;

// ---------------------------------------------------------------------------
// zsh
// ---------------------------------------------------------------------------
//
// preexec / precmd through add-zsh-hook. zsh runs every hook with `$?` still equal to
// the last command's status and restores it afterwards, so ours is safe in any position.
const ZSH: &str = r####"# ScreenSense shell integration for zsh.
# Add to ~/.zshrc:   eval "$(screensense shell-init zsh)"
# Reports every interactive command (line, exit status, duration, directory) to
# `screensense record`, in the background and silently. Lines that start with a space and
# `screensense ...` commands are skipped.
if [[ -o interactive && -z ${__ss_on-} ]]; then
  typeset -g __ss_on=1 __ss_exe=@@EXE@@ __ss_ran= __ss_cmd= __ss_cwd=
  typeset -gF __ss_t0=0
  typeset -gi __ss_s0=0
  zmodload zsh/datetime 2>/dev/null
  autoload -Uz add-zsh-hook

  __ss_preexec() {
    emulate -L zsh
    __ss_ran=1
    __ss_cmd=$1
    __ss_cwd=$PWD
    __ss_s0=$SECONDS
    (( __ss_t0 = ${EPOCHREALTIME:-0} ))
  }

  __ss_precmd() {
    local __ss_s=$?
    emulate -L zsh
    if [[ -n $__ss_ran ]]; then
      __ss_ran=
      local cmd=$__ss_cmd first
      local -i ms=0
      if [[ $cmd != [[:space:]]* ]]; then
        first=${cmd%%[[:space:]]*}
        first=${first//[\"\']/}
        first=${first##*[/\\]}
        if [[ $first != (screensense|screensense.exe) ]]; then
          if (( __ss_t0 > 0 )) && [[ -n ${EPOCHREALTIME-} ]]; then
            (( ms = (EPOCHREALTIME - __ss_t0) * 1000 ))
          else
            (( ms = (SECONDS - __ss_s0) * 1000 ))
          fi
          if (( ms < 0 )); then ms=0; fi
          "$__ss_exe" record --shell zsh --exit "$__ss_s" --ms "$ms" --cwd "$__ss_cwd" -- "${cmd:0:4000}" </dev/null >/dev/null 2>&1 &!
        fi
      fi
    fi
    return $__ss_s
  }

  add-zsh-hook preexec __ss_preexec
  add-zsh-hook precmd __ss_precmd
fi
"####;

// ---------------------------------------------------------------------------
// fish
// ---------------------------------------------------------------------------
//
// fish_preexec remembers the directory; fish_postexec (which receives the command line and
// runs with $status still set) reports. The handler ends with `return $code`, so the status
// the user sees afterwards is the command's, whether or not fish restores it itself.
const FISH: &str = r####"# ScreenSense shell integration for fish.
# Add to ~/.config/fish/config.fish:   screensense shell-init fish | source
# Reports every interactive command (line, exit status, duration, directory) to
# `screensense record`, in the background and silently. Lines that start with a space and
# `screensense ...` commands are skipped.
if status is-interactive; and not set -q __ss_on
    set -g __ss_on 1
    set -g __ss_exe @@EXE@@
    set -g __ss_cwd $PWD
    set -g __ss_ran ''

    function __ss_report
        set -l code $argv[1]
        set -l cmd (string sub -l 4000 -- $argv[2] | string collect)
        test -n "$cmd"; or return 0
        string match -qr '^\s' -- $cmd; and return 0
        set -l first (string match -r '^\S+' -- $cmd)
        set first (string replace -ar '[\'"]' '' -- $first)
        set first (string replace -r '^.*[/\\\\]' '' -- $first)
        contains -- $first screensense screensense.exe; and return 0
        set -l ms 0
        set -q CMD_DURATION; and set ms $CMD_DURATION
        command $__ss_exe record --shell fish --exit $code --ms $ms --cwd $__ss_cwd -- $cmd </dev/null >/dev/null 2>&1 &
        disown $last_pid 2>/dev/null
        return 0
    end

    function __ss_preexec --on-event fish_preexec
        set -g __ss_ran 1
        set -g __ss_cwd $PWD
    end

    function __ss_postexec --on-event fish_postexec
        set -l code $status
        if test -n "$__ss_ran"
            set -g __ss_ran ''
            __ss_report $code $argv[1]
        end
        return $code
    end
end
"####;

// ---------------------------------------------------------------------------
// PowerShell
// ---------------------------------------------------------------------------
//
// NOT run by the tests (no PowerShell on the build machine); written for 5.1 and 7+.
// The prompt function is wrapped: the original is kept and called last, its output returned.
// `$?` and `$LASTEXITCODE` are read first. PowerShell offers no way to set `$?`, so the
// wrapped prompt sees `$?` of our own last statement; `$LASTEXITCODE` is restored.
// Exit code: `$?` true -> 0. Otherwise a new entry at the top of `$Error` means a
// PowerShell-level failure -> 1; no new error and a non-zero `$LASTEXITCODE` means a native
// command failed -> that code; else 1. The process is started with
// [Diagnostics.Process]::Start (a few ms); Start-Process costs about 100 ms on 5.1.
const POWERSHELL: &str = r####"# ScreenSense shell integration for PowerShell 5.1 and 7+.
# Add to your $PROFILE:   Invoke-Expression (& screensense shell-init powershell | Out-String)
# Reports every interactive command (line, exit code, duration, directory) to
# `screensense record`, hidden and without waiting. Lines that start with a space and
# `screensense ...` commands are skipped. Wraps the `prompt` function (the original keeps
# running). Limits: the wrapped prompt cannot see the real `$?`; a native command that fails
# while its stderr is merged into the error stream (2>&1) is reported as exit 1.
if (-not (Get-Variable -Name __ScreenSenseOn -Scope Global -ErrorAction SilentlyContinue)) {
    $global:__ScreenSenseOn = $true
    $global:__ScreenSenseExe = @@EXE@@
    $global:__ScreenSenseLastId = 0
    $global:__ScreenSenseErr = $null
    $global:__ScreenSenseCwd = ''
    $global:__ScreenSensePrompt = $null
    try { $global:__ScreenSenseCwd = $PWD.ProviderPath } catch { }
    try { $global:__ScreenSenseLastId = [int](Get-History -Count 1).Id } catch { }
    try { $global:__ScreenSensePrompt = (Get-Command prompt -CommandType Function -ErrorAction Stop).ScriptBlock } catch { }

    # One argument quoted for CommandLineToArgvW (backslashes before a quote are doubled).
    function global:__ScreenSenseArg([string]$s) {
        '"' + (($s -replace '(\\*)"', '$1$1\"') -replace '(\\+)$', '$1$1') + '"'
    }

    function global:__ScreenSenseReport($h, $code) {
        $cmd = [string]$h.CommandLine
        if ($cmd -match '^\s') { return }
        $first = (($cmd -replace '^&\s*', '') -split '\s+', 2)[0]
        $first = $first.Trim([char[]]@(34, 39)) -replace '^.*[\\/]', ''
        if ($first -match '^screensense(\.exe)?$') { return }
        if ($cmd.Length -gt 4000) { $cmd = $cmd.Substring(0, 4000) }
        $ms = 0
        try { $ms = [int64](($h.EndExecutionTime - $h.StartExecutionTime).TotalMilliseconds) } catch { }
        $psi = New-Object System.Diagnostics.ProcessStartInfo
        $psi.FileName = $global:__ScreenSenseExe
        $psi.Arguments = 'record --shell powershell --exit ' + $code + ' --ms ' + $ms + ' --cwd ' + (__ScreenSenseArg $global:__ScreenSenseCwd) + ' -- ' + (__ScreenSenseArg $cmd)
        $psi.UseShellExecute = $false
        $psi.CreateNoWindow = $true
        $p = [System.Diagnostics.Process]::Start($psi)
        if ($p) { $p.Dispose() }
    }

    function global:prompt {
        $ssOk = $?
        $ssLec = $null
        try { $ssLec = $global:LASTEXITCODE } catch { }
        try {
            $h = Get-History -Count 1 -ErrorAction SilentlyContinue
            if ($h -and ($h.Id -ne $global:__ScreenSenseLastId)) {
                $global:__ScreenSenseLastId = $h.Id
                $code = 0
                if (-not $ssOk) {
                    $e0 = $null
                    if ($global:Error.Count -gt 0) { $e0 = $global:Error[0] }
                    if (($null -ne $e0) -and (-not [object]::ReferenceEquals($e0, $global:__ScreenSenseErr))) { $code = 1 }
                    elseif (($ssLec -is [int]) -and ($ssLec -ne 0)) { $code = $ssLec }
                    else { $code = 1 }
                }
                __ScreenSenseReport $h $code
            }
        } catch { }
        $ssOut = $null
        if ($global:__ScreenSensePrompt) {
            $ssOut = & $global:__ScreenSensePrompt
        } else {
            $ssOut = 'PS ' + $PWD.Path + '> '
        }
        try {
            $global:__ScreenSenseCwd = $PWD.ProviderPath
            if ($global:Error.Count -gt 0) { $global:__ScreenSenseErr = $global:Error[0] } else { $global:__ScreenSenseErr = $null }
        } catch { }
        $global:LASTEXITCODE = $ssLec
        $ssOut
    }
}
"####;

#[cfg(test)]
mod tests {
    use super::*;

    const TRICKY: &str = "/tmp/it's here/screensense";

    #[test]
    fn shell_names_parse() {
        assert_eq!(Shell::parse("bash"), Some(Shell::Bash));
        assert_eq!(Shell::parse("ZSH"), Some(Shell::Zsh));
        assert_eq!(Shell::parse("fish"), Some(Shell::Fish));
        for s in ["powershell", "pwsh", "ps", "PowerShell", " pwsh "] {
            assert_eq!(Shell::parse(s), Some(Shell::PowerShell), "{s}");
        }
        assert_eq!(Shell::parse("cmd"), None);
        assert_eq!(Shell::parse(""), None);
        for s in Shell::ALL {
            assert_eq!(Shell::parse(s.name()), Some(s));
        }
    }

    #[test]
    fn rc_lines_mention_the_right_shell() {
        assert_eq!(
            rc_line(Shell::Bash),
            r#"eval "$(screensense shell-init bash)""#
        );
        assert_eq!(
            rc_line(Shell::Zsh),
            r#"eval "$(screensense shell-init zsh)""#
        );
        assert_eq!(rc_line(Shell::Fish), "screensense shell-init fish | source");
        assert!(rc_line(Shell::PowerShell).contains("shell-init powershell"));
        assert!(rc_line(Shell::PowerShell).contains("Invoke-Expression"));
    }

    #[test]
    fn exe_is_embedded_with_each_shells_quoting() {
        let exe = Path::new(TRICKY);
        assert!(init_script(Shell::Bash, exe).contains(r"__ss_exe='/tmp/it'\''s here/screensense'"));
        assert!(init_script(Shell::Zsh, exe).contains(r"__ss_exe='/tmp/it'\''s here/screensense' "));
        assert!(init_script(Shell::Fish, exe).contains(r"__ss_exe '/tmp/it\'s here/screensense'"));
        assert!(init_script(Shell::PowerShell, exe)
            .contains("__ScreenSenseExe = '/tmp/it''s here/screensense'"));
    }

    #[test]
    fn windows_paths_are_normalised() {
        let exe = Path::new(r"\\?\C:\Program Files\ScreenSense\screensense.exe");
        for s in Shell::ALL {
            let script = init_script(s, exe);
            assert!(
                script.contains("'C:/Program Files/ScreenSense/screensense.exe'"),
                "{s:?}"
            );
            assert!(!script.contains(r"\\?\"));
        }
    }

    #[test]
    fn powershell_typographic_quotes_are_doubled() {
        assert_eq!(quote_powershell("a\u{2019}b"), "'a\u{2019}\u{2019}b'");
        assert_eq!(quote_powershell("a'b"), "'a''b'");
        assert_eq!(quote_fish(r"a\b'c"), r"'a\\b\'c'");
        assert_eq!(quote_posix("a'b"), r"'a'\''b'");
    }

    #[test]
    fn every_script_has_the_record_call_and_an_idempotence_guard() {
        let exe = Path::new("/usr/bin/screensense");
        for s in Shell::ALL {
            let script = init_script(s, exe);
            assert!(!script.contains("@@"), "{s:?}: unreplaced placeholder");
            assert!(script.ends_with('\n'));
            assert!(
                script.contains(&format!("record --shell {}", s.name())),
                "{s:?}"
            );
            for flag in ["--exit", "--ms", "--cwd", " -- "] {
                assert!(script.contains(flag), "{s:?} lacks {flag}");
            }
            let guard = match s {
                Shell::PowerShell => "__ScreenSenseOn",
                _ => "__ss_on",
            };
            assert!(script.contains(guard), "{s:?}");
            let own = match s {
                Shell::PowerShell => "screensense(\\.exe)?",
                _ => "screensense.exe",
            };
            assert!(script.contains(own), "{s:?}: skips its own commands");
            assert!(script.contains("4000"), "{s:?}: command text cap");
        }
    }

    #[test]
    fn scripts_background_the_call_silently() {
        let exe = Path::new("/usr/bin/screensense");
        let bash = init_script(Shell::Bash, exe);
        assert!(bash.contains("( \"$__ss_exe\" record"));
        assert!(bash.contains("</dev/null >/dev/null 2>&1 & )"));
        let zsh = init_script(Shell::Zsh, exe);
        assert!(zsh.contains("</dev/null >/dev/null 2>&1 &!"));
        let fish = init_script(Shell::Fish, exe);
        assert!(fish.contains("</dev/null >/dev/null 2>&1 &"));
        assert!(fish.contains("disown $last_pid"));
        let ps = init_script(Shell::PowerShell, exe);
        assert!(ps.contains("CreateNoWindow = $true"));
        assert!(ps.contains("UseShellExecute = $false"));
    }

    #[test]
    fn scripts_chain_instead_of_clobbering() {
        let exe = Path::new("/usr/bin/screensense");
        let bash = init_script(Shell::Bash, exe);
        assert!(
            bash.contains("trap -p DEBUG"),
            "existing DEBUG trap is chained"
        );
        assert!(bash.contains("${PROMPT_COMMAND[@]}"));
        assert!(bash.contains("${PROMPT_COMMAND:+$PROMPT_COMMAND"));
        assert!(init_script(Shell::Zsh, exe).contains("add-zsh-hook precmd"));
        assert!(init_script(Shell::Fish, exe).contains("--on-event fish_postexec"));
        let ps = init_script(Shell::PowerShell, exe);
        assert!(ps.contains("__ScreenSensePrompt = (Get-Command prompt"));
        assert!(ps.contains("& $global:__ScreenSensePrompt"));
        assert!(ps.contains("$global:LASTEXITCODE = $ssLec"));
        // $? and $LASTEXITCODE are the first things the wrapper reads
        let body = &ps[ps.find("function global:prompt").unwrap()..];
        let first_lines: Vec<&str> = body.lines().skip(1).take(3).map(str::trim).collect();
        assert_eq!(first_lines[0], "$ssOk = $?");
        assert!(first_lines[2].contains("$global:LASTEXITCODE"));
    }

    #[test]
    fn scripts_use_no_unquoted_command_text() {
        let exe = Path::new("/usr/bin/screensense");
        assert!(init_script(Shell::Bash, exe).contains("-- \"${text:0:4000}\""));
        assert!(init_script(Shell::Zsh, exe).contains("-- \"${cmd:0:4000}\""));
        assert!(init_script(Shell::Fish, exe).contains("-- $cmd </dev/null"));
        assert!(!init_script(Shell::Bash, exe).contains("eval \"$text"));
    }
}

/// End-to-end tests: the generated scripts are executed by the real shells installed on
/// the machine, with a fake `screensense` that stores the arguments it receives. Each test
/// skips (and says so) when its shell is not installed. PowerShell is not covered.
#[cfg(all(test, unix))]
mod e2e {
    use super::*;
    use std::fs;
    use std::io::Write;
    use std::os::unix::fs::PermissionsExt;
    use std::path::PathBuf;
    use std::process::{Command, Stdio};
    use std::sync::{Mutex, MutexGuard};
    use std::time::{Duration, Instant};

    use tempfile::TempDir;

    /// Tests fork shells and execute a script they just wrote; serialising them avoids
    /// "text file busy" (ETXTBSY) from a concurrently forked child holding the write fd.
    static LOCK: Mutex<()> = Mutex::new(());

    fn have(bin: &str) -> bool {
        Command::new(bin)
            .arg("--version")
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .is_ok()
    }

    /// One received `screensense` invocation.
    struct Rec {
        args: Vec<Vec<u8>>,
    }

    impl Rec {
        fn arg(&self, i: usize) -> String {
            self.args
                .get(i)
                .map(|a| String::from_utf8_lossy(a).into_owned())
                .unwrap_or_default()
        }
        fn cmd(&self) -> &[u8] {
            self.args.last().map(Vec::as_slice).unwrap_or(&[])
        }
        fn exit(&self) -> String {
            self.arg(4)
        }
        fn ms(&self) -> i64 {
            self.arg(6).parse().unwrap_or(-1)
        }
        fn cwd(&self) -> String {
            self.arg(8)
        }
    }

    struct Sandbox {
        _root: TempDir,
        log: PathBuf,
        work: PathBuf,
        home: PathBuf,
        init: PathBuf,
        count: PathBuf,
    }

    impl Sandbox {
        fn new(shell: Shell) -> Sandbox {
            let root = TempDir::new().expect("tempdir");
            let r = root.path().canonicalize().expect("canonicalize");
            // A directory name with a space and a quote exercises the exe quoting.
            let bin = r.join("bin dir").join("it's");
            let log = r.join("log");
            let work = r.join("work");
            let home = r.join("home");
            for d in [&bin, &log, &work, &home] {
                fs::create_dir_all(d).expect("mkdir");
            }
            let exe = bin.join("screensense");
            let script = format!(
                "#!/bin/sh\nd=$(mktemp -d '{}/rec.XXXXXX') || exit 0\ni=0\nfor a in \"$@\"; do\n  printf '%s' \"$a\" > \"$d/$i\"\n  i=$((i + 1))\ndone\n: > \"$d/done\"\n",
                log.display()
            );
            fs::write(&exe, script).expect("write fake exe");
            fs::set_permissions(&exe, fs::Permissions::from_mode(0o755)).expect("chmod");
            // Make sure the fake can be executed (retry on ETXTBSY), then clear its record.
            for attempt in 0..40 {
                match Command::new(&exe).arg("smoke").status() {
                    Ok(_) => break,
                    Err(e) if e.raw_os_error() == Some(26) && attempt < 39 => {
                        std::thread::sleep(Duration::from_millis(50))
                    }
                    Err(e) => panic!("cannot run the fake screensense: {e}"),
                }
            }
            fs::remove_dir_all(&log).expect("clear log");
            fs::create_dir_all(&log).expect("recreate log");
            let init = r.join("init.txt");
            fs::write(&init, init_script(shell, &exe)).expect("write init");
            let count = r.join("count");
            Sandbox {
                _root: root,
                log,
                work,
                home,
                init,
                count,
            }
        }

        fn records(&self) -> Vec<Rec> {
            let mut out = Vec::new();
            let Ok(rd) = fs::read_dir(&self.log) else {
                return out;
            };
            for e in rd.flatten() {
                let d = e.path();
                if !d.join("done").exists() {
                    continue;
                }
                let mut args = Vec::new();
                while let Ok(a) = fs::read(d.join(args.len().to_string())) {
                    args.push(a);
                }
                out.push(Rec { args });
            }
            out
        }

        /// Wait until at least `expect` records arrived, then a moment more so an
        /// unwanted extra one would show up too.
        fn settled(&self, expect: usize) -> Vec<Rec> {
            let deadline = Instant::now() + Duration::from_secs(10);
            while self.records().len() < expect && Instant::now() < deadline {
                std::thread::sleep(Duration::from_millis(50));
            }
            std::thread::sleep(Duration::from_millis(500));
            self.records()
        }
    }

    struct Run {
        stdout: String,
        stderr: String,
    }

    /// Run `shell` interactively with `script` as its input.
    ///
    /// bash and zsh run their prompt / pre-exec hooks when reading from a pipe, so they
    /// get the script on stdin. fish emits its `fish_preexec` / `fish_postexec` events only
    /// when it reads from a terminal, so fish runs on a pseudo-terminal made by
    /// util-linux `script`.
    fn run(shell: Shell, sb: &Sandbox, script: &str) -> Run {
        let mut cmd = if have("timeout") {
            let mut c = Command::new("timeout");
            c.arg("30");
            c
        } else {
            Command::new("env")
        };
        let mut input = script.to_string();
        match shell {
            Shell::Bash => cmd.args(["bash", "--norc", "--noprofile", "-i"]),
            Shell::Zsh => cmd.args(["zsh", "-f", "-i"]),
            Shell::Fish => {
                input.push_str("exit\n");
                cmd.args(["script", "-qfec", "fish --no-config -i", "/dev/null"])
            }
            Shell::PowerShell => panic!("PowerShell is not covered by the end-to-end tests"),
        };
        let mut child = cmd
            .current_dir(&sb.work)
            .env("HOME", &sb.home)
            .env("XDG_CONFIG_HOME", sb.home.join(".config"))
            .env("XDG_DATA_HOME", sb.home.join(".local/share"))
            .env("TERM", "dumb")
            .env("LC_ALL", "C.UTF-8")
            .env("SS_INIT", &sb.init)
            .env("SS_COUNT", &sb.count)
            .env("SS_WORK", &sb.work)
            .env_remove("PROMPT_COMMAND")
            .env_remove("BASH_ENV")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .expect("spawn shell");
        let mut stdin = child.stdin.take().expect("stdin");
        stdin.write_all(input.as_bytes()).expect("write script");
        drop(stdin);
        let out = child.wait_with_output().expect("wait");
        Run {
            stdout: String::from_utf8_lossy(&out.stdout).into_owned(),
            stderr: String::from_utf8_lossy(&out.stderr).into_owned(),
        }
    }

    fn find<'a>(recs: &'a [Rec], cmd: &str) -> Vec<&'a Rec> {
        recs.iter().filter(|r| r.cmd() == cmd.as_bytes()).collect()
    }

    fn one<'a>(recs: &'a [Rec], cmd: &str) -> &'a Rec {
        let v = find(recs, cmd);
        assert_eq!(
            v.len(),
            1,
            "expected exactly one record for {cmd:?}, got {}; all: {:?}",
            v.len(),
            recs.iter()
                .map(|r| String::from_utf8_lossy(r.cmd()).into_owned())
                .collect::<Vec<_>>()
        );
        v[0]
    }

    /// The command lines of the scenario, per shell. `weird` has quotes, `$(...)`, a
    /// backtick (where the shell has them), a UTF-8 word and a backslash, and appends one
    /// `x` to $SS_COUNT every time it really runs.
    struct Scenario {
        weird: &'static str,
        status_echo: &'static str,
        pre: &'static str,
        pre_marker: &'static str,
        source_twice: &'static str,
    }

    fn scenario(shell: Shell) -> Scenario {
        match shell {
            Shell::Bash => Scenario {
                weird: r#"printf x >> "$SS_COUNT"; : 'sq' "dq" $(echo x) `echo y` héllo 日本語 \back"#,
                status_echo: r#"echo "S=$?""#,
                pre: "PROMPT_COMMAND='echo PC_RAN'",
                pre_marker: "PC_RAN",
                source_twice: r#"source "$SS_INIT"; source "$SS_INIT""#,
            },
            Shell::Zsh => Scenario {
                weird: r#"printf x >> "$SS_COUNT"; : 'sq' "dq" $(echo x) `echo y` héllo 日本語 \back"#,
                status_echo: r#"echo "S=$?""#,
                pre: "precmd() { print PC_RAN }",
                pre_marker: "PC_RAN",
                source_twice: r#"source "$SS_INIT"; source "$SS_INIT""#,
            },
            Shell::Fish => Scenario {
                weird: r#"printf x >> "$SS_COUNT"; true 'sq' "dq" (echo x) '`echo y`' héllo 日本語 \\back"#,
                status_echo: r#"echo "S=$status""#,
                pre: "function mypost --on-event fish_postexec; echo PC_RAN; end",
                pre_marker: "PC_RAN",
                source_twice: r#"source $SS_INIT; source $SS_INIT"#,
            },
            Shell::PowerShell => unreachable!(),
        }
    }

    impl Scenario {
        fn script(&self) -> String {
            let lines = [
                self.pre,
                self.source_twice,
                "echo plain",
                "false",
                "sh -c 'exit 7'",
                self.status_echo,
                "sleep 0.3",
                self.weird,
                " echo leading-space",
                "",
                "screensense now",
                "cd /",
                "true",
                "echo DONE",
            ];
            let mut s = lines.join("\n");
            s.push('\n');
            s
        }
    }

    fn check_scenario(shell: Shell) {
        let bin = shell.name();
        if !have(bin) {
            eprintln!("note: {bin} is not installed here; skipping its end-to-end test");
            return;
        }
        if shell == Shell::Fish && !have("script") {
            eprintln!("note: util-linux `script` is not installed; fish needs a pty, skipping");
            return;
        }
        let _guard: MutexGuard<'_, ()> = LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let sb = Sandbox::new(shell);
        let sc = scenario(shell);
        let out = run(shell, &sb, &sc.script());
        // recorded: plain, false, sh -c, S=, sleep, weird, cd /, true, DONE
        let recs = sb.settled(9);
        let names: Vec<String> = recs
            .iter()
            .map(|r| String::from_utf8_lossy(r.cmd()).into_owned())
            .collect();
        let diag = format!(
            "records: {names:?}\n--- stdout ---\n{}\n--- stderr ---\n{}",
            out.stdout, out.stderr
        );

        // every record has the documented argument layout, as separate argv entries
        for r in &recs {
            assert_eq!(r.args.len(), 11, "{diag}");
            assert_eq!(r.arg(0), "record", "{diag}");
            assert_eq!(r.arg(1), "--shell", "{diag}");
            assert_eq!(r.arg(2), bin, "{diag}");
            assert_eq!(r.arg(3), "--exit", "{diag}");
            assert_eq!(r.arg(5), "--ms", "{diag}");
            assert_eq!(r.arg(7), "--cwd", "{diag}");
            assert_eq!(r.arg(9), "--", "{diag}");
            assert!(r.ms() >= 0, "--ms must be a number: {diag}");
        }
        assert_eq!(recs.len(), 9, "{diag}");

        // exit codes
        assert_eq!(one(&recs, "echo plain").exit(), "0", "{diag}");
        assert_eq!(one(&recs, "false").exit(), "1", "{diag}");
        assert_eq!(one(&recs, "sh -c 'exit 7'").exit(), "7", "{diag}");
        // the user-visible status is untouched by the hooks
        assert!(out.stdout.contains("S=7"), "{diag}");
        assert_eq!(one(&recs, &sc.status_echo_cmd()).exit(), "0", "{diag}");

        // duration
        let slept = one(&recs, "sleep 0.3");
        assert!(
            slept.ms() >= 250 && slept.ms() < 5000,
            "ms={} {diag}",
            slept.ms()
        );

        // the awkward line: one argv entry, byte-identical, executed exactly once
        let w = one(&recs, sc.weird);
        assert_eq!(w.cmd(), sc.weird.as_bytes(), "{diag}");
        assert_eq!(w.exit(), "0", "{diag}");
        assert_eq!(
            fs::read_to_string(&sb.count).unwrap_or_default(),
            "x",
            "{diag}"
        );

        // skipped: leading space, empty line, our own command
        assert!(find(&recs, " echo leading-space").is_empty(), "{diag}");
        assert!(find(&recs, "echo leading-space").is_empty(), "{diag}");
        assert!(find(&recs, "").is_empty(), "{diag}");
        assert!(find(&recs, "screensense now").is_empty(), "{diag}");
        // sourcing twice (same line) did not double anything: each recorded exactly once
        assert_eq!(one(&recs, "echo DONE").exit(), "0", "{diag}");

        // directory: where the command ran, not where it ended
        let work = sb.work.to_string_lossy().into_owned();
        assert_eq!(one(&recs, "echo plain").cwd(), work, "{diag}");
        assert_eq!(one(&recs, "cd /").cwd(), work, "{diag}");
        assert_eq!(one(&recs, "true").cwd(), "/", "{diag}");

        // an existing hook still runs on every prompt
        let n = out.stdout.matches(sc.pre_marker).count();
        assert!(n >= 10, "pre-existing hook ran {n} times: {diag}");

        // nothing leaked into the terminal: no job-control lines ("[1] 12345"), no
        // "Done" / "has ended" notices, no stray output of the record call
        let job_line = regex::Regex::new(r"\[[0-9]+\][ +-]+[0-9]{2,}").expect("regex");
        for (name, text) in [("stdout", &out.stdout), ("stderr", &out.stderr)] {
            assert!(
                !job_line.is_match(text),
                "job-control line in {name}: {diag}"
            );
            for noise in ["Done", "has ended", "record --shell", "Terminated"] {
                assert!(!text.contains(noise), "{noise:?} in {name}: {diag}");
            }
        }
    }

    impl Scenario {
        fn status_echo_cmd(&self) -> String {
            self.status_echo.to_string()
        }
    }

    /// Not an assertion: records which shell versions the end-to-end tests ran against.
    #[test]
    fn shell_versions_note() {
        for bin in ["bash", "zsh", "fish"] {
            match Command::new(bin).arg("--version").output() {
                Ok(o) => eprintln!(
                    "note: {bin}: {}",
                    String::from_utf8_lossy(&o.stdout)
                        .lines()
                        .next()
                        .unwrap_or("")
                ),
                Err(_) => eprintln!("note: {bin}: not installed"),
            }
        }
        eprintln!("note: PowerShell is never exercised by these tests");
    }

    #[test]
    fn bash_end_to_end() {
        check_scenario(Shell::Bash);
    }

    #[test]
    fn zsh_end_to_end() {
        check_scenario(Shell::Zsh);
    }

    #[test]
    fn fish_end_to_end() {
        check_scenario(Shell::Fish);
    }

    /// Run `lines`, wait for `expect` records, return output and records.
    fn session(shell: Shell, lines: &[&str], expect: usize) -> Option<(Run, Vec<Rec>, Sandbox)> {
        if !have(shell.name()) {
            eprintln!("note: {} is not installed here; skipping", shell.name());
            return None;
        }
        let sb = Sandbox::new(shell);
        let mut script = lines.join("\n");
        script.push('\n');
        let out = run(shell, &sb, &script);
        let recs = sb.settled(expect);
        Some((out, recs, sb))
    }

    #[test]
    fn bash_without_prompt_command_and_with_timestamps_array_and_debug_trap() {
        let _guard = LOCK.lock().unwrap_or_else(|e| e.into_inner());
        // 1. no PROMPT_COMMAND at all
        let Some((out, recs, _sb)) = session(
            Shell::Bash,
            &[
                "unset PROMPT_COMMAND",
                "set -u",
                r#"source "$SS_INIT""#,
                "echo one",
                "false",
                r#"echo "S=$?""#,
            ],
            3,
        ) else {
            return;
        };
        assert_eq!(recs.len(), 3, "{}", out.stderr);
        assert_eq!(one(&recs, "false").exit(), "1");
        assert!(out.stdout.contains("S=1"), "{}", out.stdout);

        // 2. HISTTIMEFORMAT set, PROMPT_COMMAND an array (bash 5.1+), a DEBUG trap of the user's
        let Some((out, recs, _sb)) = session(
            Shell::Bash,
            &[
                "HISTTIMEFORMAT='%F %T '",
                "trap 'echo DBG_RAN' DEBUG",
                "PROMPT_COMMAND=('echo PCA1' 'echo PCA2')",
                r#"source "$SS_INIT""#,
                "echo one",
                "false",
                r#"echo "S=$?""#,
            ],
            3,
        ) else {
            return;
        };
        assert_eq!(recs.len(), 3, "{}", out.stdout);
        assert_eq!(
            one(&recs, "echo one").cmd(),
            b"echo one",
            "no timestamp in the text"
        );
        assert_eq!(one(&recs, "false").exit(), "1");
        assert!(out.stdout.contains("S=1"), "{}", out.stdout);
        assert!(out.stdout.matches("DBG_RAN").count() >= 3, "{}", out.stdout);
        assert!(out.stdout.matches("PCA1").count() >= 3, "{}", out.stdout);
        assert!(out.stdout.matches("PCA2").count() >= 3, "{}", out.stdout);
    }

    #[test]
    fn zsh_keeps_existing_hooks() {
        let _guard = LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let Some((out, recs, _sb)) = session(
            Shell::Zsh,
            &[
                "setopt nounset",
                "preexec() { print PX_RAN }",
                "mypf() { print PF_RAN }",
                "precmd_functions+=(mypf)",
                r#"source "$SS_INIT""#,
                " echo hidden",
                "echo one",
                "false",
                r#"echo "S=$?""#,
            ],
            3,
        ) else {
            return;
        };
        assert_eq!(recs.len(), 3, "{}", out.stdout);
        assert_eq!(one(&recs, "false").exit(), "1");
        assert!(out.stdout.contains("S=1"), "{}", out.stdout);
        assert!(out.stdout.matches("PX_RAN").count() >= 4, "{}", out.stdout);
        assert!(out.stdout.matches("PF_RAN").count() >= 4, "{}", out.stdout);
    }

    #[test]
    fn multi_line_command_is_one_argument() {
        let _guard = LOCK.lock().unwrap_or_else(|e| e.into_inner());
        for shell in [Shell::Bash, Shell::Zsh] {
            let Some((out, recs, _sb)) = session(
                shell,
                &[
                    r#"source "$SS_INIT""#,
                    "echo 'first",
                    "second'",
                    "echo after",
                ],
                2,
            ) else {
                continue;
            };
            assert_eq!(recs.len(), 2, "{shell:?}: {}", out.stderr);
            let multi: Vec<&Rec> = recs
                .iter()
                .filter(|r| String::from_utf8_lossy(r.cmd()).contains("first"))
                .collect();
            assert_eq!(multi.len(), 1, "{shell:?}");
            let text = String::from_utf8_lossy(multi[0].cmd()).into_owned();
            assert!(text.starts_with("echo 'first"), "{shell:?}: {text:?}");
            assert!(text.contains("second'"), "{shell:?}: {text:?}");
            assert_eq!(multi[0].args.len(), 11, "{shell:?}");
            eprintln!("note: {shell:?} multi-line command arrives as {text:?}");
        }
    }

    /// Generated scripts must at least parse in the real shells.
    #[test]
    fn scripts_pass_the_shells_own_syntax_check() {
        let _guard = LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let dir = TempDir::new().expect("tempdir");
        let exe = Path::new("/tmp/it's a dir/screensense");
        for (shell, bin, flag) in [
            (Shell::Bash, "bash", "-n"),
            (Shell::Zsh, "zsh", "-n"),
            (Shell::Fish, "fish", "--no-execute"),
        ] {
            if !have(bin) {
                eprintln!("note: {bin} is not installed here; skipping its syntax check");
                continue;
            }
            let file = dir.path().join(format!("init.{bin}"));
            fs::write(&file, init_script(shell, exe)).expect("write");
            let out = Command::new(bin)
                .arg(flag)
                .arg(&file)
                .output()
                .expect("spawn");
            assert!(
                out.status.success(),
                "{bin} {flag}: {}",
                String::from_utf8_lossy(&out.stderr)
            );
        }
    }
}
