//! Text matching of shell commands, for the command rules and the built-in
//! lists of commands that always ask or never run.
//!
//! This reads the command as text: it splits it on `&&`, `||`, `;`, `|`,
//! `&`, new lines and command substitutions, takes wrappers such as `sudo`,
//! `env` or `time` off each part, and looks at the words. It catches the
//! model's mistakes, not a deliberate attempt to hide a command (an alias, a
//! script, an encoded or generated command line). Only the OS sandbox and
//! the file tools' own checks are enforced.

use std::path::{Component, Path, PathBuf};

/// One simple command of a command line.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Segment {
    /// The words as written, quotes removed, without redirections.
    pub raw: Vec<String>,
    /// The words that run: `raw` without leading variable assignments and
    /// wrappers (`sudo`, `env`, `command`, `time`, `nohup`, `nice`, ...).
    pub words: Vec<String>,
    /// A wrapper elevated it (`sudo`, `doas`, `su`, `pkexec`, `gsudo`,
    /// `runas`), or it is `Start-Process -Verb RunAs`.
    pub elevated: bool,
    /// A shell given code inline (`bash -c`, `powershell -Command`,
    /// `cmd /c`, an encoded PowerShell command).
    pub inline_code: bool,
    /// Targets of output redirections (`> f`, `>> f`, `2> f`, `&> f`).
    pub redirects: Vec<String>,
    /// Nothing before the program changes what runs or how: no variable
    /// assignment (`PATH=.`, `LD_PRELOAD=…`), no `env`, `xargs` or
    /// elevation, only wrappers such as `time` or `nohup`. An allow rule
    /// matches only such a part (see `allow_words`).
    pub plain: bool,
}

impl Segment {
    /// The program's name: its file name, lower case, without `.exe`.
    pub fn program(&self) -> String {
        self.words
            .first()
            .map(|word| program_name(word))
            .unwrap_or_default()
    }

    fn args(&self) -> &[String] {
        self.words.get(1..).unwrap_or_default()
    }

    fn lower_args(&self) -> Vec<String> {
        self.args().iter().map(|arg| arg.to_lowercase()).collect()
    }
}

/// The file name of `word`, lower case, without `.exe` (`/usr/bin/sudo` and
/// `C:\Windows\System32\reg.exe` give `sudo` and `reg`).
pub fn program_name(word: &str) -> String {
    let name = word.rsplit(['/', '\\']).next().unwrap_or(word);
    let name = name.to_lowercase();
    name.strip_suffix(".exe")
        .map(str::to_string)
        .unwrap_or(name)
}

/// The simple commands of `command`, including those given inline to a
/// shell (`bash -c "…"`).
pub fn segments(command: &str) -> Vec<Segment> {
    let mut out = Vec::new();
    collect_segments(command, false, 0, false, &mut out);
    // A backslash is an escape to a POSIX shell and a path separator to
    // PowerShell (`Remove-Item "C:\Users\me\" -Recurse`). Read it both ways:
    // neither reading may hide a part from the lists.
    if command.contains('\\') {
        let mut windows = Vec::new();
        collect_segments(command, false, 0, true, &mut windows);
        for part in windows {
            if !out.contains(&part) {
                out.push(part);
            }
        }
    }
    out
}

fn collect_segments(
    command: &str,
    elevated: bool,
    depth: usize,
    windows: bool,
    out: &mut Vec<Segment>,
) {
    for tokens in lex(command, windows) {
        let mut raw = Vec::new();
        let mut redirects = Vec::new();
        let mut tokens = tokens.into_iter();
        while let Some(token) = tokens.next() {
            match token {
                Token::Word(word) => raw.push(word),
                Token::Redirect(op) => {
                    if let Some(Token::Word(target)) = tokens.next() {
                        if op.starts_with('>') && !is_null_device(&target) {
                            redirects.push(target);
                        }
                    }
                }
            }
        }
        if raw.is_empty() && redirects.is_empty() {
            continue;
        }
        let stripped = strip_wrappers(&raw);
        let mut segment = Segment {
            words: stripped.words,
            raw,
            elevated: elevated || stripped.elevated,
            inline_code: false,
            redirects,
            plain: stripped.plain,
        };
        if is_start_process_runas(&segment) {
            segment.elevated = true;
        }
        let inline = inline_code(&segment);
        let nested_elevated = segment.elevated;
        if let Some(code) = &inline {
            segment.inline_code = true;
            if depth < 4 {
                // Pushed after its own segment, so callers see the shell first.
                let mut inner = Vec::new();
                if let Inline::Code(code) = code {
                    collect_segments(code, nested_elevated, depth + 1, windows, &mut inner);
                }
                out.push(segment);
                out.extend(inner);
                continue;
            }
        }
        if let Some(code) = stripped.inner {
            out.push(segment);
            if depth < 4 {
                collect_segments(&code, true, depth + 1, windows, out);
            }
            continue;
        }
        // `find … -exec cmd {} ;`: the command it runs is a command too (a
        // delete is the find's own, see `delete_targets`).
        let executed = find_executes(&segment);
        let nested_elevated = segment.elevated;
        out.push(segment);
        if depth < 4 {
            for words in executed {
                let mut inner = Vec::new();
                collect_segments(
                    &join_words(&words),
                    nested_elevated,
                    depth + 1,
                    windows,
                    &mut inner,
                );
                if inner.first().is_some_and(|first| !is_delete(first)) {
                    out.extend(inner);
                }
            }
        }
    }
}

/// The commands `find` runs with `-exec`, `-execdir`, `-ok` and `-okdir`,
/// without the `{}` placeholder.
fn find_executes(segment: &Segment) -> Vec<Vec<String>> {
    if segment.program() != "find" {
        return Vec::new();
    }
    let mut out = Vec::new();
    let args = segment.args();
    let mut i = 0;
    while let Some(arg) = args.get(i) {
        i += 1;
        if !matches!(arg.as_str(), "-exec" | "-execdir" | "-ok" | "-okdir") {
            continue;
        }
        let mut words = Vec::new();
        while let Some(word) = args.get(i) {
            i += 1;
            if word == ";" || word == "+" {
                break;
            }
            if word != "{}" {
                words.push(word.clone());
            }
        }
        if !words.is_empty() {
            out.push(words);
        }
    }
    out
}

/// Words back into one command line, quoted where the shell would split
/// them.
fn join_words(words: &[String]) -> String {
    words
        .iter()
        .map(|word| {
            if !word.is_empty()
                && word
                    .chars()
                    .all(|c| c.is_ascii_alphanumeric() || "-_./:=@%+,".contains(c))
            {
                word.clone()
            } else {
                format!("'{}'", word.replace('\'', "'\\''"))
            }
        })
        .collect::<Vec<_>>()
        .join(" ")
}

fn is_null_device(target: &str) -> bool {
    matches!(
        target.to_lowercase().as_str(),
        "/dev/null" | "nul" | "$null" | "/dev/stdout" | "/dev/stderr"
    )
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum Token {
    Word(String),
    /// `>`, `>>`, `<`, `<<<`: the next word is its target.
    Redirect(String),
}

/// Splits a command line into the tokens of each simple command. With
/// `windows`, a backslash in a word that is a Windows path is the path's,
/// not an escape (see `segments`).
fn lex(command: &str, windows: bool) -> Vec<Vec<Token>> {
    let chars: Vec<char> = command.chars().collect();
    let mut segments: Vec<Vec<Token>> = Vec::new();
    let mut current: Vec<Token> = Vec::new();
    let mut word = String::new();
    let mut in_word = false;
    let mut heredocs: Vec<(String, bool)> = Vec::new();
    let mut i = 0;
    let at = |index: usize| chars.get(index).copied();

    fn end_word(word: &mut String, in_word: &mut bool, current: &mut Vec<Token>) {
        if *in_word {
            current.push(Token::Word(std::mem::take(word)));
            *in_word = false;
        }
    }
    fn end_segment(current: &mut Vec<Token>, segments: &mut Vec<Vec<Token>>) {
        if !current.is_empty() {
            segments.push(std::mem::take(current));
        }
    }

    while i < chars.len() {
        let c = chars[i];
        match c {
            '\'' => {
                in_word = true;
                i += 1;
                while i < chars.len() && chars[i] != '\'' {
                    word.push(chars[i]);
                    i += 1;
                }
                i += 1;
            }
            '"' => {
                in_word = true;
                i += 1;
                while i < chars.len() && chars[i] != '"' {
                    if chars[i] == '\\'
                        && matches!(at(i + 1), Some('"' | '\\' | '$' | '`'))
                        && !(windows && is_windows_path(&word))
                    {
                        word.push(chars[i + 1]);
                        i += 2;
                        continue;
                    }
                    word.push(chars[i]);
                    i += 1;
                }
                i += 1;
            }
            // A backslash escapes the shell's own characters; before any
            // other character it is kept, so a Windows path stays whole.
            // Read the PowerShell way (`windows`), in a word that is a
            // Windows path it is the path's (`C:\ -Recurse`).
            '\\' if matches!(
                at(i + 1),
                Some(' ' | '"' | '\'' | '\\' | ';' | '&' | '|' | '>' | '<' | '$' | '`' | '(' | ')')
            ) && !(windows && is_windows_path(&word)) =>
            {
                word.push(chars[i + 1]);
                in_word = true;
                i += 2;
            }
            '\\' if at(i + 1) == Some('\n') => i += 2,
            ' ' | '\t' | '\r' => {
                end_word(&mut word, &mut in_word, &mut current);
                i += 1;
            }
            '\n' => {
                end_word(&mut word, &mut in_word, &mut current);
                end_segment(&mut current, &mut segments);
                i += 1;
                // Here-document bodies are data, not commands.
                for (delimiter, strip_tabs) in std::mem::take(&mut heredocs) {
                    loop {
                        let start = i;
                        while i < chars.len() && chars[i] != '\n' {
                            i += 1;
                        }
                        let line: String = chars[start..i].iter().collect();
                        i += 1;
                        let line = if strip_tabs {
                            line.trim_start_matches('\t').to_string()
                        } else {
                            line
                        };
                        if line.trim_end_matches('\r') == delimiter || i >= chars.len() {
                            break;
                        }
                    }
                }
            }
            ';' => {
                end_word(&mut word, &mut in_word, &mut current);
                end_segment(&mut current, &mut segments);
                i += 1;
            }
            '&' => {
                end_word(&mut word, &mut in_word, &mut current);
                match at(i + 1) {
                    Some('&') => {
                        end_segment(&mut current, &mut segments);
                        i += 2;
                    }
                    Some('>') => {
                        // `&>` and `&>>`: stdout and stderr to a file.
                        i += 2;
                        if at(i) == Some('>') {
                            i += 1;
                        }
                        current.push(Token::Redirect(">".to_string()));
                    }
                    _ => {
                        end_segment(&mut current, &mut segments);
                        i += 1;
                    }
                }
            }
            '|' => {
                end_word(&mut word, &mut in_word, &mut current);
                end_segment(&mut current, &mut segments);
                i += if matches!(at(i + 1), Some('|' | '&')) {
                    2
                } else {
                    1
                };
            }
            '>' | '<' => {
                // `2>`: the digits before are a file descriptor, not a word.
                if in_word && !word.is_empty() && word.chars().all(|c| c.is_ascii_digit()) {
                    word.clear();
                    in_word = false;
                } else {
                    end_word(&mut word, &mut in_word, &mut current);
                }
                let mut op = c.to_string();
                i += 1;
                while matches!(at(i), Some('>' | '<')) {
                    op.push(chars[i]);
                    i += 1;
                }
                if at(i) == Some('|') {
                    i += 1;
                }
                if at(i) == Some('&') {
                    // `>&2`, `2>&1`, `<&-`: a descriptor, no file.
                    i += 1;
                    while matches!(at(i), Some(c) if c.is_ascii_digit() || c == '-') {
                        i += 1;
                    }
                    continue;
                }
                if op == "<<" || op == "<<-" {
                    while matches!(at(i), Some(' ' | '\t')) {
                        i += 1;
                    }
                    let mut delimiter = String::new();
                    while let Some(c) = at(i) {
                        if c.is_whitespace() || matches!(c, ';' | '&' | '|' | '>' | '<') {
                            break;
                        }
                        if !matches!(c, '\'' | '"' | '\\') {
                            delimiter.push(c);
                        }
                        i += 1;
                    }
                    if !delimiter.is_empty() {
                        heredocs.push((delimiter, op == "<<-"));
                    }
                    continue;
                }
                current.push(Token::Redirect(op));
            }
            '(' | ')' | '`' => {
                end_word(&mut word, &mut in_word, &mut current);
                end_segment(&mut current, &mut segments);
                i += 1;
            }
            '$' if at(i + 1) == Some('(') => {
                end_word(&mut word, &mut in_word, &mut current);
                end_segment(&mut current, &mut segments);
                i += 2;
            }
            // `${NAME}`: one word, braces and all.
            '$' if at(i + 1) == Some('{') => {
                in_word = true;
                while let Some(c) = at(i) {
                    word.push(c);
                    i += 1;
                    if c == '}' {
                        break;
                    }
                }
            }
            // `{}` (find's placeholder) is a word.
            '{' if !in_word && at(i + 1) == Some('}') => {
                word.push_str("{}");
                in_word = true;
                i += 2;
            }
            // A group (`{ cmd; }`) or a PowerShell script block
            // (`ForEach-Object { Remove-Item $_ }`): what follows is a
            // command. Inside a word (`file.{c,h}`, `@{a=1}`) it is text.
            '{' if !in_word => {
                end_segment(&mut current, &mut segments);
                i += 1;
            }
            '}' if !(in_word && word.contains('{')) => {
                end_word(&mut word, &mut in_word, &mut current);
                end_segment(&mut current, &mut segments);
                i += 1;
            }
            _ => {
                word.push(c);
                in_word = true;
                i += 1;
            }
        }
    }
    end_word(&mut word, &mut in_word, &mut current);
    end_segment(&mut current, &mut segments);
    segments
}

/// `C:…` or `\\server…`: a Windows path, whose backslashes are
/// separators.
fn is_windows_path(word: &str) -> bool {
    let bytes = word.as_bytes();
    (bytes.len() >= 2 && bytes[0].is_ascii_alphabetic() && bytes[1] == b':')
        || word.starts_with("\\\\")
}

struct Stripped {
    words: Vec<String>,
    elevated: bool,
    /// The command `su -c` or `runas` runs, given as one string.
    inner: Option<String>,
    /// See `Segment::plain`.
    plain: bool,
}

fn is_assignment(word: &str) -> bool {
    match word.split_once('=') {
        Some((name, _)) => {
            !name.is_empty()
                && name
                    .chars()
                    .next()
                    .is_some_and(|c| c.is_ascii_alphabetic() || c == '_')
                && name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
        }
        None => false,
    }
}

/// Skips `-x` options from `index`, and the value after any option in
/// `with_value`; stops after `--`.
fn skip_options(words: &[String], mut index: usize, with_value: &[&str]) -> usize {
    while let Some(word) = words.get(index) {
        if word == "--" {
            return index + 1;
        }
        if !word.starts_with('-') || word == "-" {
            break;
        }
        index += if with_value.contains(&word.as_str()) {
            2
        } else {
            1
        };
    }
    index
}

/// Takes leading variable assignments and wrappers off `raw`.
fn strip_wrappers(raw: &[String]) -> Stripped {
    let mut elevated = false;
    let mut plain = true;
    let mut i = 0;
    while let Some(word) = raw.get(i) {
        if matches!(word.as_str(), "!" | "{" | "}") {
            i += 1;
            continue;
        }
        if is_assignment(word) {
            plain = false;
            i += 1;
            continue;
        }
        let program = program_name(word);
        if matches!(
            program.as_str(),
            "sudo" | "doas" | "pkexec" | "gsudo" | "runas" | "su" | "env" | "xargs"
        ) {
            plain = false;
        }
        match program.as_str() {
            "sudo" => {
                elevated = true;
                i = skip_options(
                    raw,
                    i + 1,
                    &[
                        "-u", "-g", "-h", "-p", "-C", "-D", "-r", "-t", "-U", "-T", "-R",
                    ],
                );
            }
            "doas" => {
                elevated = true;
                i = skip_options(raw, i + 1, &["-u", "-C"]);
            }
            "pkexec" => {
                elevated = true;
                i = skip_options(raw, i + 1, &["--user"]);
            }
            "gsudo" => {
                elevated = true;
                i = skip_options(raw, i + 1, &["-u", "--user", "-d"]);
            }
            "runas" => {
                elevated = true;
                let rest: Vec<&String> = raw[i + 1..]
                    .iter()
                    .filter(|word| !word.starts_with('/'))
                    .collect();
                return Stripped {
                    words: rest.iter().map(|word| word.to_string()).collect(),
                    elevated,
                    inner: (rest.len() == 1).then(|| rest[0].to_string()),
                    plain,
                };
            }
            "su" => {
                let rest = &raw[i + 1..];
                let code = rest
                    .iter()
                    .position(|word| word == "-c" || word == "--command")
                    .and_then(|at| rest.get(at + 1))
                    .cloned();
                return Stripped {
                    words: raw[i..].to_vec(),
                    elevated: true,
                    inner: code,
                    plain,
                };
            }
            "env" => {
                i = skip_options(raw, i + 1, &["-u", "-C", "-S", "--unset", "--chdir"]);
                while raw.get(i).is_some_and(|word| is_assignment(word)) {
                    i += 1;
                }
            }
            "command" => {
                if raw
                    .get(i + 1)
                    .is_some_and(|word| word == "-v" || word == "-V")
                {
                    // A lookup: nothing runs.
                    return Stripped {
                        words: Vec::new(),
                        elevated,
                        inner: None,
                        plain,
                    };
                }
                i = skip_options(raw, i + 1, &[]);
            }
            "builtin" | "nohup" => i += 1,
            "exec" => i = skip_options(raw, i + 1, &["-a"]),
            "time" => i = skip_options(raw, i + 1, &["-f", "-o", "--format", "--output"]),
            "nice" => i = skip_options(raw, i + 1, &["-n", "--adjustment"]),
            "timeout" => {
                i = skip_options(raw, i + 1, &["-s", "-k", "--signal", "--kill-after"]);
                // The duration.
                i += 1;
            }
            "xargs" => {
                i = skip_options(
                    raw,
                    i + 1,
                    &[
                        "-n",
                        "-P",
                        "-I",
                        "-L",
                        "-d",
                        "-E",
                        "-s",
                        "-a",
                        "--max-args",
                        "--max-procs",
                        "--delimiter",
                        "--arg-file",
                    ],
                );
            }
            _ => break,
        }
    }
    Stripped {
        words: raw.get(i..).unwrap_or_default().to_vec(),
        elevated,
        inner: None,
        plain,
    }
}

enum Inline {
    Code(String),
    /// Code that cannot be read (`powershell -EncodedCommand`).
    Opaque,
}

/// Code a shell is given inline: `bash -c "…"`, `powershell -Command …`,
/// `cmd /c …`.
fn inline_code(segment: &Segment) -> Option<Inline> {
    let program = segment.program();
    let args = segment.args();
    match program.as_str() {
        "bash" | "sh" | "zsh" | "dash" | "ksh" | "ash" | "fish" | "mksh" => {
            let flag = args.iter().position(|arg| {
                arg == "-c"
                    || (arg.starts_with('-')
                        && !arg.starts_with("--")
                        && arg.len() > 1
                        && arg[1..].chars().all(|c| c.is_ascii_alphabetic())
                        && arg.contains('c'))
            })?;
            Some(Inline::Code(
                args.get(flag + 1).cloned().unwrap_or_default(),
            ))
        }
        "powershell" | "pwsh" => {
            let lower: Vec<String> = segment.lower_args();
            let flag = lower.iter().position(|arg| {
                let name = arg.trim_start_matches('-');
                arg.starts_with('-')
                    && (name == "c" || (name.len() >= 3 && "command".starts_with(name)))
            });
            if let Some(flag) = flag {
                return Some(Inline::Code(args[flag + 1..].join(" ")));
            }
            let encoded = lower.iter().any(|arg| {
                let name = arg.trim_start_matches('-');
                arg.starts_with('-')
                    && (name == "e"
                        || name == "ec"
                        || (name.len() >= 2 && "encodedcommand".starts_with(name)))
            });
            if encoded {
                return Some(Inline::Opaque);
            }
            None
        }
        "cmd" => {
            let flag = args
                .iter()
                .position(|arg| matches!(arg.to_lowercase().as_str(), "/c" | "/k" | "/r"))?;
            Some(Inline::Code(args[flag + 1..].join(" ")))
        }
        _ => None,
    }
}

fn is_start_process_runas(segment: &Segment) -> bool {
    let program = segment.program();
    if program != "start-process" && program != "saps" && program != "start" {
        return false;
    }
    let lower = segment.lower_args();
    lower
        .windows(2)
        .any(|pair| pair[0] == "-verb" && pair[1].trim_matches('\'') == "runas")
        || lower.iter().any(|arg| arg == "-verb:runas")
}

/// Programs that run whatever code they are given. Never allowed beyond one
/// approval, as a prefix rule for them would allow anything.
pub fn is_interpreter(segment: &Segment) -> bool {
    if segment.inline_code {
        return true;
    }
    let program = segment.program();
    program.starts_with("python")
        || program.starts_with("pypy")
        || is_shell(&program)
        || matches!(
            program.as_str(),
            "py" | "node"
                | "nodejs"
                | "deno"
                | "ruby"
                | "perl"
                | "php"
                | "lua"
                | "luajit"
                | "osascript"
                | "rscript"
                | "eval"
                | "invoke-expression"
                | "iex"
                | "source"
                | "."
        )
}

/// A shell: given a script (`bash build.sh`) it runs whatever the script
/// says, as it does with inline code.
fn is_shell(program: &str) -> bool {
    matches!(
        program,
        "bash"
            | "sh"
            | "zsh"
            | "dash"
            | "ksh"
            | "ash"
            | "fish"
            | "mksh"
            | "csh"
            | "tcsh"
            | "powershell"
            | "pwsh"
            | "cmd"
    )
}

/// Programs that delete files, and `find` with `-delete` or running one.
pub fn is_delete(segment: &Segment) -> bool {
    let program = segment.program();
    if program == "find" {
        return segment.args().iter().any(|arg| arg == "-delete")
            || find_executes(segment).iter().any(|words| {
                words
                    .first()
                    .is_some_and(|word| is_delete_program(&program_name(word)))
            });
    }
    is_delete_program(&program)
}

fn is_delete_program(program: &str) -> bool {
    matches!(
        program,
        "rm" | "rmdir" | "unlink" | "shred" | "srm" | "del" | "erase" | "rd" | "remove-item" | "ri"
    )
}

/// The paths a delete command names, as written.
pub fn delete_targets(segment: &Segment) -> Vec<String> {
    if !is_delete(segment) {
        return Vec::new();
    }
    let program = segment.program();
    let args = segment.args();
    let mut targets = Vec::new();
    if program == "find" {
        // The starting points: after the options on symlinks and
        // optimisation (`-L`, `-H`, `-P`, `-O3`, `-D tree`), before the
        // first test or action; `.` when there are none.
        let mut i = 0;
        while let Some(arg) = args.get(i) {
            match arg.as_str() {
                "-H" | "-L" | "-P" => i += 1,
                "-D" => i += 2,
                _ if arg.starts_with("-O") => i += 1,
                _ => break,
            }
        }
        targets.extend(
            args[i.min(args.len())..]
                .iter()
                .take_while(|arg| !arg.starts_with('-') && *arg != "(" && *arg != "!")
                .cloned(),
        );
        if targets.is_empty() {
            targets.push(".".to_string());
        }
        return targets;
    }
    if program == "remove-item" || program == "ri" || (program == "rm" && windows_style(args)) {
        let mut i = 0;
        while let Some(arg) = args.get(i) {
            let lower = arg.to_lowercase();
            if lower == "-path" || lower == "-literalpath" {
                if let Some(value) = args.get(i + 1) {
                    targets.extend(value.split(',').map(|v| v.trim().to_string()));
                }
                i += 2;
            } else if matches!(
                lower.as_str(),
                "-filter" | "-include" | "-exclude" | "-credential" | "-stream"
            ) {
                i += 2;
            } else if arg.starts_with('-') {
                i += 1;
            } else {
                targets.extend(arg.split(',').map(|v| v.trim().to_string()));
                i += 1;
            }
        }
        return targets;
    }
    let cmd_switches = matches!(program.as_str(), "del" | "erase" | "rd");
    let mut options = true;
    for arg in args {
        if options && arg == "--" {
            options = false;
            continue;
        }
        if options && arg.starts_with('-') && arg != "-" {
            continue;
        }
        if cmd_switches && arg.len() == 2 && arg.starts_with('/') {
            continue;
        }
        targets.push(arg.clone());
    }
    targets
}

/// PowerShell's `rm` alias with its own parameters (`-Recurse`, `-Force`).
fn windows_style(args: &[String]) -> bool {
    args.iter().any(|arg| {
        let lower = arg.to_lowercase();
        matches!(
            lower.as_str(),
            "-recurse" | "-force" | "-path" | "-literalpath"
        )
    })
}

/// The paths a command writes that its text shows: redirections, `tee`,
/// the destination of `cp`/`mv`/`ln`/`install`/`rsync`, `mkdir`, `touch`,
/// `curl -o`, `wget -O`, and their PowerShell counterparts.
pub fn write_targets(segment: &Segment) -> Vec<String> {
    let mut targets = segment.redirects.clone();
    let program = segment.program();
    let args = segment.args();
    let positional = |skip_values: &[&str]| -> Vec<String> {
        let mut out = Vec::new();
        let mut i = 0;
        let mut options = true;
        while let Some(arg) = args.get(i) {
            if options && arg == "--" {
                options = false;
            } else if options && arg.starts_with('-') && arg != "-" {
                if skip_values.contains(&arg.as_str()) {
                    i += 1;
                }
            } else {
                out.push(arg.clone());
            }
            i += 1;
        }
        out
    };
    let value_of = |names: &[&str]| -> Option<String> {
        args.iter().enumerate().find_map(|(i, arg)| {
            let lower = arg.to_lowercase();
            if names.contains(&lower.as_str()) {
                return args.get(i + 1).cloned();
            }
            names.iter().find_map(|name| {
                lower
                    .strip_prefix(&format!("{name}="))
                    .map(|_| arg[name.len() + 1..].to_string())
            })
        })
    };
    match program.as_str() {
        "tee" => targets.extend(positional(&[])),
        "mkdir" | "md" | "touch" => targets.extend(positional(&["-m", "--mode", "-d", "-r", "-t"])),
        "cp" | "mv" | "ln" | "install" | "rsync" | "scp" | "copy" | "move" | "xcopy"
        | "robocopy" => {
            if let Some(dir) = value_of(&["-t", "--target-directory"]) {
                targets.push(dir);
            } else {
                let paths = positional(&["-m", "--mode", "-o", "-g", "-e", "-S", "--suffix"]);
                if program == "robocopy" {
                    targets.extend(paths.get(1).cloned());
                } else if paths.len() >= 2 {
                    targets.extend(paths.last().cloned());
                }
            }
        }
        "dd" => targets.extend(
            args.iter()
                .filter_map(|arg| arg.strip_prefix("of="))
                .map(str::to_string),
        ),
        "curl" => targets.extend(value_of(&["-o", "--output"])),
        "wget" => targets.extend(value_of(&["-o", "--output-document"]).or_else(|| {
            args.iter()
                .position(|arg| arg == "-O")
                .and_then(|i| args.get(i + 1).cloned())
        })),
        "new-item" | "ni" | "set-content" | "sc" | "add-content" | "ac" | "out-file" => {
            let value = value_of(&["-path", "-literalpath", "-filepath"]);
            match value {
                Some(path) => targets.push(path),
                None => targets.extend(args.iter().find(|arg| !arg.starts_with('-')).cloned()),
            }
        }
        "copy-item" | "cpi" | "move-item" | "mi" => match value_of(&["-destination"]) {
            Some(path) => targets.push(path),
            None => targets.extend(
                args.iter()
                    .filter(|arg| !arg.starts_with('-'))
                    .nth(1)
                    .cloned(),
            ),
        },
        _ => {}
    }
    targets.retain(|target| !target.is_empty() && !is_null_device(target));
    targets
}

/// `git push`: Some(true) when it forces (`--force`, `-f`,
/// `--force-with-lease`, a `+refspec`).
pub fn git_push(segment: &Segment) -> Option<bool> {
    if segment.program() != "git" {
        return None;
    }
    let args = segment.args();
    let mut i = 0;
    // git's own options before the subcommand.
    while let Some(arg) = args.get(i) {
        if matches!(
            arg.as_str(),
            "-C" | "-c" | "--git-dir" | "--work-tree" | "--namespace"
        ) {
            i += 2;
        } else if arg.starts_with('-') {
            i += 1;
        } else {
            break;
        }
    }
    // Windows finds `git PUSH` too (git-push.exe on a case-insensitive
    // file system).
    if !args
        .get(i)
        .is_some_and(|arg| arg.eq_ignore_ascii_case("push"))
    {
        return None;
    }
    let force = args[i + 1..].iter().any(|arg| {
        arg == "-f"
            || arg == "--force"
            || arg.starts_with("--force-with-lease")
            || arg == "--force-if-includes"
            || arg == "--mirror"
            || (arg.starts_with('-') && !arg.starts_with("--") && arg.contains('f'))
            || (arg.starts_with('+') && arg.len() > 1)
    });
    Some(force)
}

/// Whether `segment` changes system settings: services and startup items,
/// the registry under HKLM, the firewall or antivirus, the execution policy.
pub fn system_setting(segment: &Segment) -> Option<&'static str> {
    let program = segment.program();
    let args = segment.lower_args();
    let first = args
        .iter()
        .find(|arg| !arg.starts_with('-'))
        .map(String::as_str)
        .unwrap_or("");
    let any = |names: &[&str]| args.iter().any(|arg| names.contains(&arg.as_str()));
    let hklm = || {
        args.iter()
            .any(|arg| arg.starts_with("hklm") || arg.contains("hkey_local_machine"))
    };
    match program.as_str() {
        "reg"
            if matches!(
                first,
                "add" | "delete" | "import" | "load" | "unload" | "restore" | "copy"
            ) && hklm() =>
        {
            Some("changes the registry (HKLM)")
        }
        "set-itemproperty"
        | "new-itemproperty"
        | "remove-itemproperty"
        | "new-item"
        | "set-item"
        | "remove-item"
        | "rename-itemproperty"
        | "sp"
        | "ni"
            if hklm() =>
        {
            Some("changes the registry (HKLM)")
        }
        "launchctl" if !matches!(first, "list" | "print" | "help" | "version" | "") => {
            Some("changes launchd services")
        }
        "systemctl"
            if matches!(
                first,
                "enable"
                    | "disable"
                    | "start"
                    | "stop"
                    | "restart"
                    | "reload"
                    | "mask"
                    | "unmask"
                    | "daemon-reload"
                    | "edit"
                    | "set-default"
                    | "isolate"
                    | "kill"
                    | "link"
                    | "preset"
                    | "revert"
                    | "reboot"
                    | "poweroff"
            ) =>
        {
            Some("changes system services")
        }
        "service" if any(&["start", "stop", "restart", "reload"]) => {
            Some("changes system services")
        }
        "sc" if matches!(
            first,
            "create" | "config" | "delete" | "start" | "stop" | "failure" | "privs" | "sdset"
        ) =>
        {
            Some("changes Windows services")
        }
        "new-service" | "set-service" | "remove-service" | "start-service" | "stop-service"
        | "restart-service" | "suspend-service" => Some("changes Windows services"),
        "schtasks" if any(&["/create", "/delete", "/change", "/run"]) => {
            Some("changes scheduled tasks")
        }
        "register-scheduledtask" | "unregister-scheduledtask" | "set-scheduledtask" => {
            Some("changes scheduled tasks")
        }
        "update-rc.d" | "chkconfig" | "rc-update" => Some("changes startup services"),
        "crontab" if !any(&["-l"]) => Some("changes scheduled jobs"),
        "netsh" if any(&["advfirewall", "firewall"]) => Some("changes the firewall"),
        "ufw" | "iptables" | "ip6tables" | "nft" | "firewall-cmd" | "pfctl" => {
            Some("changes the firewall")
        }
        "new-netfirewallrule"
        | "set-netfirewallrule"
        | "remove-netfirewallrule"
        | "disable-netfirewallrule"
        | "enable-netfirewallrule"
        | "set-netfirewallprofile" => Some("changes the firewall"),
        "set-mppreference" | "add-mppreference" | "remove-mppreference" => {
            Some("changes antivirus settings")
        }
        "spctl" | "csrutil" => Some("changes macOS security settings"),
        "set-executionpolicy" => Some("changes the PowerShell execution policy"),
        _ => None,
    }
}

/// Commands that only move around, print or filter: a command line is
/// allowed by the rules when every other part is, and they do not count for
/// the prefix offered with an approval. Not with a redirection, nor with an
/// option that writes a file (`sort -o`, `uniq in out`) or runs a program
/// (`rg --pre`). `export` and `set` are not: they change what later parts
/// run (`export PATH=.:$PATH`).
pub fn is_neutral(segment: &Segment) -> bool {
    if !segment.redirects.is_empty() {
        return false;
    }
    let args = segment.lower_args();
    let positional = || args.iter().filter(|arg| !arg.starts_with('-')).count();
    match segment.program().as_str() {
        "sort" => !args.iter().any(|arg| {
            arg.starts_with("--output")
                || (arg.starts_with('-') && !arg.starts_with("--") && arg.contains('o'))
        }),
        "uniq" => positional() <= 1,
        "rg" | "grep" | "egrep" | "fgrep" => !args.iter().any(|arg| arg.starts_with("--pre")),
        "cd" | "pushd" | "popd" | "pwd" | "true" | "false" | ":" | "echo" | "printf" | "sleep"
        | "clear" | "cls" | "head" | "tail" | "wc" | "cut" | "tr" | "jq" | "less" | "more"
        | "cat" | "column" | "nl" | "test" | "[" | "[[" | "which" | "type" | "date"
        | "basename" | "dirname" | "write-output" | "write-host" | "select-object"
        | "select-string" | "format-table" | "format-list" | "out-string" | "measure-object"
        | "sort-object" | "where-object" => true,
        _ => false,
    }
}

/// The command prefix an approval for `segment` can remember: its words as
/// written up to the first word after the program that is not an option
/// (`npm install`, `npx -y create-vite`, `make`), at most four. None for a
/// part an allow rule would not match (`allow_words`) or with a word that
/// has white space in it.
pub fn prefix(segment: &Segment) -> Option<String> {
    if !segment.plain {
        return None;
    }
    let mut words: Vec<&str> = Vec::new();
    for word in &segment.words {
        if word.is_empty() || word.chars().any(char::is_whitespace) {
            return None;
        }
        words.push(word);
        if (words.len() > 1 && !word.starts_with('-')) || words.len() == 4 {
            break;
        }
    }
    (!words.is_empty()).then(|| words.join(" "))
}

/// The words an `allow` rule is matched against: the part as written,
/// wrappers such as `time` taken off only when nothing before the program
/// changes what runs (`Segment::plain`). `PATH=. git commit`, `env … git
/// commit` or `./git commit` do not match `git commit`.
pub fn allow_words(segment: &Segment) -> &[String] {
    if segment.plain {
        &segment.words
    } else {
        &segment.raw
    }
}

/// The absolute path `word` names, from `cwd`: `~`, `$HOME`, `${HOME}`,
/// `%USERPROFILE%` and `$env:USERPROFILE` expanded, `.` and `..` resolved,
/// a trailing glob taken as its directory (`~/*` is `~`). None for a word
/// that is not a path or uses another variable.
pub fn resolve_word(word: &str, cwd: &Path, home: Option<&Path>) -> Option<PathBuf> {
    let word = word.trim();
    if word.is_empty() || word.starts_with('-') || word.contains("://") {
        return None;
    }
    let mut text = word.to_string();
    // `~name`: that user's home directory. Only the current user's is
    // known; another's is not a path this can follow.
    if let Some(rest) = text.strip_prefix('~') {
        let end = rest.find(['/', '\\']).unwrap_or(rest.len());
        let (name, after) = rest.split_at(end);
        if !name.is_empty() {
            let home = home?;
            let own = home
                .file_name()
                .is_some_and(|own| own.to_string_lossy().eq_ignore_ascii_case(name));
            if !own {
                return None;
            }
            text = format!("{}{}", home.display(), after);
        }
    }
    let temp = std::env::temp_dir();
    for (variable, base) in [
        ("~", home),
        ("$HOME", home),
        ("${HOME}", home),
        ("%USERPROFILE%", home),
        ("$env:USERPROFILE", home),
        ("$env:HOME", home),
        ("$TMPDIR", Some(temp.as_path())),
        ("${TMPDIR}", Some(temp.as_path())),
        ("%TEMP%", Some(temp.as_path())),
        ("%TMP%", Some(temp.as_path())),
        ("$env:TEMP", Some(temp.as_path())),
        ("$env:TMP", Some(temp.as_path())),
    ] {
        let matches = if variable.starts_with('%') || variable.starts_with("$env") {
            text.to_lowercase().starts_with(&variable.to_lowercase())
        } else {
            text.starts_with(variable)
        };
        if matches {
            let rest = &text[variable.len()..];
            if !(rest.is_empty() || rest.starts_with('/') || rest.starts_with('\\')) {
                continue;
            }
            let base = base?;
            text = format!("{}{}", base.display(), rest);
            break;
        }
    }
    if text.contains('$') || text.contains('%') {
        return None;
    }
    // A glob names the directory it lists.
    if let Some(at) = text.find(['*', '?', '[']) {
        let before = &text[..at];
        let cut = before.rfind(['/', '\\']).map(|at| at + 1).unwrap_or(0);
        text = before[..cut].to_string();
        if text.is_empty() {
            text = ".".to_string();
        }
    }
    let path = PathBuf::from(&text);
    let path = if path.is_absolute() || has_drive(&text) {
        path
    } else {
        cwd.join(path)
    };
    Some(normalize(&path))
}

fn has_drive(text: &str) -> bool {
    let bytes = text.as_bytes();
    bytes.len() >= 2 && bytes[0].is_ascii_alphabetic() && bytes[1] == b':'
}

/// `.` and `..` resolved without the file system.
pub fn normalize(path: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for component in path.components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir => {
                if !out.pop() {
                    out.push("..");
                }
            }
            other => out.push(other),
        }
    }
    if out.as_os_str().is_empty() {
        out.push(".");
    }
    out
}

/// Whether `word` looks like a path (for the credential check).
pub fn looks_like_path(word: &str) -> bool {
    word.contains('/')
        || word.contains('\\')
        || word.starts_with('~')
        || word.starts_with('.')
        || word.starts_with('$')
        || word.starts_with('%')
}

#[cfg(test)]
mod tests {
    use super::*;

    fn words(command: &str) -> Vec<Vec<String>> {
        segments(command).into_iter().map(|s| s.words).collect()
    }

    fn strings(items: &[&str]) -> Vec<String> {
        items.iter().map(|item| item.to_string()).collect()
    }

    #[test]
    fn a_command_line_splits_into_its_simple_commands() {
        assert_eq!(
            words("cd app && npm install || echo failed; make | tail -5 & sleep 1"),
            vec![
                strings(&["cd", "app"]),
                strings(&["npm", "install"]),
                strings(&["echo", "failed"]),
                strings(&["make"]),
                strings(&["tail", "-5"]),
                strings(&["sleep", "1"]),
            ]
        );
        // Quotes keep separators; escapes and Windows paths stay whole.
        assert_eq!(
            words(r#"echo "a && b" 'c; d' e\;f C:\Users\me"#),
            vec![strings(&["echo", "a && b", "c; d", "e;f", r"C:\Users\me"])]
        );
        // Command substitutions are commands too.
        assert_eq!(
            words("echo $(rm -rf build) `whoami`"),
            vec![
                strings(&["echo"]),
                strings(&["rm", "-rf", "build"]),
                strings(&["whoami"])
            ]
        );
    }

    #[test]
    fn redirections_are_targets_not_words() {
        let parts = segments("cargo test 2>&1 > out.log 2>>err.log < in.txt | tee copy.txt");
        assert_eq!(parts[0].words, strings(&["cargo", "test"]));
        assert_eq!(parts[0].redirects, strings(&["out.log", "err.log"]));
        assert_eq!(write_targets(&parts[1]), strings(&["copy.txt"]));
        let null = segments("make >/dev/null 2>&1 &> /dev/null");
        assert!(null[0].redirects.is_empty(), "{null:?}");
    }

    #[test]
    fn here_documents_are_data() {
        let parts = segments("cat <<'EOF' > notes.txt\nrm -rf /\nsudo reboot\nEOF\nls");
        assert_eq!(parts.len(), 2, "{parts:?}");
        assert_eq!(parts[0].redirects, strings(&["notes.txt"]));
        assert_eq!(parts[1].words, strings(&["ls"]));
    }

    #[test]
    fn wrappers_come_off_and_elevation_is_kept() {
        let parts = segments("sudo -u root env FOO=1 nice -n 5 time rm -rf /tmp/x");
        assert_eq!(parts[0].words, strings(&["rm", "-rf", "/tmp/x"]));
        assert!(parts[0].elevated);
        let parts = segments("FOO=bar command npm install");
        assert_eq!(parts[0].words, strings(&["npm", "install"]));
        assert!(!parts[0].elevated);
        assert!(segments("command -v rm")[0].words.is_empty());
        assert!(segments("/usr/bin/doas make install")[0].elevated);
        assert!(segments("Start-Process cmd -Verb RunAs")[0].elevated);
        let su = segments("su -c 'rm -rf /' root");
        assert!(su.iter().all(|part| part.elevated));
        assert_eq!(su[1].words, strings(&["rm", "-rf", "/"]));
    }

    #[test]
    fn inline_shell_code_is_read_and_marked() {
        let parts = segments(r#"bash -lc "git push --force && echo done""#);
        assert!(parts[0].inline_code && is_interpreter(&parts[0]));
        assert_eq!(git_push(&parts[1]), Some(true));
        let parts = segments(r#"powershell -NoProfile -Command "Remove-Item -Recurse C:\x""#);
        assert!(is_interpreter(&parts[0]));
        assert_eq!(delete_targets(&parts[1]), strings(&[r"C:\x"]));
        assert!(segments("pwsh -EncodedCommand ZQBjAGgAbwA=")[0].inline_code);
        assert!(segments("cmd /c del /q build")[0].inline_code);
        // A shell given a script runs whatever the script says.
        assert!(is_interpreter(&segments("bash build.sh")[0]));
        assert!(is_interpreter(&segments("pwsh -File build.ps1")[0]));
        assert!(is_interpreter(&segments("source env.sh")[0]));
        assert!(!is_interpreter(&segments("make build")[0]));
        assert!(is_interpreter(&segments("python3 -m pip install x")[0]));
    }

    #[test]
    fn deletes_writes_and_pushes_are_recognized() {
        let rm = &segments("rm -rf -- -weird build/ ~/Desktop/a.txt")[0];
        assert_eq!(
            delete_targets(rm),
            strings(&["-weird", "build/", "~/Desktop/a.txt"])
        );
        let del = &segments(r"del /s /q C:\temp\x")[0];
        assert_eq!(delete_targets(del), strings(&[r"C:\temp\x"]));
        // PowerShell: a backslash before a space or a quote ends the path.
        let targets = |command: &str| -> Vec<String> {
            segments(command).iter().flat_map(delete_targets).collect()
        };
        assert!(targets(r"Remove-Item C:\ -Recurse -Force").contains(&r"C:\".to_string()));
        assert!(targets(r#"Remove-Item "C:\Users\me\" -Recurse"#)
            .contains(&r"C:\Users\me\".to_string()));
        // Read the POSIX way too: an escaped quote does not hide what runs.
        let parts = segments(r#"echo "C:\" ; echo "; rm -rf ~"#);
        assert!(parts.iter().any(|part| part.program() == "rm"), "{parts:?}");
        let parts = segments(r#"ls C:\" ; rm -rf ~"#);
        assert!(parts.iter().any(|part| part.program() == "rm"), "{parts:?}");
        assert_eq!(
            write_targets(&segments("cp -r src ~/Desktop/out")[0]),
            strings(&["~/Desktop/out"])
        );
        assert_eq!(
            write_targets(&segments("mkdir -p a/b c")[0]),
            strings(&["a/b", "c"])
        );
        assert_eq!(
            write_targets(&segments("curl -fsSL -o ~/bin/tool https://x")[0]),
            strings(&["~/bin/tool"])
        );
        assert_eq!(
            write_targets(&segments(r"Copy-Item a.txt -Destination C:\Users\me\Desktop")[0]),
            strings(&[r"C:\Users\me\Desktop"])
        );
        assert_eq!(git_push(&segments("git push origin main")[0]), Some(false));
        assert_eq!(git_push(&segments("git -C repo push -f")[0]), Some(true));
        assert_eq!(git_push(&segments("git push origin +main")[0]), Some(true));
        assert_eq!(git_push(&segments("git pull")[0]), None);
    }

    #[test]
    fn system_settings_are_recognized() {
        for command in [
            r"reg add HKLM\Software\X /v Y /d 1",
            "Set-ItemProperty -Path HKLM:\\Software\\X -Name Y -Value 1",
            "launchctl load ~/Library/LaunchAgents/x.plist",
            "systemctl enable nginx",
            "sc.exe create svc binPath= C:\\x.exe",
            "netsh advfirewall set allprofiles state off",
            "Set-MpPreference -DisableRealtimeMonitoring $true",
            "Set-ExecutionPolicy Unrestricted",
            "crontab mycron",
        ] {
            assert!(system_setting(&segments(command)[0]).is_some(), "{command}");
        }
        for command in [
            "reg query HKLM\\Software",
            "systemctl status nginx",
            "launchctl list",
            "crontab -l",
        ] {
            assert!(system_setting(&segments(command)[0]).is_none(), "{command}");
        }
    }

    #[test]
    fn words_resolve_to_paths() {
        let cwd = Path::new("/work/app");
        let home = Some(Path::new("/home/me"));
        assert_eq!(
            resolve_word("~/Desktop/x", cwd, home),
            Some(PathBuf::from("/home/me/Desktop/x"))
        );
        assert_eq!(
            resolve_word("$HOME", cwd, home),
            Some(PathBuf::from("/home/me"))
        );
        assert_eq!(
            resolve_word("~/*", cwd, home),
            Some(PathBuf::from("/home/me"))
        );
        assert_eq!(resolve_word("/*", cwd, home), Some(PathBuf::from("/")));
        assert_eq!(resolve_word("..", cwd, home), Some(PathBuf::from("/work")));
        assert_eq!(
            resolve_word("build/*.o", cwd, home),
            Some(PathBuf::from("/work/app/build"))
        );
        assert_eq!(resolve_word("$OUT/x", cwd, home), None);
        assert_eq!(resolve_word("-rf", cwd, home), None);
        // `~name` is a home directory, not a file in the working directory.
        assert_eq!(resolve_word("~user/x", cwd, home), None);
        assert_eq!(
            resolve_word("~me/x", cwd, home),
            Some(PathBuf::from("/home/me/x"))
        );
        assert_eq!(
            resolve_word("~me", cwd, home),
            Some(PathBuf::from("/home/me"))
        );
        assert_eq!(
            resolve_word("$TMPDIR/x", cwd, home),
            Some(normalize(&std::env::temp_dir().join("x")))
        );
    }

    #[test]
    fn prefixes_take_the_program_and_its_first_word() {
        assert_eq!(
            prefix(&segments("npm install lodash")[0]).as_deref(),
            Some("npm install")
        );
        assert_eq!(prefix(&segments("make")[0]).as_deref(), Some("make"));
        // Options do not end it: `npx -y` would allow any package.
        assert_eq!(
            prefix(&segments("npx -y create-vite app")[0]).as_deref(),
            Some("npx -y create-vite")
        );
        assert_eq!(prefix(&segments("ls -la")[0]).as_deref(), Some("ls -la"));
        assert_eq!(
            prefix(&segments("time cargo test -q")[0]).as_deref(),
            Some("cargo test")
        );
        // A part an allow rule would not match offers none.
        for command in [
            "sudo apt install x",
            "PATH=. git commit -m x",
            "env LD_PRELOAD=/tmp/x.so npm install",
            "git commit -m 'a b'",
        ] {
            let part = segments(command).remove(0);
            if command.starts_with("git commit -m") {
                assert_eq!(prefix(&part).as_deref(), Some("git commit"), "{command}");
            } else {
                assert_eq!(prefix(&part), None, "{command}");
            }
        }
        assert!(is_neutral(&segments("cd app")[0]));
        assert!(!is_neutral(&segments("echo x > f")[0]));
    }

    #[test]
    fn allow_words_keep_what_changes_the_program() {
        let words = |command: &str| allow_words(&segments(command)[0]).to_vec();
        assert_eq!(
            words("git commit -m x"),
            strings(&["git", "commit", "-m", "x"])
        );
        assert_eq!(words("nohup git commit"), strings(&["git", "commit"]));
        assert_eq!(words("./git commit"), strings(&["./git", "commit"]));
        assert_eq!(
            words("PATH=. git commit"),
            strings(&["PATH=.", "git", "commit"])
        );
        assert_eq!(
            words("env GIT_DIR=x git commit"),
            strings(&["env", "GIT_DIR=x", "git", "commit"])
        );
        assert_eq!(
            words("xargs git commit"),
            strings(&["xargs", "git", "commit"])
        );
    }

    #[test]
    fn neutral_parts_neither_write_nor_run_code() {
        for command in ["cd app", "grep -n x f", "sort f", "uniq f", "tail -f log"] {
            assert!(is_neutral(&segments(command)[0]), "{command}");
        }
        for command in [
            "export PATH=.:$PATH",
            "set PATH=C:\\evil",
            "sort -o ~/.zshrc f",
            "sort -uo out f",
            "uniq in.txt ~/.bashrc",
            "rg --pre ./x pattern",
            "echo x >> f",
        ] {
            assert!(!is_neutral(&segments(command)[0]), "{command}");
        }
    }

    #[test]
    fn script_blocks_and_groups_are_commands() {
        let parts =
            segments("Get-ChildItem C:\\x | ForEach-Object { Remove-Item $_.FullName -Recurse }");
        assert!(parts.iter().any(is_delete), "{parts:?}");
        let parts = segments("if ($x) {Remove-Item C:\\ -Recurse}");
        assert!(parts.iter().any(is_delete), "{parts:?}");
        let parts = segments("{ git push; }");
        assert_eq!(git_push(&parts[0]), Some(false));
        // Braces inside a word stay text.
        assert_eq!(
            words("cp src/{a,b}.c ${HOME}/x @{a=1}"),
            vec![strings(&["cp", "src/{a,b}.c", "${HOME}/x", "@{a=1}"])]
        );
        assert_eq!(
            words("find . -name x -exec grep -l y {} +"),
            vec![
                strings(&["find", ".", "-name", "x", "-exec", "grep", "-l", "y", "{}", "+"]),
                strings(&["grep", "-l", "y"]),
            ]
        );
    }

    #[test]
    fn find_can_delete_and_run_commands() {
        let delete = &segments("find ~ -name '*.log' -delete")[0];
        assert!(is_delete(delete));
        assert_eq!(delete_targets(delete), strings(&["~"]));
        let follow = &segments("find -L -O2 ~ /srv -delete")[0];
        assert_eq!(delete_targets(follow), strings(&["~", "/srv"]));
        let exec = segments("find /srv build -exec rm -rf {} +");
        assert!(is_delete(&exec[0]));
        assert_eq!(delete_targets(&exec[0]), strings(&["/srv", "build"]));
        assert_eq!(exec.len(), 1, "the rm is the find's: {exec:?}");
        assert!(!is_delete(&segments("find . -name x")[0]));
        let pushed = segments("find . -maxdepth 0 -exec git push \\;");
        assert_eq!(git_push(&pushed[1]), Some(false));
        assert!(segments("find . -exec sh -c 'x' \\;")[1].inline_code);
    }

    #[test]
    fn a_push_is_found_in_any_case() {
        for command in [
            "GIT PUSH origin",
            "git.exe Push",
            r"C:\Git\cmd\git.EXE push -F",
        ] {
            assert!(git_push(&segments(command)[0]).is_some(), "{command}");
        }
    }
}
